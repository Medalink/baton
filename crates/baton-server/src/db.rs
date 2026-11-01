//! `server.sqlite`. Only `devices` is authoritative. `projects` and `threads` index what the repos hold
//! and are rebuilt from refs by `reindex`.

use anyhow::{Context, Result, bail};
use baton_core::{MachineId, ProjectKey, ThreadId, TurnId, UnixMillis};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS devices (
    id INTEGER PRIMARY KEY,
    machine TEXT NOT NULL,
    token_sha256 BLOB NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
CREATE TABLE IF NOT EXISTS projects (
    key TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS threads (
    project TEXT NOT NULL,
    thread TEXT NOT NULL,
    owner TEXT NOT NULL,
    tip TEXT NOT NULL,
    turn INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (project, thread)
);
";

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug)]
pub struct Device {
    pub id: i64,
    pub machine: MachineId,
}

pub struct DeviceRow {
    pub id: i64,
    pub machine: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

pub struct ThreadRow {
    pub project: String,
    pub thread: String,
    pub owner: String,
    pub tip: String,
    pub turn: i64,
}

pub struct IndexRow {
    pub project: ProjectKey,
    pub thread: ThreadId,
    pub owner: MachineId,
    pub tip: TurnId,
    pub turn: u32,
    pub at: UnixMillis,
}

fn digest(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn new_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("btn_{hex}"))
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Creates a device and returns its token, which is not stored and cannot be shown again.
    pub fn add_device(&self, machine: &MachineId) -> Result<String> {
        let token = new_token()?;
        self.conn().execute(
            "INSERT INTO devices (machine, token_sha256, created_at) VALUES (?1, ?2, ?3)",
            params![machine.as_str(), digest(&token), UnixMillis::now().millis()],
        )?;
        Ok(token)
    }

    pub fn device_for_token(&self, token: &str) -> Result<Option<Device>> {
        let found: Option<(i64, String)> = self
            .conn()
            .query_row(
                "SELECT id, machine FROM devices WHERE token_sha256 = ?1 AND revoked_at IS NULL",
                params![digest(token)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        found
            .map(|(id, machine)| {
                Ok(Device {
                    id,
                    machine: MachineId::parse(&machine)?,
                })
            })
            .transpose()
    }

    pub fn device_is_active(&self, id: i64) -> Result<bool> {
        let active: Option<i64> = self
            .conn()
            .query_row(
                "SELECT id FROM devices WHERE id = ?1 AND revoked_at IS NULL",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(active.is_some())
    }

    pub fn devices(&self) -> Result<Vec<DeviceRow>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id, machine, created_at, revoked_at FROM devices ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok(DeviceRow {
                id: row.get(0)?,
                machine: row.get(1)?,
                created_at: row.get(2)?,
                revoked_at: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Revokes every active device of the machine. Fails when there is none, so a typo is not silent.
    pub fn revoke(&self, machine: &MachineId) -> Result<usize> {
        let n = self.conn().execute(
            "UPDATE devices SET revoked_at = ?2 WHERE machine = ?1 AND revoked_at IS NULL",
            params![machine.as_str(), UnixMillis::now().millis()],
        )?;
        if n == 0 {
            bail!("no active device named {machine}");
        }
        Ok(n)
    }

    pub fn record_thread(&self, row: &IndexRow) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR IGNORE INTO projects (key, created_at) VALUES (?1, ?2)",
            params![row.project.as_str(), row.at.millis()],
        )?;
        conn.execute(
            "INSERT INTO threads (project, thread, owner, tip, turn, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (project, thread) DO UPDATE SET
                 owner = excluded.owner, tip = excluded.tip,
                 turn = excluded.turn, updated_at = excluded.updated_at",
            params![
                row.project.as_str(),
                row.thread.as_str(),
                row.owner.as_str(),
                row.tip.to_string(),
                row.turn,
                row.at.millis()
            ],
        )?;
        Ok(())
    }

    /// Replaces the index with what the repos hold, in one transaction.
    pub fn rebuild_index(&self, projects: &[ProjectKey], rows: &[IndexRow]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute_batch("DELETE FROM threads; DELETE FROM projects;")?;
        let now = UnixMillis::now().millis();
        for project in projects {
            tx.execute(
                "INSERT INTO projects (key, created_at) VALUES (?1, ?2)",
                params![project.as_str(), now],
            )?;
        }
        for row in rows {
            tx.execute(
                "INSERT INTO threads (project, thread, owner, tip, turn, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.project.as_str(),
                    row.thread.as_str(),
                    row.owner.as_str(),
                    row.tip.to_string(),
                    row.turn,
                    row.at.millis()
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn threads(&self) -> Result<Vec<ThreadRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT project, thread, owner, tip, turn FROM threads ORDER BY project, thread",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ThreadRow {
                project: row.get(0)?,
                thread: row.get(1)?,
                owner: row.get(2)?,
                tip: row.get(3)?,
                turn: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}
