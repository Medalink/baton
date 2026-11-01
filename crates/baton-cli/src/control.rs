//! Messages between a `baton` command and the daemon. JSON on the same endpoint as the hook, behind the
//! `BTC1` magic. The hook never sends these.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Enroll { path: String },
    Unenroll { path: String },
    Turn(TurnArgs),
    Status,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TurnArgs {
    pub cwd: String,
    pub thread: String,
    pub message: String,
    pub agent: String,
    pub model: Option<String>,
    pub checks: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorktreeStatus {
    pub root: String,
    pub project: String,
    pub files: usize,
    pub tree: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Pong {
        pid: u32,
    },
    Enrolled {
        root: String,
        project: String,
        files: usize,
        already: bool,
    },
    Unenrolled {
        root: String,
        was_enrolled: bool,
    },
    Turned {
        thread: String,
        seq: u32,
        id: String,
        changed: usize,
    },
    Status {
        pid: u32,
        up_secs: u64,
        worktrees: Vec<WorktreeStatus>,
    },
    Failed {
        message: String,
    },
}
