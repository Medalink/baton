//! Per-change line statistics, read straight from the object databases: a turn against its parent turn
//! (or, for a thread's first turn, against the commit it started from in the user's repository), a user
//! commit against its first parent, and a thread's last tree against its base. All of them are keyed by an
//! immutable object id, which is what makes them cacheable.

use super::lines::{self, Side, TEXT_MAX};
use crate::error::{Error, Result};
use crate::id::{GitSha, TreeId};
use crate::names::MAX_TREE_DEPTH;
use crate::paths::Dirs;
use crate::policy::StorePolicy;
use crate::store::SideRepo;
use crate::turndoc::{BaseChange, GitBase, changes_against_base};
use gix::ObjectId;
use gix::objs::tree::EntryKind;
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

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
    /// The line diff stopped at its deadline, so the lists may be longer than the minimum. Never cached.
    pub cut: bool,
}

/// The object databases a report reads, and what it needs to compare a first turn with the user's commit
/// the way the store sees it. Both repositories are opened read-only and without the user's git config.
pub(super) struct Readers {
    pub side: gix::Repository,
    pub user: Option<gix::Repository>,
    store: SideRepo,
    root: Option<PathBuf>,
    dirs: Dirs,
    /// Loaded on the first comparison with a base commit; a failure is kept as its message.
    policy: OnceCell<std::result::Result<StorePolicy, String>>,
    /// The last few base commits read, since the threads of a project mostly share them.
    bases: RefCell<Vec<(ObjectId, Rc<GitBase>)>>,
}

const BASES_KEPT: usize = 4;

impl Readers {
    pub(super) fn open(side_dir: &Path, user: Option<&Path>, dirs: &Dirs) -> Result<Self> {
        let open = |dir: &Path| gix::open_opts(dir, gix::open::Options::isolated());
        let user_repo = user.and_then(|dir| open(dir).ok());
        Ok(Self {
            side: open(side_dir).map_err(Error::git)?,
            root: user_repo.as_ref().and(user).map(Path::to_owned),
            user: user_repo,
            store: SideRepo::open_existing(side_dir)?,
            dirs: dirs.clone(),
            policy: OnceCell::new(),
            bases: RefCell::default(),
        })
    }

    fn policy(&self) -> Result<&StorePolicy> {
        let loaded = self.policy.get_or_init(|| match &self.root {
            Some(root) => crate::worktree::open_policy(root, &self.dirs).map_err(|e| e.to_string()),
            None => Err("no worktree to read the store policy from".to_owned()),
        });
        loaded.as_ref().map_err(|e| Error::Refused(e.clone()))
    }

    fn base(&self, sha: ObjectId, user: &gix::Repository) -> Result<Rc<GitBase>> {
        let mut bases = self.bases.borrow_mut();
        if let Some((_, base)) = bases.iter().find(|(id, _)| *id == sha) {
            return Ok(Rc::clone(base));
        }
        let base = Rc::new(GitBase::from_repo(
            user.clone(),
            Some(GitSha::from_gix(sha)),
        )?);
        if bases.len() >= BASES_KEPT {
            bases.remove(0);
        }
        bases.push((sha, Rc::clone(&base)));
        Ok(base)
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

/// One file's line statistics. `compare_text` drops a file whose two sides differ only in bytes the line
/// comparison ignores (line endings, indentation): needed against the user's repository, where a
/// checkout may differ from git's blob.
fn measure(
    path: String,
    old: &Side,
    new: &Side,
    to: [u8; 20],
    compare_text: bool,
) -> Option<FileStat> {
    let status = match (old.is_absent(), new.is_absent()) {
        (true, _) => Status::Added,
        (_, true) => Status::Removed,
        _ => Status::Modified,
    };
    let mut stat = FileStat {
        path,
        status,
        lines: false,
        added_n: 0,
        removed_n: 0,
        to,
        added: Vec::new(),
        removed: Vec::new(),
        cut: false,
    };
    if let Some(delta) = lines::diff(old, new) {
        if compare_text && delta.is_empty() && !old.is_absent() && !new.is_absent() {
            return None;
        }
        stat.lines = true;
        stat.added_n = delta.added_n;
        stat.removed_n = delta.removed_n;
        stat.added = delta.added;
        stat.removed = delta.removed;
        stat.cut = delta.cut;
    }
    Some(stat)
}

fn file_stats(
    deltas: Vec<Delta>,
    from: &gix::Repository,
    to: &gix::Repository,
) -> Result<Vec<FileStat>> {
    let mut out = Vec::with_capacity(deltas.len());
    for d in deltas {
        let (old, new) = (side_of(from, d.from)?, side_of(to, d.to)?);
        out.extend(measure(d.path, &old, &new, to_bytes(d.to), false));
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn bytes_side(bytes: Option<Vec<u8>>) -> Side {
    match bytes {
        None => Side::Absent,
        Some(b) if b.len() as u64 > TEXT_MAX => Side::Big,
        Some(b) => Side::Bytes(b),
    }
}

/// The statistics of changes the store sees (`changes_against_base`), already sorted by path.
fn base_stats(changes: Vec<BaseChange>) -> Vec<FileStat> {
    let blob_id = |bytes: &Option<Vec<u8>>| {
        bytes.as_ref().map_or([0; 20], |b| {
            gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, b)
                .ok()
                .and_then(|id| <[u8; 20]>::try_from(id.as_bytes()).ok())
                .unwrap_or([0; 20])
        })
    };
    let mut out = Vec::with_capacity(changes.len());
    for c in changes {
        let to = blob_id(&c.new);
        let (old, new) = (bytes_side(c.old), bytes_side(c.new));
        out.extend(measure(c.path, &old, &new, to, true));
    }
    out
}

fn commit_tree(repo: &gix::Repository, commit: ObjectId) -> Result<ObjectId> {
    let commit = repo.find_commit(commit).map_err(Error::git)?;
    Ok(commit.tree_id().map_err(Error::git)?.detach())
}

/// A turn's changes. `Ok(None)` means they cannot be known here (the base commit is not in this
/// repository), which must not be cached: a later `git fetch` fixes it.
///
/// Against a base commit the comparison is the one the turn document makes (`changes_against_base`): only
/// paths the store could hold, with the commit's side put through the store's redaction. The raw trees
/// differ in every excluded file and every file the store redacted, and those are not changes.
pub(super) fn turn_stats(
    readers: &Readers,
    tree: ObjectId,
    basis: Basis,
) -> Result<Option<Vec<FileStat>>> {
    let mut deltas = Vec::new();
    let from = match basis {
        Basis::Parent(parent) => Some(parent),
        // Nothing to hold back from an empty base: every stored file is an addition.
        Basis::Base(None) => None,
        Basis::Base(Some(sha)) => {
            let (Some(user), Some(root)) = (&readers.user, &readers.root) else {
                return Ok(None);
            };
            if user.find_commit(sha).is_err() {
                return Ok(None);
            }
            let base = readers.base(sha, user)?;
            let changes = changes_against_base(
                &readers.store,
                TreeId::from_gix(tree),
                &base,
                readers.policy()?,
                root,
            )?;
            return Ok(Some(base_stats(changes)));
        }
    };
    tree_deltas(
        (&readers.side, from),
        (&readers.side, Some(tree)),
        &mut String::new(),
        &mut deltas,
        0,
    )?;
    file_stats(deltas, &readers.side, &readers.side).map(Some)
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
    file_stats(deltas, user, user).map(Some)
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
