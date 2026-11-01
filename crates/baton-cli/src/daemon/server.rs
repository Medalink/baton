//! Accepts connections on the endpoint and dispatches the two frame types.

use super::Daemon;
use crate::control::{Reply, Request};
use crate::endpoint::{CONTROL_MAGIC, HOOK_MAGIC, MAX_PAYLOAD};
use anyhow::{Result, anyhow};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

const FRAME_DEADLINE: Duration = Duration::from_secs(2);

enum Frame {
    Hook { agent: String, payload: Vec<u8> },
    Control(Vec<u8>),
}

async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Frame> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header).await?;
    let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if len > MAX_PAYLOAD + 64 {
        return Err(anyhow!("frame of {len} bytes is too large"));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    match &header[..4] {
        magic if magic == HOOK_MAGIC => {
            let split = body
                .iter()
                .position(|b| *b == b'\n')
                .ok_or_else(|| anyhow!("hook frame has no agent name"))?;
            let agent = String::from_utf8(body[..split].to_vec())?;
            Ok(Frame::Hook {
                agent,
                payload: body[split + 1..].to_vec(),
            })
        }
        magic if magic == CONTROL_MAGIC => Ok(Frame::Control(body)),
        _ => Err(anyhow!("unknown frame magic")),
    }
}

async fn handle<S: AsyncRead + AsyncWrite + Unpin>(daemon: Arc<Daemon>, mut stream: S) {
    let Ok(Ok(frame)) = timeout(FRAME_DEADLINE, read_frame(&mut stream)).await else {
        return;
    };
    match frame {
        Frame::Hook { agent, payload } => daemon.on_hook(&agent, &payload),
        Frame::Control(body) => {
            let reply = match serde_json::from_slice::<Request>(&body) {
                Ok(request) => daemon.on_control(request).await,
                Err(e) => Reply::Failed {
                    message: format!("unreadable request: {e}"),
                },
            };
            let Ok(bytes) = serde_json::to_vec(&reply) else {
                return;
            };
            let mut out = (bytes.len() as u32).to_le_bytes().to_vec();
            out.extend_from_slice(&bytes);
            if stream.write_all(&out).await.is_err() {
                return;
            }
            let _ = stream.flush().await;
            // Closing a pipe server end with unread bytes discards them, so wait for the client to hang up.
            let mut rest = [0u8; 1];
            let _ = timeout(FRAME_DEADLINE, stream.read(&mut rest)).await;
        }
    }
}

/// Several instances are always waiting, so a burst of hooks finds a free one instead of `PIPE_BUSY`.
#[cfg(windows)]
const LISTENERS: usize = 4;

#[cfg(windows)]
pub async fn serve(daemon: Arc<Daemon>) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = crate::endpoint::pipe_name();
    // `first_pipe_instance` fails when the name already exists, so a process that squatted on it first
    // cannot receive turns meant for us.
    let first = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(&name)?;
    daemon.log.line(&format!("listening on {name}"));
    let mut listeners = tokio::task::JoinSet::new();
    let mut seed = Some(first);
    for _ in 0..LISTENERS {
        listeners.spawn(listen(Arc::clone(&daemon), name.clone(), seed.take()));
    }
    tokio::select! {
        done = listeners.join_next() => match done {
            Some(Ok(result)) => result,
            Some(Err(e)) => Err(anyhow!("{e}")),
            None => Ok(()),
        },
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

#[cfg(windows)]
async fn listen(
    daemon: Arc<Daemon>,
    name: String,
    mut pending: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    loop {
        let server = match pending.take() {
            Some(server) => server,
            None => ServerOptions::new()
                .reject_remote_clients(true)
                .create(&name)?,
        };
        server.connect().await?;
        tokio::spawn(handle(Arc::clone(&daemon), server));
    }
}

#[cfg(unix)]
pub async fn serve(daemon: Arc<Daemon>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = crate::endpoint::socket_path();
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("socket path has no directory"))?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    // The single-instance lock is held, so a socket file left here belongs to a dead daemon.
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    daemon.log.line(&format!("listening on {}", path.display()));
    // SIGTERM is how `kill`, systemd and a logout ask a process to stop; Ctrl-C is SIGINT.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                tokio::spawn(handle(Arc::clone(&daemon), stream));
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}
