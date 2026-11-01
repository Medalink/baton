use crate::error::{Error, IoContext, Result};
use aho_corasick::{AhoCorasick, MatchKind};
use regex::bytes::Regex;
use serde::Deserialize;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// A value of fewer characters than this is too likely to be ordinary text to redact as a literal.
const MIN_LITERAL_LEN: usize = 8;

pub(crate) const BUILTIN: &[(&str, &str)] = &[
    (
        "private-key",
        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
    ),
    (
        "aws-access-key",
        r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA|ANPA|ANVA)[0-9A-Z]{16}\b",
    ),
    (
        "aws-secret-key",
        r#"(?i)aws_?secret_?access_?key["']?\s*[=:]\s*["']?(?P<secret>[A-Za-z0-9/+=]{40,})"#,
    ),
    (
        "github-token",
        r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})",
    ),
    ("anthropic-key", r"\bsk-ant-[A-Za-z0-9_-]{20,}"),
    ("openai-key", r"\bsk-(?:proj-)?[A-Za-z0-9_-]{32,}"),
    ("slack-token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}"),
    ("stripe-key", r"\b[sr]k_(?:live|test)_[A-Za-z0-9]{16,}"),
    ("google-api-key", r"\bAIza[0-9A-Za-z_-]{35}\b"),
    (
        "jwt",
        r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
    ),
    ("url-password", r#"://[^/\s:@]*:(?P<secret>[^\s@"]{3,})@"#),
    (
        "assigned-secret",
        r#"(?i)(?:password|passwd|secret|api[_-]?key|access[_-]?token|auth[_-]?token|private[_-]?key)\w*["']?\s*[=:]\s*"(?P<secret>[^"\r\n]{8,})""#,
    ),
    (
        "assigned-secret",
        r#"(?i)(?:password|passwd|secret|api[_-]?key|access[_-]?token|auth[_-]?token|private[_-]?key)\w*["']?\s*[=:]\s*'(?P<secret>[^'\r\n]{8,})'"#,
    ),
    (
        "assigned-secret",
        r#"(?i)(?:password|passwd|secret|api[_-]?key|access[_-]?token|auth[_-]?token|private[_-]?key)\w*["']?[ \t]*[=:][ \t]*(?P<secret>[^\s"'`;,(){}<>$\[\]\\]{12,})"#,
    ),
];

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    pub name: String,
    pub regex: String,
}

struct Rule {
    marker: Vec<u8>,
    regex: Regex,
}

pub(crate) const REDACTION_PREFIX: &[u8] = b"[baton:redacted:";

fn marker(rule: &str) -> Vec<u8> {
    format!("[baton:redacted:{rule}]").into_bytes()
}

/// True for exactly one marker, so a later rule keeps the earlier rule's name instead of relabelling it.
fn is_marker(bytes: &[u8]) -> bool {
    bytes
        .strip_prefix(REDACTION_PREFIX)
        .and_then(|rest| rest.strip_suffix(b"]"))
        .is_some_and(|name| {
            !name.is_empty()
                && name
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        })
}

pub struct Redactor {
    rules: Vec<Rule>,
    values: Option<AhoCorasick>,
    notices: Vec<String>,
}

impl Redactor {
    pub fn new(project_rules: &[RuleSpec], literals: &[String]) -> Result<Self> {
        let mut notices = Vec::new();
        let mut rules = Vec::with_capacity(BUILTIN.len() + project_rules.len());
        for (name, pattern) in BUILTIN {
            rules.push(Rule {
                marker: marker(name),
                regex: Regex::new(pattern)
                    .map_err(|e| Error::invalid("built-in redaction rule", e))?,
            });
        }
        for spec in project_rules {
            let name_ok = !spec.name.is_empty()
                && spec.name.len() <= 40
                && spec
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !name_ok {
                return Err(Error::invalid(
                    "redaction rule name",
                    format!(
                        "{:?} must be 1 to 40 characters of a-z, 0-9 and '-'",
                        spec.name
                    ),
                ));
            }
            let regex = Regex::new(&spec.regex).map_err(|e| {
                Error::invalid("redaction rule regex", format!("{}: {e}", spec.name))
            })?;
            if regex.is_match(b"") {
                notices.push(format!(
                    "redaction rule {} matches the empty string and was dropped",
                    spec.name
                ));
                continue;
            }
            rules.push(Rule {
                marker: marker(&spec.name),
                regex,
            });
        }
        let values = if literals.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .match_kind(MatchKind::LeftmostLongest)
                    .build(literals)
                    .map_err(|e| Error::invalid("redaction values", e))?,
            )
        };
        Ok(Self {
            rules,
            values,
            notices,
        })
    }

    /// What the loader dropped or ignored while building this redactor.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// Only the literal secrets, for files that must otherwise be stored byte for byte.
    pub fn redact_values<'a>(&self, input: &'a [u8]) -> Cow<'a, [u8]> {
        let Some(values) = &self.values else {
            return Cow::Borrowed(input);
        };
        if !values.is_match(input) {
            return Cow::Borrowed(input);
        }
        let mut out = Vec::with_capacity(input.len());
        let mut last = 0;
        for m in values.find_iter(input) {
            out.extend_from_slice(&input[last..m.start()]);
            out.extend_from_slice(&marker("value"));
            last = m.end();
        }
        out.extend_from_slice(&input[last..]);
        Cow::Owned(out)
    }

    pub fn redact<'a>(&self, input: &'a [u8]) -> Cow<'a, [u8]> {
        let mut current = self.redact_values(input);
        for rule in &self.rules {
            if !rule.regex.is_match(&current) {
                continue;
            }
            let mut out = Vec::with_capacity(current.len());
            let mut last = 0;
            for caps in rule.regex.captures_iter(&current) {
                let whole = caps.get(0).expect("group 0 always matches");
                let target = caps.name("secret").unwrap_or(whole);
                if target.is_empty() || is_marker(target.as_bytes()) {
                    continue;
                }
                out.extend_from_slice(&current[last..target.start()]);
                out.extend_from_slice(&rule.marker);
                last = target.end();
            }
            out.extend_from_slice(&current[last..]);
            current = Cow::Owned(out);
        }
        current
    }

    pub fn holds_secret(&self, text: &str) -> bool {
        matches!(self.redact(text.as_bytes()), Cow::Owned(_))
    }

    pub fn redact_text(&self, text: &str) -> String {
        String::from_utf8_lossy(&self.redact(text.as_bytes())).into_owned()
    }
}

/// Reads the literal secrets named by `[redact] value_files`. A `.json` file contributes every string
/// value; any other file contributes every line. Values shorter than 8 characters are dropped.
/// A file that cannot be read is an error: redaction that silently skips a file is worse than none.
pub fn load_literals(files: &[PathBuf]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(path)
            .ctx(|| format!("reading redact value file {}", path.display()))?;
        if is_json(path) {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| Error::Config {
                    path: path.clone(),
                    reason: format!("not JSON: {e}"),
                })?;
            collect_strings(&value, &mut out);
        } else {
            out.extend(text.lines().map(|l| l.trim().to_owned()));
        }
    }
    out.retain(|s| s.chars().count() >= MIN_LITERAL_LEN);
    out.sort();
    out.dedup();
    Ok(out)
}

fn is_json(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}
