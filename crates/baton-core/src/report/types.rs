//! The `baton.report.v1` document, and the per-thread input the rules read.

use super::stats::FileStat;
use serde::Serialize;

pub const REPORT_SCHEMA: &str = "baton.report.v1";

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub project: String,
    /// Unix milliseconds.
    pub generated_at: i64,
    /// Start of the window, Unix milliseconds.
    pub since: i64,
    pub repo: RepoInfo,
    pub counts: Counts,
    pub thrash: Vec<Thrash>,
    pub rework: Vec<Rework>,
    pub abandoned: Vec<Abandoned>,
    pub hotspots: Vec<Hotspot>,
    pub models: Vec<Group>,
    pub agents: Vec<Group>,
    pub skipped: Skipped,
}

#[derive(Debug, Serialize)]
pub struct RepoInfo {
    /// False when no repository of the project was found, so rework and abandonment were not checked.
    pub available: bool,
    /// The commit `HEAD` pointed at, if the repository has one.
    pub head: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Counts {
    pub threads: u32,
    pub turns: u32,
    pub thrash: u32,
    pub rework: u32,
    pub abandoned: u32,
    pub hotspots: u32,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Thrash {
    /// One file changed in three or more turns of a thread, which took out lines an earlier turn added.
    File {
        thread: String,
        /// The turn at which the rule first held.
        turn: u32,
        file: String,
        /// Every turn of the thread that changed the file.
        turns: Vec<u32>,
        /// Lines taken out that an earlier turn of the thread had added.
        lines: u32,
    },
    /// A check that failed, passed and failed again within a thread.
    Check {
        thread: String,
        /// The turn of the second failure.
        turn: u32,
        check: String,
        runs: Vec<CheckRun>,
    },
}

#[derive(Debug, Serialize)]
pub struct CheckRun {
    pub turn: u32,
    pub exit: i32,
}

#[derive(Debug, Serialize)]
pub struct Rework {
    pub thread: String,
    pub turn: u32,
    pub file: String,
    pub lines: u32,
    /// When the earliest of those lines reached `HEAD`'s history, Unix milliseconds.
    pub shipped_at: i64,
    pub by: ReworkedBy,
}

#[derive(Debug, Serialize)]
pub struct ReworkedBy {
    pub thread: String,
    pub turn: u32,
    pub at: i64,
}

#[derive(Debug, Serialize)]
pub struct Abandoned {
    pub thread: String,
    /// The thread's last turn.
    pub turn: u32,
    pub last_at: i64,
    pub changed_lines: u32,
    pub reached_lines: u32,
    pub files: Vec<AbandonedFile>,
    pub files_total: u32,
}

#[derive(Debug, Serialize)]
pub struct AbandonedFile {
    pub path: String,
    pub changed_lines: u32,
    pub reached_lines: u32,
}

#[derive(Debug, Serialize)]
pub struct Hotspot {
    pub file: String,
    pub thrash: u32,
    pub rework: u32,
}

/// Turns, threads and rates for one model or one agent (`name` is null for a turn that recorded none).
#[derive(Debug, Serialize)]
pub struct Group {
    pub name: Option<String>,
    pub turns: u32,
    pub threads: u32,
    pub shipped_lines: u32,
    pub reworked_lines: u32,
    /// `reworked_lines / shipped_lines`; null when nothing shipped.
    pub rework_rate: Option<f64>,
    pub thrash_threads: u32,
    /// `thrash_threads / threads`.
    pub thrash_rate: Option<f64>,
}

#[derive(Debug, Default, Serialize)]
pub struct Skipped {
    /// Threads whose `Baton-Base` commit is not in the repository, so their base-relative parts were left out.
    pub threads_without_base: u32,
    /// Objects that could not be read.
    pub unreadable: u32,
    /// The walk over the repository's recent commits stopped at its cap.
    pub commits_truncated: bool,
}

/// One turn inside the window, with what it changed (none when that cannot be known here).
pub(super) struct TurnRun {
    pub seq: u32,
    pub at: i64,
    pub agent: String,
    pub model: Option<String>,
    pub checks: Vec<(String, i32)>,
    pub files: Option<Vec<FileStat>>,
}

/// A thread's turns inside the window, oldest first.
pub(super) struct ThreadRun {
    pub name: String,
    pub turns: Vec<TurnRun>,
    /// The thread's last tree against its base, for a thread that has gone quiet.
    pub tip: Option<Vec<FileStat>>,
}
