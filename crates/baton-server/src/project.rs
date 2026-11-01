//! One bare repo per project, written by one thread. Everything that touches the repo's objects or refs
//! goes through that thread's job queue, so two sessions can never race on a ref or on the order
//! objects are stored in.

use crate::db::{Db, IndexRow};
use crate::log::log;
use anyhow::{Context, Result};
use baton_core::{
    Head, INLINE_MAX, MachineId, Meta, ObjectKind, Oid, Pool, ProjectKey, RefuseReason, SideRepo,
    Stored, ThreadId, TipMove, TurnId, Verdict, plan_ingest, store_ingest,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, oneshot};

const EVENT_QUEUE: usize = 256;

/// A ref that moved, for every other session subscribed to the project.
pub struct Event {
    pub project: ProjectKey,
    pub thread: ThreadId,
    pub old: Option<TurnId>,
    pub new: TurnId,
    pub meta: Meta,
    /// The new objects, empty when they total more than `INLINE_MAX`.
    pub objects: Vec<(ObjectKind, Oid, Arc<[u8]>)>,
    pub origin: u64,
}

pub enum Outcome {
    Advanced,
    Need(Vec<Oid>),
    Refused {
        reason: RefuseReason,
        current: Option<TurnId>,
        detail: Option<String>,
    },
}

pub struct Advance {
    pub machine: MachineId,
    pub thread: ThreadId,
    pub expected: Option<TurnId>,
    pub new: TurnId,
    pub pool: Pool,
    pub origin: u64,
}

enum Job {
    Advance(Advance, oneshot::Sender<Result<Outcome>>),
    Heads(oneshot::Sender<Result<Vec<Head>>>),
    /// Reads objects until `budget` bytes are gathered and reports how many ids it consumed.
    Read(Vec<Oid>, usize, oneshot::Sender<Result<(Stored, usize)>>),
}

pub struct Project {
    key: ProjectKey,
    dir: PathBuf,
    db: Arc<Db>,
    events: broadcast::Sender<Arc<Event>>,
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
}

impl Project {
    fn new(key: ProjectKey, dir: PathBuf, db: Arc<Db>) -> Self {
        Self {
            key,
            dir,
            db,
            events: broadcast::channel(EVENT_QUEUE).0,
            jobs: Mutex::new(None),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.events.subscribe()
    }

    fn submit(&self, job: Job) -> Result<()> {
        let mut slot = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let mut job = job;
        for _ in 0..2 {
            let tx = match slot.as_ref() {
                Some(tx) => tx.clone(),
                None => {
                    let (tx, rx) = mpsc::channel();
                    let worker = Worker {
                        key: self.key.clone(),
                        dir: self.dir.clone(),
                        db: self.db.clone(),
                        events: self.events.clone(),
                    };
                    std::thread::Builder::new()
                        .name(format!("project-{}", self.key))
                        .spawn(move || worker.run(rx))
                        .context("starting the project writer")?;
                    *slot = Some(tx.clone());
                    tx
                }
            };
            match tx.send(job) {
                Ok(()) => return Ok(()),
                Err(mpsc::SendError(back)) => {
                    job = back;
                    *slot = None;
                }
            }
        }
        anyhow::bail!("the writer of {} keeps stopping", self.key)
    }

    pub async fn advance(&self, request: Advance) -> Result<Outcome> {
        let (tx, rx) = oneshot::channel();
        self.submit(Job::Advance(request, tx))?;
        rx.await.context("the project writer stopped")?
    }

    fn exists(&self) -> bool {
        self.dir.join("HEAD").is_file()
    }

    pub async fn heads(&self) -> Result<Vec<Head>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let (tx, rx) = oneshot::channel();
        self.submit(Job::Heads(tx))?;
        rx.await.context("the project writer stopped")?
    }

    pub async fn read(&self, ids: Vec<Oid>, budget: usize) -> Result<(Stored, usize)> {
        if !self.exists() {
            return Ok((Vec::new(), ids.len()));
        }
        let (tx, rx) = oneshot::channel();
        self.submit(Job::Read(ids, budget, tx))?;
        rx.await.context("the project writer stopped")?
    }
}

struct Worker {
    key: ProjectKey,
    dir: PathBuf,
    db: Arc<Db>,
    events: broadcast::Sender<Arc<Event>>,
}

impl Worker {
    fn run(self, jobs: mpsc::Receiver<Job>) {
        let repo = match SideRepo::open_or_init(&self.dir) {
            Ok(repo) => repo,
            Err(e) => {
                log(&format!(
                    "project {}: cannot open {}: {e}",
                    self.key,
                    self.dir.display()
                ));
                return;
            }
        };
        for job in jobs {
            let survived = match job {
                Job::Advance(request, reply) => {
                    let thread = request.thread.clone();
                    let result = self.guarded(|| self.advance(&repo, request));
                    let survived = result.is_some();
                    let outcome = result.unwrap_or_else(|| {
                        Ok(Outcome::Refused {
                            reason: RefuseReason::Invalid,
                            current: None,
                            detail: Some(format!("the server failed while checking {thread}")),
                        })
                    });
                    let _ = reply.send(outcome);
                    survived
                }
                Job::Heads(reply) => match self.guarded(|| self.heads(&repo)) {
                    Some(heads) => reply.send(heads).is_ok() || true,
                    None => false,
                },
                Job::Read(ids, budget, reply) => {
                    match self.guarded(|| repo.read_batch(&ids, budget).map_err(Into::into)) {
                        Some(read) => reply.send(read).is_ok() || true,
                        None => false,
                    }
                }
            };
            if !survived {
                // The repo handle may hold half-updated state, so this writer stops; the next job starts a new one.
                return;
            }
        }
    }

    /// Runs one job. A panic is logged and reported as `None`, so one project's bad input cannot take the
    /// process down; a stack overflow cannot be caught, which is why every walk of a received tree is
    /// iterative and depth-capped.
    fn guarded<T>(&self, job: impl FnOnce() -> T) -> Option<T> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
            Ok(value) => Some(value),
            Err(panic) => {
                let what = panic
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                log(&format!(
                    "project {}: the writer panicked: {what}",
                    self.key
                ));
                None
            }
        }
    }

    fn heads(&self, repo: &SideRepo) -> Result<Vec<Head>> {
        let mut out = Vec::new();
        for (thread, tip) in repo.tips()? {
            match repo.read_turn(tip) {
                Ok(record) => out.push(Head {
                    project: self.key.clone(),
                    thread,
                    tip,
                    owner: record.turn.draft.machine,
                }),
                Err(e) => log(&format!("project {} thread {thread}: {e}", self.key)),
            }
        }
        Ok(out)
    }

    fn advance(&self, repo: &SideRepo, request: Advance) -> Result<Outcome> {
        let Advance {
            machine,
            thread,
            expected,
            new,
            pool,
            origin,
        } = request;
        let refuse = |reason, current, detail: Option<String>| Outcome::Refused {
            reason,
            current,
            detail,
        };

        let tip = repo.tip(&thread)?;
        if let Some(tip) = tip
            && repo.read_turn(tip)?.turn.draft.machine != machine
        {
            return Ok(refuse(RefuseReason::NotOwner, Some(tip), None));
        }
        if tip != expected {
            return Ok(refuse(RefuseReason::Stale, tip, None));
        }
        if expected == Some(new) {
            return Ok(Outcome::Advanced);
        }

        let intake = match plan_ingest(repo, &pool, new, expected)? {
            Verdict::Missing(ids) => return Ok(Outcome::Need(ids)),
            Verdict::Invalid(why) => return Ok(refuse(RefuseReason::Invalid, tip, Some(why))),
            Verdict::Ready(intake) => intake,
        };

        let mut previous = match expected {
            Some(tip) => Some(repo.read_turn(tip)?.turn.seq),
            None => None,
        };
        for record in &intake.chain {
            let draft = &record.turn.draft;
            if draft.machine != machine {
                let why = format!("turn {} names machine {}", record.id.short(), draft.machine);
                return Ok(refuse(RefuseReason::MachineMismatch, tip, Some(why)));
            }
            let expected_seq = match previous {
                Some(seq) => seq.next()?,
                None => baton_core::TurnSeq::first(),
            };
            if draft.thread != thread || record.turn.seq != expected_seq {
                let why = format!(
                    "turn {} does not continue thread {thread}",
                    record.id.short()
                );
                return Ok(refuse(RefuseReason::Invalid, tip, Some(why)));
            }
            previous = Some(record.turn.seq);
        }

        store_ingest(repo, &pool, &intake)?;
        match repo.move_tip(&thread, expected, new)? {
            TipMove::Stale(current) => return Ok(refuse(RefuseReason::Stale, current, None)),
            TipMove::Moved => {}
        }

        let last = intake
            .chain
            .last()
            .expect("a new tip has at least one turn");
        let meta = Meta::of(last);
        if let Err(e) = self.db.record_thread(&IndexRow {
            project: self.key.clone(),
            thread: thread.clone(),
            owner: machine,
            tip: new,
            turn: last.turn.seq.get(),
            at: last.at,
        }) {
            log(&format!("project {}: index update failed: {e}", self.key));
        }

        let objects: Vec<_> = intake
            .write
            .iter()
            .filter_map(|(kind, id)| pool.get(id).map(|(_, bytes)| (*kind, *id, bytes.clone())))
            .collect();
        let inline = objects.iter().map(|(_, _, b)| b.len()).sum::<usize>() <= INLINE_MAX;
        let _ = self.events.send(Arc::new(Event {
            project: self.key.clone(),
            thread,
            old: expected,
            new,
            meta,
            objects: if inline { objects } else { Vec::new() },
            origin,
        }));
        Ok(Outcome::Advanced)
    }
}

/// The projects a server knows, each created on first use.
pub struct Projects {
    data: PathBuf,
    db: Arc<Db>,
    map: Mutex<HashMap<ProjectKey, Arc<Project>>>,
}

impl Projects {
    pub fn new(data: PathBuf, db: Arc<Db>) -> Self {
        Self {
            data,
            db,
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &ProjectKey) -> Arc<Project> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(key.clone())
            .or_insert_with(|| {
                let dir = self.data.join("projects").join(key.dir_name());
                Arc::new(Project::new(key.clone(), dir, self.db.clone()))
            })
            .clone()
    }
}
