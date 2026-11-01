//! Deciding whether received objects may enter a side repo. The server uses it for pushes, the daemon for
//! what other machines moved, and both hold the same invariants: an object is stored only when everything
//! it references is stored, trees are well formed, and a ref moves last.
//!
//! `plan` is pure over a repo and a pool of received objects. `store` writes what a plan names.

use crate::error::{Error, Result};
use crate::id::{Oid, TreeId, TurnId, UnixMillis};
use crate::names::{self, MAX_TREE_DEPTH, Special};
use crate::store::{SideRepo, TurnRecord};
use crate::turn::Turn;
use crate::wire::{IDS_PER_MESSAGE, Object, ObjectKind};
use gix::objs::WriteTo as _;
use gix::objs::tree::EntryKind;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// A thread longer than this is not walked in one request.
const MAX_CHAIN: usize = 20_000;

/// Objects received and hash-checked but not yet stored. Cloning shares the bytes.
#[derive(Clone, Default)]
pub struct Pool {
    objects: HashMap<Oid, (ObjectKind, Arc<[u8]>)>,
    bytes: usize,
}

impl Pool {
    pub fn insert(&mut self, object: Object) {
        let Object {
            kind, id, bytes, ..
        } = object;
        self.bytes += bytes.len();
        if let Some((_, old)) = self.objects.insert(id, (kind, bytes.into())) {
            self.bytes -= old.len();
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    pub fn get(&self, id: &Oid) -> Option<(ObjectKind, &Arc<[u8]>)> {
        self.objects.get(id).map(|(kind, bytes)| (*kind, bytes))
    }
}

pub enum Verdict {
    Ready(Intake),
    /// What the closure still lacks, at most `IDS_PER_MESSAGE` ids.
    Missing(Vec<Oid>),
    Invalid(String),
}

pub struct Intake {
    /// The turns after `stop` up to the tip, oldest first.
    pub chain: Vec<TurnRecord>,
    /// The pooled objects to store, in the order to store them.
    pub write: Vec<(ObjectKind, Oid)>,
}

struct TreeEntry {
    name: Vec<u8>,
    mode: u32,
    kind: EntryKind,
    id: Oid,
    special: Option<Special>,
}

/// A tree that `git fsck --strict` accepts and whose names are safe: sorted, unique, every name and mode
/// one git allows, and exactly the bytes git would write for it.
fn parse_tree(bytes: &[u8]) -> std::result::Result<Vec<TreeEntry>, String> {
    let tree = gix::objs::TreeRef::from_bytes(bytes, gix::hash::Kind::Sha1)
        .map_err(|e| format!("tree does not parse: {e}"))?;
    let mut names = HashSet::new();
    for pair in tree.entries.windows(2) {
        if pair[0].partial_cmp(&pair[1]) != Some(std::cmp::Ordering::Less) {
            return Err("tree entries are not in git order".into());
        }
    }
    let mut out = Vec::with_capacity(tree.entries.len());
    for entry in &tree.entries {
        let mode = u32::from(entry.mode.value());
        let special = names::check_entry(entry.filename, mode)?;
        if entry.oid.is_null() {
            return Err(format!(
                "tree entry {:?} points to the null id",
                String::from_utf8_lossy(entry.filename)
            ));
        }
        if !names.insert(entry.filename) {
            return Err(format!("tree names {:?} twice", entry.filename));
        }
        out.push(TreeEntry {
            name: entry.filename.to_vec(),
            mode,
            kind: entry.mode.kind(),
            id: Oid::from_gix(entry.oid.to_owned()),
            special,
        });
    }
    let mut canonical = Vec::with_capacity(bytes.len());
    gix::objs::Tree::from(tree)
        .write_to(&mut canonical)
        .map_err(|e| e.to_string())?;
    if canonical != bytes {
        return Err("tree is not in canonical form".into());
    }
    Ok(out)
}

struct ParsedCommit {
    tree: TreeId,
    parent: Option<TurnId>,
    at: UnixMillis,
    turn: Turn,
}

fn parse_commit(bytes: &[u8]) -> std::result::Result<ParsedCommit, String> {
    let commit = gix::objs::CommitRef::from_bytes(bytes, gix::hash::Kind::Sha1)
        .map_err(|e| format!("commit does not parse: {e}"))?;
    let parents: Vec<_> = commit.parents().collect();
    if parents.len() > 1 {
        return Err("a turn has at most one parent".into());
    }
    let at = commit
        .time()
        .map_err(|e| format!("commit time: {e}"))?
        .seconds;
    let message = std::str::from_utf8(commit.message).map_err(|_| "message is not UTF-8")?;
    let turn = Turn::parse_message(message).map_err(|e| e.to_string())?;
    let tree = TreeId::from_gix(commit.tree());
    let parent = parents.first().map(|p| TurnId::from_gix(*p));
    let mut canonical = Vec::with_capacity(bytes.len());
    commit
        .into_owned()
        .map_err(|e| e.to_string())?
        .write_to(&mut canonical)
        .map_err(|e| e.to_string())?;
    if canonical != bytes {
        return Err("commit is not in canonical form".into());
    }
    Ok(ParsedCommit {
        tree,
        parent,
        at: UnixMillis::from_secs(at),
        turn,
    })
}

type Siblings = HashMap<Vec<u8>, (u32, EntryKind, Oid)>;

struct Frame {
    entries: std::vec::IntoIter<TreeEntry>,
    id: Oid,
    before: Siblings,
}

struct Walk<'a> {
    repo: &'a SideRepo,
    pool: &'a Pool,
    /// Ids met as a tree and ids met as a blob are kept apart, so one id used as both is still caught.
    walked: HashSet<Oid>,
    blobbed: HashSet<Oid>,
    checked: HashSet<(Oid, Special)>,
    heights: HashMap<Oid, usize>,
    blobs: Vec<Oid>,
    trees: Vec<Oid>,
    missing: Vec<Oid>,
}

fn store_err(e: Error) -> String {
    format!("reading the stored objects failed: {e}")
}

impl Walk<'_> {
    fn want(&mut self, id: Oid) {
        if self.missing.len() < IDS_PER_MESSAGE && !self.missing.contains(&id) {
            self.missing.push(id);
        }
    }

    /// Walks the new part of the tree `root` without recursion, so no input can exhaust the stack.
    /// `old` is the same path in the parent turn. An entry equal to the parent's is complete already.
    fn tree(&mut self, root: Oid, old: Option<Oid>) -> std::result::Result<(), String> {
        let mut stack: Vec<Frame> = Vec::new();
        stack.extend(self.open(root, old, 0)?);
        while let Some(top) = stack.last_mut() {
            let Some(entry) = top.entries.next() else {
                let frame = stack.pop().expect("a frame was just read");
                self.trees.push(frame.id);
                continue;
            };
            let previous = top.before.get(&entry.name).copied();
            if previous.is_some_and(|(mode, _, id)| mode == entry.mode && id == entry.id) {
                continue;
            }
            match entry.kind {
                EntryKind::Tree => {
                    let old = previous
                        .filter(|(_, kind, _)| *kind == EntryKind::Tree)
                        .map(|(_, _, id)| id);
                    let depth = stack.len();
                    stack.extend(self.open(entry.id, old, depth)?);
                }
                EntryKind::Commit => {}
                _ => self.blob(entry.id, entry.special)?,
            }
        }
        Ok(())
    }

    /// The frame for a tree that has to be walked, or nothing when the tree needs no walking: it is the
    /// parent's, it is stored, it was seen, or it is still to come.
    fn open(
        &mut self,
        id: Oid,
        old: Option<Oid>,
        depth: usize,
    ) -> std::result::Result<Option<Frame>, String> {
        if depth > MAX_TREE_DEPTH {
            return Err(format!("trees are nested deeper than {MAX_TREE_DEPTH}"));
        }
        if Some(id) == old {
            return Ok(None);
        }
        if !self.walked.insert(id) {
            self.fits(id, depth)?;
            return Ok(None);
        }
        let pool = self.pool;
        let Some((kind, bytes)) = pool.get(&id) else {
            match self.repo.object_kind(id).map_err(store_err)? {
                None => self.want(id),
                Some(ObjectKind::Tree) => self.fits(id, depth)?,
                Some(_) => return Err(format!("{id} is not a tree")),
            }
            return Ok(None);
        };
        if kind != ObjectKind::Tree {
            return Err(format!("{id} is not a tree"));
        }
        let entries = parse_tree(bytes)?;
        let before = match old {
            Some(old) => self.old_entries(old),
            None => HashMap::new(),
        };
        Ok(Some(Frame {
            entries: entries.into_iter(),
            id,
            before,
        }))
    }

    /// A tree met at `depth` that is not walked here must still fit below it, whatever it holds.
    fn fits(&mut self, id: Oid, depth: usize) -> std::result::Result<(), String> {
        if depth + self.height(id)? > MAX_TREE_DEPTH {
            return Err(format!("trees are nested deeper than {MAX_TREE_DEPTH}"));
        }
        Ok(())
    }

    /// How many trees nest below `root`, read from the pool and the repo. A tree not received yet
    /// counts as empty, since the plan asks for it and checks again when it arrives.
    fn height(&mut self, root: Oid) -> std::result::Result<usize, String> {
        struct Level {
            id: Oid,
            kids: std::vec::IntoIter<Oid>,
            height: usize,
            complete: bool,
        }
        if let Some(known) = self.known_height(root) {
            return Ok(known);
        }
        let (kids, complete) = self.child_trees(root)?;
        let mut stack = vec![Level {
            id: root,
            kids: kids.into_iter(),
            height: 0,
            complete,
        }];
        loop {
            let top = stack
                .last_mut()
                .expect("the root level stays until the end");
            if let Some(kid) = top.kids.next() {
                if let Some(known) = self.known_height(kid) {
                    top.height = top.height.max(known + 1);
                    continue;
                }
                if stack.len() > MAX_TREE_DEPTH {
                    return Err(format!("trees are nested deeper than {MAX_TREE_DEPTH}"));
                }
                let (kids, complete) = self.child_trees(kid)?;
                stack.push(Level {
                    id: kid,
                    kids: kids.into_iter(),
                    height: 0,
                    complete,
                });
                continue;
            }
            let level = stack.pop().expect("a level was just read");
            self.remember_height(level.id, level.height, level.complete);
            match stack.last_mut() {
                Some(parent) => {
                    parent.height = parent.height.max(level.height + 1);
                    parent.complete &= level.complete;
                }
                None => return Ok(level.height),
            }
        }
    }

    fn known_height(&self, id: Oid) -> Option<usize> {
        self.heights
            .get(&id)
            .copied()
            .or_else(|| self.repo.stored_height(id))
    }

    /// The repo-wide cache outlives this push, so it only takes heights measured over trees that
    /// are all present: a missing subtree counts as empty here and would cap a later push too low.
    fn remember_height(&mut self, id: Oid, height: usize, complete: bool) {
        self.heights.insert(id, height);
        if complete && self.pool.get(&id).is_none() {
            self.repo.remember_height(id, height);
        }
    }

    /// The child trees of `id`, and whether `id` itself is present in the pool or the repo.
    fn child_trees(&self, id: Oid) -> std::result::Result<(Vec<Oid>, bool), String> {
        let stored;
        let bytes: &[u8] = match self.pool.get(&id) {
            Some((_, bytes)) => bytes,
            None => match self.repo.read_raw(id).map_err(store_err)? {
                Some((ObjectKind::Tree, bytes)) => {
                    stored = bytes;
                    &stored
                }
                _ => return Ok((Vec::new(), false)),
            },
        };
        let kids = gix::objs::TreeRef::from_bytes(bytes, gix::hash::Kind::Sha1)
            .map(|tree| {
                tree.entries
                    .iter()
                    .filter(|e| e.mode.is_tree())
                    .map(|e| Oid::from_gix(e.oid.to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        Ok((kids, true))
    }

    fn blob(&mut self, id: Oid, special: Option<Special>) -> std::result::Result<(), String> {
        if self.blobbed.insert(id) {
            match self.pool.get(&id) {
                Some((ObjectKind::Blob, _)) => self.blobs.push(id),
                Some(_) => return Err(format!("{id} is not a blob")),
                None => match self.repo.object_kind(id).map_err(store_err)? {
                    Some(ObjectKind::Blob) => {}
                    Some(_) => return Err(format!("{id} is not a blob")),
                    None => self.want(id),
                },
            }
        }
        match special {
            Some(special) if !self.checked.contains(&(id, special)) => {
                self.check_content(id, special)
            }
            _ => Ok(()),
        }
    }

    /// git checks `.gitmodules` and `.gitattributes` by content, so the blob is read here when it is
    /// stored and awaited when it is not.
    fn check_content(&mut self, id: Oid, special: Special) -> std::result::Result<(), String> {
        let stored;
        let bytes: &[u8] = match self.pool.get(&id) {
            Some((_, bytes)) => bytes,
            None => match self.repo.read_raw(id).map_err(store_err)? {
                Some((_, bytes)) => {
                    stored = bytes;
                    &stored
                }
                None => return Ok(()),
            },
        };
        names::check_blob(special, bytes)?;
        self.checked.insert((id, special));
        Ok(())
    }

    fn old_entries(&self, old: Oid) -> Siblings {
        let Ok(Some((ObjectKind::Tree, bytes))) = self.repo.read_raw(old) else {
            return HashMap::new();
        };
        gix::objs::TreeRef::from_bytes(&bytes, gix::hash::Kind::Sha1)
            .map(|tree| {
                tree.entries
                    .iter()
                    .map(|e| {
                        (
                            e.filename.to_vec(),
                            (
                                u32::from(e.mode.value()),
                                e.mode.kind(),
                                Oid::from_gix(e.oid.to_owned()),
                            ),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

// Plans storing the closure of `tip` from `pool` and the repo, for a thread whose ref is at `stop`.
/// The chain of commits must lead from `tip` to `stop`, or to a first turn when `stop` is `None`.
pub fn plan(repo: &SideRepo, pool: &Pool, tip: TurnId, stop: Option<TurnId>) -> Result<Verdict> {
    let mut walk = Walk {
        repo,
        pool,
        walked: HashSet::new(),
        blobbed: HashSet::new(),
        checked: HashSet::new(),
        heights: HashMap::new(),
        blobs: Vec::new(),
        trees: Vec::new(),
        missing: Vec::new(),
    };
    let mut commits: Vec<(Oid, ParsedCommit)> = Vec::new();
    let mut next = Some(tip);
    while let Some(id) = next.filter(|id| Some(*id) != stop) {
        if commits.len() >= MAX_CHAIN {
            return Ok(Verdict::Invalid("the chain of turns is too long".into()));
        }
        let oid = Oid::from(id);
        let found = match pool.get(&oid) {
            Some((kind, bytes)) => Some((kind, bytes.to_vec())),
            None => match repo.object_kind(oid)? {
                Some(ObjectKind::Commit) => repo.read_raw(oid)?,
                Some(_) => return Ok(Verdict::Invalid(format!("{id} is not a commit"))),
                None => None,
            },
        };
        let Some((kind, bytes)) = found else {
            walk.want(oid);
            break;
        };
        if kind != ObjectKind::Commit {
            return Ok(Verdict::Invalid(format!("{id} is not a commit")));
        }
        let parsed = match parse_commit(&bytes) {
            Ok(parsed) => parsed,
            Err(reason) => return Ok(Verdict::Invalid(format!("commit {id}: {reason}"))),
        };
        next = parsed.parent;
        commits.push((oid, parsed));
        if next.is_none() && stop.is_some() {
            return Ok(Verdict::Invalid(
                "the tip does not descend from the thread's current turn".into(),
            ));
        }
    }
    commits.reverse();

    let mut before = commits
        .first()
        .and_then(|(_, c)| c.parent)
        .and_then(|parent| repo.read_turn(parent).ok())
        .map(|record| Oid::from(record.tree));
    for (_, commit) in &commits {
        if let Err(reason) = walk.tree(Oid::from(commit.tree), before) {
            return Ok(Verdict::Invalid(reason));
        }
        before = Some(Oid::from(commit.tree));
    }

    if !walk.missing.is_empty() {
        return Ok(Verdict::Missing(walk.missing));
    }
    let mut write: Vec<(ObjectKind, Oid)> = walk
        .blobs
        .iter()
        .map(|id| (ObjectKind::Blob, *id))
        .chain(walk.trees.iter().map(|id| (ObjectKind::Tree, *id)))
        .collect();
    let mut chain = Vec::with_capacity(commits.len());
    for (oid, commit) in commits {
        write.push((ObjectKind::Commit, oid));
        chain.push(TurnRecord {
            id: TurnId::from_oid(oid),
            tree: commit.tree,
            parent: commit.parent,
            at: commit.at,
            turn: commit.turn,
        });
    }
    Ok(Verdict::Ready(Intake { chain, write }))
}

/// Stores what a plan names: blobs, then trees with children first, then commits oldest first.
pub fn store(repo: &SideRepo, pool: &Pool, plan: &Intake) -> Result<()> {
    for (kind, id) in &plan.write {
        let Some((_, bytes)) = pool.get(id) else {
            continue;
        };
        let stored = repo.write_raw(*kind, bytes)?;
        if stored != *id {
            return Err(Error::Git(format!("object {id} was stored as {stored}")));
        }
    }
    Ok(())
}
