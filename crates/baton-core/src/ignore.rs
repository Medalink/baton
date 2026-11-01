//! Every ignore decision goes through gix's excludes stack over an empty in-memory index. Measured against
//! `git check-ignore`, that agrees exactly, and the `ignore` crate does not (repo-local `core.excludesFile`,
//! nested repos). The user's `.git/index` is never opened.

use crate::error::{Error, Result};
use gix::bstr::ByteSlice;
use gix::config::Source;
use gix::dir::walk::{Action, Delegate, EmissionMode};
use gix::dir::{EntryRef, entry};
use gix::index::entry::Mode;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub(crate) struct IgnoreRules {
    repo: gix::Repository,
    index: gix::index::State,
    /// The files in HEAD's tree, keyed by the commit they were read from.
    tracked: RefCell<Option<(gix::ObjectId, Arc<BTreeSet<String>>)>>,
}

#[derive(Default)]
struct CollectFiles(Vec<String>);

impl Delegate for CollectFiles {
    fn emit(&mut self, entry: EntryRef<'_>, _collapsed: Option<entry::Status>) -> Action {
        let is_file = matches!(entry.status, entry::Status::Untracked)
            && !matches!(
                entry.disk_kind,
                Some(entry::Kind::Directory | entry::Kind::Repository)
            );
        if is_file {
            self.0.push(entry.rela_path.to_string());
        }
        Action::Continue(())
    }
}

impl IgnoreRules {
    pub(crate) fn discover(path: &Path) -> Result<Self> {
        let repo = gix::discover(path).map_err(Error::git)?;
        if repo.workdir().is_none() {
            return Err(Error::Refused(format!(
                "{} is a bare repository",
                path.display()
            )));
        }
        let index = gix::index::State::new(repo.object_hash());
        Ok(Self {
            repo,
            index,
            tracked: RefCell::new(None),
        })
    }

    /// Files whose contents decide ignore verdicts, outside the worktree's own `.gitignore` tree: the
    /// caller hashes them so a change is noticed without waiting for a watcher event.
    pub(crate) fn input_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for dir in [self.repo.git_dir(), self.repo.common_dir()] {
            for file in [dir.join("info").join("exclude"), dir.join("config")] {
                if !files.contains(&file) {
                    files.push(file);
                }
            }
        }
        if let Some(workdir) = self.repo.workdir() {
            files.push(workdir.join(".gitignore"));
        }
        // `core.excludesFile` can come from any of these, and from a file they include. The ones that do
        // not exist yet are listed too, so creating one is noticed.
        let snapshot = self.repo.config_snapshot();
        let mut env = |var: &str| std::env::var_os(var);
        let locations = [
            Source::GitInstallation,
            Source::System,
            Source::Git,
            Source::User,
        ]
        .into_iter()
        .filter_map(|source| source.storage_location(&mut env));
        let loaded = snapshot
            .plumbing()
            .sections()
            .filter_map(|section| section.meta().path.clone());
        for file in locations.chain(loaded) {
            if !files.contains(&file) {
                files.push(file);
            }
        }
        let configured = self
            .repo
            .config_snapshot()
            .trusted_path("core.excludesFile")
            .ok()
            .flatten();
        let home = |var| std::env::var_os(var).map(PathBuf::from);
        let global = configured.or_else(|| {
            home("XDG_CONFIG_HOME")
                .or_else(|| {
                    home("HOME")
                        .or_else(|| home("USERPROFILE"))
                        .map(|h| h.join(".config"))
                })
                .map(|dir| dir.join("git").join("ignore"))
        });
        files.extend(global);
        files
    }

    /// The files git tracks at HEAD, read straight from the object database. Git does not apply ignore
    /// rules to a tracked file, so neither does baton. The user's `.git/index` is not opened, which means a
    /// file that is staged but not yet committed is still subject to the ignore rules.
    pub(crate) fn tracked(&self) -> Arc<BTreeSet<String>> {
        let Ok(head) = self.repo.head_id() else {
            return Arc::default();
        };
        let head = head.detach();
        if let Some((cached, files)) = &*self.tracked.borrow()
            && *cached == head
        {
            return Arc::clone(files);
        }
        let files: Arc<BTreeSet<String>> = Arc::new(self.read_tree(head).unwrap_or_default());
        *self.tracked.borrow_mut() = Some((head, Arc::clone(&files)));
        files
    }

    fn read_tree(&self, commit: gix::ObjectId) -> Option<BTreeSet<String>> {
        let tree = self.repo.find_commit(commit).ok()?.tree().ok()?;
        let entries = tree.traverse().breadthfirst.files().ok()?;
        Some(
            entries
                .into_iter()
                .filter(|e| !e.mode.is_tree() && !e.mode.is_commit())
                .filter_map(|e| e.filepath.to_str().ok().map(str::to_owned))
                .collect(),
        )
    }

    pub(crate) fn head(&self) -> Option<crate::id::GitSha> {
        let id = self.repo.head_id().ok()?;
        Some(crate::id::GitSha::from_gix(id.detach()))
    }

    /// Ignore verdicts for `(relative path, is a directory)` pairs, with one stack so each `.gitignore`
    /// is read once. A fresh stack per call also means an edited `.gitignore` is never served stale.
    pub(crate) fn verdicts(&self, paths: &[(&str, bool)]) -> Result<Vec<bool>> {
        let mut stack = self
            .repo
            .excludes(
                &self.index,
                None,
                gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
            )
            .map_err(Error::git)?;
        paths
            .iter()
            .map(|(path, is_dir)| {
                let mode = if *is_dir { Mode::DIR } else { Mode::FILE };
                stack
                    .at_path(path, Some(mode))
                    .map(|platform| platform.is_excluded())
                    .map_err(Error::git)
            })
            .collect()
    }

    /// Non-ignored files and symlinks under `prefix` (or everywhere), without descending into ignored
    /// directories or nested repositories.
    pub(crate) fn walk(&self, prefix: Option<&str>) -> Result<Vec<String>> {
        let options = self
            .repo
            .dirwalk_options()
            .map_err(Error::git)?
            .emit_untracked(EmissionMode::Matching);
        let mut delegate = CollectFiles::default();
        let patterns: Vec<gix::bstr::BString> = prefix
            .map(|p| gix::bstr::BString::from(format!(":(literal){p}")))
            .into_iter()
            .collect();
        self.repo
            .dirwalk(
                &self.index,
                patterns,
                &AtomicBool::new(false),
                options,
                &mut delegate,
            )
            .map_err(Error::git)?;
        Ok(delegate.0)
    }
}
