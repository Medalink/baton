//! One connection's worth of sync: pushing the turns this machine made and following the turns other
//! machines moved.
//!
//! A task owns the read half of the socket and this loop owns the write half, so a full send buffer on
//! either side cannot stop the other side from being read. The loop handles one event at a time, so
//! the objects of a push and its `advance` are always contiguous on the wire, which is what the
//! server's per-connection object pool needs.

use super::connect::Ws;
use super::{Cmd, End, Projects};
use crate::daemon::log::Log;
use crate::daemon::writer::{Landed, Writer};
use anyhow::{Context, Result, anyhow};
use baton_core::{
    Assembler, Control, Frame, Head, MAX_OBJECT, MachineId, Object, Oid, PROTO, Pool, ProjectKey,
    RefuseReason, Stored, ThreadId, TurnId, decode, encode_control, encode_object,
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::{interval, timeout};
use tokio_tungstenite::tungstenite::{Bytes, Message};

/// Bytes read from a side repo per batch while sending.
const BATCH: usize = 4 << 20;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT: Duration = Duration::from_secs(15);
const SILENCE: Duration = Duration::from_secs(45);
const POOL_MAX: usize = 512 << 20;
/// Times the server may ask for more objects for one `advance`.
const MAX_NEEDS: u8 = 3;
/// Times a push may be retried after `stale` before the thread is left alone until its next turn.
const MAX_STALE: u8 = 2;
/// Candidate trees to diff a thread's first turn against.
const BASES: usize = 8;

type Key = (ProjectKey, ThreadId);

enum In {
    Control(Control),
    Object(Object),
    Beat,
    Closed(String),
}

struct Flight {
    key: Key,
    new: TurnId,
    known: Option<TurnId>,
    needs: u8,
    stale: u8,
}

struct Follow {
    new: TurnId,
    asked: Vec<Oid>,
}

struct Session<'a> {
    sink: SplitSink<Ws, Message>,
    inbox: UnboundedReceiver<In>,
    projects: &'a mut Projects,
    log: &'a Log,
    me: MachineId,
    /// What the server holds, from `welcome`, `heads`, `advanced` and `moved`.
    heads: HashMap<Key, (TurnId, MachineId)>,
    flights: HashMap<u32, Flight>,
    busy: HashMap<Key, u32>,
    again: BTreeSet<Key>,
    stale: HashMap<Key, u8>,
    /// Tips the server refused for good, so they are not offered again until the thread moves on.
    blocked: HashMap<Key, TurnId>,
    follows: HashMap<Key, Follow>,
    /// The threads of the `want`s sent, oldest first. The server answers them in order, and its
    /// `objects_sent` carries its own transfer number, so order is the only thing that pairs them.
    waiting: VecDeque<Key>,
    pool: Pool,
    unsynced: BTreeSet<ProjectKey>,
    xfer: u32,
}

async fn read_loop(mut stream: SplitStream<Ws>, tx: UnboundedSender<In>) {
    let mut assembler = Assembler::new(MAX_OBJECT);
    let closed = loop {
        let Some(message) = stream.next().await else {
            break "the server closed the connection".to_owned();
        };
        let event = match message {
            Ok(Message::Binary(bytes)) => match decode(&bytes) {
                Ok(Frame::Control(control)) => In::Control(control),
                Ok(Frame::Object(chunk)) => match assembler.push(chunk) {
                    Ok(Some(object)) => In::Object(object),
                    Ok(None) => continue,
                    Err(e) => break e.to_string(),
                },
                Err(e) => break e.to_string(),
            },
            Ok(Message::Ping(_) | Message::Pong(_)) => In::Beat,
            Ok(Message::Close(_)) => break "the server closed the connection".to_owned(),
            Ok(Message::Text(_)) => break "the server sent text".to_owned(),
            Ok(_) => continue,
            Err(e) => break e.to_string(),
        };
        if tx.send(event).is_err() {
            return;
        }
    };
    let _ = tx.send(In::Closed(closed));
}

pub async fn run(
    ws: Ws,
    machine: &MachineId,
    rx: &mut UnboundedReceiver<Cmd>,
    projects: &mut Projects,
    log: &Log,
) -> End {
    let (sink, stream) = ws.split();
    let (tx, inbox) = unbounded_channel();
    let reader = tokio::spawn(read_loop(stream, tx));
    let mut session = Session {
        sink,
        inbox,
        projects,
        log,
        me: machine.clone(),
        heads: HashMap::new(),
        flights: HashMap::new(),
        busy: HashMap::new(),
        again: BTreeSet::new(),
        stale: HashMap::new(),
        blocked: HashMap::new(),
        follows: HashMap::new(),
        waiting: VecDeque::new(),
        pool: Pool::default(),
        unsynced: BTreeSet::new(),
        xfer: 1,
    };
    let end = match session.serve(machine, rx).await {
        Ok(end) => end,
        Err(e) => End::Lost(format!("{e:#}")),
    };
    reader.abort();
    let _ = session.sink.close().await;
    end
}

impl Session<'_> {
    fn next_xfer(&mut self) -> u32 {
        self.xfer = self.xfer.wrapping_add(2);
        self.xfer
    }

    async fn send_frames(&mut self, frames: Vec<Vec<u8>>) -> Result<()> {
        for frame in frames {
            self.sink
                .feed(Message::Binary(Bytes::from(frame)))
                .await
                .context("sending")?;
        }
        self.sink.flush().await.context("sending")
    }

    async fn send(&mut self, message: &Control) -> Result<()> {
        self.send_frames(vec![encode_control(message)]).await
    }

    async fn send_objects(&mut self, xfer: u32, objects: &Stored) -> Result<()> {
        let mut frames = Vec::new();
        for (kind, id, bytes) in objects {
            frames.extend(encode_object(xfer, *kind, *id, bytes));
        }
        self.send_frames(frames).await
    }

    async fn serve(&mut self, machine: &MachineId, rx: &mut UnboundedReceiver<Cmd>) -> Result<End> {
        let hello = Control::Hello {
            proto: PROTO,
            machine: machine.to_string(),
            projects: self.projects.keys().cloned().collect(),
            client: format!("baton {}", env!("CARGO_PKG_VERSION")),
        };
        self.send(&hello).await?;
        let first = timeout(HELLO_TIMEOUT, self.inbox.recv())
            .await
            .map_err(|_| anyhow!("the server did not answer hello"))?;
        match first {
            Some(In::Control(Control::Welcome { machine, heads, .. })) => {
                self.me = machine;
                self.on_heads(heads);
            }
            Some(In::Control(Control::Error { code, message })) => {
                return Err(anyhow!("the server said {code}: {message}"));
            }
            Some(In::Closed(why)) => return Err(anyhow!("{why}")),
            _ => return Err(anyhow!("the server did not start with welcome")),
        }
        self.log.line(&format!(
            "sync: connected as {} with {} projects",
            self.me,
            self.projects.len()
        ));

        let mut beat = interval(HEARTBEAT);
        beat.tick().await;
        let mut heard = Instant::now();
        loop {
            self.flush_syncs().await?;
            tokio::select! {
                event = self.inbox.recv() => {
                    heard = Instant::now();
                    let Some(event) = event else { return Err(anyhow!("the reader stopped")) };
                    self.on_event(event).await?;
                    while let Ok(event) = self.inbox.try_recv() {
                        self.on_event(event).await?;
                    }
                }
                cmd = rx.recv() => match cmd {
                    None => return Ok(End::Shutdown),
                    Some(Cmd::Project(key, writer)) => {
                        if self.projects.insert(key.clone(), writer).is_none() {
                            self.send(&Control::Subscribe { project: key }).await?;
                        }
                    }
                    Some(Cmd::Dirty(project, thread)) => {
                        self.push(&(project, thread)).await?;
                        while let Ok(cmd) = rx.try_recv() {
                            match cmd {
                                Cmd::Dirty(project, thread) => self.push(&(project, thread)).await?,
                                Cmd::Project(key, writer) => {
                                    if self.projects.insert(key.clone(), writer).is_none() {
                                        self.send(&Control::Subscribe { project: key }).await?;
                                    }
                                }
                            }
                        }
                    }
                },
                _ = beat.tick() => {
                    if heard.elapsed() > SILENCE {
                        return Err(anyhow!("the server has been silent for {SILENCE:?}"));
                    }
                    self.sink.send(Message::Ping(Bytes::new())).await.context("sending")?;
                }
            }
        }
    }

    async fn on_event(&mut self, event: In) -> Result<()> {
        match event {
            In::Beat => Ok(()),
            In::Closed(why) => Err(anyhow!("{why}")),
            In::Object(object) => {
                self.pool.insert(object);
                if self.pool.bytes() > POOL_MAX {
                    return Err(anyhow!("too many unfinished objects from the server"));
                }
                Ok(())
            }
            In::Control(control) => self.on_control(control).await,
        }
    }

    fn on_heads(&mut self, heads: Vec<Head>) {
        for head in heads {
            if !self.projects.contains_key(&head.project) {
                continue;
            }
            self.unsynced.insert(head.project.clone());
            self.heads
                .insert((head.project, head.thread), (head.tip, head.owner));
        }
    }

    async fn on_control(&mut self, control: Control) -> Result<()> {
        match control {
            Control::Heads { heads } => {
                self.on_heads(heads);
                Ok(())
            }
            Control::Advanced { xfer, new, .. } => self.on_advanced(xfer, new).await,
            Control::Need { xfer, ids, .. } => self.on_need(xfer, ids).await,
            Control::Refused {
                xfer,
                reason,
                current,
                detail,
                ..
            } => self.on_refused(xfer, reason, current, detail).await,
            Control::Moved {
                project,
                thread,
                new,
                meta,
                ..
            } => {
                if !self.projects.contains_key(&project) {
                    return Ok(());
                }
                let key = (project, thread);
                self.heads.insert(key.clone(), (new, meta.machine.clone()));
                if meta.machine == self.me {
                    return Ok(());
                }
                self.follow(key, new).await
            }
            Control::ObjectsSent { .. } => {
                let Some(key) = self.waiting.pop_front() else {
                    return Ok(());
                };
                // A later answer for the same thread is still coming; judge after the last one.
                if self.waiting.contains(&key) {
                    return Ok(());
                }
                match self.follows.get(&key).map(|f| f.new) {
                    Some(new) => self.follow(key, new).await,
                    None => Ok(()),
                }
            }
            Control::Error { code, message } => {
                self.log
                    .line(&format!("sync: the server said {code}: {message}"));
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn writer(&self, project: &ProjectKey) -> Option<Writer> {
        self.projects.get(project).cloned()
    }

    async fn advance(
        &mut self,
        xfer: u32,
        key: &Key,
        expected: Option<TurnId>,
        new: TurnId,
    ) -> Result<()> {
        self.send(&Control::Advance {
            xfer,
            project: key.0.clone(),
            thread: key.1.clone(),
            expected,
            new,
        })
        .await
    }

    /// Sends the turns of `key` the server lacks. At most one push per thread is in flight; a turn that
    /// lands meanwhile is picked up when the answer arrives.
    async fn push(&mut self, key: &Key) -> Result<()> {
        if self.busy.contains_key(key) {
            self.again.insert(key.clone());
            return Ok(());
        }
        let Some(writer) = self.writer(&key.0) else {
            return Ok(());
        };
        let head = self.heads.get(key).cloned();
        if let Some((_, owner)) = head.as_ref().filter(|(_, owner)| owner != &self.me) {
            self.log.line(&format!(
                "sync: {} belongs to {owner}; its turns recorded here stay on this machine",
                key.1
            ));
            return Ok(());
        }
        let known = head.map(|(tip, _)| tip);
        let bases = if known.is_none() {
            self.bases(&key.0)
        } else {
            Vec::new()
        };
        let began = Instant::now();
        let outgoing = match writer.outgoing(key.1.clone(), known, bases, BATCH).await {
            Ok(Some(outgoing)) => outgoing,
            Ok(None) => return Ok(()),
            Err(e) => {
                self.log
                    .line(&format!("sync: cannot read the turns of {}: {e}", key.1));
                return Ok(());
            }
        };
        if self.blocked.get(key) == Some(&outgoing.tip) {
            return Ok(());
        }
        let xfer = self.next_xfer();
        let mut count = outgoing.objects.len();
        self.send_objects(xfer, &outgoing.objects).await?;
        let mut rest = outgoing.rest;
        while !rest.is_empty() {
            let (objects, used) = match writer.read(rest.clone(), BATCH).await {
                Ok(read) => read,
                Err(e) => {
                    self.log
                        .line(&format!("sync: cannot read objects of {}: {e}", key.1));
                    return Ok(());
                }
            };
            count += objects.len();
            self.send_objects(xfer, &objects).await?;
            rest.drain(..used.max(1).min(rest.len()));
        }
        self.advance(xfer, key, known, outgoing.tip).await?;
        self.log.line(&format!(
            "push {} {} sent objects={count} read={:.1}ms",
            key.1,
            outgoing.tip.short(),
            began.elapsed().as_secs_f64() * 1e3
        ));
        self.busy.insert(key.clone(), xfer);
        self.flights.insert(
            xfer,
            Flight {
                key: key.clone(),
                new: outgoing.tip,
                known,
                needs: 0,
                stale: self.stale.get(key).copied().unwrap_or(0),
            },
        );
        Ok(())
    }

    /// Turns the server holds in this project, ours first, to diff a new thread's first turn against.
    fn bases(&self, project: &ProjectKey) -> Vec<TurnId> {
        let mut found: Vec<(bool, TurnId)> = self
            .heads
            .iter()
            .filter(|(key, _)| &key.0 == project)
            .map(|(_, (tip, owner))| (owner != &self.me, *tip))
            .collect();
        found.sort();
        found.into_iter().take(BASES).map(|(_, tip)| tip).collect()
    }

    fn finish(&mut self, xfer: u32) -> Option<Flight> {
        let flight = self.flights.remove(&xfer)?;
        self.busy.remove(&flight.key);
        Some(flight)
    }

    async fn on_advanced(&mut self, xfer: u32, new: TurnId) -> Result<()> {
        let Some(flight) = self.finish(xfer) else {
            return Ok(());
        };
        self.log
            .line(&format!("push {} {} acked", flight.key.1, new.short()));
        self.heads
            .insert(flight.key.clone(), (new, self.me.clone()));
        self.blocked.remove(&flight.key);
        self.stale.remove(&flight.key);
        if self.again.remove(&flight.key) {
            self.push(&flight.key).await?;
        }
        Ok(())
    }

    async fn on_need(&mut self, xfer: u32, ids: Vec<Oid>) -> Result<()> {
        let Some(flight) = self.flights.get_mut(&xfer) else {
            return Ok(());
        };
        flight.needs += 1;
        let (key, known, new, needs) = (flight.key.clone(), flight.known, flight.new, flight.needs);
        if needs > MAX_NEEDS {
            self.finish(xfer);
            self.blocked.insert(key.clone(), new);
            self.log.line(&format!(
                "sync: the server keeps asking for objects of {}; giving up on {}",
                key.1,
                new.short()
            ));
            return Ok(());
        }
        let Some(writer) = self.writer(&key.0) else {
            return Ok(());
        };
        let objects_xfer = self.next_xfer();
        let mut rest = ids;
        while !rest.is_empty() {
            let (objects, used) = match writer.read(rest.clone(), BATCH).await {
                Ok(read) => read,
                Err(e) => {
                    self.finish(xfer);
                    self.log
                        .line(&format!("sync: cannot read objects of {}: {e}", key.1));
                    return Ok(());
                }
            };
            self.send_objects(objects_xfer, &objects).await?;
            rest.drain(..used.max(1).min(rest.len()));
        }
        self.advance(xfer, &key, known, new).await
    }

    async fn on_refused(
        &mut self,
        xfer: u32,
        reason: RefuseReason,
        current: Option<TurnId>,
        detail: Option<String>,
    ) -> Result<()> {
        let Some(flight) = self.finish(xfer) else {
            return Ok(());
        };
        let key = flight.key.clone();
        match reason {
            // The answer to an earlier attempt was lost; the server already holds this turn.
            RefuseReason::Stale if current == Some(flight.new) => {
                self.heads
                    .insert(key.clone(), (flight.new, self.me.clone()));
                self.stale.remove(&key);
                self.push(&key).await
            }
            RefuseReason::Stale if flight.stale < MAX_STALE => {
                self.stale.insert(key.clone(), flight.stale + 1);
                match current {
                    Some(tip) => {
                        self.heads.insert(key.clone(), (tip, self.me.clone()));
                    }
                    None => {
                        self.heads.remove(&key);
                    }
                }
                self.push(&key).await
            }
            reason => {
                self.blocked.insert(key.clone(), flight.new);
                self.log.line(&format!(
                    "sync: the server refused {} of {} ({reason:?}){}",
                    flight.new.short(),
                    key.1,
                    detail.map_or_else(String::new, |d| format!(": {d}"))
                ));
                Ok(())
            }
        }
    }

    /// Brings a project in line with the server's heads: pushes our threads that are ahead, follows the
    /// others that are.
    async fn flush_syncs(&mut self) -> Result<()> {
        for project in std::mem::take(&mut self.unsynced) {
            self.sync_project(&project).await?;
        }
        Ok(())
    }

    async fn sync_project(&mut self, project: &ProjectKey) -> Result<()> {
        let Some(writer) = self.writer(project) else {
            return Ok(());
        };
        let tips = match writer.tips().await {
            Ok(tips) => tips,
            Err(e) => {
                self.log
                    .line(&format!("sync: cannot list the threads of {project}: {e}"));
                return Ok(());
            }
        };
        let local: HashMap<&ThreadId, _> = tips.iter().map(|t| (&t.thread, t)).collect();
        let theirs: Vec<(ThreadId, TurnId, MachineId)> = self
            .heads
            .iter()
            .filter(|(key, _)| &key.0 == project)
            .map(|(key, (tip, owner))| (key.1.clone(), *tip, owner.clone()))
            .collect();
        for (thread, tip, owner) in &theirs {
            let key = (project.clone(), thread.clone());
            match local.get(thread) {
                Some(mine) if mine.tip == *tip => {}
                Some(_) if *owner == self.me => self.push(&key).await?,
                _ => self.follow(key, *tip).await?,
            }
        }
        for mine in &tips {
            let known = theirs.iter().any(|(thread, ..)| *thread == mine.thread);
            if mine.owner == self.me && !known {
                self.push(&(project.clone(), mine.thread.clone())).await?;
            }
        }
        Ok(())
    }

    /// Stores the turns up to `new` in the local side repo and moves the thread's ref. What the server
    /// did not send inline is asked for with `want`, one round per missing layer.
    async fn follow(&mut self, key: Key, new: TurnId) -> Result<()> {
        let Some(writer) = self.writer(&key.0) else {
            return Ok(());
        };
        let began = Instant::now();
        match writer.ingest(key.1.clone(), new, self.pool.clone()).await {
            Ok(Landed::Moved(record)) => {
                self.follows.remove(&key);
                self.log.line(&format!(
                    "follow {} {} from {} stored seq={} in {:.1}ms",
                    key.1,
                    new.short(),
                    record.turn.draft.machine,
                    record.turn.seq,
                    began.elapsed().as_secs_f64() * 1e3
                ));
            }
            Ok(Landed::Current) => {
                self.follows.remove(&key);
            }
            Ok(Landed::Missing(ids)) => {
                let repeated = self
                    .follows
                    .get(&key)
                    .is_some_and(|f| f.new == new && f.asked == ids);
                if repeated {
                    self.follows.remove(&key);
                    self.log.line(&format!(
                        "sync: the server could not send {} objects of {}",
                        ids.len(),
                        key.1
                    ));
                } else {
                    let xfer = self.next_xfer();
                    self.send(&Control::Want {
                        xfer,
                        project: key.0.clone(),
                        ids: ids.clone(),
                    })
                    .await?;
                    self.waiting.push_back(key.clone());
                    self.follows.insert(key, Follow { new, asked: ids });
                }
            }
            Ok(Landed::Rejected(why)) => {
                self.follows.remove(&key);
                self.log
                    .line(&format!("sync: not following {}: {why}", key.1));
            }
            Err(e) => {
                self.follows.remove(&key);
                self.log.line(&format!(
                    "sync: cannot store {} of {}: {e}",
                    new.short(),
                    key.1
                ));
            }
        }
        if self.follows.is_empty() {
            self.pool = Pool::default();
        }
        Ok(())
    }
}
