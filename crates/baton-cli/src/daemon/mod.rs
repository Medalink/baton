//! The per-user daemon: the enrolled worktrees, one writer per project, and the endpoint.
//!
//! Ownership: a worktree task owns its dirty set, mirror and tree; a writer owns its side repo. The
//! registry here is only a routing table (cwd to task, project to writer) behind a short lock.

mod link;
mod log;
mod plugins;
mod server;
mod task;
mod writer;

use crate::control::{Reply, Request, WorktreeStatus};
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
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use task::{Cmd, Handle, TurnInput, TurnJob};
use writer::Writer;

#[derive(Default, Serialize, Deserialize)]
struct EnrolledFile {
    roots: Vec<String>,
}

#[derive(Default)]
struct Registry {
    worktrees: BTreeMap<String, Handle>,
    writers: HashMap<ProjectKey, Writer>,
    starting: HashSet<String>,
}

pub struct Daemon {
    dirs: Dirs,
    log: Arc<Log>,
    registry: Mutex<Registry>,
    link: Link,
    plugins: Runner,
    /// Enrolled roots that could not be started (a drive that is not mounted). They stay enrolled on disk.
    unrestored: Mutex<BTreeMap<String, String>>,
    started: Instant,
}

enum Route {
    Task(std::sync::mpsc::Sender<Cmd>),
    /// The cwd sits in a git repo nested inside an enrolled worktree. That repo is not the project's, and
    /// the outer worktree does not track it.
    NestedRepo(std::path::PathBuf),
    Unenrolled,
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
    fn new(dirs: Dirs, log: Arc<Log>, link: Link) -> Self {
        Self {
            plugins: Runner::start(dirs.clone(), Arc::clone(&log)),
            dirs,
            log,
            registry: Mutex::new(Registry::default()),
            link,
            unrestored: Mutex::new(BTreeMap::new()),
            started: Instant::now(),
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

    fn enrolled_roots(&self) -> Vec<String> {
        let text = std::fs::read(self.dirs.enrolled_file()).unwrap_or_default();
        serde_json::from_slice::<EnrolledFile>(&text)
            .map(|f| f.roots)
            .unwrap_or_default()
    }

    fn save_enrolled(&self) -> Result<()> {
        let mut roots: Vec<String> = self
            .registry()
            .worktrees
            .values()
            .map(|h| h.root.to_string_lossy().into_owned())
            .collect();
        roots.extend(self.lost().values().cloned());
        let file = self.dirs.enrolled_file();
        std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))?;
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&EnrolledFile { roots })?)?;
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
    fn enroll(&self, root: &Path) -> Result<(ProjectKey, usize, bool)> {
        let key = path_key(root);
        {
            let mut registry = self.registry();
            if let Some(handle) = registry.worktrees.get(&key) {
                let files = handle.status.lock().map(|s| s.files).unwrap_or(0);
                return Ok((handle.project.clone(), files, true));
            }
            if !registry.starting.insert(key.clone()) {
                return Err(anyhow!("{} is already being enrolled", root.display()));
            }
        }
        let started = self.start_worktree(root);
        self.registry().starting.remove(&key);
        let (handle, ready) = started?;
        self.registry().worktrees.insert(key, handle);
        self.save_enrolled()?;
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

    fn unenroll(&self, root: &Path) -> Result<bool> {
        self.lost().remove(&path_key(root));
        let removed = self.registry().worktrees.remove(&path_key(root));
        if let Some(handle) = &removed {
            let _ = handle.tx.send(Cmd::Stop);
        }
        self.save_enrolled()?;
        Ok(removed.is_some())
    }

    /// The task of the enrolled worktree that contains `cwd`; the innermost one when they nest.
    fn route(&self, cwd: &Path) -> Route {
        // A cwd spelled through a junction or symlink names the same directory as the enrolled root.
        let cwd = cwd
            .canonicalize()
            .map_or_else(|_| simplified(cwd), |real| simplified(&real));
        let key = path_key(&cwd);
        let (root, tx) = {
            let registry = self.registry();
            let Some((_, handle)) = registry
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
            (handle.root.clone(), handle.tx.clone())
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
        Route::Task(tx)
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
        let tx = match self.route(&cwd) {
            Route::Task(tx) => tx,
            Route::NestedRepo(repo) => {
                self.log.line(&format!(
                    "ignored a turn from {}: it is inside the nested repo {}, which is not enrolled",
                    cwd.display(),
                    repo.display()
                ));
                return;
            }
            Route::Unenrolled => {
                self.log.line(&format!(
                    "ignored a turn from {}: not an enrolled worktree",
                    cwd.display()
                ));
                return;
            }
        };
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
        let _ = tx.send(Cmd::Turn(job));
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
                    let (project, files, already) = this.enroll(&root)?;
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
                    let was_enrolled = this.unenroll(&root)?;
                    Ok(Reply::Unenrolled {
                        root: root.to_string_lossy().into_owned(),
                        was_enrolled,
                    })
                })
                .await
            }
            Request::Turn(args) => this.turn(args).await,
        };
        outcome.unwrap_or_else(|e| Reply::Failed {
            message: format!("{e:#}"),
        })
    }

    async fn turn(&self, args: crate::control::TurnArgs) -> Result<Reply> {
        let input = TurnInput::from_args(&args).map_err(|e| anyhow!("{e}"))?;
        let tx = match self.route(Path::new(&args.cwd)) {
            Route::Task(tx) => tx,
            Route::NestedRepo(repo) => {
                return Err(anyhow!(
                    "{} is inside the nested repo {}, which is not enrolled; run `baton enroll` there",
                    args.cwd,
                    repo.display()
                ));
            }
            Route::Unenrolled => {
                return Err(anyhow!(
                    "{} is not inside an enrolled worktree; run `baton enroll` there",
                    args.cwd
                ));
            }
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        tx.send(Cmd::Turn(TurnJob {
            args: input,
            reply: Some(reply),
        }))
        .map_err(|_| anyhow!("the worktree task has stopped"))?;
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
                }
            })
            .collect();
        Reply::Status {
            pid: std::process::id(),
            up_secs: self.started.elapsed().as_secs(),
            worktrees,
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
    let daemon = Arc::new(Daemon::new(dirs, Arc::clone(&log), link));

    let restoring: Vec<_> = daemon
        .enrolled_roots()
        .into_iter()
        .map(|root| {
            let daemon = Arc::clone(&daemon);
            std::thread::spawn(move || {
                if let Err(e) = daemon.enroll(Path::new(&root)) {
                    daemon.log.line(&format!("could not restore {root}: {e:#}"));
                    daemon.lost().insert(path_key(Path::new(&root)), root);
                }
            })
        })
        .collect();
    for thread in restoring {
        let _ = thread.join();
    }

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
