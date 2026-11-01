use crate::gitcmp::{self, Verdict};
use crate::util::{Res, arg_path};
use crate::walk;
use gix::index::entry::Mode;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

const BELOW_SKIPPED_CAP: usize = 2000;
const PER_DIR_CAP: usize = 10;

struct Probe {
    path: String,
    is_dir: bool,
}

struct GixVerdict {
    ignored: bool,
    rule: Option<String>,
}

/// gix's excludes stack over an empty in-memory index, so the user's `.git/index` is never opened.
fn gix_verdicts(repo: &gix::Repository, probes: &[Probe]) -> Res<Vec<GixVerdict>> {
    let index = gix::index::State::new(repo.object_hash());
    let mut stack = repo.excludes(
        &index,
        None,
        gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
    )?;
    probes
        .iter()
        .map(|p| {
            let mode = if p.is_dir { Mode::DIR } else { Mode::FILE };
            let platform = stack.at_path(&p.path, Some(mode))?;
            let m = platform.matching_exclude_pattern();
            Ok(GixVerdict {
                ignored: platform.is_excluded(),
                rule: m.map(|m| {
                    format!(
                        "{}:{}:{}",
                        m.source
                            .map(|s| s.display().to_string())
                            .unwrap_or_default(),
                        m.sequence_number,
                        m.pattern
                    )
                }),
            })
        })
        .collect()
}

/// Up to `PER_DIR_CAP` files from each skipped directory, breadth first, to test paths the sweep never visits.
fn files_below(root: &Path, skipped_dirs: &[&Probe]) -> Vec<Probe> {
    let mut out = Vec::new();
    for dir in skipped_dirs {
        let mut queue = vec![dir.path.clone()];
        let mut taken = 0;
        while let Some(d) = queue.pop() {
            let Ok(rd) = std::fs::read_dir(root.join(&d)) else {
                continue;
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name == ".git" {
                    continue;
                }
                let rel = format!("{d}/{name}");
                match e.file_type() {
                    Ok(t) if t.is_dir() => queue.push(rel),
                    Ok(t) if t.is_file() && taken < PER_DIR_CAP => {
                        taken += 1;
                        out.push(Probe {
                            path: rel,
                            is_dir: false,
                        });
                    }
                    _ => {}
                }
            }
            if taken >= PER_DIR_CAP || out.len() >= BELOW_SKIPPED_CAP {
                break;
            }
        }
        if out.len() >= BELOW_SKIPPED_CAP {
            break;
        }
    }
    out
}

fn report(label: &str, probes: &[Probe], gix: &[GixVerdict], git: &[Verdict]) {
    let (mut both_ignored, mut both_kept) = (0, 0);
    let (mut gix_only, mut git_only) = (Vec::new(), Vec::new());
    for ((p, g), v) in probes.iter().zip(gix).zip(git) {
        match (g.ignored, v.ignored) {
            (true, true) => both_ignored += 1,
            (false, false) => both_kept += 1,
            (true, false) => gix_only.push((p, g, v)),
            (false, true) => git_only.push((p, g, v)),
        }
    }
    println!(
        "{label}: {} paths, {both_ignored} ignored by both, {both_kept} kept by both, {} ignored only by gix, {} ignored only by git",
        probes.len(),
        gix_only.len(),
        git_only.len()
    );
    for (title, list) in [
        ("gix ignores, git does not", &gix_only),
        ("git ignores, gix does not", &git_only),
    ] {
        for (p, g, v) in list.iter().take(10) {
            println!(
                "    {title}: {}{} gix={:?} git={:?}",
                p.path,
                if p.is_dir { "/" } else { "" },
                g.rule,
                v.rule
            );
        }
    }
}

pub fn run(args: &[String]) -> Res<()> {
    let root = arg_path(args, "worktree")?;
    let repo = gix::open(&root)?;
    let gix_sweep = walk::gix_dirwalk(&repo, &root)?;
    let ignore_sweep = walk::ignore_serial(&root)?;

    let visited: BTreeSet<&str> = gix_sweep
        .files
        .iter()
        .chain(&ignore_sweep.files)
        .map(|s| s.path.as_str())
        .collect();
    let visited: Vec<Probe> = visited
        .into_iter()
        .map(|p| Probe {
            path: p.to_owned(),
            is_dir: false,
        })
        .collect();
    let skipped: Vec<Probe> = gix_sweep
        .skipped
        .iter()
        .map(|p| {
            let path = p.trim_end_matches('/').to_owned();
            let is_dir = std::fs::symlink_metadata(root.join(&path)).is_ok_and(|m| m.is_dir());
            Probe { path, is_dir }
        })
        .collect();
    let skipped_dirs: Vec<&Probe> = skipped.iter().filter(|p| p.is_dir).collect();
    let below = files_below(&root, &skipped_dirs);

    println!("excludes of {}", root.display());
    for (label, probes) in [
        ("files the sweeps visited", &visited),
        ("ignored entries gix skipped", &skipped),
        ("files below skipped dirs", &below),
    ] {
        let t = Instant::now();
        let ours = gix_verdicts(&repo, probes)?;
        let gix_time = t.elapsed();
        let t = Instant::now();
        let theirs = gitcmp::check_ignore(
            &root,
            &probes.iter().map(|p| p.path.clone()).collect::<Vec<_>>(),
        )?;
        let git_time = t.elapsed();
        report(label, probes, &ours, &theirs);
        println!(
            "    time: gix {:.1} ms in-process, git check-ignore {:.1} ms including process spawn",
            gix_time.as_secs_f64() * 1000.0,
            git_time.as_secs_f64() * 1000.0
        );
    }
    println!(
        "skipped ignored directories: {} (of {} ignored entries)",
        skipped_dirs.len(),
        skipped.len()
    );
    Ok(())
}
