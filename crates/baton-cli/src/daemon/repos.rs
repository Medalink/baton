//! Enrollment by repository. A repository is enrolled once, by its common git dir, which every linked
//! worktree of it shares. A turn from a worktree of an enrolled repository enrolls that worktree on the
//! spot, so a worktree an agent tool creates later needs no step of its own.
//!
//! A repository is enrolled either by the person (`baton enroll --repo`, saved) or by the user config's
//! `[enroll] projects` list (not saved: the list is evaluated again when the daemon starts).
//!
//! The watch set stays bounded and clean. At most `max_watched` such worktrees are watched; past that
//! the least recently used one is unwatched and rescanned at its next turn. A worktree whose directory
//! is deleted is dropped. A worktree enrolled by itself is never dropped by either rule.

use super::{Daemon, Registry, path_key};
use crate::control::Reply;
use crate::daemon::task::Cmd;
use anyhow::{Result, anyhow};
use baton_core::{ProjectKey, RepoConfig, RepoLocation, UserConfig, project_key, repo_of};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

/// How often the daemon looks for repository worktrees whose directory has been deleted. A worktree task
/// also ends by itself when its watcher reports the deletion; this is the backstop.
pub(super) const REAP_EVERY: Duration = Duration::from_secs(2);

/// How long a repository the list did not admit is left alone, if neither its git config, its
/// `.baton/config.toml` nor the user config changes. Those changes are seen at once; this is the backstop
/// for what is not watched (an included git config file, `config.worktree`).
const MISS_TTL: Duration = Duration::from_secs(30);
/// Most repositories remembered as not admitted. Past it the memory starts over.
const MISS_LIMIT: usize = 1024;

/// A file's modification time and size, or none when it is absent.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// What a repository's project key is derived from, besides the remotes' URLs inside the first file.
fn stamps(location: &RepoLocation, workdir: &Path) -> [Stamp; 2] {
    [
        stamp(&location.common_dir.join("config")),
        stamp(&workdir.join(".baton").join("config.toml")),
    ]
}

struct Miss {
    at: Instant,
    stamps: [Stamp; 2],
}

/// What `[enroll] projects` says, and the repositories it was found not to admit. Only the user config is
/// read for the list.
#[derive(Default)]
pub(super) struct Listing {
    /// The user config as last parsed: none when absent or unreadable. Read again when it differs.
    bytes: Option<Vec<u8>>,
    loaded: bool,
    projects: Arc<Vec<ProjectKey>>,
    /// Repositories (by `path_key` of the common git dir) the list did not admit.
    misses: HashMap<String, Miss>,
}

/// The project key of the repository whose worktree is `workdir`, as `Worktree::open` derives it.
fn key_of(workdir: &Path) -> Result<ProjectKey> {
    let config = RepoConfig::load(workdir).map_err(|e| anyhow!("{e}"))?;
    project_key(workdir, &config).map_err(|e| anyhow!("{e}"))
}

impl Daemon {
    fn listing(&self) -> MutexGuard<'_, Listing> {
        self.listing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn restore_repos(&self, repos: &[String], declined: &[String]) {
        let mut registry = self.registry();
        for dir in repos {
            let dir = PathBuf::from(dir);
            registry.repos.insert(path_key(&dir), dir);
        }
        for dir in declined {
            let dir = PathBuf::from(dir);
            registry.declined.insert(path_key(&dir), dir);
        }
    }

    /// The projects `[enroll] projects` names now. The user config is read again whenever its bytes differ
    /// from the ones last parsed, so an edit counts from the next call; a small file read is the whole cost.
    /// A config that does not parse keeps the list that was in force, since a half-written file must not
    /// stop recording.
    pub(super) fn listed_projects(&self) -> Arc<Vec<ProjectKey>> {
        let path = self.dirs.user_config();
        let bytes = std::fs::read(&path).ok();
        let mut listing = self.listing();
        if listing.loaded && listing.bytes == bytes {
            return Arc::clone(&listing.projects);
        }
        // A file seen empty while a list is in force is a write in progress, not a request to stop.
        if listing.loaded && bytes.as_ref().is_some_and(Vec::is_empty) {
            return Arc::clone(&listing.projects);
        }
        match UserConfig::load(&path) {
            Ok(user) => {
                for entry in &user.enroll.skipped {
                    self.log
                        .line(&format!("enroll list: skipped {entry}; not a project key"));
                }
                if user.enroll.projects != *listing.projects {
                    self.log.line(&format!(
                        "enroll list: {} projects",
                        user.enroll.projects.len()
                    ));
                }
                listing.projects = Arc::new(user.enroll.projects);
            }
            Err(e) => self.log.line(&format!(
                "user config unreadable, keeping the enroll list as it was: {e}"
            )),
        }
        listing.bytes = bytes;
        listing.loaded = true;
        listing.misses.clear();
        Arc::clone(&listing.projects)
    }

    fn remember_miss(&self, repo: &str, stamps: [Stamp; 2]) {
        let mut listing = self.listing();
        if listing.misses.len() >= MISS_LIMIT {
            listing.misses.clear();
        }
        listing.misses.insert(
            repo.to_owned(),
            Miss {
                at: Instant::now(),
                stamps,
            },
        );
    }

    /// The project the list admits the repository under, if it does. An answer of no is remembered per
    /// repository until something it was derived from changes, so a busy repository the list does not
    /// name costs a few `stat` calls per turn, not a repository open.
    fn admit(&self, location: &RepoLocation, workdir: &Path, repo: &str) -> Option<ProjectKey> {
        if self.registry().declined.contains_key(repo) {
            return None;
        }
        let projects = self.listed_projects();
        if projects.is_empty() {
            return None;
        }
        let seen = stamps(location, workdir);
        {
            let listing = self.listing();
            if let Some(miss) = listing.misses.get(repo)
                && miss.at.elapsed() < MISS_TTL
                && miss.stamps == seen
            {
                return None;
            }
        }
        match key_of(workdir) {
            Ok(key) if projects.contains(&key) => Some(key),
            Ok(key) => {
                self.log.line(&format!(
                    "{} is not in [enroll] projects (key {key})",
                    location.common_dir.display()
                ));
                self.remember_miss(repo, seen);
                None
            }
            Err(e) => {
                self.log.line(&format!(
                    "{} has no project key, so [enroll] projects cannot admit it: {e:#}",
                    location.common_dir.display()
                ));
                self.remember_miss(repo, seen);
                None
            }
        }
    }

    /// Enrolls the repository `path` is in. It must name a project, as a worktree must.
    pub(super) fn enroll_repo(&self, path: &Path) -> Result<Reply> {
        let location = repo_of(path).map_err(|e| anyhow!("{e}"))?;
        let probe = location.workdir.as_deref().unwrap_or(&location.common_dir);
        let project = key_of(probe)?;
        let key = path_key(&location.common_dir);
        let already = {
            let mut registry = self.registry();
            // Enrolling by hand makes a repository the list brought in permanent, and undoes an opt-out.
            registry.listed.remove(&key);
            registry.declined.remove(&key);
            registry
                .repos
                .insert(key, location.common_dir.clone())
                .is_some()
        };
        self.save_enrolled()?;
        Ok(Reply::EnrolledRepo {
            dir: location.common_dir.to_string_lossy().into_owned(),
            project: project.to_string(),
            already,
            cap: self.max_watched,
        })
    }

    /// Takes the repository out and stops the worktrees it brought in; one enrolled by itself stays.
    /// Returns whether it was enrolled, how many worktrees stopped, and the project it was enrolled
    /// under when that was through the list.
    fn stop_repo(&self, key: &str) -> (bool, usize, Option<ProjectKey>) {
        let mut registry = self.registry();
        let was_enrolled = registry.repos.remove(key).is_some();
        let listed = registry.listed.remove(key);
        let ours: Vec<String> = registry
            .worktrees
            .iter()
            .filter(|(_, h)| h.via.as_deref() == Some(key))
            .map(|(k, _)| k.clone())
            .collect();
        for k in &ours {
            if let Some(handle) = registry.worktrees.remove(k) {
                let _ = handle.tx.send(Cmd::Stop);
            }
        }
        (was_enrolled, ours.len(), listed)
    }

    /// Whether the list names the project of the repository `location` is in.
    fn names_listed_project(&self, location: &RepoLocation) -> bool {
        let projects = self.listed_projects();
        if projects.is_empty() {
            return false;
        }
        let probe = location.workdir.as_deref().unwrap_or(&location.common_dir);
        key_of(probe).is_ok_and(|key| projects.contains(&key))
    }

    /// Stops recording the worktrees the repository brought in. A worktree enrolled by itself stays. A
    /// repository the list names (or brought in) is opted out, so the next turn does not enroll it again
    /// until `enroll --repo` says so.
    pub(super) fn unenroll_repo(&self, path: &Path) -> Result<Reply> {
        let location = repo_of(path).map_err(|e| anyhow!("{e}"))?;
        let key = path_key(&location.common_dir);
        let (was_enrolled, stopped, listed) = self.stop_repo(&key);
        let opted_out = listed.is_some() || self.names_listed_project(&location);
        if opted_out {
            self.registry()
                .declined
                .insert(key, location.common_dir.clone());
            self.log.line(&format!(
                "{} opted out of [enroll] projects; `baton enroll --repo` undoes it",
                location.common_dir.display()
            ));
        }
        self.save_enrolled()?;
        Ok(Reply::UnenrolledRepo {
            dir: location.common_dir.to_string_lossy().into_owned(),
            was_enrolled,
            stopped,
            opted_out,
        })
    }

    /// The worktree `cwd` is in, enrolled through its repository: one the person enrolled, or one whose
    /// project `[enroll] projects` names. None when `cwd` is in neither, or sits in a bare repository with
    /// no worktree.
    pub(super) fn enroll_by_repo(&self, cwd: &Path) -> Result<Option<String>> {
        let Ok(location) = repo_of(cwd) else {
            return Ok(None);
        };
        let Some(workdir) = location.workdir.clone() else {
            return Ok(None);
        };
        let repo = path_key(&location.common_dir);
        let (enrolled, listed) = {
            let registry = self.registry();
            (
                registry.repos.contains_key(&repo),
                registry.listed.get(&repo).cloned(),
            )
        };
        match (enrolled, listed) {
            (true, None) => {}
            (true, Some(was)) => {
                if !self.still_listed(&location, &workdir, &repo, &was) {
                    return Ok(None);
                }
            }
            (false, _) => {
                let Some(project) = self.admit(&location, &workdir, &repo) else {
                    return Ok(None);
                };
                let new = {
                    let mut registry = self.registry();
                    registry.listed.insert(repo.clone(), project.clone());
                    registry
                        .repos
                        .insert(repo.clone(), location.common_dir.clone())
                        .is_none()
                };
                if new {
                    self.log.line(&format!(
                        "enrolled {} as {project}: it is in [enroll] projects",
                        location.common_dir.display()
                    ));
                }
            }
        }
        self.enroll(&workdir, Some(repo))?;
        Ok(Some(path_key(&workdir)))
    }

    /// Whether a repository the list brought in may still bring in `workdir`: its project must still be
    /// named. Run when one of its worktrees is first seen, so a changed remote is noticed there. A key
    /// that cannot be worked out right now is not a reason to stop.
    fn still_listed(
        &self,
        location: &RepoLocation,
        workdir: &Path,
        repo: &str,
        was: &ProjectKey,
    ) -> bool {
        let projects = self.listed_projects();
        match key_of(workdir) {
            Ok(key) if projects.contains(&key) => {
                if key != *was {
                    self.log.line(&format!(
                        "{} is now the project {key}, still in [enroll] projects",
                        location.common_dir.display()
                    ));
                    self.registry().listed.insert(repo.to_owned(), key);
                }
                true
            }
            Ok(key) => {
                self.leave_list(location, repo, &format!("its project is now {key}"));
                self.remember_miss(repo, stamps(location, workdir));
                false
            }
            Err(_) => true,
        }
    }

    fn leave_list(&self, location: &RepoLocation, repo: &str, why: &str) {
        let (_, stopped, _) = self.stop_repo(repo);
        self.log.line(&format!(
            "{} left [enroll] projects ({why}); {stopped} worktrees stopped",
            location.common_dir.display()
        ));
    }

    /// Stops the repositories whose project the list no longer names. The list is read every
    /// `REAP_EVERY`, so a removed project stops recording within seconds.
    fn prune_unlisted(&self) {
        let projects = self.listed_projects();
        let gone: Vec<(String, ProjectKey, PathBuf)> = {
            let registry = self.registry();
            registry
                .listed
                .iter()
                .filter(|(_, project)| !projects.contains(project))
                .filter_map(|(repo, project)| {
                    registry
                        .repos
                        .get(repo)
                        .map(|dir| (repo.clone(), project.clone(), dir.clone()))
                })
                .collect()
        };
        for (repo, project, dir) in gone {
            let (_, stopped, _) = self.stop_repo(&repo);
            self.log.line(&format!(
                "{} left [enroll] projects (the list no longer names {project}); {stopped} worktrees stopped",
                dir.display()
            ));
        }
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
    /// looks the same, and the person named it. Also stops the repositories the list stopped naming.
    pub(super) fn reap(&self) {
        self.prune_unlisted();
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
