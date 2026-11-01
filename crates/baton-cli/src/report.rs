//! `baton report`: reads the side repo and, read-only, the user's repository. It never starts the daemon.
//! The analysis lives in `baton_core::build_report`; this file finds the project and prints the result.

use crate::timefmt;
use anyhow::{Context, Result, anyhow, bail};
use baton_core::{
    Abandoned, Dirs, Group, Hotspot, ProjectKey, RepoConfig, Report, ReportOptions, Rework, Thrash,
    UnixMillis, build_report, find_root, project_key,
};
use std::path::{Path, PathBuf};

const TOP: usize = 10;
const FILES_SHOWN: usize = 3;

pub struct ReportArgs {
    pub since: String,
    pub project: Option<String>,
    pub json: bool,
}

/// `7d`, `48h`, `90m`, `2w` or a plain number of seconds with `s`.
fn parse_window(text: &str) -> Result<i64> {
    let bad = || anyhow!("{text:?} is not a window like \"7d\", \"48h\", \"90m\" or \"2w\"");
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .filter(|&i| i > 0)
        .ok_or_else(bad)?;
    let (digits, unit) = text.split_at(split);
    let n: i64 = digits.parse().map_err(|_| bad())?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(bad()),
    };
    n.checked_mul(secs).filter(|s| *s > 0).ok_or_else(bad)
}

fn key_of(root: &Path) -> Option<ProjectKey> {
    let config = RepoConfig::load(root).ok()?;
    project_key(root, &config).ok()
}

/// A worktree of `project` that the daemon knows, for a run outside any worktree.
fn enrolled_root(dirs: &Dirs, project: &ProjectKey) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dirs.enrolled_file()).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&text).ok()?;
    let roots = doc["roots"].as_array().into_iter().flatten();
    let repos = doc["repos"].as_array().into_iter().flatten();
    let from_repos = repos.filter_map(|dir| {
        let dir = Path::new(dir.as_str()?);
        (dir.file_name()? == ".git").then(|| dir.parent().map(Path::to_owned))?
    });
    roots
        .filter_map(|r| r.as_str().map(PathBuf::from))
        .chain(from_repos)
        .find(|root| root.is_dir() && key_of(root).as_ref() == Some(project))
}

pub fn report(args: ReportArgs) -> Result<()> {
    let window = parse_window(&args.since)?;
    let dirs = Dirs::resolve().map_err(|e| anyhow!("{e}"))?;
    let cwd = std::env::current_dir()?;
    let (project, repo) = match &args.project {
        Some(text) => {
            let project = ProjectKey::parse(text).map_err(|e| anyhow!("{e}"))?;
            let here = find_root(&cwd)
                .ok()
                .filter(|r| key_of(r).as_ref() == Some(&project));
            let repo = here.or_else(|| enrolled_root(&dirs, &project));
            (project, repo)
        }
        None => {
            let root = find_root(&cwd).map_err(|e| anyhow!("{e}"))?;
            let config = RepoConfig::load(&root).map_err(|e| anyhow!("{e}"))?;
            let project = project_key(&root, &config).map_err(|e| anyhow!("{e}"))?;
            (project, Some(root))
        }
    };
    let side_dir = dirs.side_repo(&project);
    if !side_dir.join("HEAD").is_file() {
        bail!("no baton history for {project} on this machine");
    }
    let report = build_report(&ReportOptions {
        project,
        side_dir,
        repo,
        cache_dir: dirs.data().join("report-cache"),
        dirs: dirs.clone(),
        window_secs: window,
        now: UnixMillis::now(),
    })
    .map_err(|e| anyhow!("{e}"))
    .context("building the report")?;
    if args.json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        print!("{}", render(&report, &args.since));
    }
    Ok(())
}

fn span(secs: i64) -> String {
    match secs {
        s if s < 90 => format!("{}s", s.max(0)),
        s if s < 5400 => format!("{}m", (s + 30) / 60),
        s if s < 172_800 => format!("{}h", (s + 1800) / 3600),
        s => format!("{}d", (s + 43_200) / 86_400),
    }
}

fn count(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn pct(rate: Option<f64>) -> String {
    rate.map_or_else(|| "-".to_owned(), |r| format!("{:.0}%", r * 100.0))
}

fn heading(out: &mut String, name: &str, total: usize) {
    out.push('\n');
    if total > TOP {
        out.push_str(&format!("{name} ({total}, top {TOP})\n"));
    } else {
        out.push_str(&format!("{name} ({total})\n"));
    }
}

fn thrash_line(t: &Thrash) -> String {
    match t {
        Thrash::File {
            thread,
            turn,
            file,
            turns,
            lines,
        } => format!(
            "  {thread}@{turn}  {file}  changed in {} turns, {} redone",
            turns.len(),
            count(*lines, "line", "lines")
        ),
        Thrash::Check {
            thread,
            turn,
            check,
            runs,
        } => {
            let history: Vec<String> = runs
                .iter()
                .map(|r| format!("{}:{}", r.turn, if r.exit == 0 { "pass" } else { "fail" }))
                .collect();
            format!(
                "  {thread}@{turn}  check {check}  failed, passed, failed again ({})",
                history.join(" ")
            )
        }
    }
}

fn rework_line(r: &Rework) -> String {
    format!(
        "  {}@{}  {}  {} changed again by {}@{}, {} after shipping",
        r.thread,
        r.turn,
        r.file,
        count(r.lines, "line", "lines"),
        r.by.thread,
        r.by.turn,
        span((r.by.at - r.shipped_at) / 1000)
    )
}

fn abandoned_line(a: &Abandoned, now: i64) -> String {
    let files: Vec<&str> = a
        .files
        .iter()
        .take(FILES_SHOWN)
        .map(|f| f.path.as_str())
        .collect();
    let more = a.files_total as usize - files.len();
    let tail = if more > 0 {
        format!(", +{more} more")
    } else {
        String::new()
    };
    format!(
        "  {}@{}  last turn {} ago  {}{tail}  ({} of {} changed lines in HEAD)",
        a.thread,
        a.turn,
        span(now / 1000 - a.last_at / 1000),
        files.join(", "),
        a.reached_lines,
        a.changed_lines
    )
}

fn hotspot_line(h: &Hotspot) -> String {
    format!("  {}  thrash {}, rework {}", h.file, h.thrash, h.rework)
}

fn group_lines(out: &mut String, name: &str, groups: &[Group]) {
    if groups.is_empty() {
        return;
    }
    out.push_str(&format!("\n{name}\n"));
    for g in groups.iter().take(TOP) {
        out.push_str(&format!(
            "  {}  {} turns  {} threads  rework {}  thrash {}\n",
            g.name.as_deref().unwrap_or("-"),
            g.turns,
            g.threads,
            pct(g.rework_rate),
            pct(g.thrash_rate)
        ));
    }
}

fn section<T>(out: &mut String, name: &str, items: &[T], line: impl Fn(&T) -> String) {
    if items.is_empty() {
        return;
    }
    heading(out, name, items.len());
    for item in items.iter().take(TOP) {
        out.push_str(&line(item));
        out.push('\n');
    }
}

fn render(report: &Report, since: &str) -> String {
    let mut out = format!(
        "baton report  {}  last {since} (from {})  {} threads, {} turns\n",
        report.project,
        timefmt::utc(UnixMillis::from_secs(report.since / 1000)),
        report.counts.threads,
        report.counts.turns
    );
    if report.counts.turns == 0 {
        out.push_str("\nNo turns recorded in this window.\n");
        return out;
    }
    let flagged = report.counts.thrash + report.counts.rework + report.counts.abandoned > 0;
    if !flagged {
        out.push_str("\nNothing flagged.\n");
    }
    section(&mut out, "Thrash", &report.thrash, thrash_line);
    section(&mut out, "Rework", &report.rework, rework_line);
    section(&mut out, "Abandoned", &report.abandoned, |a| {
        abandoned_line(a, report.generated_at)
    });
    section(&mut out, "Hotspots", &report.hotspots, hotspot_line);
    group_lines(&mut out, "By model", &report.models);
    group_lines(&mut out, "By agent", &report.agents);
    let mut notes = Vec::new();
    if !report.repo.available {
        notes.push(
            "no repository of this project was found here, so rework and abandonment were not checked"
                .to_owned(),
        );
    }
    if report.skipped.threads_without_base > 0 {
        notes.push(format!(
            "{} threads were left out of the checks against their base: its commit is not in this repository",
            report.skipped.threads_without_base
        ));
    }
    if report.skipped.unreadable > 0 {
        notes.push(format!(
            "{} objects could not be read",
            report.skipped.unreadable
        ));
    }
    if report.skipped.commits_truncated {
        notes.push("only the newest commits were read for rework".to_owned());
    }
    if !notes.is_empty() {
        out.push('\n');
        for note in notes {
            out.push_str(&format!("note: {note}\n"));
        }
    }
    out
}
