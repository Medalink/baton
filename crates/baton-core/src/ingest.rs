//! Deciding whether received objects may enter a side repo. The server uses it for pushes, the daemon for
//! what other machines moved, and both hold the same invariants: an object is stored only when everything
//! it references is stored, trees are well formed, and a ref moves last.
//!
//! `plan` is pure over a repo and a pool of received objects. `store` writes what a plan names.

use crate::error::{Error, Result};
use crate::id::{Oid, TreeId, TurnId, UnixMillis};
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
    kind: EntryKind,
    id: Oid,
}

fn forbidden_name(name: &[u8]) -> bool {
    name.is_empty()
        || name.iter().any(|b| matches!(b, b'/' | 0))
        || name == b"."
        || name == b".."
        || name.eq_ignore_ascii_case(b".git")
}

/// A tree that `git fsck --strict` accepts and whose names are safe: sorted, unique, no forbidden
/// name, and exactly the bytes git would write for it.
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
        if forbidden_name(entry.filename) {
            return Err(format!(
                "tree entry name {:?} is not allowed",
                entry.filename
            ));
        }
        if !names.insert(entry.filename) {
            return Err(format!("tree names {:?} twice", entry.filename));
        }
        out.push(TreeEntry {
            name: entry.filename.to_vec(),
            kind: entry.mode.kind(),
            id: Oid::from_gix(entry.oid.to_owned()),
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

struct Walk<'a> {
    repo: &'a SideRepo,
    pool: &'a Pool,
    done: HashSet<Oid>,
    blobs: Vec<Oid>,
    trees: Vec<Oid>,
    missing: Vec<Oid>,
}

impl Walk<'_> {
    fn want(&mut self, id: Oid) {
        if self.missing.len() < IDS_PER_MESSAGE && !self.missing.contains(&id) {
            self.missing.push(id);
        }
    }

    /// `old` is the same path in the parent turn. An entry equal to the parent's is complete already.
    fn tree(&mut self, id: Oid, old: Option<Oid>) -> std::result::Result<(), String> {
        if Some(id) == old || !self.done.insert(id) {
            return Ok(());
        }
        let Some((kind, bytes)) = self.pool.get(&id) else {
            if !self.repo.has_object(id) {
                self.want(id);
            }
            return Ok(());
        };
        if kind != ObjectKind::Tree {
            return Err(format!("{id} is not a tree"));
        }
        let entries = parse_tree(bytes)?;
        let before: HashMap<Vec<u8>, (EntryKind, Oid)> = match old {
            Some(old) => self.old_entries(old),
            None => HashMap::new(),
        };
        for entry in entries {
            let previous = before.get(&entry.name);
            if previous.map(|(_, id)| *id) == Some(entry.id) {
                continue;
            }
            match entry.kind {
                EntryKind::Tree => {
                    let old = previous
                        .filter(|(k, _)| *k == EntryKind::Tree)
                        .map(|(_, id)| *id);
                    self.tree(entry.id, old)?;
                }
                EntryKind::Commit => {}
                _ => self.blob(entry.id)?,
            }
        }
        self.trees.push(id);
        Ok(())
    }

    fn blob(&mut self, id: Oid) -> std::result::Result<(), String> {
        if !self.done.insert(id) {
            return Ok(());
        }
        match self.pool.get(&id) {
            Some((ObjectKind::Blob, _)) => self.blobs.push(id),
            Some(_) => return Err(format!("{id} is not a blob")),
            None if self.repo.has_object(id) => {}
            None => self.want(id),
        }
        Ok(())
    }

    fn old_entries(&self, old: Oid) -> HashMap<Vec<u8>, (EntryKind, Oid)> {
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
                            (e.mode.kind(), Oid::from_gix(e.oid.to_owned())),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Plans storing the closure of `tip` from `pool` and the repo, for a thread whose ref is at `stop`.
/// The chain of commits must lead from `tip` to `stop`, or to a first turn when `stop` is `None`.
pub fn plan(repo: &SideRepo, pool: &Pool, tip: TurnId, stop: Option<TurnId>) -> Result<Verdict> {
    let mut walk = Walk {
        repo,
        pool,
        done: HashSet::new(),
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
            None => repo.read_raw(oid)?,
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
