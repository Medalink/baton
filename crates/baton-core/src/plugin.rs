//! Calling a plug-in command: one JSON document on its stdin, one JSON document read from its stdout.
//! Any failure is an `Err`; the caller decides whether that fails open (classify, decide, notify) or
//! closed (auth).

use crate::error::{Error, IoContext, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
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

/// The command and everything it started, so that one call ends all of it. A grandchild that inherited
/// the output pipe would otherwise keep the read open long after the command answered or timed out.
struct Tree {
    #[cfg(unix)]
    group: libc::pid_t,
    #[cfg(windows)]
    job: Option<job::Job>,
}

impl Tree {
    fn spawn(command: &mut Command) -> std::io::Result<(Child, Self)> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let child = command.process_group(0).spawn()?;
            let group = libc::pid_t::try_from(child.id()).unwrap_or(0);
            Ok((child, Self { group }))
        }
        #[cfg(windows)]
        {
            let child = command.spawn()?;
            let job = job::Job::adopt(&child);
            Ok((child, Self { job }))
        }
    }

    fn kill(&self, child: &mut Child) {
        #[cfg(unix)]
        if self.group > 0 {
            // SAFETY: a plain signal to the process group this call created.
            unsafe { libc::kill(-self.group, libc::SIGKILL) };
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.kill();
        }
        let _ = child.kill();
    }
}

#[cfg(windows)]
mod job {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn TerminateJobObject(job: *mut c_void, exit_code: u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    pub struct Job(*mut c_void);

    // SAFETY: a job handle is a kernel object id that any thread may use.
    unsafe impl Send for Job {}

    impl Job {
        pub fn adopt(child: &Child) -> Option<Self> {
            // SAFETY: both calls take handles that are valid for the duration of the call.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                if AssignProcessToJobObject(job, child.as_raw_handle().cast()) == 0 {
                    CloseHandle(job);
                    return None;
                }
                Some(Self(job))
            }
        }

        pub fn kill(&self) {
            // SAFETY: the handle is open until `drop`.
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closes the handle `adopt` opened, once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Runs the command once. `timeout_ms` bounds the whole call: starting it, its exit and the read of its
/// answer. When the command exits, or the time is up, everything it started is killed, so a helper that
/// kept the output open cannot hold the call.
pub fn run_command<I: Serialize, O: DeserializeOwned>(spec: &CommandSpec, input: &I) -> Result<O> {
    spec.check()?;
    let program = &spec.command[0];
    let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);
    let mut command = Command::new(program);
    command
        .args(&spec.command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let (mut child, tree) = Tree::spawn(&mut command).ctx(|| format!("starting {program}"))?;
    let payload = serde_json::to_vec(input).map_err(|e| Error::invalid("plug-in input", e))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
    });
    let stdout = child.stdout.take().expect("stdout is piped");
    let (answer, read) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.take(OUTPUT_MAX).read_to_end(&mut out);
        let _ = answer.send(out);
    });

    let timed_out = |tree: &Tree, child: &mut Child| {
        tree.kill(child);
        let _ = child.wait();
        Error::Refused(format!(
            "{program} did not answer within {} ms",
            spec.timeout_ms
        ))
    };
    let status = loop {
        match child.try_wait().ctx(|| format!("waiting for {program}"))? {
            Some(status) => break status,
            None if Instant::now() >= deadline => return Err(timed_out(&tree, &mut child)),
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    };
    tree.kill(&mut child);
    let out = read
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| timed_out(&tree, &mut child))?;
    if !status.success() {
        return Err(Error::Refused(format!("{program} exited with {status}")));
    }
    serde_json::from_slice(&out).map_err(|e| Error::invalid("plug-in output", e))
}
