//! Builds `baton.turn.v1` for a turn that was just committed: its changed files, with counts, and a
//! unified diff. Everything in it is redacted. A later turn is compared with its parent turn, and both
//! sides come from the side repo, where content is already redacted. A thread's first turn is compared
//! with the commit it started from in the user's own repository; that side is raw git content, so it goes
//! through the same `StorePolicy::prepare` a snapshot does before it can appear in a diff line.

use crate::error::{Error, Result};
use crate::id::{BlobId, GitSha, ProjectKey, RelPath};
use crate::policy::StorePolicy;
use crate::schema::{CheckResult, FileChange, FileStatus, TURN_SCHEMA, TurnDoc};
use crate::store::{SideRepo, TurnRecord};
use gix::bstr::ByteSlice;
use similar::{ChangeTag, TextDiff};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DIFF_MAX_BYTES: usize = 256 * 1024;
const FILES_MAX: usize = 2000;
/// A side larger than this is listed but not diffed.
const FILE_DIFF_MAX: usize = 1024 * 1024;
const BINARY_PROBE: usize = 8 * 1024;
const CONTEXT: usize = 3;

/// One side of a changed file.
enum Side {
    Absent,
    Text(String),
    Binary,
    TooLarge,
}

impl Side {
    fn from_bytes(bytes: &[u8]) -> Self {
        if bytes.len() > FILE_DIFF_MAX {
            Self::TooLarge
        } else if bytes[..bytes.len().min(BINARY_PROBE)].contains(&0) {
            Self::Binary
        } else {
            // Line endings are not a change: a checkout with CRLF and git's LF blob are the same file.
            Self::Text(String::from_utf8_lossy(bytes).replace("\r\n", "\n"))
        }
    }
}

/// The commit a thread's first turn started from, read straight from the user's object database.
struct GitBase {
    repo: gix::Repository,
    root: PathBuf,
    files: BTreeMap<String, gix::ObjectId>,
}

impl GitBase {
    fn open(root: &Path, sha: Option<GitSha>) -> Result<Self> {
        let repo = gix::open_opts(root, gix::open::Options::isolated())
            .map_err(|e| Error::Git(e.to_string()))?;
        let mut files = BTreeMap::new();
        if let Some(sha) = sha {
            let git = |e: &dyn std::fmt::Display| Error::Git(e.to_string());
            let commit = repo.find_commit(sha.oid()).map_err(|_| {
                Error::Refused(format!(
                    "the base commit {} is not in the repository",
                    sha.short()
                ))
            })?;
            let tree = commit.tree().map_err(|e| git(&e))?;
            for entry in tree.traverse().breadthfirst.files().map_err(|e| git(&e))? {
                if entry.mode.is_tree() || entry.mode.is_commit() {
                    continue;
                }
                if let Ok(path) = entry.filepath.to_str() {
                    files.insert(path.to_owned(), entry.oid);
                }
            }
        }
        Ok(Self {
            repo,
            root: root.to_owned(),
            files,
        })
    }

    fn prepared(&self, path: &str, id: gix::ObjectId, policy: &StorePolicy) -> Result<Vec<u8>> {
        Ok(prepare(path, self.blob(id)?, policy))
    }

    /// Whether the store would have skipped the file for its size: in the base commit, or on disk now,
    /// where a file that grew past the limit after the base is skipped too.
    fn over_limit(&self, path: &str, base_len: usize, policy: &StorePolicy) -> bool {
        policy.exceeds_size_limit(base_len as u64)
            || std::fs::symlink_metadata(self.root.join(path))
                .is_ok_and(|meta| policy.exceeds_size_limit(meta.len()))
    }

    fn blob(&self, id: gix::ObjectId) -> Result<Vec<u8>> {
        Ok(self
            .repo
            .find_blob(id)
            .map_err(|e| Error::Git(e.to_string()))?
            .data
            .clone())
    }
}

struct Changed {
    path: String,
    old: Side,
    new: Side,
}

fn blob_side(side: &SideRepo, blob: BlobId) -> Result<Side> {
    Ok(Side::from_bytes(&side.read_blob(blob)?))
}

fn changes_since_parent(
    side: &SideRepo,
    record: &TurnRecord,
    parent: crate::id::TurnId,
) -> Result<Vec<Changed>> {
    let before = side.read_turn(parent)?.tree;
    let mut out = Vec::new();
    for change in side.diff_trees(Some(before), record.tree)? {
        let find = |tree| side.entry_at(tree, &change.path);
        let old = match find(before)? {
            Some((_, blob)) => blob_side(side, blob)?,
            None => Side::Absent,
        };
        let new = match find(record.tree)? {
            Some((_, blob)) => blob_side(side, blob)?,
            None => Side::Absent,
        };
        out.push(Changed {
            path: change.path.as_str().to_owned(),
            old,
            new,
        });
    }
    Ok(out)
}

/// The first turn against the commit the thread started from. Only paths the side repo could hold are
/// compared: a base file that is missing from the turn's tree is a deletion only if the store policy
/// would have stored it, and an unchanged file never appears. The git side goes through the same
/// `StorePolicy::prepare` a snapshot does.
fn changes_since_base(
    side: &SideRepo,
    record: &TurnRecord,
    base: &GitBase,
    policy: &StorePolicy,
) -> Result<Vec<Changed>> {
    let mut out = Vec::new();
    let mut now: BTreeMap<String, BlobId> = side
        .flatten(record.tree)?
        .into_iter()
        .map(|e| (e.path.as_str().to_owned(), e.blob))
        .collect();
    let mut missing = Vec::new();
    for (path, &git_blob) in &base.files {
        let Some(blob) = now.remove(path) else {
            missing.push((path.as_str(), git_blob));
            continue;
        };
        if blob.oid() == git_blob {
            continue;
        }
        let old = Side::from_bytes(&base.prepared(path, git_blob, policy)?);
        let new = blob_side(side, blob)?;
        if !matches!((&old, &new), (Side::Text(a), Side::Text(b)) if a == b) {
            out.push(Changed {
                path: path.clone(),
                old,
                new,
            });
        }
    }
    let asked: Vec<(&str, bool)> = missing.iter().map(|&(path, _)| (path, false)).collect();
    for ((path, git_blob), admitted) in missing.into_iter().zip(policy.admits(&asked)?) {
        if !admitted || policy.holds_secret(path) {
            continue;
        }
        let raw = base.blob(git_blob)?;
        if base.over_limit(path, raw.len(), policy) {
            continue;
        }
        out.push(Changed {
            path: path.to_owned(),
            old: Side::from_bytes(&prepare(path, raw, policy)),
            new: Side::Absent,
        });
    }
    for (path, blob) in now {
        out.push(Changed {
            path,
            old: Side::Absent,
            new: blob_side(side, blob)?,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn prepare(path: &str, bytes: Vec<u8>, policy: &StorePolicy) -> Vec<u8> {
    match RelPath::parse(path) {
        Ok(rel) => policy.prepare(&rel, bytes),
        Err(_) => bytes,
    }
}

/// The file's entry in the list, and its section of the diff.
fn render(change: &Changed) -> (FileChange, String) {
    let status = match (&change.old, &change.new) {
        (Side::Absent, _) => FileStatus::Added,
        (_, Side::Absent) => FileStatus::Removed,
        _ => FileStatus::Modified,
    };
    let path = &change.path;
    let mut section = format!("diff --git a/{path} b/{path}\n");
    let (mut added, mut removed) = (0u32, 0u32);
    match (&change.old, &change.new) {
        (Side::Text(_) | Side::Absent, Side::Text(_) | Side::Absent) => {
            let text = |side: &Side| match side {
                Side::Text(text) => text.clone(),
                _ => String::new(),
            };
            let (old, new) = (text(&change.old), text(&change.new));
            let diff = TextDiff::from_lines(&old, &new);
            for c in diff.iter_all_changes() {
                match c.tag() {
                    ChangeTag::Insert => added += 1,
                    ChangeTag::Delete => removed += 1,
                    ChangeTag::Equal => {}
                }
            }
            let from = if matches!(change.old, Side::Absent) {
                "/dev/null".to_owned()
            } else {
                format!("a/{path}")
            };
            let to = if matches!(change.new, Side::Absent) {
                "/dev/null".to_owned()
            } else {
                format!("b/{path}")
            };
            section.push_str(
                &diff
                    .unified_diff()
                    .context_radius(CONTEXT)
                    .header(&from, &to)
                    .to_string(),
            );
        }
        (Side::TooLarge, _) | (_, Side::TooLarge) => {
            section.push_str(&format!(
                "Files a/{path} and b/{path} differ (too large to diff)\n"
            ));
        }
        _ => section.push_str(&format!("Binary files a/{path} and b/{path} differ\n")),
    }
    (
        FileChange {
            path: path.clone(),
            status,
            added,
            removed,
        },
        section,
    )
}

/// Cuts `text` to at most `max` bytes on a line boundary.
fn cut_to_lines(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    match text[..end].rfind('\n') {
        Some(i) => &text[..=i],
        None => &text[..end],
    }
}

pub struct TurnDocSource<'a> {
    pub side: &'a SideRepo,
    pub record: &'a TurnRecord,
    pub project: &'a ProjectKey,
    /// The user's worktree, read for the commit a first turn started from.
    pub root: &'a Path,
    pub policy: &'a StorePolicy,
}

impl TurnDocSource<'_> {
    pub fn build(&self, lens: Option<String>) -> Result<TurnDoc> {
        let record = self.record;
        let changed = match record.parent {
            Some(parent) => changes_since_parent(self.side, record, parent)?,
            None => {
                let base = GitBase::open(self.root, record.turn.draft.base)?;
                changes_since_base(self.side, record, &base, self.policy)?
            }
        };
        let mut files = Vec::new();
        let mut diff = String::new();
        let mut diff_truncated = false;
        for change in &changed {
            let (entry, section) = render(change);
            if files.len() < FILES_MAX {
                files.push(entry);
            }
            if diff_truncated {
                continue;
            }
            if diff.len() + section.len() <= DIFF_MAX_BYTES {
                diff.push_str(&section);
            } else {
                let room = DIFF_MAX_BYTES - diff.len();
                diff.push_str(cut_to_lines(&section, room));
                diff_truncated = true;
            }
        }
        let d = &record.turn.draft;
        Ok(TurnDoc {
            schema: TURN_SCHEMA,
            project: self.project.to_string(),
            thread: d.thread.clone(),
            turn: record.turn.seq.get(),
            commit: record.id,
            parent: record.parent,
            base: d.base,
            subject: d.subject.as_str().to_owned(),
            agent: d.agent.clone(),
            model: d.model.as_ref().map(ToString::to_string),
            machine: d.machine.clone(),
            at: record.at,
            checks: d.checks.iter().map(CheckResult::from).collect(),
            files_truncated: changed.len() > FILES_MAX,
            files,
            diff,
            diff_truncated,
            lens,
        })
    }
}
