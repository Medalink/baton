//! Calling a plug-in command: one JSON document on its stdin, one JSON document read from its stdout.
//! Any failure is an `Err`; the caller decides whether that fails open (classify, decide, notify) or
//! closed (auth). This is the only way baton runs a configured command.

use crate::error::{Error, IoContext, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const OUTPUT_MAX: u64 = 1 << 20;
const DEFAULT_TIMEOUT_MS: u64 = 2000;

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// A program and its arguments, run without a shell.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub command: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

impl CommandSpec {
    pub fn new(command: Vec<String>, timeout: Duration) -> Result<Self> {
        let spec = Self {
            command,
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        };
        spec.check()?;
        Ok(spec)
    }

    pub fn check(&self) -> Result<()> {
        if self
            .command
            .first()
            .is_none_or(|program| program.is_empty())
        {
            return Err(Error::invalid("command", "names no program"));
        }
        if self.timeout_ms == 0 {
            return Err(Error::invalid("command", "timeout_ms must be positive"));
        }
        Ok(())
    }
}

/// Runs the command with `input` as JSON on stdin and parses its stdout as JSON.
pub fn run_command<I: Serialize, O: DeserializeOwned>(spec: &CommandSpec, input: &I) -> Result<O> {
    let out = run_raw(spec, input)?;
    serde_json::from_slice(&out).map_err(|e| Error::invalid("plug-in output", e))
}

/// Runs the command and returns once it has exited, whatever it printed. For a notifier, whose answer is
/// ignored: a webhook script that prints nothing, or a progress line, is not a failure.
pub fn run_for_effect<I: Serialize>(spec: &CommandSpec, input: &I) -> Result<()> {
    run_raw(spec, input).map(drop)
}

/// The call returns by the deadline whatever the child and its descendants do. A killed child's
/// grandchild may outlive it and keep the pipe open, so the output is read on a thread and waited for
/// only until the deadline.
fn run_raw<I: Serialize>(spec: &CommandSpec, input: &I) -> Result<Vec<u8>> {
    spec.check()?;
    let program = &spec.command[0];
    let payload = serde_json::to_vec(input).map_err(|e| Error::invalid("plug-in input", e))?;
    let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);
    let mut child = Command::new(program)
        .args(&spec.command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ctx(|| format!("starting {program}"))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
    });
    let stdout = child.stdout.take().expect("stdout is piped");
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.take(OUTPUT_MAX).read_to_end(&mut out);
        let _ = sent.send(out);
    });

    let too_slow = || {
        Error::Refused(format!(
            "{program} did not answer within {} ms",
            spec.timeout_ms
        ))
    };
    let status = loop {
        match child.try_wait().ctx(|| format!("waiting for {program}"))? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(too_slow());
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    };
    let out = received
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| too_slow())?;
    if !status.success() {
        return Err(Error::Refused(format!("{program} exited with {status}")));
    }
    Ok(out)
}
