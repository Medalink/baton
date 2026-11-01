//! The built-in classifier: no model and no network. A rule names a class, a kind, and what it matches: a
//! path glob, a regex over the added lines of the diff, or both. Each rule yields at most one finding,
//! the first place it matches; the turn's class is the highest class among the findings.

use crate::error::{Error, Result};
use crate::policy::globs;
use crate::schema::{Class, Evidence, FileChange, Finding, TurnDoc, TurnVerdict};
use globset::GlobSet;
use regex::Regex;
use serde::Deserialize;

/// A rule as written in `[[classify.rule]]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub class: Class,
    pub kind: String,
    #[serde(default)]
    pub paths: Vec<String>,
    pub diff: Option<String>,
}

struct Rule {
    class: Class,
    kind: String,
    paths: Option<GlobSet>,
    diff: Option<Regex>,
}

pub struct RuleSet(Vec<Rule>);

const DROPPED_SCHEMA: &str = r"(?i)\b(?:drop\s+(?:table|column|schema|database)|truncate\s+table)\b|\b(?:drop_table|drop_column|remove_column|droptable|dropcolumn|dropifexists)\b";

/// (class, kind, paths, diff regex)
const DEFAULTS: &[(Class, &str, &[&str], Option<&str>)] = &[
    (
        Class::React,
        "dropped_table_or_column",
        &["**/migrations/**", "**/migrate/**", "**/*.sql"],
        Some(DROPPED_SCHEMA),
    ),
    (
        Class::Check,
        "lockfile_changed",
        &[
            "Cargo.lock",
            "package-lock.json",
            "npm-shrinkwrap.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "bun.lockb",
            "composer.lock",
            "Gemfile.lock",
            "poetry.lock",
            "uv.lock",
            "go.sum",
        ],
        None,
    ),
    (
        Class::Check,
        "env_template_changed",
        &[".env.example", ".env.sample", ".env.dist"],
        None,
    ),
    (
        Class::Check,
        "auth_code_changed",
        &[
            "**/auth/**",
            "**/authn/**",
            "**/authz/**",
            "**/authentication/**",
            "**/authorization/**",
            "**/permissions/**",
            "**/policies/**",
        ],
        None,
    ),
];

impl RuleConfig {
    fn build(&self) -> Result<Rule> {
        if self.kind.trim().is_empty() {
            return Err(Error::invalid("classify rule", "kind is empty"));
        }
        if self.paths.is_empty() && self.diff.is_none() {
            return Err(Error::invalid(
                "classify rule",
                format!(
                    "{:?} has neither paths nor diff, so it would match every turn",
                    self.kind
                ),
            ));
        }
        let paths = (!self.paths.is_empty())
            .then(|| globs(&self.paths, "classify rule paths", false))
            .transpose()?;
        let diff = self
            .diff
            .as_deref()
            .map(Regex::new)
            .transpose()
            .map_err(|e| Error::invalid("classify rule diff", format!("{:?}: {e}", self.kind)))?;
        Ok(Rule {
            class: self.class,
            kind: self.kind.clone(),
            paths,
            diff,
        })
    }
}

impl RuleSet {
    pub fn defaults() -> Self {
        let configs = DEFAULTS
            .iter()
            .map(|(class, kind, paths, diff)| RuleConfig {
                class: *class,
                kind: (*kind).into(),
                paths: paths.iter().map(|p| (*p).into()).collect(),
                diff: diff.map(str::to_owned),
            });
        Self(
            configs
                .map(|c| c.build().expect("the default rules are valid"))
                .collect(),
        )
    }

    pub fn empty() -> Self {
        Self(Vec::new())
    }

    pub fn with(mut self, configs: &[RuleConfig]) -> Result<Self> {
        for config in configs {
            self.0.push(config.build()?);
        }
        Ok(self)
    }

    pub fn classify(&self, turn: &TurnDoc) -> TurnVerdict {
        let diffs = parse_diff(&turn.diff);
        let findings: Vec<(Class, Finding)> = self
            .0
            .iter()
            .filter_map(|rule| rule.first_match(&turn.files, &diffs))
            .collect();
        let class = findings
            .iter()
            .map(|(class, _)| *class)
            .max()
            .unwrap_or(Class::Quiet);
        TurnVerdict {
            class,
            findings: findings.into_iter().map(|(_, f)| f).collect(),
        }
    }
}

impl Rule {
    fn first_match(&self, files: &[FileChange], diffs: &[FileDiff]) -> Option<(Class, Finding)> {
        let wanted = |path: &str| self.paths.as_ref().is_none_or(|g| g.is_match(path));
        let evidence = match &self.diff {
            Some(regex) => diffs.iter().filter(|d| wanted(&d.path)).find_map(|d| {
                let (line, text) = d.added.iter().find(|(_, text)| regex.is_match(text))?;
                Some((
                    Evidence {
                        path: d.path.clone(),
                        lines: Some([*line, *line]),
                    },
                    format!("+{}", text.trim()),
                ))
            })?,
            None => {
                let file = files.iter().find(|f| wanted(&f.path))?;
                let lines = diffs
                    .iter()
                    .find(|d| d.path == file.path)
                    .and_then(|d| d.first_added);
                (
                    Evidence {
                        path: file.path.clone(),
                        lines,
                    },
                    format!("{} matches the rule", file.path),
                )
            }
        };
        Some((
            self.class,
            Finding {
                kind: self.kind.clone(),
                score: 1.0,
                evidence: vec![evidence.0],
                note: evidence.1,
            },
        ))
    }
}

#[derive(Debug, Default)]
struct FileDiff {
    path: String,
    /// Added lines with their number in the new file.
    added: Vec<(u32, String)>,
    /// The first and last added line of the first hunk that adds any.
    first_added: Option<[u32; 2]>,
}

/// Reads the diff `diff::render` writes: `diff --git a/<path> b/<path>`, two header lines, then hunks
/// whose line counts say where each hunk ends, so a content line that looks like a header is not one.
fn parse_diff(diff: &str) -> Vec<FileDiff> {
    let mut out: Vec<FileDiff> = Vec::new();
    let mut lines = diff.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            let path = rest
                .rsplit_once(" b/")
                .map_or(rest, |(path, _)| path)
                .to_owned();
            out.push(FileDiff {
                path,
                ..FileDiff::default()
            });
        } else if let (Some(header), Some(file)) = (line.strip_prefix("@@ "), out.last_mut()) {
            let Some((old_len, new_start, new_len)) = hunk_header(header) else {
                continue;
            };
            let (mut old_left, mut new_left, mut number) = (old_len, new_len, new_start);
            while old_left > 0 || new_left > 0 {
                let Some(body) = lines.next() else { break };
                match body.as_bytes().first() {
                    Some(b'+') => {
                        file.added.push((number, body[1..].to_owned()));
                        number += 1;
                        new_left = new_left.saturating_sub(1);
                    }
                    Some(b'-') => old_left = old_left.saturating_sub(1),
                    Some(b'\\') => {}
                    _ => {
                        number += 1;
                        old_left = old_left.saturating_sub(1);
                        new_left = new_left.saturating_sub(1);
                    }
                }
            }
            if file.first_added.is_none() && new_len > 0 {
                file.first_added = Some([new_start, new_start + new_len - 1]);
            }
        }
    }
    out
}

/// `-a,b +c,d @@` gives `(b, c, d)`; a missing count is 1.
fn hunk_header(header: &str) -> Option<(u32, u32, u32)> {
    let mut parts = header.split(' ');
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let count = |range: &str| -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    let (_, old_len) = count(old)?;
    let (new_start, new_len) = count(new)?;
    Some((old_len, new_start, new_len))
}
