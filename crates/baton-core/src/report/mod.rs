//! `baton report`: patterns in how agents work, read from the side repo and, read-only, the user's repo.
//! The rules are defined in `docs/design.md`, History, Report, and the document in `docs/protocol.md`.
//!
//! Nothing here writes to either repository. The only thing written is the statistics cache under baton's
//! data dir, keyed by immutable object ids (see `cache.rs`).

mod cache;
mod lines;
mod rules;
mod stats;
mod types;

use crate::error::Result;
use crate::id::{ProjectKey, UnixMillis};
use crate::paths::{Dirs, fnv1a};
use crate::store::{SideRepo, TurnRecord};
use cache::{Cache, Kind};
use gix::ObjectId;
use rules::{QUIET_SECS, ShipIndex};
use stats::{Basis, FileStat, Readers};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use types::{ThreadRun, TurnRun};

pub use types::{
    Abandoned, AbandonedFile, CheckRun, Counts, Group, Hotspot, REPORT_SCHEMA, RepoInfo, Report,
    Rework, ReworkedBy, Skipped, Thrash,
};

/// The walk over the repository's recent commits stops here.
const COMMITS_MAX: usize = 20_000;
const WORKERS_MAX: usize = 8;
/// Fewer jobs than this run on the calling thread.
const PARALLEL_FROM: usize = 16;

pub struct ReportOptions {
    pub project: ProjectKey,
    pub side_dir: PathBuf,
    /// A worktree or git dir of the project's repository; none when this machine has none to read.
    pub repo: Option<PathBuf>,
    /// Where the statistics cache lives; created when needed.
    pub cache_dir: PathBuf,
    /// Baton's dirs: the user config feeds the store policy a first turn is compared through.
    pub dirs: Dirs,
    /// The window, counted back from `now`.
    pub window_secs: i64,
    pub now: UnixMillis,
}

enum Job {
    Turn(ObjectId, Basis),
    Commit(ObjectId),
}

type Key = cache::Key;
type Outcome = Result<Option<Vec<FileStat>>>;

fn run_job(readers: &Readers, job: &Job) -> Outcome {
    match job {
        Job::Turn(tree, basis) => stats::turn_stats(readers, *tree, *basis),
        Job::Commit(id) => stats::commit_stats(readers, *id),
    }
}

/// Runs every job, on several threads when there are many. Results come back in job order.
fn run_jobs(
    side_dir: &Path,
    repo: Option<&Path>,
    dirs: &Dirs,
    jobs: &[(Key, Job)],
) -> Vec<Outcome> {
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(WORKERS_MAX);
    let workers = if jobs.len() < PARALLEL_FROM {
        1
    } else {
        workers
    };
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<(usize, Outcome)>> = Mutex::new(Vec::with_capacity(jobs.len()));
    let work = || {
        let readers = Readers::open(side_dir, repo, dirs);
        let mut local = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let Some((_, job)) = jobs.get(i) else { break };
            let result = match &readers {
                Ok(readers) => run_job(readers, job),
                Err(e) => Err(crate::error::Error::Refused(e.to_string())),
            };
            local.push((i, result));
        }
        if let Ok(mut done) = done.lock() {
            done.extend(local);
        }
    };
    std::thread::scope(|scope| {
        // A deep tree recurses; the spawned threads get room for the deepest one a repo can hold.
        for _ in 1..workers {
            let _ = std::thread::Builder::new()
                .stack_size(32 << 20)
                .spawn_scoped(scope, work);
        }
        work();
    });
    let mut done = done.into_inner().unwrap_or_default();
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

/// The user's recent commits, newest first by committer time, and whether the cap cut the walk short.
fn recent_commits(
    repo: &gix::Repository,
    head: ObjectId,
    cutoff: i64,
) -> (Vec<(ObjectId, i64, bool)>, bool) {
    let mut out = Vec::new();
    let mut seen = HashSet::from([head]);
    let mut heap = BinaryHeap::new();
    match stats::commit_info(repo, head) {
        Ok(info) => heap.push((info.time, head)),
        Err(_) => return (out, false),
    }
    while let Some((time, id)) = heap.pop() {
        if time < cutoff {
            break;
        }
        if out.len() >= COMMITS_MAX {
            return (out, true);
        }
        let Ok(info) = stats::commit_info(repo, id) else {
            continue;
        };
        out.push((id, time, info.parents.len() > 1));
        for parent in info.parents {
            if seen.insert(parent)
                && let Ok(p) = stats::commit_info(repo, parent)
            {
                heap.push((p.time, parent));
            }
        }
    }
    (out, false)
}

struct Chain {
    name: String,
    /// The turns inside the window, oldest first.
    turns: Vec<TurnRecord>,
    /// Aligned with `turns`: the tree of each turn's parent turn, none for a thread's first turn.
    parents: Vec<Option<ObjectId>>,
}

/// Every thread's turns inside the window, and the cache keys still in use.
struct Walk {
    chains: Vec<Chain>,
    live: HashSet<Key>,
}

fn salted(id: ObjectId, salt: &[u8]) -> ObjectId {
    let bytes = [id.as_bytes(), salt].concat();
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, &bytes).unwrap_or(id)
}

/// The key a turn's statistics are cached under. A first turn is measured against a commit of the user's
/// repository through the store policy, so its key also names the policy: a changed `exclude` or
/// redaction rule finds nothing cached.
fn turn_key(id: ObjectId, first: bool, salt: &[u8]) -> Key {
    (Kind::Turn, if first { salted(id, salt) } else { id })
}

fn tip_key(last: ObjectId, salt: &[u8]) -> Key {
    (Kind::Tip, salted(last, salt))
}

/// Each thread is walked to its first turn and the window filters by time afterwards. Turns written on
/// machines whose clocks disagree are out of time order along the parent links, so a turn older than the
/// window says nothing about the turns before it. A turn whose parent cannot be read has no known basis and
/// is left out.
fn chains(side: &SideRepo, cutoff: i64, salt: &[u8], skipped: &mut Skipped) -> Result<Walk> {
    let mut walk = Walk {
        chains: Vec::new(),
        live: HashSet::new(),
    };
    for (thread, tip) in side.tips()? {
        walk.live.insert(tip_key(tip.oid(), salt));
        let mut records = Vec::new();
        let mut next = Some(tip);
        while let Some(id) = next {
            let Ok(record) = side.read_turn(id) else {
                skipped.unreadable += 1;
                break;
            };
            walk.live
                .insert(turn_key(id.oid(), record.parent.is_none(), salt));
            next = record.parent;
            records.push(record);
        }
        let trees: Vec<ObjectId> = records.iter().map(|r| r.tree.oid()).collect();
        let (mut turns, mut parents) = (Vec::new(), Vec::new());
        for (i, record) in records.into_iter().enumerate() {
            let parent = match record.parent {
                None => None,
                Some(_) => match trees.get(i + 1) {
                    Some(&tree) => Some(tree),
                    None => continue,
                },
            };
            if record.at.secs() >= cutoff {
                turns.push(record);
                parents.push(parent);
            }
        }
        if turns.is_empty() {
            continue;
        }
        turns.reverse();
        parents.reverse();
        walk.chains.push(Chain {
            name: thread.to_string(),
            turns,
            parents,
        });
    }
    Ok(walk)
}

pub fn build_report(o: &ReportOptions) -> Result<Report> {
    let side = SideRepo::open_existing(&o.side_dir)?;
    let probe = Readers::open(&o.side_dir, o.repo.as_deref(), &o.dirs)?;
    let now = o.now.secs();
    let cutoff = now - o.window_secs;
    let mut skipped = Skipped::default();
    // What a first turn's statistics also depend on: the store policy of the user's worktree.
    let policy_print = o
        .repo
        .as_deref()
        .and_then(|root| crate::worktree::open_policy_fingerprinted(root, &o.dirs).ok())
        .map(|(_, print)| print);
    let salt = policy_print.clone().unwrap_or_default().into_bytes();
    let Walk { chains, live } = chains(&side, cutoff, &salt, &mut skipped)?;

    let head = probe.user.as_ref().and_then(stats::head_commit);
    let head_info = match (&probe.user, head) {
        (Some(user), Some(head)) => stats::commit_info(user, head).ok(),
        _ => None,
    };
    let available = probe.user.is_some();

    // Everything to look up or compute, each under the immutable id that names it.
    let mut wanted: Vec<(Key, Job)> = Vec::new();
    let mut tips: HashMap<usize, Key> = HashMap::new();
    for (ci, chain) in chains.iter().enumerate() {
        for (ui, turn) in chain.turns.iter().enumerate() {
            let basis = match chain.parents[ui] {
                None => Basis::Base(turn.turn.draft.base.map(|b| b.oid())),
                Some(parent) => Basis::Parent(parent),
            };
            wanted.push((
                turn_key(turn.id.oid(), turn.parent.is_none(), &salt),
                Job::Turn(turn.tree.oid(), basis),
            ));
        }
        let last = chain.turns.last().expect("a chain has a turn");
        if last.at.secs() < now - QUIET_SECS && last.parent.is_some() && head_info.is_some() {
            let key = tip_key(last.id.oid(), &salt);
            wanted.push((
                key,
                Job::Turn(
                    last.tree.oid(),
                    Basis::Base(last.turn.draft.base.map(|b| b.oid())),
                ),
            ));
            tips.insert(ci, key);
        }
    }
    let mut commits = Vec::new();
    if let (Some(user), Some(head)) = (&probe.user, head) {
        let (recent, truncated) = recent_commits(user, head, cutoff);
        skipped.commits_truncated = truncated;
        for &(id, _, merge) in &recent {
            if !merge {
                wanted.push(((Kind::Commit, id), Job::Commit(id)));
            }
        }
        commits = recent;
    }

    let cache_name = o.project.dir_name();
    let mut cache = Cache::open(cache::path_in(&o.cache_dir, &cache_name));
    let mut found: HashMap<Key, Option<Vec<FileStat>>> = HashMap::with_capacity(wanted.len());
    let mut missing = Vec::new();
    for (key, job) in wanted {
        match cache.get(&key) {
            Some(files) => {
                found.insert(key, Some(files));
            }
            None => missing.push((key, job)),
        }
    }
    let results = run_jobs(&o.side_dir, o.repo.as_deref(), &o.dirs, &missing);
    let mut unknown_base: HashSet<Key> = HashSet::new();
    for ((key, job), result) in missing.iter().zip(results) {
        match result {
            Ok(Some(files)) => {
                cache.put(key, &files);
                found.insert(*key, Some(files));
            }
            Ok(None) => {
                if matches!(job, Job::Turn(_, Basis::Base(Some(_)))) {
                    unknown_base.insert(*key);
                }
                found.insert(*key, None);
            }
            Err(_) => {
                skipped.unreadable += 1;
                found.insert(*key, None);
            }
        }
    }
    // Drop what no ref reaches any more. With no policy to name the first-turn keys by, or no repository to
    // ask about commits, nothing is dropped.
    let known = policy_print.is_some();
    let user = probe.user.as_ref();
    cache.save(&|key: &Key| match key.0 {
        Kind::Commit => user.is_none_or(|u| u.has_object(key.1)),
        _ => !known || live.contains(key),
    });

    let mut ship: ShipIndex = HashMap::new();
    for (id, time, merge) in commits {
        if merge {
            continue;
        }
        let Some(Some(files)) = found.remove(&(Kind::Commit, id)) else {
            continue;
        };
        for f in files.iter().filter(|f| f.lines) {
            let path = fnv1a(f.path.as_bytes());
            for h in &f.added {
                ship.entry((path, *h)).or_default().push(time);
            }
        }
    }
    for times in ship.values_mut() {
        times.sort_unstable();
    }

    let mut runs = Vec::with_capacity(chains.len());
    for (ci, chain) in chains.iter().enumerate() {
        let mut without_base = false;
        let mut turns = Vec::with_capacity(chain.turns.len());
        for turn in &chain.turns {
            let key = turn_key(turn.id.oid(), turn.parent.is_none(), &salt);
            without_base |= unknown_base.contains(&key);
            let d = &turn.turn.draft;
            turns.push(TurnRun {
                seq: turn.turn.seq.get(),
                at: turn.at.secs(),
                agent: d.agent.to_string(),
                model: d.model.as_ref().map(ToString::to_string),
                checks: d
                    .checks
                    .iter()
                    .map(|c| (c.name.to_string(), c.exit))
                    .collect(),
                files: found.remove(&key).flatten(),
            });
        }
        let tip = match tips.get(&ci) {
            Some(key) => {
                without_base |= unknown_base.contains(key);
                found.remove(key).flatten()
            }
            None => {
                let last = chain.turns.last().expect("a chain has a turn");
                let quiet = last.at.secs() < now - QUIET_SECS && head_info.is_some();
                (quiet && last.parent.is_none())
                    .then(|| turns.last().and_then(|t| t.files.clone()))
                    .flatten()
            }
        };
        if available && without_base {
            skipped.threads_without_base += 1;
        }
        runs.push(ThreadRun {
            name: chain.name.clone(),
            turns,
            tip,
        });
    }

    let thrash = rules::thrash(&runs);
    let shipped = rules::rework(&runs, &ship);
    let abandoned = match (&probe.user, &head_info) {
        (Some(user), Some(info)) => rules::abandoned(&runs, user, info.tree).unwrap_or_else(|_| {
            skipped.unreadable += 1;
            Vec::new()
        }),
        _ => Vec::new(),
    };
    let hotspots = rules::hotspots(&thrash, &shipped.entries);
    let models = rules::groups(&runs, |t| t.model.clone(), &thrash, &shipped.per_turn);
    let agents = rules::groups(&runs, |t| Some(t.agent.clone()), &thrash, &shipped.per_turn);
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    Ok(Report {
        schema: REPORT_SCHEMA,
        project: o.project.to_string(),
        generated_at: o.now.millis(),
        since: cutoff.saturating_mul(1000),
        repo: RepoInfo {
            available,
            head: head.map(|h| h.to_string()),
        },
        counts: Counts {
            threads: count(runs.len()),
            turns: count(runs.iter().map(|r| r.turns.len()).sum()),
            thrash: count(thrash.len()),
            rework: count(shipped.entries.len()),
            abandoned: count(abandoned.len()),
            hotspots: count(hotspots.len()),
        },
        thrash: thrash.into_iter().map(|h| h.entry).collect(),
        rework: shipped.entries,
        abandoned,
        hotspots,
        models,
        agents,
        skipped,
    })
}
