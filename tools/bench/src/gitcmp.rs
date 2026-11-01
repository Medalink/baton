use crate::util::{Res, git};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

pub struct GitFiles {
    pub cached: BTreeSet<String>,
    pub others: BTreeSet<String>,
}

fn nul_list(bytes: &[u8]) -> BTreeSet<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// `git ls-files --cached --others --exclude-standard`, run once and split into its two halves.
pub fn ls_files(root: &Path) -> Res<GitFiles> {
    let run = |extra: &[&str]| -> Res<BTreeSet<String>> {
        let out = git(root).args(["ls-files", "-z"]).args(extra).output()?;
        if !out.status.success() {
            return Err(format!(
                "git ls-files failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )
            .into());
        }
        Ok(nul_list(&out.stdout))
    };
    Ok(GitFiles {
        cached: run(&["--cached"])?,
        others: run(&["--others", "--exclude-standard"])?,
    })
}

pub struct Verdict {
    pub path: String,
    /// `source:line:pattern` of the last rule that matched, `None` when nothing matched. A negated rule matches without ignoring.
    pub rule: Option<String>,
    pub ignored: bool,
}

/// `git check-ignore --no-index`, so tracked files are judged by pattern alone like gix over an empty index.
pub fn check_ignore(root: &Path, paths: &[String]) -> Res<Vec<Verdict>> {
    let mut child = git(root)
        .args(["check-ignore", "--no-index", "-v", "-n", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input: Vec<u8> = paths.iter().flat_map(|p| p.bytes().chain([0u8])).collect();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    writer.join().map_err(|_| "stdin writer panicked")??;
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(format!(
            "git check-ignore failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    let fields: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
    let mut verdicts = Vec::with_capacity(paths.len());
    for rec in fields.as_chunks::<4>().0 {
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let rule = (!rec[0].is_empty())
            .then(|| format!("{}:{}:{}", text(rec[0]), text(rec[1]), text(rec[2])));
        let ignored = !rec[0].is_empty() && !rec[2].starts_with(b"!");
        verdicts.push(Verdict {
            ignored,
            path: text(rec[3]),
            rule,
        });
    }
    if verdicts.len() != paths.len() {
        return Err(format!(
            "check-ignore answered {} of {} paths",
            verdicts.len(),
            paths.len()
        )
        .into());
    }
    Ok(verdicts)
}
