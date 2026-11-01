//! The report's cache of per-change statistics, one file per project under baton's data dir.
//!
//! Every key is an immutable object id (a turn commit, a user commit, a thread tip), so an entry never
//! goes stale; deleting the file only costs time. The file is a header and then records, each `magic`,
//! `u32 length`, payload. A reader keeps the whole file in memory and stops at the first record that does
//! not parse, so a torn tail loses entries and nothing else; the next save rewrites the file from the
//! records that did parse. The version byte changes whenever the definition of a statistic does.

use super::stats::{FileStat, Status};
use gix::ObjectId;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const HEADER: &[u8; 8] = b"BATONRP\x01";
const MAGIC: u32 = 0xBA70_5EC0;

/// What an entry describes. The same object id means different statistics under each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    /// A turn against its parent turn, or a first turn against its base.
    Turn = 1,
    /// A user commit against its first parent.
    Commit = 2,
    /// A thread's last tree against its base.
    Tip = 3,
}

pub(super) type Key = (Kind, ObjectId);

pub(super) struct Cache {
    path: PathBuf,
    bytes: Vec<u8>,
    index: HashMap<Key, (usize, usize)>,
    /// Where the parsed records end, or 0 when the file must be written from scratch.
    valid_end: usize,
    rewrite: bool,
    fresh: Vec<u8>,
}

impl Cache {
    /// Never fails: a missing, foreign or damaged file is an empty cache.
    pub(super) fn open(path: PathBuf) -> Self {
        let bytes = std::fs::read(&path).unwrap_or_default();
        let existed = !bytes.is_empty();
        let mut cache = Self {
            path,
            bytes: Vec::new(),
            index: HashMap::new(),
            valid_end: 0,
            rewrite: existed,
            fresh: Vec::new(),
        };
        if bytes.starts_with(HEADER) {
            cache.bytes = bytes;
            cache.scan();
        }
        cache
    }

    fn scan(&mut self) {
        let mut pos = HEADER.len();
        let bytes = &self.bytes;
        while pos + 8 <= bytes.len() {
            let magic = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap_or_default());
            let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap_or_default());
            let (start, end) = (pos + 8, pos + 8 + len as usize);
            if magic != MAGIC || end > bytes.len() || len < 21 {
                break;
            }
            let Some(kind) = kind_of(bytes[start]) else {
                break;
            };
            let Ok(id) = ObjectId::try_from(&bytes[start + 1..start + 21]) else {
                break;
            };
            self.index.insert((kind, id), (start + 21, end));
            pos = end;
        }
        self.valid_end = pos;
        self.rewrite = pos != bytes.len();
    }

    pub(super) fn get(&self, key: &Key) -> Option<Vec<FileStat>> {
        let &(start, end) = self.index.get(key)?;
        decode(&self.bytes[start..end])
    }

    pub(super) fn put(&mut self, key: &Key, files: &[FileStat]) {
        let payload = encode(key, files);
        self.fresh.extend_from_slice(&MAGIC.to_le_bytes());
        self.fresh.extend_from_slice(
            &u32::try_from(payload.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        self.fresh.extend_from_slice(&payload);
    }

    /// Best effort: a cache that cannot be written only makes the next run slower.
    pub(super) fn save(&self) {
        if self.fresh.is_empty() && !self.rewrite {
            return;
        }
        let _ = self.write();
    }

    fn write(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if self.valid_end >= HEADER.len() && !self.rewrite {
            let mut file = std::fs::OpenOptions::new().append(true).open(&self.path)?;
            return file.write_all(&self.fresh);
        }
        let temp = self
            .path
            .with_extension(format!("tmp{}", std::process::id()));
        let mut out = Vec::with_capacity(HEADER.len() + self.valid_end + self.fresh.len());
        out.extend_from_slice(HEADER);
        if self.valid_end > HEADER.len() {
            out.extend_from_slice(&self.bytes[HEADER.len()..self.valid_end]);
        }
        out.extend_from_slice(&self.fresh);
        std::fs::write(&temp, out)?;
        std::fs::rename(&temp, &self.path).inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
    }
}

fn kind_of(byte: u8) -> Option<Kind> {
    match byte {
        1 => Some(Kind::Turn),
        2 => Some(Kind::Commit),
        3 => Some(Kind::Tip),
        _ => None,
    }
}

fn put_u32(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes());
}

fn encode(key: &Key, files: &[FileStat]) -> Vec<u8> {
    let mut out = vec![key.0 as u8];
    out.extend_from_slice(key.1.as_bytes());
    put_u32(&mut out, files.len());
    for f in files {
        put_u32(&mut out, f.path.len());
        out.extend_from_slice(f.path.as_bytes());
        out.push(match f.status {
            Status::Added => 0,
            Status::Modified => 1,
            Status::Removed => 2,
        });
        out.push(u8::from(f.lines));
        out.extend_from_slice(&f.added_n.to_le_bytes());
        out.extend_from_slice(&f.removed_n.to_le_bytes());
        out.extend_from_slice(&f.to);
        for set in [&f.added, &f.removed] {
            put_u32(&mut out, set.len());
            for h in set {
                out.extend_from_slice(&h.to_le_bytes());
            }
        }
    }
    out
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn hashes(&mut self) -> Option<Vec<u64>> {
        let n = self.u32()? as usize;
        let raw = self.take(n.checked_mul(8)?)?;
        Some(
            raw.as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_le_bytes(*c))
                .collect(),
        )
    }
}

/// `payload` starts after the key.
fn decode(payload: &[u8]) -> Option<Vec<FileStat>> {
    let mut r = Reader {
        bytes: payload,
        pos: 0,
    };
    let count = r.u32()? as usize;
    let mut files = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let path_len = r.u32()? as usize;
        let path = String::from_utf8(r.take(path_len)?.to_vec()).ok()?;
        let status = match r.u8()? {
            0 => Status::Added,
            1 => Status::Modified,
            2 => Status::Removed,
            _ => return None,
        };
        let lines = r.u8()? != 0;
        let added_n = r.u32()?;
        let removed_n = r.u32()?;
        let to: [u8; 20] = r.take(20)?.try_into().ok()?;
        let added = r.hashes()?;
        let removed = r.hashes()?;
        files.push(FileStat {
            path,
            status,
            lines,
            added_n,
            removed_n,
            to,
            added,
            removed,
        });
    }
    Some(files)
}

/// Where a project's cache lives.
pub(super) fn path_in(dir: &Path, project_dir_name: &str) -> PathBuf {
    dir.join(format!("{project_dir_name}.bin"))
}
