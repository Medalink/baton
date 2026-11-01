//! The daemon's link to `baton serve`: one WebSocket for every project on this machine.
//!
//! Ownership: the link task owns the connection and everything about the server. Other threads reach it
//! only through `Link`, an unbounded queue of facts (a project exists, a thread moved), so a slow or
//! absent server never makes a writer or a hook wait.
//!
//! Nothing about what the server has is stored on this machine. After every connect the server's
//! `welcome` lists its heads, and what to push is whatever local refs say that the heads do not.

mod connect;
mod session;

use super::log::Log;
use super::writer::Writer;
use baton_core::{MachineId, ProjectKey, ServerConfig, ThreadId};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_RETRY: Duration = Duration::from_millis(100);
const LAST_RETRY: Duration = Duration::from_secs(3);

pub type Projects = BTreeMap<ProjectKey, Writer>;

pub enum Cmd {
    Project(ProjectKey, Writer),
    Dirty(ProjectKey, ThreadId),
}

/// The sending end, held by the daemon and by each project's writer.
#[derive(Clone)]
pub struct Link {
    tx: UnboundedSender<Cmd>,
}

impl Link {
    pub fn channel() -> (Self, UnboundedReceiver<Cmd>) {
        let (tx, rx) = unbounded_channel();
        (Self { tx }, rx)
    }

    pub fn project(&self, project: &ProjectKey, writer: &Writer) {
        let _ = self.tx.send(Cmd::Project(project.clone(), writer.clone()));
    }

    pub fn dirty(&self, project: &ProjectKey, thread: &ThreadId) {
        let _ = self.tx.send(Cmd::Dirty(project.clone(), thread.clone()));
    }
}

pub enum End {
    Lost(String),
    Shutdown,
}

fn register(projects: &mut Projects, cmd: Cmd) {
    if let Cmd::Project(key, writer) = cmd {
        projects.insert(key, writer);
    }
}

/// Runs until the daemon drops its `Link`. Without a `[server]` section it only drains the queue.
pub async fn run(
    mut rx: UnboundedReceiver<Cmd>,
    server: Option<ServerConfig>,
    machine: MachineId,
    log: Arc<Log>,
) {
    let Some(server) = server else {
        while rx.recv().await.is_some() {}
        return;
    };
    log.line(&format!("sync: server {}", server.url.url()));
    let mut projects = Projects::new();
    let mut wait = FIRST_RETRY;
    let mut last_failure = String::new();
    loop {
        while let Ok(cmd) = rx.try_recv() {
            register(&mut projects, cmd);
        }
        let started = Instant::now();
        let opened = tokio::time::timeout(CONNECT_TIMEOUT, connect::open(&server)).await;
        let end = match opened {
            Ok(Ok(ws)) => session::run(ws, &machine, &mut rx, &mut projects, &log).await,
            Ok(Err(e)) => End::Lost(format!("{e:#}")),
            Err(_) => End::Lost("timed out connecting".into()),
        };
        match end {
            End::Shutdown => return,
            End::Lost(why) => {
                if why != last_failure {
                    log.line(&format!("sync: disconnected: {why}"));
                    last_failure = why;
                }
            }
        }
        if started.elapsed() > LAST_RETRY {
            wait = FIRST_RETRY;
            last_failure.clear();
        }
        let pause = tokio::time::sleep(wait);
        tokio::pin!(pause);
        loop {
            tokio::select! {
                () = &mut pause => break,
                cmd = rx.recv() => match cmd {
                    Some(cmd) => register(&mut projects, cmd),
                    None => return,
                },
            }
        }
        wait = (wait * 2).min(LAST_RETRY);
    }
}
