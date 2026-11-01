//! The only owner of a project's side repo. Every object and ref write for the project goes through
//! this thread, one job at a time, which is what the loose-write measurements require. The link to the
//! server reads and stores through it too, so a followed turn and a local turn never write at once.

use super::link::Link;
use baton_core::{
    BlobPayload, Error, MachineId, Oid, Pool, ProjectKey, Result, SideRepo, StageTimes, Stored,
    ThreadId, TipMove, TreeEdit, TreeId, TurnDraft, TurnId, TurnRecord, UnixMillis, Verdict,
    plan_ingest, store_ingest,
};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, SyncSender, channel, sync_channel};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

type Reply<T> = oneshot::Sender<Result<T>>;

/// A thread's local ref and the machine that made its tip.
#[derive(Clone, Debug)]
pub struct Tip {
    pub thread: ThreadId,
    pub tip: TurnId,
    pub owner: MachineId,
}

/// The first batch of objects of a push, and the ids still to read.
pub struct Outgoing {
    pub tip: TurnId,
    pub objects: Stored,
    pub rest: Vec<Oid>,
}

/// What became of the turns another machine moved.
pub enum Landed {
    Moved(Box<TurnRecord>),
    Current,
    Missing(Vec<Oid>),
    Rejected(String),
}

enum Job {
    HasTree(TreeId, SyncSender<bool>),
    Apply {
        base: TreeId,
        blobs: Vec<BlobPayload>,
        edits: Vec<TreeEdit>,
        reply: SyncSender<Result<(TreeId, Duration)>>,
    },
    Commit {
        tree: TreeId,
        draft: TurnDraft,
        at: UnixMillis,
        reply: SyncSender<Result<(TurnRecord, StageTimes)>>,
    },
    Tips(Reply<Vec<Tip>>),
    Outgoing {
        thread: ThreadId,
        known: Option<TurnId>,
        bases: Vec<TurnId>,
        budget: usize,
        reply: Reply<Option<Outgoing>>,
    },
    Read(Vec<Oid>, usize, Reply<(Stored, usize)>),
    Ingest {
        thread: ThreadId,
        new: TurnId,
        pool: Pool,
        reply: Reply<Landed>,
    },
}

#[derive(Clone)]
pub struct Writer {
    tx: Sender<Job>,
}

fn gone() -> Error {
    Error::Refused("the side repo writer has stopped".into())
}

impl Writer {
    /// Starts the writer thread and returns once the repo is open (or has failed to open). After each
    /// turn it commits, it tells `link` which thread moved.
    pub fn spawn(dir: PathBuf, project: ProjectKey, link: Link) -> Result<Self> {
        let (tx, rx) = channel::<Job>();
        let (ready_tx, ready_rx) = sync_channel(1);
        std::thread::Builder::new()
            .name("baton-writer".into())
            .spawn(move || {
                let repo = match SideRepo::open_or_init(&dir) {
                    Ok(repo) => {
                        let _ = ready_tx.send(Ok(()));
                        repo
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                for job in rx {
                    match job {
                        Job::HasTree(tree, reply) => {
                            let _ = reply.send(repo.has_tree(tree));
                        }
                        Job::Apply {
                            base,
                            blobs,
                            edits,
                            reply,
                        } => {
                            let started = Instant::now();
                            let result = repo
                                .write_blobs(&blobs)
                                .and_then(|()| repo.edit_tree(base, &edits))
                                .map(|tree| (tree, started.elapsed()));
                            let _ = reply.send(result);
                        }
                        Job::Commit {
                            tree,
                            draft,
                            at,
                            reply,
                        } => {
                            let committed = repo.commit_turn(tree, &draft, at);
                            if committed.is_ok() {
                                link.dirty(&project, &draft.thread);
                            }
                            let _ = reply.send(committed);
                        }
                        Job::Tips(reply) => {
                            let _ = reply.send(tips(&repo));
                        }
                        Job::Outgoing {
                            thread,
                            known,
                            bases,
                            budget,
                            reply,
                        } => {
                            let _ = reply.send(outgoing(&repo, &thread, known, &bases, budget));
                        }
                        Job::Read(ids, budget, reply) => {
                            let _ = reply.send(repo.read_batch(&ids, budget));
                        }
                        Job::Ingest {
                            thread,
                            new,
                            pool,
                            reply,
                        } => {
                            let _ = reply.send(ingest(&repo, &thread, new, &pool));
                        }
                    }
                }
            })
            .map_err(|e| Error::Refused(format!("starting the writer thread: {e}")))?;
        ready_rx.recv().map_err(|_| gone())??;
        Ok(Self { tx })
    }

    pub fn has_tree(&self, tree: TreeId) -> bool {
        let (reply, answer) = sync_channel(1);
        self.tx.send(Job::HasTree(tree, reply)).is_ok() && answer.recv().unwrap_or(false)
    }

    /// Stores `blobs`, applies `edits` to `base`, and returns the new tree and how long that took.
    pub fn apply(
        &self,
        base: TreeId,
        blobs: Vec<BlobPayload>,
        edits: Vec<TreeEdit>,
    ) -> Result<(TreeId, Duration)> {
        let (reply, answer) = sync_channel(1);
        self.tx
            .send(Job::Apply {
                base,
                blobs,
                edits,
                reply,
            })
            .map_err(|_| gone())?;
        answer.recv().map_err(|_| gone())?
    }

    pub fn commit(
        &self,
        tree: TreeId,
        draft: TurnDraft,
        at: UnixMillis,
    ) -> Result<(TurnRecord, StageTimes)> {
        let (reply, answer) = sync_channel(1);
        self.tx
            .send(Job::Commit {
                tree,
                draft,
                at,
                reply,
            })
            .map_err(|_| gone())?;
        answer.recv().map_err(|_| gone())?
    }

    async fn ask<T>(&self, job: impl FnOnce(Reply<T>) -> Job) -> Result<T> {
        let (reply, answer) = oneshot::channel();
        self.tx.send(job(reply)).map_err(|_| gone())?;
        answer.await.map_err(|_| gone())?
    }

    pub async fn tips(&self) -> Result<Vec<Tip>> {
        self.ask(Job::Tips).await
    }

    /// The objects of the turns after `known` up to the thread's tip, or `None` when there are none.
    /// `bases` are turns the receiver holds; a thread's first turn is compared with the first one this
    /// repo has, so a new thread sends only what differs from its siblings.
    pub async fn outgoing(
        &self,
        thread: ThreadId,
        known: Option<TurnId>,
        bases: Vec<TurnId>,
        budget: usize,
    ) -> Result<Option<Outgoing>> {
        self.ask(|reply| Job::Outgoing {
            thread,
            known,
            bases,
            budget,
            reply,
        })
        .await
    }

    pub async fn read(&self, ids: Vec<Oid>, budget: usize) -> Result<(Stored, usize)> {
        self.ask(|reply| Job::Read(ids, budget, reply)).await
    }

    pub async fn ingest(&self, thread: ThreadId, new: TurnId, pool: Pool) -> Result<Landed> {
        self.ask(|reply| Job::Ingest {
            thread,
            new,
            pool,
            reply,
        })
        .await
    }
}

fn tips(repo: &SideRepo) -> Result<Vec<Tip>> {
    let mut out = Vec::new();
    for (thread, tip) in repo.tips()? {
        if let Ok(record) = repo.read_turn(tip) {
            out.push(Tip {
                thread,
                tip,
                owner: record.turn.draft.machine,
            });
        }
    }
    Ok(out)
}

fn outgoing(
    repo: &SideRepo,
    thread: &ThreadId,
    known: Option<TurnId>,
    bases: &[TurnId],
    budget: usize,
) -> Result<Option<Outgoing>> {
    let Some(tip) = repo.tip(thread)? else {
        return Ok(None);
    };
    if Some(tip) == known {
        return Ok(None);
    }
    let base_tree = bases
        .iter()
        .find_map(|turn| repo.read_turn(*turn).ok())
        .map(|record| record.tree);
    let ids: Vec<Oid> = repo
        .turn_objects(tip, known, base_tree)?
        .into_iter()
        .map(|(_, id)| id)
        .collect();
    let (objects, used) = repo.read_batch(&ids, budget)?;
    Ok(Some(Outgoing {
        tip,
        objects,
        rest: ids[used..].to_vec(),
    }))
}

fn ingest(repo: &SideRepo, thread: &ThreadId, new: TurnId, pool: &Pool) -> Result<Landed> {
    let local = repo.tip(thread)?;
    if local == Some(new) {
        return Ok(Landed::Current);
    }
    let intake = match plan_ingest(repo, pool, new, local)? {
        Verdict::Missing(ids) => return Ok(Landed::Missing(ids)),
        Verdict::Invalid(why) => return Ok(Landed::Rejected(why)),
        Verdict::Ready(intake) => intake,
    };
    if intake.chain.iter().any(|r| &r.turn.draft.thread != thread) {
        return Ok(Landed::Rejected(format!(
            "a turn in {} belongs to another thread",
            new.short()
        )));
    }
    store_ingest(repo, pool, &intake)?;
    match repo.move_tip(thread, local, new)? {
        TipMove::Stale(_) => Ok(Landed::Rejected(format!(
            "{thread} moved while {} was being stored",
            new.short()
        ))),
        TipMove::Moved => {
            let record = intake
                .chain
                .into_iter()
                .next_back()
                .expect("a new tip has at least one turn");
            Ok(Landed::Moved(Box::new(record)))
        }
    }
}
