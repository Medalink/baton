//! `baton bisect <thread> -- <command>`: the first turn of a thread at which a command starts failing.
//! Turns are applied into a scratch worktree made the way `baton pull` makes one, never into the user's
//! worktree, and the worktree is removed on every way out, Ctrl+C included. A baton that was killed
//! hard cannot clean up; the next `baton bisect` in the repository removes what it left.

use crate::commands::{describe, open_here};
use crate::pull::git;
use anyhow::{Context, Result, anyhow, bail};
use baton_core::{
    Applied, CheckEnd, Guard, SideRepo, ThreadId, TreeId, TurnRecord, apply_tree, run_check,
};
use std::collections::{BTreeSet, HashSet};
use std::fs::File;
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

/// Every scratch directory is `<temp>/baton-bisect-<pid>-<random>`, made by this run with `create_dir`
/// (it fails if the name exists), holding `owner.lock` and the worktree in `tree/`.
const HOME_PREFIX: &str = "baton-bisect-";
const LOCK_NAME: &str = "owner.lock";
const TREE_NAME: &str = "tree";

/// Makes a new, empty directory in the temp dir. Never reuses or touches one that already exists.
fn new_home() -> Result<PathBuf> {
    let temp = std::env::temp_dir();
    for _ in 0..16 {
        let mut bytes = [0u8; 6];
        getrandom::fill(&mut bytes).map_err(|e| anyhow!("no randomness: {e}"))?;
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let home = temp.join(format!("{HOME_PREFIX}{}-{suffix}", std::process::id()));
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&home) {
            Ok(()) => return Ok(home),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", home.display())),
        }
    }
    bail!(
        "could not find an unused scratch directory name in {}",
        temp.display()
    )
}

/// A worktree made by `git worktree add --detach` at the thread's base, holding one turn at a time.
/// The run holds an exclusive lock on `owner.lock` for as long as it lives; the operating system drops
/// the lock when the process dies however it dies, so another baton can tell a dead owner from a live one.
/// Dropping it removes the worktree and the directory, which this run made.
struct Scratch<'a> {
    root: &'a Path,
    home: PathBuf,
    dir: PathBuf,
    lock: Option<File>,
    base: String,
    held: Option<TreeId>,
}

impl<'a> Scratch<'a> {
    fn create(root: &'a Path, base: String) -> Result<Self> {
        let home = new_home()?;
        let mut scratch = Self {
            root,
            dir: home.join(TREE_NAME),
            home,
            lock: None,
            base,
            held: None,
        };
        let lock =
            File::create_new(scratch.home.join(LOCK_NAME)).context("creating the scratch lock")?;
        lock.try_lock()
            .map_err(|e| anyhow!("locking the scratch directory: {e}"))?;
        scratch.lock = Some(lock);
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

    /// Removes the worktree directory `tree/`, which this run made.
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
        self.lock = None;
        // The directory is this run's own: it was made by `new_home` and holds only the lock and `tree/`.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.home).is_ok() || !self.home.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if self.home.exists() {
            eprintln!(
                "baton: could not remove the scratch worktree {}; run `git -C {} worktree remove --force {}`,                  then delete {} (the next `baton bisect` in this repository also tries)",
                self.dir.display(),
                self.root.display(),
                self.dir.display(),
                self.home.display()
            );
        }
    }
}

/// Removes the scratch worktrees that earlier runs in this repository left behind because they were
/// killed hard. Only a worktree registered with git at `<temp>/baton-bisect-*/tree`, in a directory
/// whose `owner.lock` nobody holds any more, is touched, and only through `git worktree remove --force`;
/// the directory itself goes only if it is then empty apart from its lock.
fn clear_stale(root: &Path) {
    let Ok(listed) = git(root, &["worktree", "list", "--porcelain"]) else {
        return;
    };
    let Ok(temp) = std::fs::canonicalize(std::env::temp_dir()) else {
        return;
    };
    for line in String::from_utf8_lossy(&listed).lines() {
        let Some(path) = line.strip_prefix("worktree ") else {
            continue;
        };
        let tree = PathBuf::from(path);
        let Some(home) = tree.parent() else { continue };
        let named = tree.file_name().is_some_and(|n| n == TREE_NAME)
            && home
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(HOME_PREFIX));
        if !named
            || std::fs::canonicalize(home.parent().unwrap_or(home))
                .ok()
                .as_ref()
                != Some(&temp)
        {
            continue;
        }
        let lock_path = home.join(LOCK_NAME);
        let Ok(lock) = File::open(&lock_path) else {
            continue;
        };
        if lock.try_lock().is_err() {
            continue; // a baton that is still running owns it
        }
        let dir = tree.to_string_lossy();
        if git(root, &["worktree", "remove", "--force", &dir]).is_err() {
            eprintln!(
                "baton: the scratch worktree {} was left by a killed baton and could not be removed; run `git -C {} worktree remove --force {}`",
                tree.display(),
                root.display(),
                tree.display()
            );
            continue;
        }
        drop(lock);
        let _ = std::fs::remove_file(&lock_path);
        let _ = std::fs::remove_dir(home);
        eprintln!(
            "baton: removed the scratch worktree {}, left by a killed baton",
            tree.display()
        );
    }
}

#[derive(Clone, Copy)]
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
    clear_stale(&here.root);
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
    let good_given = options
        .good
        .map(|n| index_of(&turns, n, "good"))
        .transpose()?;
    let bad_given = options
        .bad
        .map(|n| index_of(&turns, n, "bad"))
        .transpose()?;
    let tip = turns.len() - 1;
    if let Some(good) = good_given
        && good >= bad_given.unwrap_or(tip)
    {
        bail!(
            "{thread} needs a good turn before a bad one; it has {} turn(s)",
            turns.len()
        );
    }

    watch_interrupts();
    let scratch =
        Scratch::create(&here.root, base.to_string()).context("making the scratch worktree")?;
    let mut probe = Probe {
        side: &here.side,
        scratch,
        command: &options.command,
        timeout,
    };

    let first_bad = |index: usize| -> Result<i32> {
        println!("first bad turn:");
        describe(&here.side, &turns[index])?;
        Ok(0)
    };

    // Without `--good` the first turn is run before anything else: if it fails it is the first bad turn.
    // One that cannot be tested (exit 125) is assumed good, but stays a candidate if nothing later passes.
    let mut lo = good_given.unwrap_or(0);
    let mut lo_tested_good = good_given.is_some();
    let mut skipped: BTreeSet<usize> = BTreeSet::new();
    if good_given.is_none() {
        let first = match probe.run(&turns[0])? {
            None => return interrupted(),
            Some(Verdict::Bad) => return first_bad(0),
            Some(verdict) => verdict,
        };
        match first {
            Verdict::Skip => {
                skipped.insert(0);
            }
            _ => lo_tested_good = true,
        }
        if bad_given.unwrap_or(tip) == 0 {
            // A one-turn thread, or `--bad` naming turn 1, which has just been run.
            return match (first, bad_given) {
                (Verdict::Skip, Some(_)) => first_bad(0),
                (Verdict::Skip, None) => {
                    bail!(
                        "the command skipped the only turn of {thread}; there is nothing to search"
                    )
                }
                (_, Some(_)) => bail!("the command passes at turn 1, which --bad names as failing"),
                (_, None) => {
                    bail!("the command passes at the latest turn, so no turn of {thread} fails it")
                }
            };
        }
    }
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
    loop {
        let candidates: Vec<usize> = (lo + 1..hi).filter(|i| !skipped.contains(i)).collect();
        let Some(&mid) = candidates.get(candidates.len() / 2) else {
            break;
        };
        match probe.run(&turns[mid])? {
            None => return interrupted(),
            Some(Verdict::Good) => {
                lo = mid;
                lo_tested_good = true;
            }
            Some(Verdict::Bad) => hi = mid,
            Some(Verdict::Skip) => {
                skipped.insert(mid);
            }
        }
    }
    let unsure: Vec<usize> = (lo + usize::from(lo_tested_good)..hi)
        .filter(|i| skipped.contains(i))
        .chain(std::iter::once(hi))
        .collect();
    if let [only] = unsure[..] {
        return first_bad(only);
    }
    println!(
        "the first bad turn is one of these {}; the command skipped all but the last:",
        unsure.len()
    );
    for i in unsure {
        describe(&here.side, &turns[i])?;
    }
    Ok(0)
}

fn interrupted() -> Result<i32> {
    eprintln!("baton: interrupted; removing the scratch worktree");
    Ok(INTERRUPTED)
}
