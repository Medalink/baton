//! Per-change line statistics, read straight from the object databases: a turn against its parent turn
//! (or, for a thread's first turn, against the commit it started from in the user's repository), a user
//! commit against its first parent, and a thread's last tree against its base. All of them are keyed by an
//! immutable object id, which is what makes them cacheable.

use super::lines::{self, Side, TEXT_MAX};
use crate::error::{Error, Result};
use crate::names::MAX_TREE_DEPTH;
use gix::ObjectId;
use gix::objs::tree::EntryKind;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Status {
    Added,
    Modified,
    Removed,
}

/// What one change did to one file. `added` and `removed` are sorted hashes of significant lines, without
/// the lines a change both adds and removes. `lines` is false for a binary or oversized file.
#[derive(Clone, Debug)]
pub(super) struct FileStat {
    pub path: String,
    pub status: Status,
    pub lines: bool,
    pub added_n: u32,
    pub removed_n: u32,
    /// The file's blob after the change; all zero when it was removed.
    pub to: [u8; 20],
    pub added: Vec<u64>,
    pub removed: Vec<u64>,
}

/// The two object databases a report reads. Both are opened read-only and without the user's git config.
pub(super) struct Readers {
    pub side: gix::Repository,
    pub user: Option<gix::Repository>,
}

impl Readers {
    pub(super) fn open(side_dir: &Path, user: Option<&Path>) -> Result<Self> {
        let open = |dir: &Path| gix::open_opts(dir, gix::open::Options::isolated());
        Ok(Self {
            side: open(side_dir).map_err(Error::git)?,
            user: user.and_then(|dir| open(dir).ok()),
        })
    }
}

/// What a change is measured against.
#[derive(Clone, Copy, Debug)]
pub(super) enum Basis {
    /// The tree of the previous turn of the thread.
    Parent(ObjectId),
    /// The commit in the user's repository the thread started from; none when that repository had no commit.
    Base(Option<ObjectId>),
}

type Entries = BTreeMap<String, (EntryKind, ObjectId)>;

fn entries(repo: &gix::Repository, tree: Option<ObjectId>) -> Result<Entries> {
    let mut map = BTreeMap::new();
    let Some(id) = tree else { return Ok(map) };
    if id == ObjectId::empty_tree(repo.object_hash()) {
        return Ok(map);
    }
    let tree = repo.find_tree(id).map_err(Error::git)?;
    let decoded = tree.decode().map_err(Error::git)?;
    for entry in &decoded.entries {
        map.insert(
            entry.filename.to_string(),
            (entry.mode.kind(), entry.oid.to_owned()),
        );
    }
    Ok(map)
}

struct Delta {
    path: String,
    from: Option<ObjectId>,
    to: Option<ObjectId>,
}

type Slot<'a> = (&'a gix::Repository, Option<ObjectId>);

fn subtree(e: Option<&(EntryKind, ObjectId)>) -> Option<ObjectId> {
    e.filter(|(k, _)| *k == EntryKind::Tree).map(|(_, id)| *id)
}

fn leaf(e: Option<&(EntryKind, ObjectId)>) -> Option<ObjectId> {
    e.filter(|(k, _)| {
        matches!(
            k,
            EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link
        )
    })
    .map(|(_, id)| *id)
}

/// Files that differ between two trees, found by skipping every subtree whose id is equal. The trees may
/// live in different repositories: an id names the same bytes in both.
fn tree_deltas(
    from: Slot<'_>,
    to: Slot<'_>,
    prefix: &mut String,
    out: &mut Vec<Delta>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_TREE_DEPTH {
        return Err(Error::Refused(format!(
            "a tree is nested deeper than {MAX_TREE_DEPTH} directories"
        )));
    }
    if from.1 == to.1 {
        return Ok(());
    }
    let (old, new) = (entries(from.0, from.1)?, entries(to.0, to.1)?);
    let names: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    for name in names {
        let len = prefix.len();
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(name);
        let (before, after) = (old.get(name), new.get(name));
        let (old_tree, new_tree) = (subtree(before), subtree(after));
        if old_tree.is_some() || new_tree.is_some() {
            tree_deltas((from.0, old_tree), (to.0, new_tree), prefix, out, depth + 1)?;
        }
        let (a, b) = (leaf(before), leaf(after));
        if a != b {
            out.push(Delta {
                path: prefix.clone(),
                from: a,
                to: b,
            });
        }
        prefix.truncate(len);
    }
    Ok(())
}

fn side_of(repo: &gix::Repository, id: Option<ObjectId>) -> Result<Side> {
    let Some(id) = id else {
        return Ok(Side::Absent);
    };
    let header = repo.find_header(id).map_err(Error::git)?;
    if header.size() > TEXT_MAX {
        return Ok(Side::Big);
    }
    let blob = repo.find_blob(id).map_err(Error::git)?;
    Ok(Side::Bytes(blob.data.clone()))
}

fn to_bytes(id: Option<ObjectId>) -> [u8; 20] {
    id.and_then(|id| <[u8; 20]>::try_from(id.as_bytes()).ok())
        .unwrap_or([0; 20])
}

/// `compare_text` drops a file whose two sides differ only in bytes the line comparison ignores (line
/// endings, indentation): needed across repositories, where equal ids are the only shortcut and a
/// checkout may differ from git's blob.
fn file_stats(
    deltas: Vec<Delta>,
    from: &gix::Repository,
    to: &gix::Repository,
    compare_text: bool,
) -> Result<Vec<FileStat>> {
    let mut out = Vec::with_capacity(deltas.len());
    for d in deltas {
        let (old, new) = (side_of(from, d.from)?, side_of(to, d.to)?);
        let status = match (d.from, d.to) {
            (None, _) => Status::Added,
            (_, None) => Status::Removed,
            _ => Status::Modified,
        };
        let mut stat = FileStat {
            path: d.path,
            status,
            lines: false,
            added_n: 0,
            removed_n: 0,
            to: to_bytes(d.to),
            added: Vec::new(),
            removed: Vec::new(),
        };
        if let Some(delta) = lines::diff(&old, &new) {
            if compare_text && delta.is_empty() && !old.is_absent() && !new.is_absent() {
                continue;
            }
            stat.lines = true;
            stat.added_n = delta.added_n;
            stat.removed_n = delta.removed_n;
            stat.added = delta.added;
            stat.removed = delta.removed;
        }
        out.push(stat);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn commit_tree(repo: &gix::Repository, commit: ObjectId) -> Result<ObjectId> {
    let commit = repo.find_commit(commit).map_err(Error::git)?;
    Ok(commit.tree_id().map_err(Error::git)?.detach())
}

/// A turn's changes. `Ok(None)` means they cannot be known here (the base commit is not in this
/// repository), which must not be cached: a later `git fetch` fixes it.
pub(super) fn turn_stats(
    readers: &Readers,
    tree: ObjectId,
    basis: Basis,
) -> Result<Option<Vec<FileStat>>> {
    let mut deltas = Vec::new();
    match basis {
        Basis::Parent(parent) => {
            tree_deltas(
                (&readers.side, Some(parent)),
                (&readers.side, Some(tree)),
                &mut String::new(),
                &mut deltas,
                0,
            )?;
            file_stats(deltas, &readers.side, &readers.side, false).map(Some)
        }
        Basis::Base(sha) => {
            let from_repo = readers.user.as_ref().unwrap_or(&readers.side);
            let base_tree = match sha {
                None => None,
                Some(sha) => match readers.user.as_ref().map(|u| commit_tree(u, sha)) {
                    Some(Ok(tree)) => Some(tree),
                    _ => return Ok(None),
                },
            };
            tree_deltas(
                (from_repo, base_tree),
                (&readers.side, Some(tree)),
                &mut String::new(),
                &mut deltas,
                0,
            )?;
            file_stats(deltas, from_repo, &readers.side, true).map(Some)
        }
    }
}

/// A user commit against its first parent.
pub(super) fn commit_stats(readers: &Readers, id: ObjectId) -> Result<Option<Vec<FileStat>>> {
    let Some(user) = &readers.user else {
        return Ok(None);
    };
    let commit = user.find_commit(id).map_err(Error::git)?;
    let tree = commit.tree_id().map_err(Error::git)?.detach();
    let parent_tree = match commit.parent_ids().next() {
        Some(parent) => Some(commit_tree(user, parent.detach())?),
        None => None,
    };
    let mut deltas = Vec::new();
    tree_deltas(
        (user, parent_tree),
        (user, Some(tree)),
        &mut String::new(),
        &mut deltas,
        0,
    )?;
    file_stats(deltas, user, user, false).map(Some)
}

/// The blob at `path` in a tree.
fn blob_at(repo: &gix::Repository, tree: ObjectId, path: &str) -> Result<Option<ObjectId>> {
    let mut current = tree;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        let found = entries(repo, Some(current))?.remove(part);
        let Some((kind, id)) = found else {
            return Ok(None);
        };
        if parts.peek().is_none() {
            return Ok(leaf(Some(&(kind, id))));
        }
        if kind != EntryKind::Tree {
            return Ok(None);
        }
        current = id;
    }
    Ok(None)
}

/// A file as `HEAD` holds it.
pub(super) enum HeadFile {
    Missing,
    Opaque([u8; 20]),
    Lines(HashSet<u64>),
}

pub(super) fn head_file(repo: &gix::Repository, tree: ObjectId, path: &str) -> Result<HeadFile> {
    let Some(blob) = blob_at(repo, tree, path)? else {
        return Ok(HeadFile::Missing);
    };
    Ok(match lines::hashes(&side_of(repo, Some(blob))?) {
        Some(set) => HeadFile::Lines(set),
        None => HeadFile::Opaque(to_bytes(Some(blob))),
    })
}

/// The commit a ref-less `HEAD` points at, if the repository has one.
pub(super) fn head_commit(repo: &gix::Repository) -> Option<ObjectId> {
    repo.head_id().ok().map(|id| id.detach())
}

/// A commit's tree, its first-parent-or-not parents and its committer time in seconds.
pub(super) struct CommitInfo {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub time: i64,
}

pub(super) fn commit_info(repo: &gix::Repository, id: ObjectId) -> Result<CommitInfo> {
    let commit = repo.find_commit(id).map_err(Error::git)?;
    Ok(CommitInfo {
        tree: commit.tree_id().map_err(Error::git)?.detach(),
        parents: commit.parent_ids().map(|p| p.detach()).collect(),
        time: commit.time().map_err(Error::git)?.seconds,
    })
}
