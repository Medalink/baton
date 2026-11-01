//! `baton bisect <thread> -- <command>`: the first turn of a thread at which a command starts failing.
//! Turns are applied into a scratch worktree made the way `baton pull` makes one, never into the user's
//! worktree, and the worktree is removed on every way out, Ctrl+C included.

use crate::commands::{describe, open_here};
use crate::pull::git;
use anyhow::{Context, Result, anyhow, bail};
use baton_core::{
    Applied, CheckEnd, Guard, SideRepo, ThreadId, TreeId, TurnRecord, apply_tree, run_check,
};
use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// `git bisect run`'s code for "this commit cannot be tested".
const SKIP: i32 = 125;
const INTERRUPTED: i32 = 130;
const LONGEST_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

static STOP: AtomicBool = AtomicBool::new(false);

/// `300`, `300s`, `500ms`, `5m` or `2h`. A bare number is seconds.
fn parse_timeout(text: &str) -> Result<Duration> {
    let bad =
        || anyhow!("{text:?} is not a timeout like \"300\", \"500ms\", \"90s\", \"5m\" or \"2h\"");
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let n: u64 = digits.parse().map_err(|_| bad())?;
    let timeout = match unit {
        "" | "s" => Duration::from_secs(n),
        "ms" => Duration::from_millis(n),
        "m" => Duration::from_secs(n.saturating_mul(60)),
        "h" => Duration::from_secs(n.saturating_mul(3600)),
        _ => return Err(bad()),
    };
    if timeout.is_zero() || timeout > LONGEST_TIMEOUT {
        bail!("{text:?} is not between 1ms and 24h");
    }
    Ok(timeout)
}

/// Sets `STOP` on Ctrl+C (and SIGTERM on Unix) so the loop can kill the check and remove the worktree.
/// Returns once the handler is installed, since a signal before that would end the process with the
/// scratch worktree still registered.
fn watch_interrupts() {
    let (ready, installed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            let _ = ready.send(());
            return;
        };
        runtime.block_on(async {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let (Ok(mut int), Ok(mut term)) = (
                    signal(SignalKind::interrupt()),
                    signal(SignalKind::terminate()),
                ) else {
                    let _ = ready.send(());
                    return;
                };
                let _ = ready.send(());
                tokio::select! { _ = int.recv() => {}, _ = term.recv() => {} }
            }
            #[cfg(windows)]
            {
                let Ok(mut int) = tokio::signal::windows::ctrl_c() else {
                    let _ = ready.send(());
                    return;
                };
                let _ = ready.send(());
                int.recv().await;
            }
            STOP.store(true, Ordering::Relaxed);
        });
    });
    let _ = installed.recv_timeout(Duration::from_secs(5));
}

fn skipped_of(turn: &TurnRecord) -> HashSet<String> {
    turn.turn
        .draft
        .skipped
        .iter()
        .map(|p| p.as_str().to_owned())
        .collect()
}

/// A worktree made by `git worktree add --detach` at the thread's base, holding one turn at a time.
/// Dropping it removes the worktree.
struct Scratch<'a> {
    root: &'a Path,
    dir: PathBuf,
    base: String,
    held: Option<TreeId>,
}

impl<'a> Scratch<'a> {
    fn create(root: &'a Path, dir: PathBuf, base: String) -> Result<Self> {
        let scratch = Self {
            root,
            dir,
            base,
            held: None,
        };
        scratch.register()?;
        Ok(scratch)
    }

    fn register(&self) -> Result<()> {
        git(
            self.root,
            &[
                "worktree",
                "add",
                "--detach",
                &self.dir.to_string_lossy(),
                &self.base,
            ],
        )
        .map(drop)
    }

    fn unregister(&self) {
        let dir = self.dir.to_string_lossy();
        if git(self.root, &["worktree", "remove", "--force", &dir]).is_err() {
            // A process that outlived its kill can hold a file on Windows for a moment.
            for _ in 0..20 {
                if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = git(self.root, &["worktree", "prune"]);
        }
        if self.dir.exists() {
            eprintln!(
                "baton: could not remove the scratch worktree {}; delete it, then run `git worktree prune`",
                self.dir.display()
            );
        }
    }

    /// Makes the directory hold `turn`. A file the check changed cannot be moved forward safely, so then
    /// the worktree is made again from the base.
    fn goto(&mut self, side: &SideRepo, turn: &TurnRecord) -> Result<()> {
        let keep = skipped_of(turn);
        if let Some(from) = self.held {
            let applied = apply_tree(
                side,
                &self.dir,
                turn.tree,
                &Guard::Holds { from, keep: &keep },
            )
            .map_err(|e| anyhow!("{e}"))?;
            if applied.refused.is_empty() {
                self.held = Some(turn.tree);
                return Ok(());
            }
            self.held = None;
            self.unregister();
            self.register()?;
        }
        let applied = self.fresh(side, turn, &keep)?;
        if !applied.refused.is_empty() {
            let first = &applied.refused[0];
            bail!(
                "{} files of {}@{} could not be written ({}: {})",
                applied.refused.len(),
                turn.turn.draft.thread,
                turn.turn.seq,
                first.path,
                first.reason
            );
        }
        self.held = Some(turn.tree);
        Ok(())
    }

    fn fresh(&self, side: &SideRepo, turn: &TurnRecord, keep: &HashSet<String>) -> Result<Applied> {
        let listed = git(&self.dir, &["ls-files", "-z"])?;
        let git_paths: Vec<String> = listed
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        apply_tree(
            side,
            &self.dir,
            turn.tree,
            &Guard::Fresh {
                git_paths: &git_paths,
                keep,
            },
        )
        .map_err(|e| anyhow!("{e}"))
    }
}

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        self.unregister();
    }
}

enum Verdict {
    Good,
    Bad,
    Skip,
}

struct Probe<'a> {
    side: &'a SideRepo,
    scratch: Scratch<'a>,
    command: &'a [String],
    timeout: Duration,
}

impl Probe<'_> {
    /// Runs the command at `turn`. `None` when interrupted.
    fn run(&mut self, turn: &TurnRecord) -> Result<Option<Verdict>> {
        if STOP.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let d = &turn.turn.draft;
        println!(
            "testing {}@{} ({})",
            d.thread,
            turn.turn.seq,
            turn.id.short()
        );
        let _ = std::io::stdout().flush();
        self.scratch.goto(self.side, turn)?;
        let ended = run_check(self.command, &self.scratch.dir, self.timeout, &STOP)
            .map_err(|e| anyhow!("{e}"))?;
        let (verdict, what) = match ended {
            CheckEnd::Interrupted => return Ok(None),
            CheckEnd::TimedOut => (
                Verdict::Bad,
                format!("bad (timed out after {}s)", self.timeout.as_secs_f64()),
            ),
            CheckEnd::Exited(Some(0)) => (Verdict::Good, "good".to_owned()),
            CheckEnd::Exited(Some(SKIP)) => (Verdict::Skip, "skipped (exit 125)".to_owned()),
            CheckEnd::Exited(Some(code)) => (Verdict::Bad, format!("bad (exit {code})")),
            CheckEnd::Exited(None) => (Verdict::Bad, "bad (ended by a signal)".to_owned()),
        };
        println!("  {what}");
        Ok(Some(verdict))
    }
}

pub struct Options {
    pub thread: String,
    pub good: Option<u32>,
    pub bad: Option<u32>,
    pub timeout: String,
    pub command: Vec<String>,
}

fn index_of(turns: &[TurnRecord], seq: u32, flag: &str) -> Result<usize> {
    turns
        .iter()
        .position(|t| t.turn.seq.get() == seq)
        .ok_or_else(|| anyhow!("--{flag} {seq}: the thread has no turn {seq}"))
}

/// The process exit code: 0 when it named a first bad turn, 130 when interrupted.
pub fn run(options: Options) -> Result<i32> {
    let timeout = parse_timeout(&options.timeout)?;
    let here = open_here()?;
    let thread = ThreadId::parse(&options.thread).map_err(|e| anyhow!("{e}"))?;
    let mut turns = here
        .side
        .turns(&thread, usize::MAX)
        .map_err(|e| anyhow!("{e}"))?;
    turns.reverse();
    let Some(first) = turns.first() else {
        bail!("this machine has no turns of {thread}; `baton log` lists the threads it has");
    };
    let base = first.turn.draft.base.as_ref().ok_or_else(|| {
        anyhow!("{thread} began in a repository with no commit, so there is nothing to base a worktree on")
    })?;
    if git(
        &here.root,
        &["cat-file", "-e", &format!("{base}^{{commit}}")],
    )
    .is_err()
    {
        bail!(
            "the base commit {base} of {thread} is not in this repository; run `git fetch` and try again"
        );
    }
    let good = options
        .good
        .map_or(Ok(0), |n| index_of(&turns, n, "good"))?;
    let bad_given = options
        .bad
        .map(|n| index_of(&turns, n, "bad"))
        .transpose()?;
    let tip = turns.len() - 1;
    if good >= bad_given.unwrap_or(tip) {
        bail!(
            "{thread} needs a good turn before a bad one; it has {} turn(s)",
            turns.len()
        );
    }

    watch_interrupts();
    let short: String = thread.as_str().chars().take(8).collect();
    let dir = std::env::temp_dir().join(format!("baton-bisect-{}-{short}", std::process::id()));
    let scratch = Scratch::create(&here.root, dir, base.to_string())
        .context("making the scratch worktree")?;
    let mut probe = Probe {
        side: &here.side,
        scratch,
        command: &options.command,
        timeout,
    };

    let mut lo = good;
    let mut hi = match bad_given {
        Some(bad) => bad,
        None => match probe.run(&turns[tip])? {
            None => return interrupted(),
            Some(Verdict::Bad) => tip,
            Some(Verdict::Good) => {
                bail!("the command passes at the latest turn, so no turn of {thread} fails it")
            }
            Some(Verdict::Skip) => {
                bail!("the command skipped the latest turn; name a failing turn with --bad")
            }
        },
    };
    let mut skipped: BTreeSet<usize> = BTreeSet::new();
    loop {
        let candidates: Vec<usize> = (lo + 1..hi).filter(|i| !skipped.contains(i)).collect();
        let Some(&mid) = candidates.get(candidates.len() / 2) else {
            break;
        };
        match probe.run(&turns[mid])? {
            None => return interrupted(),
            Some(Verdict::Good) => lo = mid,
            Some(Verdict::Bad) => hi = mid,
            Some(Verdict::Skip) => {
                skipped.insert(mid);
            }
        }
    }
    let unsure: Vec<usize> = (lo + 1..hi)
        .filter(|i| skipped.contains(i))
        .chain(std::iter::once(hi))
        .collect();
    if let [only] = unsure[..] {
        println!("first bad turn:");
        describe(&here.side, &turns[only])?;
    } else {
        println!(
            "the first bad turn is one of these {}; the command skipped all but the last:",
            unsure.len()
        );
        for i in unsure {
            describe(&here.side, &turns[i])?;
        }
    }
    Ok(0)
}

fn interrupted() -> Result<i32> {
    eprintln!("baton: interrupted; removing the scratch worktree");
    Ok(INTERRUPTED)
}
