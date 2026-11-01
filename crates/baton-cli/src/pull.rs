//! `baton pull` and `baton follow`: bringing a thread another machine recorded into a worktree here.
//!
//! `pull` is the one place baton writes to the user's repository: `git worktree add --detach` registers
//! a worktree under `.git/worktrees`. Everything inside the new directory is written by `apply_tree`,
//! which never overwrites a file that differs from what it put there.

use crate::commands::{describe, open_here};
use anyhow::{Context, Result, anyhow, bail};
use baton_core::{
    Applied, Dirs, Guard, SideRepo, ThreadId, TurnId, TurnRecord, apply_tree, simplified,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(50);
const SHOWN_REFUSALS: usize = 20;

pub fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn report(applied: &Applied) {
    println!(
        "  wrote {} files, removed {}, {} already matched",
        applied.written, applied.removed, applied.same
    );
    for refusal in applied.refused.iter().take(SHOWN_REFUSALS) {
        println!("  kept {}: {}", refusal.path, refusal.reason);
    }
    if applied.refused.len() > SHOWN_REFUSALS {
        println!(
            "  ... and {} more kept",
            applied.refused.len() - SHOWN_REFUSALS
        );
    }
}

/// How a directory is named in `pulled.json`: its resolved path, folded where the file system folds case.
fn pulled_key(dir: &Path) -> Result<String> {
    let resolved =
        std::fs::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    let text = simplified(&resolved).to_string_lossy().into_owned();
    Ok(if cfg!(any(windows, target_os = "macos")) {
        text.to_lowercase()
    } else {
        text
    })
}

fn pulled_list(dirs: &Dirs) -> Vec<String> {
    std::fs::read(dirs.pulled_file())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn remember_pulled(dirs: &Dirs, dir: &Path) -> Result<()> {
    let key = pulled_key(dir)?;
    let mut list = pulled_list(dirs);
    if !list.contains(&key) {
        list.push(key);
    }
    let file = dirs.pulled_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let temp = file.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec(&list)?)
        .with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &file).with_context(|| format!("replacing {}", file.display()))
}

fn thread_tip(side: &SideRepo, thread: &ThreadId) -> Result<TurnRecord> {
    let tip = side
        .tip(thread)
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| {
            anyhow!("this machine has no turns of {thread}; `baton log` lists the threads it has")
        })?;
    side.read_turn(tip).map_err(|e| anyhow!("{e}"))
}

pub fn pull(thread: &str, into: Option<&Path>) -> Result<()> {
    let here = open_here()?;
    let thread = ThreadId::parse(thread).map_err(|e| anyhow!("{e}"))?;
    let dirs = Dirs::resolve().map_err(|e| anyhow!("{e}"))?;
    let tip = thread_tip(&here.side, &thread)?;
    let draft = &tip.turn.draft;
    let base = draft.base.as_ref().ok_or_else(|| {
        anyhow!("{thread} began in a repository with no commit, so there is nothing to base a worktree on")
    })?;
    let has_base = git(
        &here.root,
        &["cat-file", "-e", &format!("{base}^{{commit}}")],
    )
    .is_ok();
    if !has_base {
        bail!(
            "the base commit {} of {thread} is not in this repository; run `git fetch` and try again",
            base
        );
    }

    let dir: PathBuf = match into {
        Some(dir) => std::path::absolute(dir).context("resolving the target directory")?,
        None => {
            let name = here
                .root
                .file_name()
                .map_or_else(|| "worktree".into(), |n| n.to_string_lossy().into_owned());
            let short: String = thread.as_str().chars().take(8).collect();
            here.root
                .parent()
                .unwrap_or(&here.root)
                .join(format!("{name}-{short}"))
        }
    };
    let dir_arg = dir.to_string_lossy().into_owned();
    git(
        &here.root,
        &["worktree", "add", "--detach", &dir_arg, &base.to_string()],
    )?;
    remember_pulled(&dirs, &dir)?;

    let listed = git(&dir, &["ls-files", "-z"])?;
    let git_paths: Vec<String> = listed
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    let keep: HashSet<String> = draft
        .skipped
        .iter()
        .map(|p| p.as_str().to_owned())
        .collect();
    let applied = apply_tree(
        &here.side,
        &dir,
        tip.tree,
        &Guard::Fresh {
            git_paths: &git_paths,
            keep: &keep,
        },
    )
    .map_err(|e| anyhow!("{e}"))?;
    println!(
        "pulled {thread}@{} ({}) into {}",
        tip.turn.seq,
        tip.id.short(),
        dir.display()
    );
    println!("  based on {}", base);
    report(&applied);
    if !applied.refused.is_empty() {
        bail!("{} files were kept as they were", applied.refused.len());
    }
    Ok(())
}

/// The turns after `last` up to `tip`, oldest first, and whether `last` is among their ancestors.
fn turns_since(
    side: &SideRepo,
    tip: TurnId,
    last: Option<TurnId>,
) -> Result<(Vec<TurnRecord>, bool)> {
    let mut chain = Vec::new();
    let mut next = Some(tip);
    while let Some(id) = next.filter(|id| Some(*id) != last) {
        let record = side.read_turn(id).map_err(|e| anyhow!("{e}"))?;
        next = record.parent;
        chain.push(record);
    }
    chain.reverse();
    Ok((chain, next == last))
}

pub fn follow(thread: &str, into: Option<&Path>) -> Result<()> {
    let here = open_here()?;
    let thread = ThreadId::parse(thread).map_err(|e| anyhow!("{e}"))?;
    if let Some(dir) = into {
        let dirs = Dirs::resolve().map_err(|e| anyhow!("{e}"))?;
        let made_by_pull = pulled_key(dir).is_ok_and(|key| pulled_list(&dirs).contains(&key));
        if !made_by_pull {
            bail!(
                "{} is not a worktree `baton pull` made on this machine; `baton pull {thread} --into {}` makes one",
                dir.display(),
                dir.display()
            );
        }
    }
    let mut last = here.side.tip(&thread).map_err(|e| anyhow!("{e}"))?;
    if into.is_some() && last.is_none() {
        bail!("this machine has no turns of {thread} yet; `baton pull {thread}` first");
    }
    match last {
        Some(tip) => println!("following {thread} from {}", tip.short()),
        None => println!("following {thread}, which has not arrived yet"),
    }
    loop {
        std::thread::sleep(POLL);
        // A ref that is being replaced can fail to read for a moment; the next poll sees it.
        let Ok(Some(now)) = here.side.tip(&thread) else {
            continue;
        };
        if Some(now) == last {
            continue;
        }
        let (chain, descends) = turns_since(&here.side, now, last)?;
        if !descends {
            println!("the history of {thread} was replaced");
        }
        let mut held = match last {
            Some(id) => Some(here.side.read_turn(id).map_err(|e| anyhow!("{e}"))?.tree),
            None => None,
        };
        // A replaced history is applied as one step from what the directory holds.
        let steps: &[TurnRecord] = if descends {
            &chain
        } else {
            &chain[chain.len().saturating_sub(1)..]
        };
        for turn in steps {
            describe(&here.side, turn)?;
            if let (Some(dir), Some(from)) = (into, held) {
                let keep: HashSet<String> = turn
                    .turn
                    .draft
                    .skipped
                    .iter()
                    .map(|p| p.as_str().to_owned())
                    .collect();
                let applied = apply_tree(
                    &here.side,
                    dir,
                    turn.tree,
                    &Guard::Holds { from, keep: &keep },
                )
                .map_err(|e| anyhow!("{e}"))?;
                report(&applied);
            }
            held = Some(turn.tree);
        }
        last = Some(now);
    }
}
