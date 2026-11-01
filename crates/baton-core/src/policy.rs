//! One `StorePolicy` decides, per path, whether to store it and how.

use crate::config::Settings;
use crate::error::{Error, Result};
use crate::id::RelPath;
use crate::ignore::IgnoreRules;
use crate::redact::{Redactor, load_literals};
use globset::{GlobSet, GlobSetBuilder};
use std::path::Path;

const BINARY_PROBE_BYTES: usize = 8 * 1024;

fn globs(patterns: &[String], what: &'static str, include_below: bool) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for raw in patterns {
        let pattern = raw.trim_start_matches('/');
        let anchored = if pattern.contains('/') {
            pattern.to_owned()
        } else {
            format!("**/{pattern}")
        };
        let add = |builder: &mut GlobSetBuilder, text: &str| -> Result<()> {
            let glob = globset::GlobBuilder::new(text)
                .literal_separator(true)
                .build()
                .map_err(|e| Error::invalid(what, format!("{raw:?}: {e}")))?;
            builder.add(glob);
            Ok(())
        };
        add(&mut builder, &anchored)?;
        if include_below {
            add(
                &mut builder,
                &format!("{}/**", anchored.trim_end_matches('/')),
            )?;
        }
    }
    builder.build().map_err(|e| Error::invalid(what, e))
}

pub struct StorePolicy {
    ignore: IgnoreRules,
    exclude: GlobSet,
    whole: GlobSet,
    max_file_bytes: u64,
    redactor: Redactor,
    notices: Vec<String>,
}

impl StorePolicy {
    pub(crate) fn open(worktree: &Path, settings: &Settings) -> Result<Self> {
        let literals = load_literals(&settings.value_files)?;
        let redactor = Redactor::new(&settings.rules, &literals)?;
        let notices = settings
            .notices
            .iter()
            .chain(redactor.notices())
            .cloned()
            .collect();
        Ok(Self {
            ignore: IgnoreRules::discover(worktree)?,
            exclude: globs(&settings.exclude, "exclude glob", true)?,
            whole: globs(&settings.store_whole, "store_whole glob", false)?,
            max_file_bytes: settings.max_file_bytes,
            redactor,
            notices,
        })
    }

    /// What loading the settings ignored or dropped.
    pub(crate) fn notices(&self) -> &[String] {
        &self.notices
    }

    pub(crate) fn rules(&self) -> &IgnoreRules {
        &self.ignore
    }

    pub fn case(&self) -> crate::ignore::Case {
        self.ignore.case()
    }

    /// True for the paths baton stores: not ignored by git unless git tracks them, and not excluded by the
    /// config. `entries` are `(path, is a directory)`.
    pub(crate) fn admits(&self, entries: &[(&str, bool)]) -> Result<Vec<bool>> {
        let tracked = self.ignore.tracked();
        let mut verdicts = self.ignore.verdicts(entries)?;
        for ((path, is_dir), verdict) in entries.iter().zip(&mut verdicts) {
            let by_git = *verdict && (*is_dir || !tracked.contains(path));
            *verdict = !(by_git || self.exclude.is_match(*path));
        }
        Ok(verdicts)
    }

    pub(crate) fn excluded_by_config(&self, path: &str) -> bool {
        self.exclude.is_match(path)
    }

    pub fn exceeds_size_limit(&self, size: u64) -> bool {
        size > self.max_file_bytes
    }

    /// The bytes to store for a file. A file with a NUL in its first 8 KiB is binary and stays as it is. A
    /// `store_whole` match (lockfiles, whose hashes look like secrets) loses only the literal secrets the
    /// user listed; every other text file gets every rule. The blob id is computed from the result.
    pub fn prepare(&self, path: &RelPath, bytes: Vec<u8>) -> Vec<u8> {
        let probe = &bytes[..bytes.len().min(BINARY_PROBE_BYTES)];
        if probe.contains(&0) {
            return bytes;
        }
        let redacted = if self.whole.is_match(path.as_str()) {
            self.redactor.redact_values(&bytes)
        } else {
            self.redactor.redact(&bytes)
        };
        match redacted {
            std::borrow::Cow::Borrowed(_) => bytes,
            std::borrow::Cow::Owned(redacted) => redacted,
        }
    }

    pub fn redact_text(&self, text: &str) -> String {
        self.redactor.redact_text(text)
    }

    /// True when `text` would change under redaction. Anything that enters an object as a name must not.
    pub fn holds_secret(&self, text: &str) -> bool {
        self.redactor.holds_secret(text)
    }
}
