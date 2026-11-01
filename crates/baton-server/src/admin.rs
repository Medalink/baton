//! What `baton admin` does, straight on the data dir, so it works whether or not the server runs.

use crate::db::{Db, DeviceRow, IndexRow, ThreadRow};
use crate::log::log;
use anyhow::{Context, Result};
use baton_core::{MachineId, ProjectKey, SideRepo};
use std::path::Path;

fn db(data: &Path) -> Result<Db> {
    Db::open(&data.join("server.sqlite"))
}

pub fn device_add(data: &Path, name: &str) -> Result<String> {
    let machine = MachineId::parse(name)?;
    db(data)?.add_device(&machine)
}

pub fn device_list(data: &Path) -> Result<Vec<DeviceRow>> {
    db(data)?.devices()
}

pub fn device_revoke(data: &Path, name: &str) -> Result<usize> {
    db(data)?.revoke(&MachineId::parse(name)?)
}

pub fn threads(data: &Path) -> Result<Vec<ThreadRow>> {
    db(data)?.threads()
}

/// Rebuilds `projects` and `threads` from the refs. Returns how many of each it found.
pub fn reindex(data: &Path) -> Result<(usize, usize)> {
    reindex_db(data, &db(data)?)
}

pub(crate) fn reindex_db(data: &Path, db: &Db) -> Result<(usize, usize)> {
    let mut projects = Vec::new();
    let mut rows = Vec::new();
    let dir = data.join("projects");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            db.rebuild_index(&[], &[])?;
            return Ok((0, 0));
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(key) = name
            .strip_suffix(".git")
            .map(|n| n.replace('+', "/"))
            .and_then(|k| ProjectKey::parse(&k).ok())
        else {
            continue;
        };
        let repo = match SideRepo::open_existing(&entry.path()) {
            Ok(repo) => repo,
            Err(e) => {
                log(&format!("reindex: skipping {name}: {e}"));
                continue;
            }
        };
        for (thread, tip) in repo.tips()? {
            match repo.read_turn(tip) {
                Ok(record) => rows.push(IndexRow {
                    project: key.clone(),
                    thread,
                    owner: record.turn.draft.machine.clone(),
                    tip,
                    turn: record.turn.seq.get(),
                    at: record.at,
                }),
                Err(e) => log(&format!("reindex: {key} {thread}: {e}")),
            }
        }
        projects.push(key);
    }
    db.rebuild_index(&projects, &rows)?;
    Ok((projects.len(), rows.len()))
}
