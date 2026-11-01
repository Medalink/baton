//! The per-user daemon: the enrolled worktrees, one writer per project, and the endpoint.
//!
//! Ownership: a worktree task owns its dirty set, mirror and tree; a writer owns its side repo. The
//! registry here is only a routing table (cwd to task, project to writer) behind a short lock.

mod link;
mod log;
mod plugins;
mod repos;
mod server;
mod task;
mod writer;

use crate::control::{Reply, RepoStatus, Request, WorktreeStatus};
use anyhow::{Context, Result, anyhow};
use baton_core::{
    Dirs, MachineId, Plugins, ProjectKey, RepoConfig, TurnReq, UserConfig, find_root, project_key,
    run_command_bytes, simplified,
};
use link::Link;
use log::Log;
use plugins::Runner;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;
use task::{Cmd, Handle, TurnInput, TurnJob};
use writer::Writer;

#[derive(Default, Serialize, Deserialize)]
struct EnrolledFile {
    /// Worktrees enrolled by themselves.
    roots: Vec<String>,
    /// Common git dirs of repositories enrolled with `--repo`. Their worktrees are not listed: each one
    /// comes back with its next turn.
    #[serde(default)]
    repos: Vec<String>,
    /// Common git dirs the person unenrolled although `[enroll] projects` names their project. The list
    /// does not enroll them again until `baton enroll --repo` says so.
    #[serde(default)]
    declined: Vec<String>,
}

#[derive(Default)]
struct Registry {
    worktrees: BTreeMap<String, Handle>,
    writers: HashMap<ProjectKey, Writer>,
    starting: HashSet<String>,
    /// Enrolled repositories: `path_key` of the common git dir to the dir itself.
    repos: BTreeMap<String, PathBuf>,
    /// The repositories among `repos` that `[enroll] projects` brought in, with the project that matched.
    /// They are not saved: the list is evaluated again when the daemon starts.
    listed: BTreeMap<String, ProjectKey>,
    /// Repositories the list must not enroll: `path_key` of the common git dir to the dir itself. Saved.
    declined: BTreeMap<String, PathBuf>,
}

pub struct Daemon {
    dirs: Dirs,
    log: Arc<Log>,
    registry: Mutex<Registry>,
    /// Signalled when a worktree stops being started, so a second caller for it can proceed.
    settled: Condvar,
    /// Most worktrees of enrolled repositories watched at once.
    max_watched: usize,
    link: Link,
    plugins: Runner,
    /// Enrolled roots that could not be started (a drive that is not mounted). They stay enrolled on disk.
    unrestored: Mutex<BTreeMap<String, String>>,
    started: Instant,
    /// What `[enroll] projects` says, and the repositories it did not admit.
    listing: Mutex<repos::Listing>,
}

enum Route {
    /// The registry key of the worktree task that owns the cwd.
    Task(String),
    /// The cwd sits in a git repo nested inside an enrolled worktree. That repo is not the project's, and
    /// the outer worktree does not track it.
    NestedRepo(std::path::PathBuf),
    Unenrolled,
}

/// Why a turn found no worktree task.
enum Undelivered {
    /// The cwd sits in a git repo nested inside an enrolled worktree.
    Nested(PathBuf),
    Unenrolled,
    Failed(String),
}

/// A path as a map key: case-folded and `/`-separated on Windows, where both spellings name one directory.
pub fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = text.trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        text.replace('\\', "/").to_lowercase()
    } else {
        text.to_owned()
    }
}

impl Daemon {
    fn new(dirs: Dirs, log: Arc<Log>, link: Link, max_watched: usize) -> Self {
        Self {
            plugins: Runner::start(dirs.clone(), Arc::clone(&log)),
            dirs,
            log,
            registry: Mutex::new(Registry::default()),
            settled: Condvar::new(),
            max_watched,
            link,
            unrestored: Mutex::new(BTreeMap::new()),
            started: Instant::now(),
            listing: Mutex::default(),
        }
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lost(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.unrestored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn enrolled_file(&self) -> EnrolledFile {
        let text = std::fs::read(self.dirs.enrolled_file()).unwrap_or_default();
        serde_json::from_slice(&text).unwrap_or_default()
    }

    fn save_enrolled(&self) -> Result<()> {
        let (mut roots, repos, declined): (Vec<String>, Vec<String>, Vec<String>) = {
            let registry = self.registry();
            (
                registry
                    .worktrees
                    .values()
                    .filter(|h| h.via.is_none())
                    .map(|h| h.root.to_string_lossy().into_owned())
                    .collect(),
                registry
                    .repos
                    .iter()
                    .filter(|(key, _)| !registry.listed.contains_key(*key))
                    .map(|(_, d)| d.to_string_lossy().into_owned())
                    .collect(),
                registry
                    .declined
                    .values()
                    .map(|d| d.to_string_lossy().into_owned())
                    .collect(),
            )
        };
        roots.extend(self.lost().values().cloned());
        let file = self.dirs.enrolled_file();
        std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))?;
        let tmp = file.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(&EnrolledFile {
                roots,
                repos,
                declined,
            })?,
        )?;
        std::fs::rename(&tmp, &file).context("saving the enrolled list")
    }

    fn writer_for(&self, project: &ProjectKey) -> Result<Writer> {
        if let Some(writer) = self.registry().writers.get(project) {
            return Ok(writer.clone());
        }
        let writer = Writer::spawn(
            self.dirs.side_repo(project),
            project.clone(),
            self.link.clone(),
        )
        .map_err(|e| anyhow!("{e}"))?;
        let writer = self
            .registry()
            .writers
            .entry(project.clone())
            .or_insert(writer)
            .clone();
        self.link.project(project, &writer);
        Ok(writer)
    }

    /// Starts watching `root` and waits for its first sync. Returns the file count and whether it was already enrolled.
    /// `via` is the key of the enrolled repository that brought the worktree in; none when the person
    /// enrolled this worktree. A second caller for a worktree that is being started waits for the first,
    /// so two hooks for a new worktree both land.
    fn enroll(&self, root: &Path, via: Option<String>) -> Result<(ProjectKey, usize, bool)> {
        let key = path_key(root);
        {
            let mut registry = self.registry();
            loop {
                if let Some(handle) = registry.worktrees.get_mut(&key) {
                    let files = handle.status.lock().map(|s| s.files).unwrap_or(0);
                    let project = handle.project.clone();
                    // Enrolling a worktree by itself makes it permanent, whatever brought it in.
                    let promoted = via.is_none() && handle.via.take().is_some();
                    drop(registry);
                    if promoted {
                        self.save_enrolled()?;
                    }
                    return Ok((project, files, true));
                }
                if registry.starting.insert(key.clone()) {
                    break;
                }
                registry = self
                    .settled
                    .wait(registry)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        // A panic must still clear `starting`, or every caller waiting on this worktree would wait forever.
        let started =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.start_worktree(root)))
                .unwrap_or_else(|_| Err(anyhow!("starting {} panicked", root.display())));
        let mut registry = self.registry();
        registry.starting.remove(&key);
        self.settled.notify_all();
        let (mut handle, ready) = started?;
        let explicit = via.is_none();
        handle.via = via;
        registry.worktrees.insert(key.clone(), handle);
        self.evict_over_cap(&mut registry, &key);
        drop(registry);
        if explicit {
            self.save_enrolled()?;
        }
        Ok((ready.project, ready.files, false))
    }

    fn start_worktree(&self, root: &Path) -> Result<(Handle, task::Ready)> {
        let project = project_key(root, &RepoConfig::load(root).map_err(|e| anyhow!("{e}"))?)
            .map_err(|e| anyhow!("{e}"))?;
        let writer = self.writer_for(&project)?;
        task::spawn(
            root.to_owned(),
            self.dirs.clone(),
            writer,
            Arc::clone(&self.log),
            self.plugins.clone(),
        )
        .map_err(|e| anyhow!("{e}"))
    }

    /// Whether the worktree was enrolled, and whether a repository brought it in (and so would again).
    fn unenroll(&self, root: &Path) -> Result<(bool, bool)> {
        self.lost().remove(&path_key(root));
        let removed = self.registry().worktrees.remove(&path_key(root));
        if let Some(handle) = &removed {
            let _ = handle.tx.send(Cmd::Stop);
        }
        self.save_enrolled()?;
        Ok((
            removed.is_some(),
            removed.is_some_and(|handle| handle.via.is_some()),
        ))
    }

    /// The task of the enrolled worktree that contains `cwd`; the innermost one when they nest.
    fn route(&self, cwd: &Path) -> Route {
        // A cwd spelled through a junction or symlink names the same directory as the enrolled root.
        let cwd = cwd
            .canonicalize()
            .map_or_else(|_| simplified(cwd), |real| simplified(&real));
        let key = path_key(&cwd);
        let (found, root) = {
            let registry = self.registry();
            let Some((found, handle)) = registry
                .worktrees
                .iter()
                .filter(|(root, _)| {
                    key == **root
                        || key
                            .strip_prefix(root.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                })
                .max_by_key(|(root, _)| root.len())
            else {
                return Route::Unenrolled;
            };
            (found.clone(), handle.root.clone())
        };
        let root_key = path_key(&root);
        for dir in cwd.ancestors() {
            if path_key(dir) == root_key {
                break;
            }
            if dir.join(".git").exists() {
                return Route::NestedRepo(dir.to_owned());
            }
        }
        Route::Task(found)
    }

    /// The worktree task for `cwd`. A cwd that no enrolled worktree owns may still sit in a worktree of an
    /// enrolled repository, which is enrolled now: the baseline runs before this returns.
    fn resolve(&self, cwd: &Path) -> std::result::Result<String, Undelivered> {
        let miss = match self.route(cwd) {
            Route::Task(key) => return Ok(key),
            Route::NestedRepo(repo) => Undelivered::Nested(repo),
            Route::Unenrolled => Undelivered::Unenrolled,
        };
        match self.enroll_by_repo(cwd) {
            Ok(Some(key)) => Ok(key),
            Ok(None) => Err(miss),
            Err(e) => Err(Undelivered::Failed(format!("{e:#}"))),
        }
    }

    /// Queues a turn on the worktree task `cwd` belongs to. The send happens under the registry lock, and
    /// so does an eviction's `Stop`, so a turn is never queued behind a stop and dropped.
    fn deliver(&self, cwd: &Path, mut job: TurnJob) -> std::result::Result<(), Undelivered> {
        for _ in 0..3 {
            let key = self.resolve(cwd)?;
            let mut registry = self.registry();
            let Some(handle) = registry.worktrees.get_mut(&key) else {
                continue;
            };
            handle.used = Instant::now();
            match handle.tx.send(Cmd::Turn(job)) {
                Ok(()) => return Ok(()),
                Err(std::sync::mpsc::SendError(cmd)) => {
                    registry.worktrees.remove(&key);
                    let Cmd::Turn(back) = cmd else {
                        return Err(Undelivered::Failed("the worktree task stopped".into()));
                    };
                    job = back;
                }
            }
        }
        Err(Undelivered::Failed(
            "the worktree task stopped while the turn was being queued".into(),
        ))
    }

    /// A hook frame: fire and forget. The work is on its own thread, since a command adapter runs a
    /// process and the caller is an async handler. A turn from a directory that is not enrolled is
    /// dropped, which is what makes enrollment the allowlist.
    fn on_hook(self: &Arc<Self>, agent: &str, payload: &[u8]) {
        let (this, agent, payload) = (Arc::clone(self), agent.to_owned(), payload.to_vec());
        let spawned = std::thread::Builder::new()
            .name("baton-hook".into())
            .spawn(move || this.hook_turn(&agent, payload));
        if let Err(e) = spawned {
            self.log.line(&format!("dropped a hook frame: {e}"));
        }
    }

    /// The turn request in a hook payload: from the user's `[agents.<name>]` command when one is
    /// configured, else from the built-in adapter. A command that prints `null` says the payload was
    /// not the end of a turn.
    fn adapt(&self, agent: &str, payload: Vec<u8>) -> Option<TurnReq> {
        let command = match Plugins::load(&self.dirs.user_config()) {
            Ok(mut plugins) => plugins.agents.remove(agent),
            Err(e) => {
                self.log.line(&format!(
                    "plug-in config unreadable, using built-in adapters: {e}"
                ));
                None
            }
        };
        let Some(spec) = command else {
            let request = crate::adapter::parse(agent, &payload);
            if request.is_none() {
                self.log
                    .line(&format!("dropped a {agent} hook frame that did not parse"));
            }
            return request;
        };
        match run_command_bytes::<Option<TurnReq>>(&spec, payload) {
            Ok(Some(request)) => Some(request),
            Ok(None) => None,
            Err(e) => {
                self.log.line(&format!(
                    "the {agent} adapter command failed, dropped the hook frame: {e}"
                ));
                None
            }
        }
    }

    fn hook_turn(&self, agent: &str, payload: Vec<u8>) {
        let Some(request) = self.adapt(agent, payload) else {
            return;
        };
        let TurnReq {
            cwd,
            thread,
            subject,
            agent,
            model,
            checks,
        } = request;
        let job = TurnJob {
            args: TurnInput {
                thread,
                agent,
                model,
                message: subject,
                checks,
            },
            reply: None,
        };
        match self.deliver(&cwd, job) {
            Ok(()) => {}
            Err(Undelivered::Nested(repo)) => self.log.line(&format!(
                "ignored a turn from {}: it is inside the nested repo {}, which is not enrolled",
                cwd.display(),
                repo.display()
            )),
            Err(Undelivered::Unenrolled) => self.log.line(&format!(
                "ignored a turn from {}: not an enrolled worktree",
                cwd.display()
            )),
            Err(Undelivered::Failed(why)) => self
                .log
                .line(&format!("dropped a turn from {}: {why}", cwd.display())),
        }
    }

    async fn on_control(self: &Arc<Self>, request: Request) -> Reply {
        let this = Arc::clone(self);
        let outcome = match request {
            Request::Ping => Ok(Reply::Pong {
                pid: std::process::id(),
            }),
            Request::Status => Ok(this.status()),
            Request::Enroll { path } => {
                run_blocking(move || {
                    let root = find_root(Path::new(&path)).map_err(|e| anyhow!("{e}"))?;
                    let (project, files, already) = this.enroll(&root, None)?;
                    Ok(Reply::Enrolled {
                        root: root.to_string_lossy().into_owned(),
                        project: project.to_string(),
                        files,
                        already,
                    })
                })
                .await
            }
            Request::Unenroll { path } => {
                run_blocking(move || {
                    let root = find_root(Path::new(&path)).map_err(|e| anyhow!("{e}"))?;
                    let (was_enrolled, via_repo) = this.unenroll(&root)?;
                    Ok(Reply::Unenrolled {
                        root: root.to_string_lossy().into_owned(),
                        was_enrolled,
                        via_repo,
                    })
                })
                .await
            }
            Request::EnrollRepo { path } => {
                run_blocking(move || this.enroll_repo(Path::new(&path))).await
            }
            Request::UnenrollRepo { path } => {
                run_blocking(move || this.unenroll_repo(Path::new(&path))).await
            }
            Request::Turn(args) => this.turn(args).await,
        };
        outcome.unwrap_or_else(|e| Reply::Failed {
            message: format!("{e:#}"),
        })
    }

    async fn turn(self: &Arc<Self>, args: crate::control::TurnArgs) -> Result<Reply> {
        let input = TurnInput::from_args(&args).map_err(|e| anyhow!("{e}"))?;
        let (reply, answer) = tokio::sync::oneshot::channel();
        let job = TurnJob {
            args: input,
            reply: Some(reply),
        };
        let this = Arc::clone(self);
        let cwd = args.cwd.clone();
        // Delivery can enroll a worktree of an enrolled repository, which is a baseline.
        let delivered = run_blocking(move || Ok(this.deliver(Path::new(&cwd), job))).await?;
        match delivered {
            Ok(()) => {}
            Err(Undelivered::Nested(repo)) => {
                return Err(anyhow!(
                    "{} is inside the nested repo {}, which is not enrolled; run `baton enroll` there",
                    args.cwd,
                    repo.display()
                ));
            }
            Err(Undelivered::Unenrolled) => {
                return Err(anyhow!(
                    "{} is not inside an enrolled worktree; run `baton enroll` there",
                    args.cwd
                ));
            }
            Err(Undelivered::Failed(why)) => return Err(anyhow!("{}: {why}", args.cwd)),
        }
        let done = answer
            .await
            .map_err(|_| anyhow!("the worktree task stopped before finishing the turn"))?
            .map_err(|e| anyhow!("{e}"))?;
        Ok(Reply::Turned {
            thread: done.thread,
            seq: done.seq,
            id: done.id,
            changed: done.changed,
        })
    }

    fn status(&self) -> Reply {
        let projects = self
            .listed_projects()
            .iter()
            .map(ToString::to_string)
            .collect();
        let registry = self.registry();
        let worktrees = registry
            .worktrees
            .values()
            .map(|h| {
                let snap = h.status.lock().map(|s| s.clone()).unwrap_or_default();
                WorktreeStatus {
                    root: h.root.to_string_lossy().into_owned(),
                    project: h.project.to_string(),
                    files: snap.files,
                    tree: snap.tree,
                    repo: h
                        .via
                        .as_ref()
                        .and_then(|key| registry.repos.get(key))
                        .map(|dir| dir.to_string_lossy().into_owned()),
                }
            })
            .collect();
        let repos = registry
            .repos
            .iter()
            .map(|(key, dir)| RepoStatus {
                dir: dir.to_string_lossy().into_owned(),
                listed: registry.listed.get(key).map(ToString::to_string),
                active: registry
                    .worktrees
                    .values()
                    .filter(|h| h.via.as_ref() == Some(key))
                    .count(),
            })
            .collect();
        let opted_out = registry
            .declined
            .values()
            .map(|dir| dir.to_string_lossy().into_owned())
            .collect();
        Reply::Status {
            pid: std::process::id(),
            up_secs: self.started.elapsed().as_secs(),
            worktrees,
            repos,
            cap: self.max_watched,
            projects,
            opted_out,
        }
    }
}

async fn run_blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow!("{e}"))?
}

/// `baton daemon`: take the single-instance lock, restore the enrolled worktrees, then serve.
pub fn run() -> Result<()> {
    let dirs = Dirs::resolve().map_err(|e| anyhow!("{e}"))?;
    std::fs::create_dir_all(dirs.data())?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dirs.lock_file())?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let log = Arc::new(Log::open(&dirs.log_file())?);
    log.line(&format!("daemon starting pid={}", std::process::id()));
    let user = UserConfig::load(&dirs.user_config()).map_err(|e| anyhow!("{e}"))?;
    let machine = user.machine.clone().unwrap_or_else(MachineId::from_host);
    let (link, link_rx) = Link::channel();
    let daemon = Arc::new(Daemon::new(
        dirs,
        Arc::clone(&log),
        link,
        user.max_repo_worktrees,
    ));

    let enrolled = daemon.enrolled_file();
    daemon.restore_repos(&enrolled.repos, &enrolled.declined);
    let restoring: Vec<_> = enrolled
        .roots
        .into_iter()
        .map(|root| {
            let daemon = Arc::clone(&daemon);
            std::thread::spawn(move || {
                if let Err(e) = daemon.enroll(Path::new(&root), None) {
                    daemon.log.line(&format!("could not restore {root}: {e:#}"));
                    daemon.lost().insert(path_key(Path::new(&root)), root);
                }
            })
        })
        .collect();
    for thread in restoring {
        let _ = thread.join();
    }

    let reaper = Arc::clone(&daemon);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(repos::REAP_EVERY);
            reaper.reap();
        }
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.spawn(link::run(link_rx, user.server, machine, Arc::clone(&log)));
    let result = runtime.block_on(server::serve(Arc::clone(&daemon)));
    if let Err(e) = &result {
        log.line(&format!("daemon stopped: {e:#}"));
    }
    drop(lock);
    result
}
