use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::num::NonZeroU32;

fn windows_reserved(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or("");
    let lower = stem.to_ascii_lowercase();
    matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || (lower.len() == 4
            && (lower.starts_with("com") || lower.starts_with("lpt"))
            && matches!(lower.as_bytes()[3], b'1'..=b'9'))
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,95}$`, and safe as a git ref component and a Windows file name.
fn ident(s: &str) -> std::result::Result<(), String> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 96 {
        return Err("must be 1 to 96 characters".into());
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return Err("must start with a letter or digit".into());
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("allowed characters are letters, digits, '.', '_' and '-'".into());
    }
    if s.contains("..") || s.ends_with('.') || s.ends_with(".lock") {
        return Err("must not contain '..' or end with '.' or '.lock'".into());
    }
    if windows_reserved(s) {
        return Err("is a reserved device name on Windows".into());
    }
    Ok(())
}

fn model(s: &str) -> std::result::Result<(), String> {
    if s.is_empty() || s.len() > 200 {
        return Err("must be 1 to 200 bytes".into());
    }
    if s.chars().any(char::is_control) || s != s.trim() {
        return Err("must be one line without control characters or edge spaces".into());
    }
    Ok(())
}

fn project(s: &str) -> std::result::Result<(), String> {
    if s.is_empty() || s.len() > 200 {
        return Err("must be 1 to 200 bytes".into());
    }
    for segment in s.split('/') {
        if segment.is_empty() || segment.starts_with('.') || segment.ends_with('.') {
            return Err("segments must be non-empty and not start or end with '.'".into());
        }
        if !segment.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        }) {
            return Err("allowed characters are a-z, 0-9, '.', '_', '-' and '/'".into());
        }
        if segment.contains("..") || windows_reserved(segment) {
            return Err("segment is not portable".into());
        }
    }
    Ok(())
}

macro_rules! text_id {
    ($(#[$meta:meta])* $name:ident, $what:literal, $check:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(Box<str>);

        impl $name {
            pub fn parse(s: &str) -> Result<Self> {
                let check: fn(&str) -> std::result::Result<(), String> = $check;
                check(s).map_err(|reason| Error::invalid($what, format!("{s:?} {reason}")))?;
                Ok(Self(s.into()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(s: String) -> Result<Self> {
                Self::parse(&s)
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.0.into()
            }
        }
    };
}

text_id!(
    /// The agent's session id. The ref name `refs/baton/<thread>` is derived from it in `turn_ref` only.
    ThreadId, "thread id", ident
);
text_id!(MachineId, "machine id", ident);
text_id!(AgentName, "agent name", ident);
text_id!(CheckName, "check name", ident);
text_id!(ModelName, "model name", model);
text_id!(
    /// Lowercase `host/path`, from `.baton/config.toml` or the normalized `origin` URL.
    ProjectKey, "project key", project
);

impl ThreadId {
    pub fn turn_ref(&self) -> String {
        format!("refs/baton/{}", self.0)
    }
}

impl ProjectKey {
    /// One directory name per project, so keys never nest and a key cannot collide with a device name.
    pub fn dir_name(&self) -> String {
        format!("{}.git", self.0.replace('/', "+"))
    }
}

impl MachineId {
    pub fn from_host() -> Self {
        let raw = ["COMPUTERNAME", "HOSTNAME"]
            .iter()
            .find_map(|k| std::env::var(k).ok())
            .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
            .unwrap_or_default();
        let cleaned: String = raw
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        Self::parse(cleaned.trim_matches(|c| c == '.' || c == '-'))
            .unwrap_or_else(|_| Self("unknown".into()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct TurnSeq(NonZeroU32);

impl TurnSeq {
    pub fn first() -> Self {
        Self(NonZeroU32::MIN)
    }

    pub fn parse(n: u32) -> Result<Self> {
        NonZeroU32::new(n)
            .map(Self)
            .ok_or_else(|| Error::invalid("turn sequence", "must be at least 1"))
    }

    pub fn next(self) -> Result<Self> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| Error::invalid("turn sequence", "overflow"))
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl TryFrom<u32> for TurnSeq {
    type Error = Error;
    fn try_from(n: u32) -> Result<Self> {
        Self::parse(n)
    }
}

impl From<TurnSeq> for u32 {
    fn from(s: TurnSeq) -> u32 {
        s.get()
    }
}

impl fmt::Display for TurnSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMillis(i64);

impl UnixMillis {
    pub fn from_secs(secs: i64) -> Self {
        Self(secs.saturating_mul(1000))
    }

    pub fn now() -> Self {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Self(ms)
    }

    pub fn millis(self) -> i64 {
        self.0
    }

    pub fn secs(self) -> i64 {
        self.0.div_euclid(1000)
    }
}

macro_rules! object_id {
    ($(#[$meta:meta])* $name:ident, $what:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(gix::ObjectId);

        impl $name {
            pub fn parse(hex: &str) -> Result<Self> {
                if hex.len() != 40 {
                    return Err(Error::invalid($what, format!("{hex:?} is not 40 hex digits")));
                }
                gix::ObjectId::from_hex(hex.as_bytes())
                    .map(Self)
                    .map_err(|e| Error::invalid($what, format!("{hex:?}: {e}")))
            }

            pub(crate) fn from_gix(id: gix::ObjectId) -> Self {
                Self(id)
            }

            #[allow(dead_code)]
            pub(crate) fn oid(&self) -> gix::ObjectId {
                self.0
            }

            pub fn short(&self) -> String {
                self.0.to_hex_with_len(8).to_string()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(s: String) -> Result<Self> {
                Self::parse(&s)
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.to_string()
            }
        }
    };
}

object_id!(
    /// Any object in a side repo. Wire frames and `want` name objects by this.
    Oid, "object id"
);

impl Oid {
    pub fn from_raw(raw: &[u8]) -> Result<Self> {
        gix::ObjectId::try_from(raw)
            .map(Self)
            .map_err(|_| Error::invalid("object id", format!("{} bytes, not 20", raw.len())))
    }

    pub fn raw(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

object_id!(
    /// A turn is a commit, so its id is the commit id.
    TurnId, "turn id"
);
object_id!(TreeId, "tree id");

impl TreeId {
    pub fn empty() -> Self {
        Self(gix::ObjectId::empty_tree(gix::hash::Kind::Sha1))
    }
}

object_id!(BlobId, "blob id");
object_id!(
    /// A commit of the user's own history, taken from `HEAD`.
    GitSha, "git sha"
);

macro_rules! as_oid {
    ($($name:ident),*) => {$(
        impl From<$name> for Oid {
            fn from(id: $name) -> Self {
                Self(id.0)
            }
        }

        impl $name {
            pub fn from_oid(oid: Oid) -> Self {
                Self(oid.0)
            }
        }
    )*};
}

as_oid!(TurnId, TreeId, BlobId);

/// A `/`-separated path inside a worktree that baton can store and later write back safely.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(Box<str>);

impl RelPath {
    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() || s.starts_with('/') || s.ends_with('/') {
            return Err(Error::invalid(
                "path",
                format!("{s:?} is empty or has an edge '/'"),
            ));
        }
        for part in s.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                return Err(Error::invalid(
                    "path",
                    format!("{s:?} has an empty, '.' or '..' part"),
                ));
            }
            if part.eq_ignore_ascii_case(".git") {
                return Err(Error::invalid("path", format!("{s:?} names .git")));
            }
            if part
                .chars()
                .any(|c| c == '\\' || c == ':' || c.is_control())
            {
                return Err(Error::invalid(
                    "path",
                    format!("{s:?} has a control character, ':' or '\\'"),
                ));
            }
        }
        Ok(Self(s.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parent(&self) -> Option<&str> {
        self.0.rsplit_once('/').map(|(dir, _)| dir)
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit_once('/').map_or(&self.0, |(_, name)| name)
    }

    pub fn is_inside(&self, dir: &RelPath) -> bool {
        self.0.len() > dir.0.len()
            && self.0.starts_with(&*dir.0)
            && self.0.as_bytes()[dir.0.len()] == b'/'
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    File,
    Executable,
    Symlink,
}

impl FileKind {
    pub(crate) fn entry_kind(self) -> gix::objs::tree::EntryKind {
        match self {
            Self::File => gix::objs::tree::EntryKind::Blob,
            Self::Executable => gix::objs::tree::EntryKind::BlobExecutable,
            Self::Symlink => gix::objs::tree::EntryKind::Link,
        }
    }

    pub(crate) fn from_entry_kind(kind: gix::objs::tree::EntryKind) -> Option<Self> {
        use gix::objs::tree::EntryKind;
        match kind {
            EntryKind::Blob => Some(Self::File),
            EntryKind::BlobExecutable => Some(Self::Executable),
            EntryKind::Link => Some(Self::Symlink),
            EntryKind::Tree | EntryKind::Commit => None,
        }
    }
}
