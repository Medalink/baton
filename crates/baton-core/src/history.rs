//! Which recorded turn wrote a line, for `baton blame`. The matching rule is in `docs/design.md`, History.
//!
//! The side repo holds every thread's turns as whole-tree snapshots, so a turn "adds" a line when its
//! copy of the file has more of that line than the previous turn's copy. A thread's first turn is
//! compared with the file at its `Baton-Base` in the user's repository, which the caller supplies.

use crate::error::Result;
use crate::id::{BlobId, FileKind, GitSha, RelPath, UnixMillis};
use crate::store::{SideRepo, TurnRecord};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

pub const BLAME_SCHEMA: &str = "baton.blame.v1";

/// A line to attribute, with the git commit that holds it when there is one.
pub struct Wanted {
    pub text: Vec<u8>,
    pub commit: Option<CommitFacts>,
}

/// What `git blame` says about the commit: its id and committer time.
pub struct CommitFacts {
    pub id: GitSha,
    pub at: UnixMillis,
}

/// What the search needs from the user's repository, which `baton-core` does not open itself.
pub struct GitView<'a> {
    /// The file at `path` in commit `base`: its bytes, empty when the commit has no such file, `None`
    /// when the repository lacks the commit.
    pub blob_at: &'a dyn Fn(&GitSha, &RelPath) -> Option<Vec<u8>>,
    /// Whether `base` is an ancestor of (or is) `commit`.
    pub is_ancestor: &'a dyn Fn(&GitSha, &GitSha) -> bool,
}

pub struct Found {
    pub best: TurnRecord,
    /// How many other turns, in any thread, also added the line.
    pub others: usize,
}

pub struct Origins {
    /// One entry per wanted line, in order; `None` when no recorded turn added it.
    pub found: Vec<Option<Found>>,
    /// Threads whose first turn could not be compared because its base commit is not in the repository.
    pub unchecked: usize,
}

struct State {
    id: Option<BlobId>,
    bytes: Vec<u8>,
}

fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|b| *b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
}

/// Lines `new` holds more often than `old`. Blank lines never count. A moved line is not added.
fn added_lines<'a>(old: &[u8], new: &'a [u8]) -> HashSet<&'a [u8]> {
    let mut left: HashMap<&[u8], usize> = HashMap::new();
    for line in lines(old) {
        *left.entry(line).or_default() += 1;
    }
    let mut added = HashSet::new();
    for line in lines(new) {
        match left.get_mut(line) {
            Some(n) if *n > 0 => *n -= 1,
            _ => {
                added.insert(line);
            }
        }
    }
    added
}

fn state_at(side: &SideRepo, turn: &TurnRecord, path: &RelPath) -> Result<State> {
    Ok(match side.entry_at(turn.tree, path)? {
        Some((FileKind::File | FileKind::Executable, id)) => State {
            id: Some(id),
            bytes: side.read_blob(id)?,
        },
        _ => State {
            id: None,
            bytes: Vec::new(),
        },
    })
}

/// The state a thread started from: the file at its base commit, or nothing for a repository that had no
/// commit. `None` when the base is not in the repository.
fn start_state(first: &TurnRecord, path: &RelPath, git: &GitView<'_>) -> Option<State> {
    let bytes = match &first.turn.draft.base {
        None => Vec::new(),
        Some(base) => (git.blob_at)(base, path)?,
    };
    Some(State { id: None, bytes })
}

/// For each wanted line, the turns that first added it, one per thread, best first. Walks every thread's
/// turns oldest first.
pub fn find_origins(
    side: &SideRepo,
    path: &RelPath,
    wanted: &[Wanted],
    git: &GitView<'_>,
) -> Result<Origins> {
    let mut hits: Vec<Vec<TurnRecord>> = vec![Vec::new(); wanted.len()];
    let mut unchecked = 0;
    for thread in side.threads()? {
        let mut chain = side.turns(&thread, usize::MAX)?;
        chain.reverse();
        let Some(first) = chain.first() else { continue };
        let mut before = start_state(first, path, git);
        let mut open: Vec<usize> = (0..wanted.len()).collect();
        for (n, turn) in chain.iter().enumerate() {
            let now = state_at(side, turn, path)?;
            let Some(prev) = &before else {
                unchecked += usize::from(n == 0);
                before = Some(now);
                continue;
            };
            if now.id.is_some() && now.id == prev.id {
                continue;
            }
            let added = added_lines(&prev.bytes, &now.bytes);
            open.retain(|&j| {
                let hit = added.contains(wanted[j].text.as_slice());
                if hit {
                    hits[j].push(turn.clone());
                }
                !hit
            });
            before = Some(now);
            if open.is_empty() {
                break;
            }
        }
    }
    let found = hits
        .into_iter()
        .zip(wanted)
        .map(|(turns, want)| best_of(turns, want, git))
        .collect();
    Ok(Origins { found, unchecked })
}

/// Turns that were recorded no later than the commit, and from a base the commit descends from, come
/// first. Within a group the earliest turn wins; a tie goes to the thread name, then the turn number.
fn best_of(turns: Vec<TurnRecord>, want: &Wanted, git: &GitView<'_>) -> Option<Found> {
    let others = turns.len().checked_sub(1)?;
    let rank = |t: &TurnRecord| {
        let late = want.commit.as_ref().is_some_and(|c| t.at > c.at);
        let foreign = want.commit.as_ref().is_some_and(|c| {
            t.turn
                .draft
                .base
                .as_ref()
                .is_some_and(|base| !(git.is_ancestor)(base, &c.id))
        });
        (
            u8::from(late) + u8::from(foreign),
            t.at,
            t.turn.draft.thread.as_str().to_owned(),
            t.turn.seq.get(),
        )
    };
    let best = turns.into_iter().min_by_key(rank)?;
    Some(Found { best, others })
}

#[derive(Serialize)]
pub struct BlameDoc {
    pub schema: &'static str,
    pub unchecked_threads: usize,
    pub lines: Vec<BlameLine>,
}

#[derive(Serialize)]
pub struct BlameLine {
    /// The file as the git commit named it, relative to the repository root.
    pub path: String,
    pub line: u32,
    pub text: String,
    /// `null` for a line that is not committed yet.
    pub commit: Option<BlameCommit>,
    /// `null` when no recorded turn added the line.
    pub turn: Option<BlameTurn>,
    /// Other turns that also added the line.
    pub others: usize,
}

#[derive(Serialize)]
pub struct BlameCommit {
    pub id: String,
    pub author: String,
    pub at: i64,
    pub subject: String,
}

#[derive(Serialize)]
pub struct BlameTurn {
    pub commit: String,
    pub thread: String,
    pub turn: u32,
    pub base: Option<String>,
    pub agent: String,
    pub model: Option<String>,
    pub machine: String,
    pub at: i64,
    /// `null` when the turn recorded no message.
    pub subject: Option<String>,
}

impl BlameTurn {
    pub fn of(record: &TurnRecord) -> Self {
        let d = &record.turn.draft;
        Self {
            commit: record.id.to_string(),
            thread: d.thread.as_str().to_owned(),
            turn: record.turn.seq.get(),
            base: d.base.as_ref().map(ToString::to_string),
            agent: d.agent.as_str().to_owned(),
            model: d.model.as_ref().map(|m| m.as_str().to_owned()),
            machine: d.machine.as_str().to_owned(),
            at: record.at.millis(),
            subject: d.subject.recorded().map(str::to_owned),
        }
    }
}
