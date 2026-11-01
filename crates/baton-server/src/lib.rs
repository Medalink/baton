//! `baton serve`: a WebSocket front for per-project bare repos.
//!
//! Each project has one writer thread (see `project`), each connected machine one session task (see
//! `session`), and each project one broadcast channel that fans a moved ref out to the other sessions.

mod admin;
mod auth;
mod db;
mod log;
mod project;
mod session;
mod tls;

pub use admin::{device_add, device_list, device_revoke, reindex, threads};
pub use db::{DeviceRow, ThreadRow};

use anyhow::{Context, Result, bail};
use auth::Auth;
use baton_core::{CommandSpec, MAX_FRAME, SUBPROTOCOL};
use db::Db;
use log::log;
use project::Projects;
use serde::Deserialize;
use session::Session;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

pub struct ServeOptions {
    pub data: PathBuf,
    pub listen: SocketAddr,
    pub tls: Option<(PathBuf, PathBuf)>,
    /// Log every advance and every fan-out with its timing.
    pub trace: bool,
}

pub(crate) struct Shared {
    pub projects: Projects,
    pub db: Arc<Db>,
    pub auth: Auth,
    pub trace: bool,
    next_session: AtomicU64,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    auth: AuthFile,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AuthFile {
    provider: Option<String>,
    command: Option<Vec<String>>,
    timeout_ms: Option<u64>,
}

fn auth_from_config(data: &std::path::Path, db: Arc<Db>) -> Result<Auth> {
    let path = data.join("server.toml");
    let file: FileConfig = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    match file.auth.provider.as_deref().unwrap_or("token") {
        "token" => Ok(Auth::Token(db)),
        "command" => {
            let command = file
                .auth
                .command
                .context("[auth] provider = \"command\" needs command = [...]")?;
            let spec = CommandSpec {
                command,
                timeout_ms: file.auth.timeout_ms.unwrap_or(2000),
            };
            spec.check()?;
            Ok(Auth::Command(spec))
        }
        other => bail!("{}: unknown auth provider {other:?}", path.display()),
    }
}

fn refuse(status: StatusCode, why: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(why.to_owned()));
    *response.status_mut() = status;
    response
}

// The handshake callback's error type is tungstenite's `ErrorResponse`, which is large.
#[allow(clippy::result_large_err)]
async fn handle<S>(stream: S, peer: SocketAddr, shared: Arc<Shared>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut identity = None;
    let check = |request: &Request, mut response: Response| -> Result<Response, ErrorResponse> {
        let offered = request
            .headers()
            .get_all("sec-websocket-protocol")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|p| p.trim() == SUBPROTOCOL);
        if !offered {
            return Err(refuse(
                StatusCode::BAD_REQUEST,
                "the subprotocol baton.v1 is required",
            ));
        }
        let headers: BTreeMap<String, String> = request
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                Some((k.as_str().to_ascii_lowercase(), v.to_str().ok()?.to_owned()))
            })
            .collect();
        identity = tokio::task::block_in_place(|| shared.auth.check(&headers, peer));
        if identity.is_none() {
            return Err(refuse(StatusCode::UNAUTHORIZED, "unauthorized"));
        }
        response.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static(SUBPROTOCOL),
        );
        Ok(response)
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME + 64))
        .max_frame_size(Some(MAX_FRAME + 64));
    let ws = match accept_hdr_async_with_config(stream, check, Some(config)).await {
        Ok(ws) => ws,
        Err(e) => {
            log(&format!("{peer}: handshake failed: {e}"));
            return;
        }
    };
    let Some(identity) = identity else { return };
    let id = shared.next_session.fetch_add(1, Ordering::Relaxed);
    log(&format!("session {id}: {peer} is {}", identity.machine));
    Session::new(ws, id, identity, shared).run().await;
}

pub async fn serve(options: ServeOptions) -> Result<()> {
    std::fs::create_dir_all(options.data.join("projects"))
        .with_context(|| format!("creating {}", options.data.display()))?;
    // The project writers clear stale ref locks when they start, which is only safe with one server.
    let lock_path = options.data.join("server.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    if lock.try_lock().is_err() {
        bail!("another baton server is using {}", options.data.display());
    }

    let db = Arc::new(Db::open(&options.data.join("server.sqlite"))?);
    let (projects, threads) = admin::reindex_db(&options.data, &db)?;
    log(&format!(
        "index holds {projects} projects and {threads} threads"
    ));
    let auth = auth_from_config(&options.data, db.clone())?;
    let acceptor = match &options.tls {
        Some((cert, key)) => Some(tls::acceptor(cert, key)?),
        None => None,
    };
    let shared = Arc::new(Shared {
        projects: Projects::new(options.data.clone(), db.clone()),
        db,
        auth,
        trace: options.trace,
        next_session: AtomicU64::new(1),
    });

    let listener = TcpListener::bind(options.listen)
        .await
        .with_context(|| format!("listening on {}", options.listen))?;
    log(&format!(
        "listening on {}://{}",
        if acceptor.is_some() { "wss" } else { "ws" },
        listener.local_addr()?
    ));
    let accepting = async {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    log(&format!("accept failed: {e}"));
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            let _ = stream.set_nodelay(true);
            let shared = shared.clone();
            match acceptor.clone() {
                Some(acceptor) => {
                    tokio::spawn(async move {
                        match acceptor.accept(stream).await {
                            Ok(stream) => handle(stream, peer, shared).await,
                            Err(e) => log(&format!("{peer}: TLS handshake failed: {e}")),
                        }
                    });
                }
                None => {
                    tokio::spawn(handle(stream, peer, shared));
                }
            }
        }
    };
    tokio::select! {
        () = accepting => {}
        _ = tokio::signal::ctrl_c() => log("stopping"),
    }
    drop(lock);
    Ok(())
}
