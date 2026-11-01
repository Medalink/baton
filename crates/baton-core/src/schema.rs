//! The JSON documents a plug-in command reads or writes (`docs/protocol.md`, Plug-in commands).
//! What baton sends is built from typed values. What a plug-in sends back is untrusted: it is parsed
//! once, here, into types that cannot hold a value out of range, so no caller re-checks them.

use crate::error::{Error, Result};
use crate::id::{AgentName, GitSha, MachineId, ThreadId, TurnId, UnixMillis};
use crate::turn::Check;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const TURN_SCHEMA: &str = "baton.turn.v1";
pub const VERDICT_SCHEMA: &str = "baton.verdict.v1";
pub const ASK_SCHEMA: &str = "baton.ask.v1";
pub const ANSWER_SCHEMA: &str = "baton.answer.v1";
pub const TURNREQ_SCHEMA: &str = "baton.turnreq.v1";

const MAX_FINDINGS: usize = 100;
const MAX_EVIDENCE: usize = 20;
const MAX_TEXT_CHARS: usize = 1000;

fn expect_schema(got: &str, want: &'static str) -> Result<()> {
    if got == want {
        Ok(())
    } else {
        Err(Error::invalid(
            "plug-in output",
            format!("schema is {got:?}, not {want:?}"),
        ))
    }
}

fn clip(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

/// How much a person should look at a turn. Declared lowest first, so the derived order is the severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Quiet,
    Log,
    Check,
    React,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::Log => "log",
            Self::Check => "check",
            Self::React => "react",
        }
    }
}

impl std::fmt::Display for Class {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Removed,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileChange {
    pub path: String,
    pub status: FileStatus,
    pub added: u32,
    pub removed: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub exit: i32,
}

impl From<&Check> for CheckResult {
    fn from(c: &Check) -> Self {
        Self {
            name: c.name.to_string(),
            exit: c.exit,
        }
    }
}

/// `baton.turn.v1`: what the classifier and the notifiers read.
#[derive(Clone, Debug, Serialize)]
pub struct TurnDoc {
    pub schema: &'static str,
    pub project: String,
    pub thread: ThreadId,
    pub turn: u32,
    pub commit: TurnId,
    pub parent: Option<TurnId>,
    pub base: Option<GitSha>,
    pub subject: String,
    pub agent: AgentName,
    pub model: Option<String>,
    pub machine: MachineId,
    pub at: UnixMillis,
    pub checks: Vec<CheckResult>,
    pub files: Vec<FileChange>,
    pub files_truncated: bool,
    pub diff: String,
    pub diff_truncated: bool,
    pub lens: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<[u32; 2]>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub kind: String,
    pub score: f64,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub note: String,
}

/// `baton.verdict.v1`. A value of this type is in range: `Deserialize` goes through `WireVerdict`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "WireVerdict", into = "WireVerdict")]
pub struct TurnVerdict {
    pub class: Class,
    pub findings: Vec<Finding>,
}

#[derive(Serialize, Deserialize)]
struct WireVerdict {
    schema: String,
    class: Class,
    #[serde(default)]
    findings: Vec<Finding>,
}

impl From<TurnVerdict> for WireVerdict {
    fn from(v: TurnVerdict) -> Self {
        Self {
            schema: VERDICT_SCHEMA.into(),
            class: v.class,
            findings: v.findings,
        }
    }
}

impl TryFrom<WireVerdict> for TurnVerdict {
    type Error = Error;

    fn try_from(wire: WireVerdict) -> Result<Self> {
        expect_schema(&wire.schema, VERDICT_SCHEMA)?;
        if wire.findings.len() > MAX_FINDINGS {
            return Err(Error::invalid(
                "verdict",
                format!("more than {MAX_FINDINGS} findings"),
            ));
        }
        let mut findings = wire.findings;
        for finding in &mut findings {
            if !(0.0..=1.0).contains(&finding.score) {
                return Err(Error::invalid(
                    "verdict",
                    format!("score {} is not between 0 and 1", finding.score),
                ));
            }
            if finding.evidence.len() > MAX_EVIDENCE {
                return Err(Error::invalid(
                    "verdict",
                    format!("more than {MAX_EVIDENCE} pieces of evidence"),
                ));
            }
            if finding
                .evidence
                .iter()
                .any(|e| e.lines.is_some_and(|[from, to]| from > to))
            {
                return Err(Error::invalid("verdict", "a line range runs backwards"));
            }
            finding.kind = clip(&finding.kind);
            finding.note = clip(&finding.note);
        }
        Ok(Self {
            class: wire.class,
            findings,
        })
    }
}

impl TurnVerdict {
    /// The strings a classifier chose go through `redact` before anything stores them.
    #[must_use]
    pub fn redacted(mut self, redact: &dyn Fn(&str) -> String) -> Self {
        for finding in &mut self.findings {
            finding.kind = redact(&finding.kind);
            finding.note = redact(&finding.note);
            for evidence in &mut finding.evidence {
                evidence.path = redact(&evidence.path);
            }
        }
        self
    }
}

/// What a notifier reads.
#[derive(Serialize)]
pub struct NotifyDoc<'a> {
    pub verdict: &'a TurnVerdict,
    pub turn: &'a TurnDoc,
}

/// What the lens command prints.
#[derive(Deserialize)]
pub struct LensAnswer {
    pub lens: String,
}

/// `baton.ask.v1`.
#[derive(Serialize)]
pub struct Ask<'a> {
    pub schema: &'static str,
    pub project: Option<&'a str>,
    pub thread: Option<&'a str>,
    pub question: &'a str,
    pub options: &'a [String],
    pub context: Option<&'a str>,
}

impl<'a> Ask<'a> {
    pub fn new(
        question: &'a str,
        options: &'a [String],
        context: Option<&'a str>,
        project: Option<&'a str>,
        thread: Option<&'a str>,
    ) -> Self {
        Self {
            schema: ASK_SCHEMA,
            project,
            thread,
            question,
            options,
            context,
        }
    }
}

/// `baton.answer.v1`, before it is checked against the options that were asked.
#[derive(Deserialize)]
pub struct Answer {
    schema: String,
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    ask_human: bool,
}

/// What `baton ask` does with an answer.
#[derive(Debug, PartialEq)]
pub enum Decision {
    Chose {
        choice: String,
        confidence: Option<f64>,
        reason: String,
    },
    AskHuman {
        reason: String,
    },
}

impl Answer {
    pub fn decide(self, options: &[String]) -> Result<Decision> {
        expect_schema(&self.schema, ANSWER_SCHEMA)?;
        if self.ask_human {
            return Ok(Decision::AskHuman {
                reason: clip(&self.reason),
            });
        }
        let choice = self
            .choice
            .filter(|c| options.contains(c))
            .ok_or_else(|| Error::invalid("answer", "choice is not one of the options"))?;
        if self.confidence.is_some_and(|c| !(0.0..=1.0).contains(&c)) {
            return Err(Error::invalid(
                "answer",
                "confidence is not between 0 and 1",
            ));
        }
        Ok(Decision::Chose {
            choice,
            confidence: self.confidence,
            reason: clip(&self.reason),
        })
    }
}

#[derive(Deserialize)]
struct WireTurnReq {
    schema: String,
    cwd: String,
    thread: String,
    #[serde(default)]
    subject: String,
    agent: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    checks: Vec<Check>,
}

/// `baton.turnreq.v1`: what an adapter says about the turn that just ended. An adapter that prints
/// `null` says the payload was not an end of turn, so callers read `Option<TurnReq>`.
#[derive(Debug, Deserialize)]
#[serde(try_from = "WireTurnReq")]
pub struct TurnReq {
    pub cwd: PathBuf,
    pub thread: ThreadId,
    pub subject: String,
    pub agent: AgentName,
    pub model: Option<String>,
    pub checks: Vec<Check>,
}

impl TryFrom<WireTurnReq> for TurnReq {
    type Error = Error;

    fn try_from(wire: WireTurnReq) -> Result<Self> {
        expect_schema(&wire.schema, TURNREQ_SCHEMA)?;
        if wire.cwd.is_empty() {
            return Err(Error::invalid("turn request", "cwd is empty"));
        }
        Ok(Self {
            cwd: PathBuf::from(wire.cwd),
            thread: ThreadId::parse(&wire.thread)?,
            subject: wire.subject,
            agent: AgentName::parse(&wire.agent)?,
            model: wire.model.filter(|m| !m.is_empty()),
            checks: wire.checks,
        })
    }
}
