//! The commands a person runs. Most talk to the daemon, starting it first when its endpoint is absent.
//! `log` and `rewind` read the side repo directly and work with the daemon stopped.

use crate::control::{Reply, Request, TurnArgs};
use crate::{endpoint, timefmt};
use anyhow::{Context, Result, anyhow, bail};
use baton_core::{
    Answer, Ask, ChangeKind, Decision, Dirs, Plugins, RepoConfig, RewindAction, SideRepo, ThreadId,
    TurnRecord, TurnVerdict, Worktree, find_root, plan_rewind, project_key, run_command,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DAEMON_START_DEADLINE: Duration = Duration::from_secs(20);
const LISTED_PATHS: usize = 12;

fn send(request: &Request) -> Result<Reply> {
    let bytes = endpoint::call(&serde_json::to_vec(request)?)?;
    match serde_json::from_slice(&bytes)? {
        Reply::Failed { message } => Err(anyhow!(message)),
        reply => Ok(reply),
    }
}

fn daemon_is_up() -> bool {
    matches!(send(&Request::Ping), Ok(Reply::Pong { .. }))
}

/// Windows lets a child inherit every inheritable handle the parent holds, including the pipe a shell is
/// reading `$(baton ...)` from, which would keep that substitution waiting for as long as the daemon lives.
#[cfg(windows)]
fn stop_inheriting_std_handles() {
    use std::os::windows::io::AsRawHandle;
    unsafe extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for handle in [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ] {
        if !handle.is_null() {
            // SAFETY: the handle is one this process owns; the call only clears its inherit flag.
            unsafe { SetHandleInformation(handle.cast(), HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

fn spawn_daemon() -> Result<()> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        stop_inheriting_std_handles();
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    command.spawn().context("starting the baton daemon")?;
    Ok(())
}

/// What the daemon said when it stopped since `from` bytes into its log: the bind error, say, that a
/// 20 s wait would otherwise hide.
fn daemon_failure(log: &Path, from: u64) -> Option<String> {
    let text = std::fs::read(log).ok()?;
    let text = String::from_utf8_lossy(text.get(from as usize..)?);
    let line = text.lines().rfind(|l| l.contains("daemon stopped: "))?;
    Some(line.split_once("daemon stopped: ")?.1.to_owned())
}

pub fn ensure_daemon() -> Result<()> {
    if daemon_is_up() {
        return Ok(());
    }
    // A path that cannot exist is an error now, not a 20 s wait.
    #[cfg(unix)]
    endpoint::socket_path().context("finding the daemon socket")?;
    let log = Dirs::resolve().map(|d| d.log_file()).unwrap_or_default();
    let log_start = std::fs::metadata(&log).map_or(0, |m| m.len());
    spawn_daemon()?;
    let deadline = Instant::now() + DAEMON_START_DEADLINE;
    while Instant::now() < deadline {
        if daemon_is_up() {
            return Ok(());
        }
        if let Some(why) = daemon_failure(&log, log_start) {
            bail!("the daemon could not start: {why}; see {}", log.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    bail!(
        "the daemon did not start within {}s; see {}",
        DAEMON_START_DEADLINE.as_secs(),
        log.display()
    )
}

fn absolute(path: Option<&Path>) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(match path {
        Some(p) if p.is_absolute() => p.to_owned(),
        Some(p) => cwd.join(p),
        None => cwd,
    })
}

pub fn enroll(path: Option<&Path>) -> Result<()> {
    let path = absolute(path)?;
    ensure_daemon()?;
    match send(&Request::Enroll {
        path: path.to_string_lossy().into_owned(),
    })? {
        Reply::Enrolled {
            root,
            project,
            files,
            already,
        } => {
            let verb = if already {
                "already enrolled"
            } else {
                "enrolled"
            };
            println!("{verb} {root} as {project} ({files} files)");
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub fn unenroll(path: Option<&Path>) -> Result<()> {
    let path = absolute(path)?;
    ensure_daemon()?;
    match send(&Request::Unenroll {
        path: path.to_string_lossy().into_owned(),
    })? {
        Reply::Unenrolled { root, was_enrolled } => {
            println!(
                "{} {root}",
                if was_enrolled {
                    "unenrolled"
                } else {
                    "was not enrolled:"
                }
            );
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub fn enroll_repo(path: Option<&Path>) -> Result<()> {
    let path = absolute(path)?;
    ensure_daemon()?;
    match send(&Request::EnrollRepo {
        path: path.to_string_lossy().into_owned(),
    })? {
        Reply::EnrolledRepo {
            dir,
            project,
            already,
            cap,
        } => {
            let verb = if already {
                "already enrolled"
            } else {
                "enrolled"
            };
            println!(
                "{verb} repository {dir} as {project}; each worktree is recorded from its first turn, up to {cap} watched at once"
            );
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub fn unenroll_repo(path: Option<&Path>) -> Result<()> {
    let path = absolute(path)?;
    ensure_daemon()?;
    match send(&Request::UnenrollRepo {
        path: path.to_string_lossy().into_owned(),
    })? {
        Reply::UnenrolledRepo {
            dir,
            was_enrolled,
            stopped,
        } => {
            if was_enrolled {
                println!("unenrolled repository {dir} ({stopped} worktrees stopped)");
            } else {
                println!("repository was not enrolled: {dir}");
            }
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub struct TurnOptions {
    pub thread: String,
    pub message: String,
    pub agent: String,
    pub model: Option<String>,
    pub checks: Vec<String>,
}

pub fn turn(options: TurnOptions) -> Result<()> {
    ensure_daemon()?;
    let cwd = absolute(None)?;
    let request = Request::Turn(TurnArgs {
        cwd: cwd.to_string_lossy().into_owned(),
        thread: options.thread,
        message: options.message,
        agent: options.agent,
        model: options.model,
        checks: options.checks,
    });
    match send(&request)? {
        Reply::Turned {
            thread,
            seq,
            id,
            changed,
        } => {
            println!("{thread}@{seq} {} ({changed} paths changed)", &id[..8]);
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub fn status() -> Result<()> {
    ensure_daemon()?;
    match send(&Request::Status)? {
        Reply::Status {
            pid,
            up_secs,
            worktrees,
            repos,
            cap,
            projects,
        } => {
            println!(
                "daemon pid {pid}, up {up_secs}s, {} enrolled",
                worktrees.len()
            );
            for w in worktrees {
                println!(
                    "{}  {}  {} files  tree {}{}",
                    w.root,
                    w.project,
                    w.files,
                    w.tree.get(..8).unwrap_or(""),
                    w.repo.map_or_else(String::new, |r| format!("  via {r}"))
                );
            }
            for r in repos {
                println!(
                    "repository {}  {} of at most {cap} worktrees watched{}",
                    r.dir,
                    r.active,
                    r.listed.map_or_else(String::new, |p| format!(
                        "  through [enroll] projects ({p})"
                    ))
                );
            }
            if !projects.is_empty() {
                println!("enroll projects: {}", projects.join(", "));
            }
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub struct Opened {
    pub root: PathBuf,
    pub side: SideRepo,
}

/// The current directory's worktree and its side repo, read-only.
pub fn open_here() -> Result<Opened> {
    let root = find_root(&absolute(None)?).map_err(|e| anyhow!("{e}"))?;
    let config = RepoConfig::load(&root).map_err(|e| anyhow!("{e}"))?;
    let project = project_key(&root, &config).map_err(|e| anyhow!("{e}"))?;
    let dirs = Dirs::resolve().map_err(|e| anyhow!("{e}"))?;
    let side = SideRepo::open_existing(&dirs.side_repo(&project)).map_err(|e| anyhow!("{e}"))?;
    Ok(Opened { root, side })
}

/// `thread@seq  id  time  agent  model  subject`, the first line of `baton log` for a turn.
pub fn log_line(turn: &TurnRecord) -> String {
    let d = &turn.turn.draft;
    format!(
        "{}@{}  {}  {}  {}  {}  {}",
        d.thread,
        turn.turn.seq,
        turn.id.short(),
        timefmt::utc(turn.at),
        d.agent,
        d.model.as_ref().map_or("-", |m| m.as_str()),
        d.subject.as_str()
    )
}

pub fn describe(side: &SideRepo, turn: &TurnRecord) -> Result<()> {
    println!("{}", log_line(turn));
    let parent_tree = match turn.parent {
        Some(parent) => Some(side.read_turn(parent).map_err(|e| anyhow!("{e}"))?.tree),
        None => None,
    };
    if parent_tree.is_none() {
        let files = side.flatten(turn.tree).map_err(|e| anyhow!("{e}"))?.len();
        println!("    first turn of the thread: {files} files");
        return Ok(());
    }
    let changes = side
        .diff_trees(parent_tree, turn.tree)
        .map_err(|e| anyhow!("{e}"))?;
    for change in changes.iter().take(LISTED_PATHS) {
        let mark = match change.kind {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Removed => 'D',
        };
        println!("    {mark} {}", change.path);
    }
    if changes.len() > LISTED_PATHS {
        println!("    ... and {} more", changes.len() - LISTED_PATHS);
    }
    Ok(())
}

pub fn log(thread: Option<&str>, verdicts: bool) -> Result<()> {
    let here = open_here()?;
    let mut turns = Vec::new();
    match thread {
        Some(thread) => {
            let thread = ThreadId::parse(thread).map_err(|e| anyhow!("{e}"))?;
            turns = here.side.turns(&thread, 50).map_err(|e| anyhow!("{e}"))?;
        }
        None => {
            for thread in here.side.threads().map_err(|e| anyhow!("{e}"))? {
                turns.extend(here.side.turns(&thread, 10).map_err(|e| anyhow!("{e}"))?);
            }
            turns.sort_by_key(|t| std::cmp::Reverse(t.at));
            turns.truncate(30);
        }
    }
    if turns.is_empty() {
        println!("no turns recorded for this project");
    }
    for turn in &turns {
        if verdicts {
            let verdict = here.side.verdict(turn.id).map_err(|e| anyhow!("{e}"))?;
            println!("{}", verdict_line(turn, verdict.as_ref()));
        } else {
            describe(&here.side, turn)?;
        }
    }
    Ok(())
}

/// `thread@seq  id  class  kinds  subject`; the class is `-` for a turn with no verdict.
fn verdict_line(turn: &TurnRecord, verdict: Option<&TurnVerdict>) -> String {
    let d = &turn.turn.draft;
    let (class, kinds) = match verdict {
        Some(v) => {
            let mut kinds: Vec<&str> = v.findings.iter().map(|f| f.kind.as_str()).collect();
            kinds.dedup();
            (v.class.as_str(), kinds.join(","))
        }
        None => ("-", String::new()),
    };
    format!(
        "{}@{}  {}  {class:<5}  {kinds}  {}",
        d.thread,
        turn.turn.seq,
        turn.id.short(),
        d.subject.as_str()
    )
}

pub fn verdict(selector: &str, thread: Option<&str>, json: bool) -> Result<()> {
    let here = open_here()?;
    let default_thread = thread
        .map(ThreadId::parse)
        .transpose()
        .map_err(|e| anyhow!("{e}"))?;
    let turn = here
        .side
        .resolve_turn(selector, default_thread.as_ref())
        .map_err(|e| anyhow!("{e}"))?;
    let Some(verdict) = here.side.verdict(turn.id).map_err(|e| anyhow!("{e}"))? else {
        bail!(
            "no verdict for {} (classification is off, still running, or failed; the daemon log says which)",
            turn.id.short()
        );
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&verdict)?);
        return Ok(());
    }
    println!("{}", verdict_line(&turn, Some(&verdict)));
    for finding in &verdict.findings {
        let place = finding
            .evidence
            .first()
            .map_or(String::new(), |e| match e.lines {
                Some([from, to]) if from == to => format!("{}:{from}", e.path),
                Some([from, to]) => format!("{}:{from}-{to}", e.path),
                None => e.path.clone(),
            });
        println!(
            "    {}  {:.2}  {place}  {}",
            finding.kind, finding.score, finding.note
        );
    }
    Ok(())
}

/// `baton ask`. Returns the exit code: 0 when the decider chose, 2 when a human must answer.
/// Every way the decider can fail is "ask a human", since the caller is an agent that must not guess.
pub fn ask(
    question: &str,
    options: &[String],
    context: Option<&str>,
    thread: Option<&str>,
) -> Result<i32> {
    if options.len() < 2 {
        bail!("give at least two --option values to choose between");
    }
    let human = |why: &str| {
        println!("ask a human");
        eprintln!("baton: {why}");
        Ok(2)
    };
    let dirs = Dirs::resolve().map_err(|e| anyhow!("{e}"))?;
    let plugins = match Plugins::load(&dirs.user_config()) {
        Ok(plugins) => plugins,
        Err(e) => return human(&e.to_string()),
    };
    let Some(decider) = plugins.decide else {
        return human("no [decide] command is configured");
    };
    let project = std::env::current_dir()
        .ok()
        .and_then(|cwd| find_root(&cwd).ok())
        .and_then(|root| {
            let config = RepoConfig::load(&root).ok()?;
            project_key(&root, &config).ok()
        })
        .map(|key| key.to_string());
    let from_env = std::env::var("BATON_THREAD").ok();
    let thread = thread
        .or(from_env.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let request = Ask::new(question, options, context, project.as_deref(), thread);
    let decision =
        run_command::<_, Answer>(&decider, &request).and_then(|answer| answer.decide(options));
    match decision {
        Ok(Decision::Chose {
            choice,
            confidence,
            reason,
        }) => {
            println!("choice: {choice}");
            if let Some(confidence) = confidence {
                println!("confidence: {confidence:.2}");
            }
            if !reason.is_empty() {
                println!("reason: {reason}");
            }
            Ok(0)
        }
        Ok(Decision::AskHuman { reason }) => {
            human(&format!("the decider asks for a human: {reason}"))
        }
        Err(e) => human(&format!("the decider failed: {e}")),
    }
}

pub fn rewind(selector: &str, thread: Option<&str>, apply: bool) -> Result<()> {
    let here = open_here()?;
    let default_thread = thread
        .map(ThreadId::parse)
        .transpose()
        .map_err(|e| anyhow!("{e}"))?;
    let turn = here
        .side
        .resolve_turn(selector, default_thread.as_ref())
        .map_err(|e| anyhow!("{e}"))?;
    let dirs = Dirs::resolve().map_err(|e| anyhow!("{e}"))?;
    let worktree = Worktree::open(&here.root, &dirs, &|tree| here.side.has_tree(tree))
        .map_err(|e| anyhow!("{e}"))?;
    let plan = plan_rewind(&here.side, &worktree, turn).map_err(|e| anyhow!("{e}"))?;
    let d = &plan.turn.turn.draft;
    println!(
        "rewind to {}@{} ({})",
        d.thread,
        plan.turn.turn.seq,
        plan.turn.id.short()
    );
    for write in plan.writes.iter().take(LISTED_PATHS * 4) {
        let verb = match write.action {
            RewindAction::Create => "create",
            RewindAction::Overwrite => "restore",
        };
        println!("  {verb} {}", write.path);
    }
    for refusal in &plan.refused {
        println!("  refuse {}: {}", refusal.path, refusal.reason);
    }
    if !plan.left_alone.is_empty() {
        println!(
            "  {} tracked files are not in this turn and are left on disk",
            plan.left_alone.len()
        );
    }
    println!("  {} files already match", plan.same);
    if !apply {
        println!("dry run; pass --apply to write {} files", plan.writes.len());
        return Ok(());
    }
    let written =
        baton_core::apply_rewind(&here.side, &here.root, &plan).map_err(|e| anyhow!("{e}"))?;
    println!("wrote {written} files");
    if !plan.refused.is_empty() {
        bail!("{} files were refused", plan.refused.len());
    }
    Ok(())
}
