//! Messages between a `baton` command and the daemon. JSON on the same endpoint as the hook, behind the
//! `BTC1` magic. The hook never sends these.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Enroll { path: String },
    Unenroll { path: String },
    EnrollRepo { path: String },
    UnenrollRepo { path: String },
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
    /// The repository it was enrolled through, or none for a worktree enrolled by itself.
    pub repo: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoStatus {
    pub dir: String,
    pub active: usize,
    /// The project `[enroll] projects` enrolled it under; none when the person enrolled it.
    #[serde(default)]
    pub listed: Option<String>,
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
        /// A repository brought the worktree in, so its next turn enrolls it again.
        #[serde(default)]
        via_repo: bool,
    },
    EnrolledRepo {
        dir: String,
        project: String,
        already: bool,
        cap: usize,
    },
    UnenrolledRepo {
        dir: String,
        was_enrolled: bool,
        stopped: usize,
        /// `[enroll] projects` names the repository's project, so it stays out until `enroll --repo`.
        #[serde(default)]
        opted_out: bool,
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
        repos: Vec<RepoStatus>,
        cap: usize,
        /// `[enroll] projects` as the daemon reads it now.
        #[serde(default)]
        projects: Vec<String>,
        /// Repositories the list would enroll that the person unenrolled.
        #[serde(default)]
        opted_out: Vec<String>,
    },
    Failed {
        message: String,
    },
}
