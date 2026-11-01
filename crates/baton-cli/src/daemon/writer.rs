//! The only owner of a project's side repo. Every object and ref write for the project goes through
//! this thread, one job at a time, which is what the loose-write measurements require.

use baton_core::{
    BlobPayload, Error, Result, SideRepo, StageTimes, TreeEdit, TreeId, TurnDraft, TurnRecord,
    UnixMillis,
};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, SyncSender, channel, sync_channel};
use std::time::{Duration, Instant};

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
}

#[derive(Clone)]
pub struct Writer {
    tx: Sender<Job>,
}

fn gone() -> Error {
    Error::Refused("the side repo writer has stopped".into())
}

impl Writer {
    /// Starts the writer thread and returns once the repo is open (or has failed to open).
    pub fn spawn(dir: PathBuf) -> Result<Self> {
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
                            let _ = reply.send(repo.commit_turn(tree, &draft, at));
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
}
