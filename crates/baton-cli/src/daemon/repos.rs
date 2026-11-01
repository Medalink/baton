//! Enrollment by repository. A repository is enrolled once, by its common git dir, which every linked
//! worktree of it shares. A turn from a worktree of an enrolled repository enrolls that worktree on the
//! spot, so a worktree an agent tool creates later needs no step of its own.
//!
//! The watch set stays bounded and clean. At most `max_watched` such worktrees are watched; past that
//! the least recently used one is unwatched and rescanned at its next turn. A worktree whose directory
//! is deleted is dropped. A worktree enrolled by itself is never dropped by either rule.

use super::{Daemon, Registry, path_key};
use crate::control::Reply;
use crate::daemon::task::Cmd;
use anyhow::{Result, anyhow};
use baton_core::{RepoConfig, project_key, repo_of};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often the daemon looks for repository worktrees whose directory has been deleted. A worktree task
/// also ends by itself when its watcher reports the deletion; this is the backstop.
pub(super) const REAP_EVERY: Duration = Duration::from_secs(2);

impl Daemon {
    pub(super) fn restore_repos(&self, dirs: &[String]) {
        let mut registry = self.registry();
        for dir in dirs {
            let dir = PathBuf::from(dir);
            registry.repos.insert(path_key(&dir), dir);
        }
    }

    /// Enrolls the repository `path` is in. It must name a project, as a worktree must.
    pub(super) fn enroll_repo(&self, path: &Path) -> Result<Reply> {
        let location = repo_of(path).map_err(|e| anyhow!("{e}"))?;
        let probe = location.workdir.as_deref().unwrap_or(&location.common_dir);
        let project = project_key(probe, &RepoConfig::load(probe).map_err(|e| anyhow!("{e}"))?)
            .map_err(|e| anyhow!("{e}"))?;
        let already = self
            .registry()
            .repos
            .insert(path_key(&location.common_dir), location.common_dir.clone())
            .is_some();
        self.save_enrolled()?;
        Ok(Reply::EnrolledRepo {
            dir: location.common_dir.to_string_lossy().into_owned(),
            project: project.to_string(),
            already,
            cap: self.max_watched,
        })
    }

    /// Stops recording the worktrees the repository brought in. A worktree enrolled by itself stays.
    pub(super) fn unenroll_repo(&self, path: &Path) -> Result<Reply> {
        let location = repo_of(path).map_err(|e| anyhow!("{e}"))?;
        let key = path_key(&location.common_dir);
        let (was_enrolled, stopped) = {
            let mut registry = self.registry();
            let was_enrolled = registry.repos.remove(&key).is_some();
            let ours: Vec<String> = registry
                .worktrees
                .iter()
                .filter(|(_, h)| h.via.as_deref() == Some(key.as_str()))
                .map(|(k, _)| k.clone())
                .collect();
            for k in &ours {
                if let Some(handle) = registry.worktrees.remove(k) {
                    let _ = handle.tx.send(Cmd::Stop);
                }
            }
            (was_enrolled, ours.len())
        };
        self.save_enrolled()?;
        Ok(Reply::UnenrolledRepo {
            dir: location.common_dir.to_string_lossy().into_owned(),
            was_enrolled,
            stopped,
        })
    }

    /// The worktree `cwd` is in, enrolled through its repository. None when `cwd` is not in an enrolled
    /// repository, or sits in a bare one with no worktree.
    pub(super) fn enroll_by_repo(&self, cwd: &Path) -> Result<Option<String>> {
        let Ok(location) = repo_of(cwd) else {
            return Ok(None);
        };
        let Some(workdir) = location.workdir else {
            return Ok(None);
        };
        let repo = path_key(&location.common_dir);
        if !self.registry().repos.contains_key(&repo) {
            return Ok(None);
        }
        self.enroll(&workdir, Some(repo))?;
        Ok(Some(path_key(&workdir)))
    }

    /// Unwatches the least recently used repository worktrees until the cap holds, never `keep`. Its
    /// state file stays, so the next turn there starts from the saved mirror and rescans. The `Stop`
    /// goes in under the registry lock, behind any turn already sent to it.
    pub(super) fn evict_over_cap(&self, registry: &mut Registry, keep: &str) {
        loop {
            let watched = registry
                .worktrees
                .values()
                .filter(|h| h.via.is_some())
                .count();
            if watched <= self.max_watched {
                return;
            }
            let Some(victim) = registry
                .worktrees
                .iter()
                .filter(|(key, h)| h.via.is_some() && key.as_str() != keep)
                .min_by_key(|(_, h)| h.used)
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            if let Some(handle) = registry.worktrees.remove(&victim) {
                let _ = handle.tx.send(Cmd::Stop);
                self.log.line(&format!(
                    "{} unwatched: {watched} repository worktrees against a cap of {}; it is rescanned at its next turn",
                    handle.root.display(),
                    self.max_watched
                ));
            }
        }
    }

    /// Drops the repository worktrees whose directory is gone. One comes back by itself if a turn arrives
    /// from a worktree at the same path again. A worktree enrolled by itself is kept: an unmounted drive
    /// looks the same, and the person named it.
    pub(super) fn reap(&self) {
        let gone = {
            let mut registry = self.registry();
            let keys: Vec<String> = registry
                .worktrees
                .iter()
                .filter(|(_, h)| h.via.is_some() && !h.root.join(".git").exists())
                .map(|(k, _)| k.clone())
                .collect();
            keys.iter()
                .filter_map(|k| registry.worktrees.remove(k))
                .collect::<Vec<_>>()
        };
        for handle in gone {
            let _ = handle.tx.send(Cmd::Stop);
            let _ = std::fs::remove_file(self.dirs.worktree_state_file(&handle.root));
            self.log.line(&format!(
                "{} is gone; no longer watched",
                handle.root.display()
            ));
        }
    }
}
