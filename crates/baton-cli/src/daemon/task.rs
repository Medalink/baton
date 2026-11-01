//! One enrolled worktree: a watcher, the dirty set, and the worktree's current tree, all owned by one
//! thread. It talks to its project's writer by message and to nobody else.

use super::log::Log;
use super::writer::Writer;
use crate::control::TurnArgs;
use baton_core::{
    AgentName, Check, ChunkLimits, Dirs, DirtySet, Error, ModelName, ProjectKey, Result, Subject,
    ThreadId, TurnDraft, UnixMillis, Worktree,
};
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel, sync_channel};
use std::time::{Duration, Instant};

/// A turn waits for the watcher to go quiet this long, but never longer than `MAX_WAIT`, so a worktree
/// that is being written constantly still gets its turn.
const QUIET: Duration = Duration::from_millis(5);
const MAX_WAIT: Duration = Duration::from_millis(30);

pub enum Cmd {
    Event(notify::Result<notify::Event>),
    Turn(TurnJob),
    Stop,
}

pub struct TurnJob {
    pub args: TurnInput,
    pub reply: Option<tokio::sync::oneshot::Sender<std::result::Result<Done, String>>>,
}

/// A turn request after the adapter or the command line has parsed it. Free text is still raw: it is
/// redacted with the worktree's policy before it becomes a typed value.
pub struct TurnInput {
    pub thread: ThreadId,
    pub agent: AgentName,
    pub model: Option<String>,
    pub message: String,
    pub checks: Vec<Check>,
}

impl TurnInput {
    pub fn from_args(args: &TurnArgs) -> Result<Self> {
        Ok(Self {
            thread: ThreadId::parse(&args.thread)?,
            agent: AgentName::parse(&args.agent)?,
            model: args.model.clone(),
            message: args.message.clone(),
            checks: args
                .checks
                .iter()
                .map(|c| Check::parse(c))
                .collect::<Result<_>>()?,
        })
    }
}

pub struct Done {
    pub thread: String,
    pub seq: u32,
    pub id: String,
    pub changed: usize,
}

pub struct Ready {
    pub project: ProjectKey,
    pub files: usize,
}

pub struct Handle {
    pub root: PathBuf,
    pub project: ProjectKey,
    pub tx: Sender<Cmd>,
    pub status: Arc<std::sync::Mutex<Snapshot>>,
}

/// What `status` shows, updated by the task after each sync.
#[derive(Default, Clone)]
pub struct Snapshot {
    pub files: usize,
    pub tree: String,
}

struct Task {
    wt: Worktree,
    writer: Writer,
    log: Arc<Log>,
    label: String,
    dirty: DirtySet,
    ignored_dirs: BTreeSet<String>,
    rx: Receiver<Cmd>,
    backlog: VecDeque<Cmd>,
    status: Arc<std::sync::Mutex<Snapshot>>,
}

/// Starts the task and waits until its first sync is done, so an enroll answer means the baseline exists.
pub fn spawn(root: PathBuf, dirs: Dirs, writer: Writer, log: Arc<Log>) -> Result<(Handle, Ready)> {
    let (tx, rx) = channel::<Cmd>();
    let (ready_tx, ready_rx) = sync_channel::<Result<Ready>>(1);
    let status = Arc::new(std::sync::Mutex::new(Snapshot::default()));
    let thread_status = Arc::clone(&status);
    let event_tx = tx.clone();
    let thread_root = root.clone();
    std::thread::Builder::new()
        .name("baton-worktree".into())
        .spawn(move || {
            let started = start(
                &thread_root,
                &dirs,
                writer,
                log,
                rx,
                event_tx,
                thread_status,
            );
            match started {
                Ok((mut task, watcher, ready)) => {
                    let _ = ready_tx.send(Ok(ready));
                    task.run();
                    drop(watcher);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| Error::Refused(format!("starting the worktree thread: {e}")))?;
    let ready = ready_rx
        .recv()
        .map_err(|_| Error::Refused("the worktree thread stopped while starting".into()))??;
    let handle = Handle {
        root,
        project: ready.project.clone(),
        tx,
        status,
    };
    Ok((handle, ready))
}

fn start(
    root: &Path,
    dirs: &Dirs,
    writer: Writer,
    log: Arc<Log>,
    rx: Receiver<Cmd>,
    event_tx: Sender<Cmd>,
    status: Arc<std::sync::Mutex<Snapshot>>,
) -> Result<(Task, notify::RecommendedWatcher, Ready)> {
    let wt = Worktree::open(root, dirs, &|tree| writer.has_tree(tree))?;
    let mut task = Task {
        label: root.display().to_string(),
        wt,
        writer,
        log,
        dirty: DirtySet::default(),
        ignored_dirs: BTreeSet::new(),
        rx,
        backlog: VecDeque::new(),
        status,
    };
    // The watcher starts before the first scan so a change made during the scan is queued, not lost.
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = event_tx.send(Cmd::Event(event));
    })
    .map_err(|e| Error::Refused(format!("starting the watcher: {e}")))?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|e| Error::Refused(format!("watching {}: {e}", root.display())))?;

    let began = Instant::now();
    task.dirty.merge(task.wt.rescan()?);
    let scanned = began.elapsed();
    let (_, changed) = task.sync()?;
    task.save();
    task.log.line(&format!(
        "{} enrolled project={} files={} changed={changed} scan={}ms total={}ms",
        task.label,
        task.wt.project(),
        task.wt.tracked_files(),
        scanned.as_millis(),
        began.elapsed().as_millis()
    ));
    let ready = Ready {
        project: task.wt.project().clone(),
        files: task.wt.tracked_files(),
    };
    Ok((task, watcher, ready))
}

impl Task {
    fn run(&mut self) {
        loop {
            let cmd = match self.backlog.pop_front() {
                Some(cmd) => cmd,
                None => match self.rx.recv() {
                    Ok(cmd) => cmd,
                    Err(_) => return,
                },
            };
            match cmd {
                Cmd::Event(first) => {
                    let mut batch = vec![first];
                    while let Ok(next) = self.rx.try_recv() {
                        match next {
                            Cmd::Event(event) => batch.push(event),
                            other => {
                                self.backlog.push_back(other);
                                break;
                            }
                        }
                    }
                    self.ingest(batch);
                }
                Cmd::Turn(job) => {
                    self.wait_quiet();
                    let outcome = self.take_turn(&job.args);
                    if let Err(e) = &outcome {
                        self.log.line(&format!("{} turn failed: {e}", self.label));
                    }
                    if let Some(reply) = job.reply {
                        let _ = reply.send(outcome.map_err(|e| e.to_string()));
                    }
                }
                Cmd::Stop => return,
            }
        }
    }

    /// Waits until no tracked path has changed for `QUIET`, or `MAX_WAIT` has passed.
    fn wait_quiet(&mut self) {
        let began = Instant::now();
        let mut last_change = began;
        loop {
            let now = Instant::now();
            let (quiet_for, waited) = (now - last_change, now - began);
            if quiet_for >= QUIET || waited >= MAX_WAIT {
                return;
            }
            match self
                .rx
                .recv_timeout((QUIET - quiet_for).min(MAX_WAIT - waited))
            {
                Ok(Cmd::Event(first)) => {
                    let mut batch = vec![first];
                    while let Ok(next) = self.rx.try_recv() {
                        match next {
                            Cmd::Event(event) => batch.push(event),
                            other => self.backlog.push_back(other),
                        }
                    }
                    if self.ingest(batch) > 0 {
                        last_change = Instant::now();
                    }
                }
                Ok(other) => self.backlog.push_back(other),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Records which tracked paths a batch of watcher events touched. Returns how many it recorded.
    fn ingest(&mut self, batch: Vec<notify::Result<notify::Event>>) -> usize {
        let mut seen: Vec<(String, bool)> = Vec::new();
        for event in batch {
            let event = match event {
                Ok(event) if event.need_rescan() => {
                    self.dirty.overflow();
                    continue;
                }
                Ok(event) => event,
                Err(e) => {
                    self.log
                        .line(&format!("{} watcher error, rescanning: {e}", self.label));
                    self.dirty.overflow();
                    continue;
                }
            };
            let structural = matches!(
                event.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(notify::event::ModifyKind::Name(_))
            );
            if matches!(event.kind, EventKind::Access(_)) {
                continue;
            }
            for path in &event.paths {
                if let Some(rel) = self.relative(path) {
                    seen.push((rel, structural));
                }
            }
        }
        if seen.is_empty() {
            return 0;
        }

        let mut candidates: Vec<(String, bool)> = Vec::new();
        for (rel, structural) in seen {
            if rel == ".git/info/exclude" || rel == ".baton/config.toml" {
                self.ignored_dirs.clear();
                self.dirty.overflow();
            } else if rel.ends_with(".gitignore") {
                self.ignored_dirs.clear();
                let dir = rel.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned();
                if dir.is_empty() {
                    self.dirty.overflow();
                } else {
                    self.dirty.mark(dir, true);
                }
            }
            let first = rel.split('/').next().unwrap_or("");
            let below_ignored = self.ignored_dirs.iter().any(|d| {
                rel.len() > d.len()
                    && rel.starts_with(d.as_str())
                    && rel.as_bytes()[d.len()] == b'/'
            });
            if first.eq_ignore_ascii_case(".git") || below_ignored {
                continue;
            }
            candidates.push((rel, structural));
        }
        if candidates.is_empty() {
            return 0;
        }
        let asked: Vec<(&str, bool)> = candidates
            .iter()
            .map(|(p, _)| (p.as_str(), false))
            .collect();
        let verdicts = match self.wt.classify(&asked) {
            Ok(v) => v,
            Err(e) => {
                self.log.line(&format!(
                    "{} classifying events failed, rescanning: {e}",
                    self.label
                ));
                self.dirty.overflow();
                return 0;
            }
        };
        let mut recorded = 0;
        for ((rel, structural), keep) in candidates.into_iter().zip(verdicts) {
            if keep {
                self.dirty.mark(rel, structural);
                recorded += 1;
            } else if let Ok(Some(dir)) = self.wt.ignored_ancestor(&rel) {
                self.ignored_dirs.insert(dir);
            }
        }
        recorded
    }

    fn relative(&self, path: &Path) -> Option<String> {
        let rel = match path.strip_prefix(self.wt.root()) {
            Ok(rel) => rel.to_owned(),
            Err(_) if cfg!(windows) => {
                let (root, full) = (self.wt.root().to_string_lossy(), path.to_string_lossy());
                let root = root.replace('/', "\\").to_lowercase();
                let full_norm = full.replace('/', "\\");
                if full_norm.to_lowercase().starts_with(&root) {
                    PathBuf::from(full_norm[root.len()..].trim_start_matches('\\'))
                } else {
                    return None;
                }
            }
            Err(_) => return None,
        };
        let text = rel.to_str()?.replace('\\', "/");
        (!text.is_empty()).then_some(text)
    }

    /// Reads every dirty path and stores what changed. Returns the tree and the number of changed paths.
    /// Chunked, so a first sync of a large worktree never holds more than one chunk's bytes in memory.
    fn sync(&mut self) -> Result<(baton_core::TreeId, usize)> {
        let mut tree = self.wt.tree();
        let mut changed = 0;
        loop {
            let mut plan = self.wt.prepare(&mut self.dirty, ChunkLimits::default())?;
            if plan.paths() == 0 {
                return Ok((tree, changed));
            }
            let (blobs, edits) = (
                std::mem::take(&mut plan.blobs),
                std::mem::take(&mut plan.edits),
            );
            changed += edits.len();
            let next = if blobs.is_empty() && edits.is_empty() {
                Ok(tree)
            } else {
                self.writer.apply(tree, blobs, edits).map(|(t, _)| t)
            };
            match next {
                Ok(next) => {
                    self.wt.accept(plan, next);
                    tree = next;
                }
                Err(e) => {
                    Worktree::abandon(plan, &mut self.dirty);
                    return Err(e);
                }
            }
        }
    }

    fn save(&self) {
        if let Err(e) = self.wt.save_state() {
            self.log
                .line(&format!("{} saving state failed: {e}", self.label));
        }
        if let Ok(mut status) = self.status.lock() {
            *status = Snapshot {
                files: self.wt.tracked_files(),
                tree: self.wt.tree().to_string(),
            };
        }
    }

    fn take_turn(&mut self, input: &TurnInput) -> Result<Done> {
        let began = Instant::now();
        let draft = self.draft(input)?;
        let (tree, changed) = self.sync()?;
        let synced = began.elapsed();
        let (record, stage) = self.writer.commit(tree, draft, UnixMillis::now())?;
        let done = began.elapsed();
        self.save();
        let saved = began.elapsed() - done;
        self.log.line(&format!(
            "{} turn {}@{} {} changed={changed} sync={:.1}ms commit={:.1}ms ref={:.1}ms total={:.1}ms save={:.1}ms",
            self.label,
            record.turn.draft.thread,
            record.turn.seq,
            record.id.short(),
            ms(synced),
            ms(stage.commit),
            ms(stage.reference),
            ms(done),
            ms(saved)
        ));
        Ok(Done {
            thread: record.turn.draft.thread.to_string(),
            seq: record.turn.seq.get(),
            id: record.id.to_string(),
            changed,
        })
    }

    /// Everything that is not a file is redacted too: the subject and the model text. An id (thread, agent)
    /// that would change under redaction is refused, because a marker is not a valid id.
    fn draft(&self, input: &TurnInput) -> Result<TurnDraft> {
        let policy = self.wt.policy();
        for id in [input.thread.as_str(), input.agent.as_str()] {
            if policy.redact_text(id) != id {
                return Err(Error::Refused("a thread or agent id holds a secret".into()));
            }
        }
        let model = match input.model.as_deref().map(|m| policy.redact_text(m)) {
            Some(text) => Some(ModelName::parse(&text)?),
            None => None,
        };
        let skipped = self
            .wt
            .skipped()
            .into_iter()
            .filter(|p| policy.redact_text(p.as_str()) == p.as_str())
            .collect();
        Ok(TurnDraft {
            thread: input.thread.clone(),
            agent: input.agent.clone(),
            model,
            machine: self.wt.settings().machine.clone(),
            base: self.wt.head(),
            checks: input.checks.clone(),
            skipped,
            subject: Subject::new(&policy.redact_text(&input.message)),
        })
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
