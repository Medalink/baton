use crate::error::{Error, Result};
use crate::id::ProjectKey;
use std::path::{Path, PathBuf};

/// When set, data, config and the daemon endpoint all live under this directory, so a run is hermetic.
pub const HOME_ENV: &str = "BATON_HOME";

#[derive(Clone, Debug)]
pub struct Dirs {
    data: PathBuf,
    config: PathBuf,
}

impl Dirs {
    pub fn resolve() -> Result<Self> {
        if let Some(home) = std::env::var_os(HOME_ENV).filter(|h| !h.is_empty()) {
            let home = PathBuf::from(home);
            return Ok(Self {
                data: home.join("data"),
                config: home.join("config"),
            });
        }
        let data =
            dirs::data_local_dir().ok_or_else(|| Error::Refused("no OS data directory".into()))?;
        let config =
            dirs::config_dir().ok_or_else(|| Error::Refused("no OS config directory".into()))?;
        Ok(Self {
            data: data.join("baton"),
            config: config.join("baton"),
        })
    }

    pub fn data(&self) -> &Path {
        &self.data
    }

    pub fn user_config(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    pub fn side_repo(&self, project: &ProjectKey) -> PathBuf {
        self.data.join("projects").join(project.dir_name())
    }

    pub fn enrolled_file(&self) -> PathBuf {
        self.data.join("enrolled.json")
    }

    pub fn worktree_state_file(&self, root: &Path) -> PathBuf {
        self.data.join("worktrees").join(format!(
            "{:016x}.json",
            fnv1a(root.to_string_lossy().to_lowercase().as_bytes())
        ))
    }

    pub fn log_file(&self) -> PathBuf {
        self.data.join("daemon.log")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.data.join("daemon.lock")
    }
}

/// A stable name for a path. Not for security.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Strips the verbatim prefix `canonicalize` adds on Windows, so paths compare equal to the plain spelling
/// that watchers and agents report.
pub fn simplified(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path.to_owned(),
    }
}
