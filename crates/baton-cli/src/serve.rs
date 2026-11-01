//! `baton serve` and `baton admin`: thin wrappers over `baton-server`, which owns the behavior.

use crate::timefmt;
use anyhow::{Context, Result, bail};
use baton_core::{Dirs, UnixMillis};
use baton_server::ServeOptions;
use std::net::SocketAddr;
use std::path::PathBuf;

pub struct ServeArgs {
    pub data: Option<PathBuf>,
    pub listen: SocketAddr,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub trace: bool,
}

/// The server's data dir: `--data`, else `server` under baton's own data dir.
pub fn data_dir(given: Option<PathBuf>) -> Result<PathBuf> {
    match given {
        Some(path) => Ok(path),
        None => Ok(Dirs::resolve()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .data()
            .join("server")),
    }
}

pub fn serve(args: ServeArgs) -> Result<()> {
    let tls = match (args.tls_cert, args.tls_key) {
        (Some(cert), Some(key)) => Some((cert, key)),
        (None, None) => None,
        _ => bail!("--tls-cert and --tls-key go together"),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the runtime")?;
    runtime.block_on(baton_server::serve(ServeOptions {
        data: data_dir(args.data)?,
        listen: args.listen,
        tls,
        trace: args.trace,
    }))
}

pub fn device_add(data: Option<PathBuf>, name: &str) -> Result<()> {
    let token = baton_server::device_add(&data_dir(data)?, name)?;
    eprintln!("Token for {name}. It is not stored and cannot be shown again:");
    println!("{token}");
    Ok(())
}

pub fn device_list(data: Option<PathBuf>) -> Result<()> {
    let rows = baton_server::device_list(&data_dir(data)?)?;
    if rows.is_empty() {
        println!("no devices");
    }
    for row in rows {
        let state = match row.revoked_at {
            Some(at) => format!("revoked {}", timefmt::utc(UnixMillis::from_secs(at / 1000))),
            None => "active".to_owned(),
        };
        println!(
            "{:>3}  {:<24} added {}  {state}",
            row.id,
            row.machine,
            timefmt::utc(UnixMillis::from_secs(row.created_at / 1000))
        );
    }
    Ok(())
}

pub fn device_revoke(data: Option<PathBuf>, name: &str) -> Result<()> {
    let n = baton_server::device_revoke(&data_dir(data)?, name)?;
    println!("revoked {n} device(s) of {name}; open sessions end within 2 s");
    Ok(())
}

pub fn threads(data: Option<PathBuf>) -> Result<()> {
    for row in baton_server::threads(&data_dir(data)?)? {
        println!(
            "{}  {}  owner {}  turn {}  {}",
            row.project, row.thread, row.owner, row.turn, row.tip
        );
    }
    Ok(())
}

pub fn reindex(data: Option<PathBuf>) -> Result<()> {
    let (projects, threads) = baton_server::reindex(&data_dir(data)?)?;
    println!("indexed {projects} projects and {threads} threads");
    Ok(())
}
