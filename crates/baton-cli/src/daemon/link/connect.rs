//! Opening the WebSocket: TCP, then TLS when the URL says `wss`, then the `baton.v1` upgrade.

use anyhow::{Context, Result, anyhow, bail};
use baton_core::{MAX_FRAME, SUBPROTOCOL, ServerConfig};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto::ring};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{WebSocketStream, client_async_with_config};

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub type Ws = WebSocketStream<Box<dyn Io>>;

/// The system roots plus the configured CA. Verification is never optional.
fn tls_config(ca_file: Option<&Path>) -> Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if let Some(path) = ca_file {
        let mut reader = std::io::BufReader::new(
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?,
        );
        let mut added = 0;
        for cert in rustls_pemfile::certs(&mut reader) {
            let cert = cert.with_context(|| format!("reading {}", path.display()))?;
            roots
                .add(cert)
                .with_context(|| format!("{} holds a bad certificate", path.display()))?;
            added += 1;
        }
        if added == 0 {
            bail!("{} holds no certificate", path.display());
        }
    }
    if roots.is_empty() {
        bail!("no trusted CA certificates; set ca_file in [server]");
    }
    let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn describe(e: WsError) -> anyhow::Error {
    match e {
        WsError::Http(response) => anyhow!(
            "the server answered the upgrade with HTTP {}{}",
            response.status().as_u16(),
            if response.status().as_u16() == 401 {
                " (the token was not accepted)"
            } else {
                ""
            }
        ),
        other => anyhow!("{other}"),
    }
}

pub async fn open(config: &ServerConfig) -> Result<Ws> {
    let url = &config.url;
    let token = config.token.read().map_err(|e| anyhow!("{e}"))?;
    let tcp = TcpStream::connect(url.authority())
        .await
        .with_context(|| format!("connecting to {}", url.authority()))?;
    tcp.set_nodelay(true)?;
    let io: Box<dyn Io> = if url.tls {
        let name = ServerName::try_from(url.host.clone())
            .map_err(|e| anyhow!("{:?} is not a valid server name: {e}", url.host))?;
        let tls = TlsConnector::from(tls_config(config.ca_file.as_deref())?)
            .connect(name, tcp)
            .await
            .with_context(|| format!("TLS handshake with {}", url.host))?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };

    let mut request = url.url().into_client_request().map_err(describe)?;
    let mut bearer = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| anyhow!("the token is not a valid header value"))?;
    bearer.set_sensitive(true);
    request.headers_mut().insert("authorization", bearer);
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static(SUBPROTOCOL),
    );
    let limits = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME + 64))
        .max_frame_size(Some(MAX_FRAME + 64));
    let (ws, _) = client_async_with_config(request, io, Some(limits))
        .await
        .map_err(describe)?;
    Ok(ws)
}
