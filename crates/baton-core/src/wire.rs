//! The `baton.v1` wire format of `docs/protocol.md`: frame codec, control messages and the reassembly of
//! split objects. It does no I/O, so the server and the daemon read and write the same bytes.

use crate::error::{Error, Result};
use crate::id::{
    AgentName, MachineId, ModelName, Oid, ProjectKey, ThreadId, TurnId, TurnSeq, UnixMillis,
};
use crate::store::TurnRecord;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const SUBPROTOCOL: &str = "baton.v1";
pub const PROTO: u32 = 1;
pub const CHUNK: usize = 1 << 20;
pub const MAX_FRAME: usize = CHUNK + 1024;
pub const MAX_OBJECT: u64 = 64 << 20;
/// New objects up to this many bytes travel ahead of a `moved`.
pub const INLINE_MAX: usize = 256 << 10;
/// Ids per `want`, `need` or `heads` message, so a control frame stays far below `MAX_FRAME`.
pub const IDS_PER_MESSAGE: usize = 2048;

const KIND_CONTROL: u8 = 0;
const KIND_OBJECT: u8 = 1;
const OBJECT_HEADER: usize = 1 + 4 + 1 + 20 + 8 + 8;
const MAX_PARTIAL: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Commit,
    Tree,
    Blob,
}

impl ObjectKind {
    fn byte(self) -> u8 {
        match self {
            Self::Commit => 1,
            Self::Tree => 2,
            Self::Blob => 3,
        }
    }

    fn from_byte(b: u8) -> Result<Self> {
        match b {
            1 => Ok(Self::Commit),
            2 => Ok(Self::Tree),
            3 => Ok(Self::Blob),
            other => Err(Error::invalid("object type", other)),
        }
    }

    pub(crate) fn gix(self) -> gix::objs::Kind {
        match self {
            Self::Commit => gix::objs::Kind::Commit,
            Self::Tree => gix::objs::Kind::Tree,
            Self::Blob => gix::objs::Kind::Blob,
        }
    }

    pub(crate) fn from_gix(kind: gix::objs::Kind) -> Option<Self> {
        match kind {
            gix::objs::Kind::Commit => Some(Self::Commit),
            gix::objs::Kind::Tree => Some(Self::Tree),
            gix::objs::Kind::Blob => Some(Self::Blob),
            gix::objs::Kind::Tag => None,
        }
    }
}

/// The id git gives bytes of this kind.
pub fn hash_object(kind: ObjectKind, bytes: &[u8]) -> Result<Oid> {
    gix::objs::compute_hash(gix::hash::Kind::Sha1, kind.gix(), bytes)
        .map(Oid::from_gix)
        .map_err(Error::git)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefuseReason {
    Stale,
    NotOwner,
    MachineMismatch,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_frame: usize,
    pub max_object: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame: MAX_FRAME,
            max_object: MAX_OBJECT,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub project: ProjectKey,
    pub thread: ThreadId,
    pub tip: TurnId,
    pub owner: MachineId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub turn: TurnSeq,
    pub subject: String,
    pub agent: AgentName,
    #[serde(default)]
    pub model: Option<ModelName>,
    pub machine: MachineId,
    pub at: UnixMillis,
}

impl Meta {
    pub fn of(record: &TurnRecord) -> Self {
        let draft = &record.turn.draft;
        Self {
            turn: record.turn.seq,
            subject: draft.subject.as_str().to_owned(),
            agent: draft.agent.clone(),
            model: draft.model.clone(),
            machine: draft.machine.clone(),
            at: record.at,
        }
    }
}

/// Both directions in one enum, tagged by `"t"`. A tag this version does not know decodes as `Unknown`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Control {
    Hello {
        proto: u32,
        #[serde(default)]
        machine: String,
        #[serde(default)]
        projects: Vec<ProjectKey>,
        #[serde(default)]
        client: String,
    },
    Advance {
        xfer: u32,
        project: ProjectKey,
        thread: ThreadId,
        expected: Option<TurnId>,
        new: TurnId,
    },
    Want {
        xfer: u32,
        project: ProjectKey,
        ids: Vec<Oid>,
    },
    Subscribe {
        project: ProjectKey,
    },
    Welcome {
        proto: u32,
        machine: MachineId,
        limits: Limits,
        heads: Vec<Head>,
    },
    Heads {
        heads: Vec<Head>,
    },
    Advanced {
        xfer: u32,
        thread: ThreadId,
        new: TurnId,
    },
    Need {
        xfer: u32,
        thread: ThreadId,
        ids: Vec<Oid>,
    },
    Refused {
        xfer: u32,
        thread: ThreadId,
        reason: RefuseReason,
        current: Option<TurnId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Moved {
        xfer: u32,
        project: ProjectKey,
        thread: ThreadId,
        old: Option<TurnId>,
        new: TurnId,
        meta: Meta,
    },
    ObjectsSent {
        xfer: u32,
    },
    Error {
        code: String,
        message: String,
    },
    #[serde(other)]
    Unknown,
}

pub struct ObjectChunk<'a> {
    pub xfer: u32,
    pub kind: ObjectKind,
    pub id: Oid,
    pub total: u64,
    pub offset: u64,
    pub payload: &'a [u8],
}

pub enum Frame<'a> {
    Control(Control),
    Object(ObjectChunk<'a>),
}

pub fn encode_control(message: &Control) -> Vec<u8> {
    let mut out = vec![KIND_CONTROL];
    serde_json::to_writer(&mut out, message).expect("control messages serialize");
    out
}

/// The frames that carry one object: a single frame, or several of at most `CHUNK` payload bytes.
pub fn encode_object<'a>(
    xfer: u32,
    kind: ObjectKind,
    id: Oid,
    bytes: &'a [u8],
) -> impl Iterator<Item = Vec<u8>> + 'a {
    let total = bytes.len() as u64;
    let mut chunks: Vec<&[u8]> = bytes.chunks(CHUNK).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    let mut offset = 0u64;
    chunks.into_iter().map(move |payload| {
        let mut out = Vec::with_capacity(OBJECT_HEADER + payload.len());
        out.push(KIND_OBJECT);
        out.extend_from_slice(&xfer.to_le_bytes());
        out.push(kind.byte());
        out.extend_from_slice(id.raw());
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(payload);
        offset += payload.len() as u64;
        out
    })
}

pub fn decode(bytes: &[u8]) -> Result<Frame<'_>> {
    match bytes.first() {
        Some(&KIND_CONTROL) => serde_json::from_slice(&bytes[1..])
            .map(Frame::Control)
            .map_err(|e| Error::invalid("control message", e)),
        Some(&KIND_OBJECT) => {
            if bytes.len() < OBJECT_HEADER {
                return Err(Error::invalid("object frame", "shorter than its header"));
            }
            let le32 = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4"));
            let le64 = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8"));
            Ok(Frame::Object(ObjectChunk {
                xfer: le32(1),
                kind: ObjectKind::from_byte(bytes[5])?,
                id: Oid::from_raw(&bytes[6..26])?,
                total: le64(26),
                offset: le64(34),
                payload: &bytes[OBJECT_HEADER..],
            }))
        }
        Some(other) => Err(Error::invalid("frame kind", other)),
        None => Err(Error::invalid("frame", "empty")),
    }
}

pub struct Object {
    pub xfer: u32,
    pub kind: ObjectKind,
    pub id: Oid,
    pub bytes: Vec<u8>,
}

struct Partial {
    kind: ObjectKind,
    total: u64,
    bytes: Vec<u8>,
}

/// Joins the frames of split objects and checks each finished object against its id. A disconnect drops
/// the assembler, which discards what was partial.
pub struct Assembler {
    max_object: u64,
    partial: HashMap<(u32, Oid), Partial>,
}

impl Assembler {
    pub fn new(max_object: u64) -> Self {
        Self {
            max_object,
            partial: HashMap::new(),
        }
    }

    /// What unfinished objects announced they will hold. A buffer grows only as bytes arrive, so a peer
    /// that announces a lot and sends little costs little; callers count this against their own limit.
    pub fn announced(&self) -> u64 {
        self.partial.values().map(|p| p.total).sum()
    }

    pub fn push(&mut self, chunk: ObjectChunk<'_>) -> Result<Option<Object>> {
        let ObjectChunk {
            xfer,
            kind,
            id,
            total,
            offset,
            payload,
        } = chunk;
        if total > self.max_object {
            return Err(Error::invalid(
                "object",
                format!("{total} bytes is over the limit of {}", self.max_object),
            ));
        }
        if payload.len() > CHUNK || offset.saturating_add(payload.len() as u64) > total {
            return Err(Error::invalid(
                "object frame",
                "payload overruns the object",
            ));
        }
        let key = (xfer, id);
        let bytes = if offset == 0 && payload.len() as u64 == total {
            payload.to_vec()
        } else {
            if !self.partial.contains_key(&key) {
                if offset != 0 || self.partial.len() >= MAX_PARTIAL {
                    return Err(Error::invalid("object frame", "out of order"));
                }
                self.partial.insert(
                    key,
                    Partial {
                        kind,
                        total,
                        bytes: Vec::new(),
                    },
                );
            }
            let entry = self.partial.get_mut(&key).expect("inserted above");
            if entry.kind != kind || entry.total != total || entry.bytes.len() as u64 != offset {
                return Err(Error::invalid(
                    "object frame",
                    "inconsistent with its first",
                ));
            }
            entry.bytes.extend_from_slice(payload);
            if (entry.bytes.len() as u64) < total {
                return Ok(None);
            }
            self.partial
                .remove(&key)
                .map(|p| p.bytes)
                .unwrap_or_default()
        };
        if hash_object(kind, &bytes)? != id {
            return Err(Error::invalid(
                "object",
                format!("bytes do not hash to {id}"),
            ));
        }
        Ok(Some(Object {
            xfer,
            kind,
            id,
            bytes,
        }))
    }
}
