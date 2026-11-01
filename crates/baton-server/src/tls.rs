use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{ServerConfig, crypto::ring};

pub fn acceptor(cert: &Path, key: &Path) -> Result<TlsAcceptor> {
    let mut cert_reader = std::io::BufReader::new(
        std::fs::File::open(cert).with_context(|| format!("opening {}", cert.display()))?,
    );
    let chain = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("reading {}", cert.display()))?;
    if chain.is_empty() {
        bail!("{} holds no certificate", cert.display());
    }
    let mut key_reader = std::io::BufReader::new(
        std::fs::File::open(key).with_context(|| format!("opening {}", key.display()))?,
    );
    let key = rustls_pemfile::private_key(&mut key_reader)
        .with_context(|| format!("reading {}", key.display()))?
        .with_context(|| format!("{} holds no private key", key.display()))?;
    let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .context("the certificate and key do not match")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}
