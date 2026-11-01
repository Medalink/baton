//! Calling a plug-in command: one JSON document on its stdin, one JSON document read from its stdout.
//! Any failure is an `Err`; the caller decides whether that fails open (classify, decide, notify) or
//! closed (auth).

use crate::error::{Error, IoContext, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OUTPUT_MAX: u64 = 1 << 20;
const DEFAULT_TIMEOUT_MS: u64 = 2000;

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// A program and its arguments, run without a shell.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub command: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

impl CommandSpec {
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

pub fn run_command<I: Serialize, O: DeserializeOwned>(spec: &CommandSpec, input: &I) -> Result<O> {
    spec.check()?;
    let program = &spec.command[0];
    let mut child = Command::new(program)
        .args(&spec.command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ctx(|| format!("starting {program}"))?;
    let payload = serde_json::to_vec(input).map_err(|e| Error::invalid("plug-in input", e))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
    });
    let stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.take(OUTPUT_MAX).read_to_end(&mut out);
        out
    });

    let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);
    let status = loop {
        match child.try_wait().ctx(|| format!("waiting for {program}"))? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Refused(format!(
                    "{program} did not answer within {} ms",
                    spec.timeout_ms
                )));
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    };
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(Error::Refused(format!("{program} exited with {status}")));
    }
    serde_json::from_slice(&out).map_err(|e| Error::invalid("plug-in output", e))
}
