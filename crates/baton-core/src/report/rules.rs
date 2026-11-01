//! The report's rules. Each one is defined in `docs/design.md`, History, Report; the comments here say
//! only how that definition is computed.

use super::stats::{FileStat, HeadFile, Status, head_file};
use super::types::{
    Abandoned, AbandonedFile, CheckRun, Group, Hotspot, Rework, ReworkedBy, Thrash, ThreadRun,
    TurnRun,
};
use crate::error::Result;
use crate::paths::fnv1a;
use gix::ObjectId;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Rework counts a change this soon after the lines reached `HEAD`'s history.
pub(super) const REWORK_WINDOW_SECS: i64 = 48 * 3600;
/// A thread with no turn for this long is judged for abandonment.
pub(super) const QUIET_SECS: i64 = 24 * 3600;
/// File lists stop here, in the JSON document too.
const FILES_LISTED: usize = 20;

/// For each (path hash, line hash), the committer times of the user's commits that added that line,
/// ascending.
pub(super) type ShipIndex = HashMap<(u64, u64), Vec<i64>>;

pub(super) struct ThrashHit {
    pub entry: Thrash,
    pub thread: usize,
    pub turn: usize,
}

#[derive(Default)]
struct FileState {
    touches: Vec<u32>,
    added: HashSet<u64>,
    redone: u32,
    trigger: Option<usize>,
}

pub(super) fn thrash(runs: &[ThreadRun]) -> Vec<ThrashHit> {
    let mut hits = Vec::new();
    for (ti, run) in runs.iter().enumerate() {
        let mut files: BTreeMap<&str, FileState> = BTreeMap::new();
        for (ui, turn) in run.turns.iter().enumerate() {
            for f in turn.files.iter().flatten() {
                let state = files.entry(f.path.as_str()).or_default();
                state.touches.push(turn.seq);
                if f.lines {
                    for h in &f.removed {
                        if state.added.remove(h) {
                            state.redone += 1;
                        }
                    }
                    state.added.extend(f.added.iter().copied());
                }
                if state.trigger.is_none() && state.touches.len() >= 3 && state.redone > 0 {
                    state.trigger = Some(ui);
                }
            }
        }
        for (path, state) in files {
            if let Some(ui) = state.trigger {
                hits.push(ThrashHit {
                    entry: Thrash::File {
                        thread: run.name.clone(),
                        turn: run.turns[ui].seq,
                        file: path.to_owned(),
                        turns: state.touches,
                        lines: state.redone,
                    },
                    thread: ti,
                    turn: ui,
                });
            }
        }
        check_flips(run, ti, &mut hits);
    }
    hits.sort_by(|a, b| {
        weight(&b.entry)
            .cmp(&weight(&a.entry))
            .then_with(|| order(&a.entry).cmp(&order(&b.entry)))
    });
    hits
}

/// Red, green, red: the runs of one check with equal neighbours merged show a failure, a pass and a
/// failure again.
fn check_flips(run: &ThreadRun, ti: usize, hits: &mut Vec<ThrashHit>) {
    let mut by_check: BTreeMap<&str, Vec<(usize, u32, i32)>> = BTreeMap::new();
    for (ui, turn) in run.turns.iter().enumerate() {
        for (name, exit) in &turn.checks {
            by_check
                .entry(name.as_str())
                .or_default()
                .push((ui, turn.seq, *exit));
        }
    }
    for (name, runs) in by_check {
        let mut states: Vec<(bool, usize)> = Vec::new();
        for &(ui, _, exit) in &runs {
            let red = exit != 0;
            if states.last().is_none_or(|&(last, _)| last != red) {
                states.push((red, ui));
            }
        }
        let flips: Vec<usize> = states
            .windows(3)
            .filter(|w| w[0].0 && !w[1].0 && w[2].0)
            .map(|w| w[2].1)
            .collect();
        if let Some(&ui) = flips.first() {
            hits.push(ThrashHit {
                entry: Thrash::Check {
                    thread: run.name.clone(),
                    turn: run.turns[ui].seq,
                    check: name.to_owned(),
                    runs: runs
                        .iter()
                        .map(|&(_, turn, exit)| CheckRun { turn, exit })
                        .collect(),
                },
                thread: ti,
                turn: ui,
            });
        }
    }
}

fn weight(t: &Thrash) -> (u8, usize, usize) {
    match t {
        Thrash::File { turns, lines, .. } => (1, *lines as usize, turns.len()),
        Thrash::Check { runs, .. } => (0, runs.len(), 0),
    }
}

fn order(t: &Thrash) -> (&str, u32, &str) {
    match t {
        Thrash::File {
            thread, turn, file, ..
        } => (thread, *turn, file),
        Thrash::Check {
            thread,
            turn,
            check,
            ..
        } => (thread, *turn, check),
    }
}

pub(super) struct ReworkOut {
    pub entries: Vec<Rework>,
    /// Per (thread, turn index): lines that shipped, lines that were reworked.
    pub per_turn: HashMap<(usize, usize), (u32, u32)>,
}

pub(super) fn rework(runs: &[ThreadRun], ship: &ShipIndex) -> ReworkOut {
    // Every removal in the window, by (path, line): when, which thread, which turn.
    type Removal = (i64, usize, u32);
    let mut removals: HashMap<(u64, u64), Vec<Removal>> = HashMap::new();
    for (ti, run) in runs.iter().enumerate() {
        for turn in &run.turns {
            for f in turn.files.iter().flatten().filter(|f| f.lines) {
                let path = fnv1a(f.path.as_bytes());
                for h in &f.removed {
                    removals
                        .entry((path, *h))
                        .or_default()
                        .push((turn.at, ti, turn.seq));
                }
            }
        }
    }
    for list in removals.values_mut() {
        list.sort_unstable();
    }
    let mut out = ReworkOut {
        entries: Vec::new(),
        per_turn: HashMap::new(),
    };
    for (ti, run) in runs.iter().enumerate() {
        for (ui, turn) in run.turns.iter().enumerate() {
            let (mut turn_shipped, mut turn_reworked) = (0u32, 0u32);
            for f in turn.files.iter().flatten().filter(|f| f.lines) {
                let path = fnv1a(f.path.as_bytes());
                let (mut reworked, mut first_ship) = (0u32, i64::MAX);
                let mut by: Option<Removal> = None;
                for h in &f.added {
                    let Some(times) = ship.get(&(path, *h)) else {
                        continue;
                    };
                    let Some(&shipped) = times.get(times.partition_point(|t| *t < turn.at)) else {
                        continue;
                    };
                    turn_shipped += 1;
                    let Some(list) = removals.get(&(path, *h)) else {
                        continue;
                    };
                    let from = list.partition_point(|r| r.0 < shipped);
                    let found = list[from..]
                        .iter()
                        .take_while(|r| r.0 <= shipped + REWORK_WINDOW_SECS)
                        .find(|r| r.1 != ti);
                    if let Some(&r) = found {
                        reworked += 1;
                        first_ship = first_ship.min(shipped);
                        by = Some(by.map_or(r, |b| b.min(r)));
                    }
                }
                if let (true, Some((at, other, other_turn))) = (reworked > 0, by) {
                    turn_reworked += reworked;
                    out.entries.push(Rework {
                        thread: run.name.clone(),
                        turn: turn.seq,
                        file: f.path.clone(),
                        lines: reworked,
                        shipped_at: first_ship.saturating_mul(1000),
                        by: ReworkedBy {
                            thread: runs[other].name.clone(),
                            turn: other_turn,
                            at: at.saturating_mul(1000),
                        },
                    });
                }
            }
            if turn_shipped > 0 {
                out.per_turn.insert((ti, ui), (turn_shipped, turn_reworked));
            }
        }
    }
    out.entries.sort_by(|a, b| {
        b.lines
            .cmp(&a.lines)
            .then_with(|| (&a.thread, a.turn, &a.file).cmp(&(&b.thread, b.turn, &b.file)))
    });
    out
}

/// Threads quiet for a day whose last tree mostly never reached `HEAD`.
pub(super) fn abandoned(
    runs: &[ThreadRun],
    user: &gix::Repository,
    head_tree: ObjectId,
) -> Result<Vec<Abandoned>> {
    let mut out = Vec::new();
    // Threads touch the same files, and reading one from `HEAD` walks its trees: do it once per path.
    let mut heads: HashMap<&str, HeadFile> = HashMap::new();
    for run in runs {
        let (Some(tip), Some(last)) = (&run.tip, run.turns.last()) else {
            continue;
        };
        let (mut changed, mut reached) = (0u32, 0u32);
        let mut files = Vec::new();
        for f in tip {
            let head = match heads.entry(f.path.as_str()) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(head_file(user, head_tree, &f.path)?),
            };
            let (c, r) = reach(f, head);
            changed += c;
            reached += r;
            if c > 0 && r * 2 < c {
                files.push(AbandonedFile {
                    path: f.path.clone(),
                    changed_lines: c,
                    reached_lines: r,
                });
            }
        }
        if changed == 0 || reached * 2 >= changed {
            continue;
        }
        files.sort_by(|a, b| {
            (b.changed_lines - b.reached_lines)
                .cmp(&(a.changed_lines - a.reached_lines))
                .then_with(|| a.path.cmp(&b.path))
        });
        let files_total = u32::try_from(files.len()).unwrap_or(u32::MAX);
        files.truncate(FILES_LISTED);
        out.push(Abandoned {
            thread: run.name.clone(),
            turn: last.seq,
            last_at: last.at.saturating_mul(1000),
            changed_lines: changed,
            reached_lines: reached,
            files,
            files_total,
        });
    }
    out.sort_by(|a, b| {
        (b.changed_lines - b.reached_lines)
            .cmp(&(a.changed_lines - a.reached_lines))
            .then_with(|| a.thread.cmp(&b.thread))
    });
    Ok(out)
}

/// `(changed, reached)` for one file: a line the thread added counts as reached when `HEAD`'s copy holds it,
/// a line it removed when `HEAD`'s copy no longer holds it. A file without line data counts as one unit.
fn reach(f: &FileStat, head: &HeadFile) -> (u32, u32) {
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    if !f.lines {
        let reached = match (f.status, head) {
            (Status::Removed, HeadFile::Missing) => true,
            (Status::Removed, _) | (_, HeadFile::Missing) => false,
            (_, HeadFile::Opaque(blob)) => *blob == f.to,
            (_, HeadFile::Lines(_)) => false,
        };
        return (1, u32::from(reached));
    }
    let changed = count(f.added.len() + f.removed.len());
    let reached = match head {
        HeadFile::Missing => count(f.removed.len()),
        HeadFile::Opaque(_) => count(f.removed.len()),
        HeadFile::Lines(set) => count(
            f.added.iter().filter(|h| set.contains(h)).count()
                + f.removed.iter().filter(|h| !set.contains(h)).count(),
        ),
    };
    (changed, reached)
}

pub(super) fn hotspots(thrash: &[ThrashHit], rework: &[Rework]) -> Vec<Hotspot> {
    let mut by_file: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    for hit in thrash {
        if let Thrash::File { file, .. } = &hit.entry {
            by_file.entry(file).or_default().0 += 1;
        }
    }
    for r in rework {
        by_file.entry(&r.file).or_default().1 += 1;
    }
    let mut out: Vec<Hotspot> = by_file
        .into_iter()
        .map(|(file, (thrash, rework))| Hotspot {
            file: file.to_owned(),
            thrash,
            rework,
        })
        .collect();
    out.sort_by(|a, b| {
        (b.thrash + b.rework)
            .cmp(&(a.thrash + a.rework))
            .then_with(|| b.rework.cmp(&a.rework))
            .then_with(|| a.file.cmp(&b.file))
    });
    out
}

#[derive(Default)]
struct Acc {
    turns: u32,
    threads: HashSet<usize>,
    shipped: u32,
    reworked: u32,
    thrash_threads: HashSet<usize>,
}

fn rate(part: u32, whole: u32) -> Option<f64> {
    (whole > 0).then(|| (f64::from(part) / f64::from(whole) * 10_000.0).round() / 10_000.0)
}

pub(super) fn groups(
    runs: &[ThreadRun],
    key: impl Fn(&TurnRun) -> Option<String>,
    thrash: &[ThrashHit],
    per_turn: &HashMap<(usize, usize), (u32, u32)>,
) -> Vec<Group> {
    let mut acc: BTreeMap<Option<String>, Acc> = BTreeMap::new();
    for (ti, run) in runs.iter().enumerate() {
        for (ui, turn) in run.turns.iter().enumerate() {
            let a = acc.entry(key(turn)).or_default();
            a.turns += 1;
            a.threads.insert(ti);
            if let Some(&(shipped, reworked)) = per_turn.get(&(ti, ui)) {
                a.shipped += shipped;
                a.reworked += reworked;
            }
        }
    }
    for hit in thrash {
        let turn = &runs[hit.thread].turns[hit.turn];
        if let Some(a) = acc.get_mut(&key(turn)) {
            a.thrash_threads.insert(hit.thread);
        }
    }
    let mut out: Vec<Group> = acc
        .into_iter()
        .map(|(name, a)| {
            let threads = u32::try_from(a.threads.len()).unwrap_or(u32::MAX);
            let thrash_threads = u32::try_from(a.thrash_threads.len()).unwrap_or(u32::MAX);
            Group {
                name,
                turns: a.turns,
                threads,
                shipped_lines: a.shipped,
                reworked_lines: a.reworked,
                rework_rate: rate(a.reworked, a.shipped),
                thrash_threads,
                thrash_rate: rate(thrash_threads, threads),
            }
        })
        .collect();
    out.sort_by(|a, b| b.turns.cmp(&a.turns).then_with(|| a.name.cmp(&b.name)));
    out
}
