use crate::gitcmp::{self, GitFiles};
use crate::stats;
use crate::util::{Res, arg_path};
use crate::walk::{self, Sweep};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

const WARM_RUNS: usize = 20;

type Engine<'a> = (&'a str, Box<dyn Fn() -> Res<Sweep> + 'a>);

pub fn run(args: &[String]) -> Res<()> {
    let root = arg_path(args, "worktree")?;
    let repo = gix::open(&root)?;
    let engines: Vec<Engine> = vec![
        (
            "ignore crate, serial",
            Box::new(|| walk::ignore_serial(&root)),
        ),
        (
            "ignore crate, 8 threads",
            Box::new(|| walk::ignore_parallel(&root)),
        ),
        (
            "gix dirwalk, paths only",
            Box::new(|| walk::gix_dirwalk(&repo, &root, false)),
        ),
        (
            "gix dirwalk + lstat per file",
            Box::new(|| walk::gix_dirwalk(&repo, &root, true)),
        ),
    ];

    let mut first = Vec::new();
    let mut results = Vec::new();
    for (name, f) in &engines {
        let t = Instant::now();
        let sweep = f()?;
        first.push((*name, t.elapsed()));
        results.push(sweep);
    }
    let mut warm: Vec<Vec<Duration>> = vec![Vec::new(); engines.len()];
    for _ in 0..WARM_RUNS {
        for (i, (_, f)) in engines.iter().enumerate() {
            let t = Instant::now();
            let sweep = f()?;
            warm[i].push(t.elapsed());
            if sweep.files.len() != results[i].files.len() {
                println!(
                    "note: {} changed file count between runs (live repo)",
                    engines[i].0
                );
            }
        }
    }

    println!("sweep of {}", root.display());
    stats::header();
    for ((name, d), w) in first.iter().zip(warm.iter_mut()) {
        stats::row(&format!("{name}, first run"), &mut [*d]);
        stats::row(&format!("{name}, warm"), w);
    }
    println!("files found:");
    for ((name, _), sweep) in engines.iter().zip(&results) {
        let bytes: u64 = sweep.files.iter().map(|s| s.size).sum();
        let newest = sweep
            .files
            .iter()
            .map(|s| s.mtime)
            .max()
            .and_then(|t| t.elapsed().ok());
        println!(
            "  {name}: {} files, {:.1} MiB, newest mtime {:.0} s ago, {} ignored dirs/files skipped (gix only)",
            sweep.files.len(),
            bytes as f64 / 1_048_576.0,
            newest.map_or(f64::NAN, |d| d.as_secs_f64()),
            sweep.skipped.len()
        );
    }

    let git_files = gitcmp::ls_files(&root)?;
    println!(
        "git ls-files --cached --others --exclude-standard: {} cached, {} untracked-not-ignored",
        git_files.cached.len(),
        git_files.others.len()
    );
    for ((name, _), sweep) in engines.iter().zip(&results) {
        compare(name, &root, sweep, &git_files)?;
    }
    Ok(())
}

fn compare(name: &str, root: &Path, sweep: &Sweep, git: &GitFiles) -> Res<()> {
    let ours: BTreeSet<&str> = sweep.files.iter().map(|s| s.path.as_str()).collect();
    let all: BTreeSet<&str> = git
        .cached
        .iter()
        .chain(&git.others)
        .map(String::as_str)
        .collect();

    let extra: Vec<&str> = ours.difference(&all).copied().collect();
    let missing: Vec<&str> = all.difference(&ours).copied().collect();

    let mut deleted = 0;
    let mut gitlinks = 0;
    let mut suspect = Vec::new();
    for p in &missing {
        match std::fs::symlink_metadata(root.join(p)) {
            Err(_) => deleted += 1,
            Ok(md) if md.is_dir() => gitlinks += 1,
            Ok(_) => suspect.push((*p).to_owned()),
        }
    }
    // A tracked file matching an ignore rule is invisible to a walk with no index, by design.
    let verdicts = gitcmp::check_ignore(root, &suspect)?;
    let tracked_ignored = verdicts.iter().filter(|v| v.ignored).count();
    let real_missing: Vec<&str> = verdicts
        .iter()
        .filter(|v| !v.ignored)
        .map(|v| v.path.as_str())
        .collect();

    println!("{name}: {} files vs git {}", ours.len(), all.len());
    println!("  in sweep, not in git: {}", extra.len());
    println!("  in git, not in sweep: {}", missing.len());
    println!("    tracked but deleted on disk: {deleted}");
    println!("    submodule or nested repo directories: {gitlinks}");
    println!("    tracked and matching an ignore rule (expected, no index): {tracked_ignored}");
    println!(
        "    present, not ignored, not swept: {}",
        real_missing.len()
    );
    for p in extra.iter().take(8) {
        println!("    only in sweep: {p}");
    }
    for p in real_missing.iter().take(8) {
        println!("    only in git: {p}");
    }
    Ok(())
}
