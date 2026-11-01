//! The turn record and the only code that reads or writes the `Baton-*` trailer grammar.

use crate::error::{Error, Result};
use crate::id::{AgentName, CheckName, GitSha, MachineId, ModelName, RelPath, ThreadId, TurnSeq};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

const SUBJECT_MAX_CHARS: usize = 100;
const NO_SUBJECT: &str = "(no message)";

/// One line, whitespace collapsed, at most 100 characters. Never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject(String);

impl Subject {
    pub fn new(raw: &str) -> Self {
        let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let cleaned: String = collapsed.chars().filter(|c| !c.is_control()).collect();
        let mut chars = cleaned.chars();
        let mut line: String = chars.by_ref().take(SUBJECT_MAX_CHARS).collect();
        if chars.next().is_some() {
            line.truncate(line.trim_end().len());
            line.push_str("...");
        }
        if line.is_empty() {
            line.push_str(NO_SUBJECT);
        }
        Self(line)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: CheckName,
    pub exit: i32,
}

impl Check {
    /// Parses the `name=code` form used by `baton turn --check`.
    pub fn parse(s: &str) -> Result<Self> {
        let (name, code) = s
            .split_once('=')
            .ok_or_else(|| Error::invalid("check", format!("{s:?} is not name=code")))?;
        let exit = code
            .parse::<i32>()
            .map_err(|_| Error::invalid("check", format!("{code:?} is not an exit code")))?;
        Ok(Self {
            name: CheckName::parse(name)?,
            exit,
        })
    }
}

/// A path baton did not store, as a turn records it. A name that held a secret appears with the secret
/// cut out, which need not be a path git or a filesystem could hold, so this is a label to show and
/// never a path to open.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SkippedPath(Box<str>);

impl SkippedPath {
    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() || s.chars().any(char::is_control) {
            return Err(Error::invalid(
                "Baton-Skipped",
                format!("{s:?} is empty or has a control character"),
            ));
        }
        Ok(Self(s.into()))
    }

    pub fn of(path: &RelPath) -> Self {
        Self(path.as_str().into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SkippedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything about a turn except the position in its thread, which the side repo assigns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnDraft {
    pub thread: ThreadId,
    pub agent: AgentName,
    pub model: Option<ModelName>,
    pub machine: MachineId,
    pub base: Option<GitSha>,
    pub checks: Vec<Check>,
    pub skipped: Vec<SkippedPath>,
    pub subject: Subject,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub seq: TurnSeq,
    pub draft: TurnDraft,
}

fn escape_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c == '%' || c.is_control() {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "%{b:02X}");
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape_path(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s
                .get(i + 1..i + 3)
                .ok_or_else(|| Error::invalid("Baton-Skipped", "truncated escape"))?;
            out.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| Error::invalid("Baton-Skipped", "bad escape"))?,
            );
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Error::invalid("Baton-Skipped", "not UTF-8"))
}

impl Turn {
    pub fn to_message(&self) -> String {
        let d = &self.draft;
        let mut out = format!("{}\n\n", d.subject.as_str());
        let mut line = |key: &str, value: &dyn std::fmt::Display| {
            let _ = writeln!(out, "Baton-{key}: {value}");
        };
        line("Thread", &d.thread);
        line("Turn", &self.seq);
        line("Agent", &d.agent);
        if let Some(model) = &d.model {
            line("Model", model);
        }
        line("Machine", &d.machine);
        if let Some(base) = &d.base {
            line("Base", base);
        }
        for check in &d.checks {
            line("Check", &format_args!("{}={}", check.name, check.exit));
        }
        for path in &d.skipped {
            line("Skipped", &escape_path(path.as_str()));
        }
        out
    }

    pub fn parse_message(message: &str) -> Result<Self> {
        let (subject_line, trailers) = message
            .split_once("\n\n")
            .ok_or_else(|| Error::invalid("turn message", "no trailer block"))?;
        if subject_line.contains('\n') {
            return Err(Error::invalid(
                "turn message",
                "subject spans several lines",
            ));
        }

        let mut thread = None;
        let mut seq = None;
        let mut agent = None;
        let mut model = None;
        let mut machine = None;
        let mut base = None;
        let mut checks = Vec::new();
        let mut skipped = Vec::new();

        fn once<T>(slot: &mut Option<T>, key: &str, value: T) -> Result<()> {
            if slot.replace(value).is_some() {
                return Err(Error::invalid(
                    "turn message",
                    format!("duplicate Baton-{key}"),
                ));
            }
            Ok(())
        }

        for line in trailers.lines().filter(|l| !l.is_empty()) {
            let Some((key, value)) = line.split_once(": ") else {
                return Err(Error::invalid(
                    "turn message",
                    format!("not a trailer: {line:?}"),
                ));
            };
            match key {
                "Baton-Thread" => once(&mut thread, "Thread", ThreadId::parse(value)?)?,
                "Baton-Turn" => {
                    let n = value
                        .parse::<u32>()
                        .map_err(|_| Error::invalid("Baton-Turn", format!("{value:?}")))?;
                    once(&mut seq, "Turn", TurnSeq::parse(n)?)?;
                }
                "Baton-Agent" => once(&mut agent, "Agent", AgentName::parse(value)?)?,
                "Baton-Model" => once(&mut model, "Model", ModelName::parse(value)?)?,
                "Baton-Machine" => once(&mut machine, "Machine", MachineId::parse(value)?)?,
                "Baton-Base" => once(&mut base, "Base", GitSha::parse(value)?)?,
                "Baton-Check" => checks.push(Check::parse(value)?),
                "Baton-Skipped" => skipped.push(SkippedPath::parse(&unescape_path(value)?)?),
                _ => {}
            }
        }

        let missing = |key: &str| Error::invalid("turn message", format!("missing Baton-{key}"));
        Ok(Self {
            seq: seq.ok_or_else(|| missing("Turn"))?,
            draft: TurnDraft {
                thread: thread.ok_or_else(|| missing("Thread"))?,
                agent: agent.ok_or_else(|| missing("Agent"))?,
                model,
                machine: machine.ok_or_else(|| missing("Machine"))?,
                base,
                checks,
                skipped,
                subject: Subject::new(subject_line),
            },
        })
    }
}
