//! One connected machine. The session owns its socket: every frame in and out passes through this loop,
//! so nothing else can interleave bytes into it.

use crate::Shared;
use crate::auth::Identity;
use crate::log::log;
use crate::project::{Advance, Event, Outcome, Project};
use anyhow::{Result, bail};
use baton_core::{
    Assembler, Control, Frame, Head, IDS_PER_MESSAGE, Limits, MAX_FRAME, MAX_OBJECT, PROTO, Pool,
    ProjectKey, decode, encode_control, encode_object,
};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{interval, timeout};
use tokio_stream::StreamMap;
use tokio_stream::wrappers::BroadcastStream;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Bytes, Message};

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT: Duration = Duration::from_secs(15);
const SILENCE: Duration = Duration::from_secs(60);
/// A client that stops reading must not hold a session (and its broadcast queue) forever.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// The goodbye (an error and a close frame) to a client that may have stopped reading.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a live session checks that its device is still enrolled. A revoked device is cut off
/// within this long, or at its next request or forwarded event, whichever comes first.
const REVOKE_CHECK: Duration = Duration::from_secs(2);
/// Projects one session may name in total, across hello, subscribe, advance and want.
const MAX_PROJECTS: usize = 256;
const HEADS_PER_MESSAGE: usize = 1000;
const POOL_MAX: usize = 512 << 20;
const READ_BUDGET: usize = 8 << 20;

/// A protocol violation. The client is told the code and then disconnected.
struct Violation {
    code: &'static str,
    message: String,
}

fn violation(code: &'static str, message: impl ToString) -> Violation {
    Violation {
        code,
        message: message.to_string(),
    }
}

impl From<anyhow::Error> for Violation {
    fn from(e: anyhow::Error) -> Self {
        violation("internal", format!("{e:#}"))
    }
}

impl From<baton_core::Error> for Violation {
    fn from(e: baton_core::Error) -> Self {
        violation("invalid_message", e)
    }
}

type Socket<S> = Pin<Box<S>>;

pub struct Session<S> {
    ws: Socket<S>,
    id: u64,
    identity: Identity,
    shared: Arc<Shared>,
    subs: StreamMap<ProjectKey, BroadcastStream<Arc<Event>>>,
    named: HashSet<ProjectKey>,
    pool: Pool,
    assembler: Assembler,
    xfer: u32,
    said_hello: bool,
}

impl<S> Session<S>
where
    S: Stream<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Sink<Message, Error = tokio_tungstenite::tungstenite::Error>,
{
    pub fn new(ws: S, id: u64, identity: Identity, shared: Arc<Shared>) -> Self {
        Self {
            ws: Box::pin(ws),
            id,
            identity,
            shared,
            subs: StreamMap::new(),
            named: HashSet::new(),
            pool: Pool::default(),
            assembler: Assembler::new(MAX_OBJECT),
            xfer: 0,
            said_hello: false,
        }
    }

    fn next_xfer(&mut self) -> u32 {
        self.xfer = self.xfer.wrapping_add(2);
        self.xfer
    }

    async fn send_frames(&mut self, frames: Vec<Vec<u8>>) -> Result<()> {
        let ws = &mut self.ws;
        timeout(SEND_TIMEOUT, async {
            for frame in frames {
                ws.feed(Message::Binary(Bytes::from(frame))).await?;
            }
            ws.flush().await
        })
        .await
        .map_err(|_| anyhow::anyhow!("the client did not read for {SEND_TIMEOUT:?}"))??;
        Ok(())
    }

    async fn send(&mut self, message: &Control) -> Result<()> {
        self.send_frames(vec![encode_control(message)]).await
    }

    pub async fn run(mut self) {
        let machine = self.identity.machine.clone();
        let outcome = self.serve().await;
        if let Err(v) = &outcome {
            log(&format!(
                "session {} ({machine}): {}: {}",
                self.id, v.code, v.message
            ));
        }
        let goodbye = async {
            if let Err(v) = &outcome {
                let error = Control::Error {
                    code: v.code.into(),
                    message: v.message.clone(),
                };
                let _ = self
                    .ws
                    .feed(Message::Binary(Bytes::from(encode_control(&error))))
                    .await;
            }
            let _ = self
                .ws
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "".into(),
                })))
                .await;
        };
        let _ = timeout(CLOSE_TIMEOUT, goodbye).await;
        log(&format!("session {} ({machine}): closed", self.id));
    }

    async fn serve(&mut self) -> std::result::Result<(), Violation> {
        let first = timeout(HELLO_TIMEOUT, self.ws.next()).await;
        let Ok(Some(Ok(Message::Binary(bytes)))) = first else {
            return Err(violation("protocol", "the first message must be hello"));
        };
        self.on_binary(&bytes).await?;
        if !self.said_hello {
            return Err(violation("protocol", "the first message must be hello"));
        }

        let mut beat = interval(HEARTBEAT);
        beat.tick().await;
        let mut enrolled = interval(REVOKE_CHECK);
        enrolled.tick().await;
        let mut heard = Instant::now();
        loop {
            tokio::select! {
                incoming = self.ws.next() => {
                    let Some(incoming) = incoming else { return Ok(()) };
                    let message = incoming.map_err(|e| violation("protocol", e))?;
                    heard = Instant::now();
                    match message {
                        Message::Binary(bytes) => {
                            self.on_binary(&bytes).await?;
                        }
                        Message::Close(_) => return Ok(()),
                        Message::Text(_) => {
                            return Err(violation("protocol", "text messages are not part of baton.v1"));
                        }
                        _ => {}
                    }
                }
                Some((project, event)) = self.subs.next() => {
                    let Ok(event) = event else {
                        log(&format!("session {}: {project} fell behind; closing so it can resync", self.id));
                        return Ok(());
                    };
                    if event.origin != self.id {
                        self.ensure_enrolled()?;
                        self.forward(&event).await.map_err(Violation::from)?;
                    }
                }
                _ = enrolled.tick() => self.ensure_enrolled()?,
                _ = beat.tick() => {
                    if heard.elapsed() > SILENCE {
                        log(&format!("session {}: silent for {SILENCE:?}", self.id));
                        return Ok(());
                    }
                    let ws = &mut self.ws;
                    timeout(SEND_TIMEOUT, ws.send(Message::Ping(Bytes::new())))
                        .await
                        .map_err(|_| violation("protocol", "the client did not read a ping"))?
                        .map_err(|e| violation("protocol", e))?;
                }
            }
        }
    }

    fn ensure_enrolled(&self) -> std::result::Result<(), Violation> {
        let active = match self.identity.device {
            Some(id) => self.shared.db.device_is_active(id).unwrap_or(false),
            None => true,
        };
        if active {
            Ok(())
        } else {
            Err(violation("unauthorized", "this device was revoked"))
        }
    }

    /// The project's handle, counting it against what this session may name.
    fn project(&mut self, key: &ProjectKey) -> std::result::Result<Arc<Project>, Violation> {
        if !self.named.contains(key) {
            if self.named.len() >= MAX_PROJECTS {
                return Err(violation("too_large", "too many projects"));
            }
            self.named.insert(key.clone());
        }
        Ok(self.shared.projects.get(key))
    }

    async fn on_binary(&mut self, bytes: &[u8]) -> std::result::Result<(), Violation> {
        if bytes.len() > MAX_FRAME {
            return Err(violation("too_large", "frame over the limit"));
        }
        match decode(bytes)? {
            Frame::Object(chunk) => {
                if let Some(object) = self.assembler.push(chunk)? {
                    self.pool.insert(object);
                }
                let held = self.pool.bytes() as u64 + self.assembler.announced();
                if held > POOL_MAX as u64 {
                    return Err(violation("too_large", "too many unfinished objects"));
                }
                Ok(())
            }
            Frame::Control(control) => self.on_control(control).await,
        }
    }

    async fn on_control(&mut self, control: Control) -> std::result::Result<(), Violation> {
        if !self.said_hello && !matches!(control, Control::Hello { .. }) {
            return Err(violation("protocol", "the first message must be hello"));
        }
        match control {
            Control::Hello {
                proto,
                projects,
                machine,
                ..
            } => {
                if self.said_hello {
                    return Err(violation("protocol", "hello twice"));
                }
                if proto != PROTO {
                    return Err(violation(
                        "unsupported_version",
                        format!("this server speaks protocol {PROTO}"),
                    ));
                }
                if projects.len() > MAX_PROJECTS {
                    return Err(violation("too_large", "too many projects"));
                }
                self.said_hello = true;
                let name = &self.identity.machine;
                if !machine.is_empty() && machine != name.as_str() {
                    log(&format!(
                        "session {}: client says it is {machine}; auth says {name}",
                        self.id
                    ));
                }
                let mut heads = Vec::new();
                for project in projects {
                    heads.extend(self.subscribe(&project).await?);
                }
                let mut chunks = heads.chunks(HEADS_PER_MESSAGE).map(<[Head]>::to_vec);
                let welcome = Control::Welcome {
                    proto: PROTO,
                    machine: self.identity.machine.clone(),
                    limits: Limits::default(),
                    heads: chunks.next().unwrap_or_default(),
                };
                let mut frames = vec![encode_control(&welcome)];
                frames.extend(chunks.map(|heads| encode_control(&Control::Heads { heads })));
                self.send_frames(frames).await?;
            }
            Control::Subscribe { project } => {
                let heads = self.subscribe(&project).await?;
                let frames = heads
                    .chunks(HEADS_PER_MESSAGE)
                    .map(|c| encode_control(&Control::Heads { heads: c.to_vec() }))
                    .collect();
                self.send_frames(frames).await?;
            }
            Control::Advance {
                xfer,
                project,
                thread,
                expected,
                new,
            } => {
                self.ensure_enrolled()?;
                let started = Instant::now();
                let outcome = self
                    .project(&project)?
                    .advance(Advance {
                        machine: self.identity.machine.clone(),
                        thread: thread.clone(),
                        expected,
                        new,
                        pool: self.pool.clone(),
                        origin: self.id,
                    })
                    .await?;
                let reply = match outcome {
                    Outcome::Advanced => {
                        self.pool = Pool::default();
                        Control::Advanced {
                            xfer,
                            thread: thread.clone(),
                            new,
                        }
                    }
                    Outcome::Need(ids) => Control::Need {
                        xfer,
                        thread: thread.clone(),
                        ids,
                    },
                    Outcome::Refused {
                        reason,
                        current,
                        detail,
                    } => {
                        self.pool = Pool::default();
                        Control::Refused {
                            xfer,
                            thread: thread.clone(),
                            reason,
                            current,
                            detail,
                        }
                    }
                };
                if self.shared.trace {
                    log(&format!(
                        "advance {thread} {} took {:?}: {}",
                        new.short(),
                        started.elapsed(),
                        match &reply {
                            Control::Advanced { .. } => "advanced".to_owned(),
                            Control::Need { ids, .. } => format!("need {}", ids.len()),
                            Control::Refused { reason, .. } => format!("refused {reason:?}"),
                            _ => String::new(),
                        }
                    ));
                }
                self.send(&reply).await?;
            }
            Control::Want {
                project, mut ids, ..
            } => {
                if ids.len() > IDS_PER_MESSAGE {
                    return Err(violation("too_large", "too many ids in one want"));
                }
                let mut seen = HashSet::new();
                ids.retain(|id| seen.insert(*id));
                self.serve_want(&project, ids).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn subscribe(
        &mut self,
        project: &ProjectKey,
    ) -> std::result::Result<Vec<Head>, Violation> {
        self.ensure_enrolled()?;
        let handle = self.project(project)?;
        if !self.subs.contains_key(project) {
            self.subs
                .insert(project.clone(), BroadcastStream::new(handle.subscribe()));
        }
        Ok(handle.heads().await?)
    }

    async fn serve_want(
        &mut self,
        project: &ProjectKey,
        ids: Vec<baton_core::Oid>,
    ) -> std::result::Result<(), Violation> {
        self.ensure_enrolled()?;
        let handle = self.project(project)?;
        let xfer = self.next_xfer();
        let mut rest = ids.as_slice();
        while !rest.is_empty() {
            let (objects, used) = handle.read(rest.to_vec(), READ_BUDGET).await?;
            let mut frames = Vec::new();
            for (kind, id, bytes) in &objects {
                frames.extend(encode_object(xfer, *kind, *id, bytes));
            }
            self.send_frames(frames).await?;
            rest = &rest[used.max(1).min(rest.len())..];
        }
        self.send(&Control::ObjectsSent { xfer }).await?;
        Ok(())
    }

    async fn forward(&mut self, event: &Event) -> Result<()> {
        let xfer = self.next_xfer();
        let mut frames = Vec::new();
        for (kind, id, bytes) in &event.objects {
            frames.extend(encode_object(xfer, *kind, *id, bytes));
        }
        frames.push(encode_control(&Control::Moved {
            xfer,
            project: event.project.clone(),
            thread: event.thread.clone(),
            old: event.old,
            new: event.new,
            meta: event.meta.clone(),
        }));
        if self.shared.trace {
            log(&format!("moved {} to session {}", event.thread, self.id));
        }
        match self.send_frames(frames).await {
            Ok(()) => Ok(()),
            Err(e) => bail!("sending moved: {e}"),
        }
    }
}
