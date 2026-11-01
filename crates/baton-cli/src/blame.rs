//! `baton blame <file>:<line>` and `baton blame <file> -L a,b`: which recorded turn wrote a line that git
//! blames on a commit. The matching rule is in `docs/design.md`, History.

use crate::commands::{log_line, open_here};
use crate::pull::git;
use crate::timefmt;
use anyhow::{Context, Result, anyhow, bail};
use baton_core::{
    BLAME_SCHEMA, BlameCommit, BlameDoc, BlameLine, BlameTurn, CommitFacts, Found, GitSha, GitView,
    RelPath, TurnRecord, UnixMillis, Wanted, find_origins,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::process::Command;

struct Entry {
    sha: String,
    line: u32,
    path: String,
    text: Vec<u8>,
}

#[derive(Default)]
struct Info {
    author: String,
    author_time: i64,
    committer_time: i64,
    summary: String,
}

/// The file and the first and last line to blame.
fn parse_target(target: &str, range: Option<&str>) -> Result<(String, u32, u32)> {
    let number = |s: &str| {
        s.parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| anyhow!("{s:?} is not a line number"))
    };
    if let Some(range) = range {
        let (from, to) = range.split_once(',').unwrap_or((range, range));
        let (from, to) = (number(from)?, number(to)?);
        if to < from {
            bail!("-L {range}: the last line is before the first");
        }
        return Ok((target.to_owned(), from, to));
    }
    match target.rsplit_once(':') {
        Some((file, line)) if !file.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) => {
            let line = number(line)?;
            Ok((file.to_owned(), line, line))
        }
        _ => bail!("name a line: `baton blame <file>:<line>` or `baton blame <file> -L <a>,<b>`"),
    }
}

/// `git blame --porcelain`: per line the commit, the file name that commit held it under, and the text.
fn parse_porcelain(out: &[u8]) -> Result<(Vec<Entry>, HashMap<String, Info>)> {
    let mut entries = Vec::new();
    let mut infos: HashMap<String, Info> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut lines = out.split(|b| *b == b'\n');
    while let Some(header) = lines.next() {
        if header.is_empty() {
            continue;
        }
        let header = String::from_utf8_lossy(header);
        let mut fields = header.split(' ');
        let sha = fields.next().unwrap_or_default().to_owned();
        let line: u32 = fields
            .nth(1)
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| anyhow!("unexpected `git blame` output: {header}"))?;
        loop {
            let Some(next) = lines.next() else {
                bail!("`git blame` output ended inside a line");
            };
            if let Some(text) = next.strip_prefix(b"\t") {
                let path = names.get(&sha).cloned().unwrap_or_default();
                entries.push(Entry {
                    sha: sha.clone(),
                    line,
                    path,
                    text: text.to_vec(),
                });
                break;
            }
            let next = String::from_utf8_lossy(next);
            let (key, value) = next.split_once(' ').unwrap_or((&next, ""));
            let info = infos.entry(sha.clone()).or_default();
            match key {
                "author" => value.clone_into(&mut info.author),
                "author-time" => info.author_time = value.parse().unwrap_or(0),
                "committer-time" => info.committer_time = value.parse().unwrap_or(0),
                "summary" => value.clone_into(&mut info.summary),
                "filename" => {
                    names.insert(sha.clone(), value.to_owned());
                }
                _ => {}
            }
        }
    }
    Ok((entries, infos))
}

pub fn run(target: &str, range: Option<&str>, json: bool) -> Result<()> {
    let (file, from, to) = parse_target(target, range)?;
    let here = open_here()?;
    let output = Command::new("git")
        .args([
            "blame",
            "--porcelain",
            "-L",
            &format!("{from},{to}"),
            "--",
            &file,
        ])
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!(
            "git blame failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let (entries, infos) = parse_porcelain(&output.stdout)?;

    let present: RefCell<HashMap<String, bool>> = RefCell::default();
    let blob_at = |base: &GitSha, path: &RelPath| {
        let id = base.to_string();
        let have = *present.borrow_mut().entry(id.clone()).or_insert_with(|| {
            git(&here.root, &["cat-file", "-e", &format!("{id}^{{commit}}")]).is_ok()
        });
        // A path the base does not hold is a file the thread created.
        have.then(|| {
            git(
                &here.root,
                &["cat-file", "blob", &format!("{id}:{}", path.as_str())],
            )
            .unwrap_or_default()
        })
    };
    let ancestry: RefCell<HashMap<(String, String), bool>> = RefCell::default();
    let is_ancestor = |base: &GitSha, commit: &GitSha| {
        let key = (base.to_string(), commit.to_string());
        *ancestry.borrow_mut().entry(key.clone()).or_insert_with(|| {
            Command::new("git")
                .arg("-C")
                .arg(&here.root)
                .args(["merge-base", "--is-ancestor", &key.0, &key.1])
                .status()
                .is_ok_and(|s| s.success())
        })
    };
    let view = GitView {
        blob_at: &blob_at,
        is_ancestor: &is_ancestor,
    };

    let mut by_path: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        match by_path.iter_mut().find(|(p, _)| *p == entry.path) {
            Some((_, group)) => group.push(i),
            None => by_path.push((entry.path.clone(), vec![i])),
        }
    }
    let mut found: Vec<Option<Found>> = entries.iter().map(|_| None).collect();
    let mut unchecked = 0;
    for (path, group) in by_path {
        let Ok(path) = RelPath::parse(&path) else {
            continue;
        };
        let wanted: Vec<Wanted> = group
            .iter()
            .map(|&i| {
                let entry = &entries[i];
                let commit = GitSha::parse(&entry.sha)
                    .ok()
                    .filter(|_| !entry.sha.bytes().all(|b| b == b'0'))
                    .map(|id| CommitFacts {
                        id,
                        at: UnixMillis::from_secs(
                            infos.get(&entry.sha).map_or(0, |i| i.committer_time),
                        ),
                    });
                let text = entry.text.strip_suffix(b"\r").unwrap_or(&entry.text);
                Wanted {
                    text: text.to_vec(),
                    commit,
                }
            })
            .collect();
        let origins =
            find_origins(&here.side, &path, &wanted, &view).map_err(|e| anyhow!("{e}"))?;
        unchecked += origins.unchecked;
        for (&i, origin) in group.iter().zip(origins.found) {
            found[i] = origin;
        }
    }

    let mut doc = BlameDoc {
        schema: BLAME_SCHEMA,
        unchecked_threads: unchecked,
        lines: Vec::new(),
    };
    let mut records: Vec<Option<TurnRecord>> = Vec::new();
    for (entry, origin) in entries.iter().zip(found) {
        let commit = GitSha::parse(&entry.sha)
            .ok()
            .filter(|_| !entry.sha.bytes().all(|b| b == b'0'))
            .map(|id| {
                let info = infos.get(&entry.sha);
                BlameCommit {
                    id: id.to_string(),
                    author: info.map(|i| i.author.clone()).unwrap_or_default(),
                    at: info.map_or(0, |i| i.author_time).saturating_mul(1000),
                    subject: info.map(|i| i.summary.clone()).unwrap_or_default(),
                }
            });
        doc.lines.push(BlameLine {
            path: entry.path.clone(),
            line: entry.line,
            text: String::from_utf8_lossy(&entry.text).into_owned(),
            commit,
            turn: origin.as_ref().map(|o| BlameTurn::of(&o.best)),
            others: origin.as_ref().map_or(0, |o| o.others),
        });
        records.push(origin.map(|o| o.best));
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(());
    }
    for (line, record) in doc.lines.iter().zip(&records) {
        print_line(line, record.as_ref());
    }
    if unchecked > 0 {
        println!(
            "{unchecked} thread(s) began at a commit this repository does not have, so their first turns were not compared; `git fetch` and run this again"
        );
    }
    Ok(())
}

fn print_line(line: &BlameLine, record: Option<&TurnRecord>) {
    println!("{}:{}  {}", line.path, line.line, line.text.trim_end());
    match &line.commit {
        Some(c) => println!(
            "  git     {}  {}  {}  {}",
            &c.id[..8],
            c.author,
            timefmt::utc(UnixMillis::from_secs(c.at / 1000)),
            c.subject
        ),
        None => println!("  git     not committed yet"),
    }
    let (Some(turn), Some(record)) = (&line.turn, record) else {
        if line.text.trim().is_empty() {
            println!("  turn    a blank line is not attributed to a turn");
        } else {
            println!(
                "  turn    not written in a recorded turn (written by hand, or before enrollment)"
            );
        }
        return;
    };
    println!(
        "  turn    thread {}  turn {}  agent {}  model {}  machine {}  at {}",
        turn.thread,
        turn.turn,
        turn.agent,
        turn.model.as_deref().unwrap_or("-"),
        turn.machine,
        timefmt::utc(UnixMillis::from_secs(turn.at / 1000))
    );
    if let Some(message) = &turn.subject {
        println!("  message {message}");
    }
    println!("  log     {}", log_line(record));
    match line.others {
        0 => {}
        1 => println!("  1 other turn also added this line"),
        n => println!("  {n} other turns also added this line"),
    }
}
