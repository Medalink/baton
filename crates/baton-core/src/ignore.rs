//! Every ignore decision goes through gix's excludes stack over an empty in-memory index. Measured against
//! `git check-ignore`, that agrees exactly, and the `ignore` crate does not (repo-local `core.excludesFile`,
//! nested repos). The user's `.git/index` is never opened.

use crate::error::{Error, Result};
use gix::dir::walk::{Action, Delegate, EmissionMode};
use gix::dir::{EntryRef, entry};
use gix::index::entry::Mode;
use std::path::Path;
use std::sync::atomic::AtomicBool;

pub(crate) struct IgnoreRules {
    repo: gix::Repository,
    index: gix::index::State,
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
        Ok(Self { repo, index })
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
