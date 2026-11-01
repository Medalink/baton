//! Putting a turn's files back on disk. The only code in baton that writes into a working tree.
//!
//! It never deletes, never overwrites a file that has edits no snapshot has seen, never writes a file whose
//! stored copy had a secret cut out of it, and never writes through a symlink.

use crate::error::{Error, IoContext, Result};
use crate::id::{BlobId, FileKind, RelPath};
use crate::redact::REDACTION_PREFIX;
use crate::store::{SideRepo, TurnRecord};
use crate::worktree::{Current, Worktree};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewindAction {
    Create,
    Overwrite,
}

#[derive(Clone, Debug)]
pub struct RewindWrite {
    pub path: RelPath,
    pub kind: FileKind,
    pub blob: BlobId,
    pub action: RewindAction,
}

#[derive(Clone, Debug)]
pub struct Refusal {
    pub path: RelPath,
    pub reason: &'static str,
}

#[derive(Debug)]
pub struct RewindPlan {
    pub turn: TurnRecord,
    pub writes: Vec<RewindWrite>,
    pub same: usize,
    pub refused: Vec<Refusal>,
    /// Tracked files the turn does not have. They stay on disk.
    pub left_alone: Vec<String>,
}

/// True when no directory between the root and the path's parent is a symlink or a file.
fn parents_are_plain(root: &Path, path: &RelPath) -> bool {
    let mut dir = root.to_owned();
    if let Some(parent) = path.parent() {
        for part in parent.split('/') {
            dir.push(part);
            match std::fs::symlink_metadata(&dir) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => return false,
                Err(_) => return true,
            }
        }
    }
    true
}

pub fn plan(side: &SideRepo, worktree: &Worktree, turn: TurnRecord) -> Result<RewindPlan> {
    let files = side.flatten(turn.tree)?;
    let wanted: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let left_alone = worktree
        .tracked_paths()
        .filter(|p| !wanted.contains(p))
        .map(str::to_owned)
        .collect();
    let mut out = RewindPlan {
        turn: turn.clone(),
        writes: Vec::new(),
        same: 0,
        refused: Vec::new(),
        left_alone,
    };
    for file in files {
        let refuse = |reason| Refusal {
            path: file.path.clone(),
            reason,
        };
        if !parents_are_plain(worktree.root(), &file.path) {
            out.refused
                .push(refuse("a parent directory is a symlink or a file"));
            continue;
        }
        if file.kind == FileKind::Symlink && cfg!(windows) {
            out.refused
                .push(refuse("symlinks are not restored on Windows"));
            continue;
        }
        let stored = side.read_blob(file.blob)?;
        if stored
            .windows(REDACTION_PREFIX.len())
            .any(|w| w == REDACTION_PREFIX)
        {
            out.refused
                .push(refuse("the stored copy has a secret cut out of it"));
            continue;
        }
        let action = match worktree.current(&file.path) {
            Current::Absent if worktree.stored_blob(&file.path).is_some() => {
                out.refused
                    .push(refuse("deleted since the last snapshot; take a turn first"));
                continue;
            }
            Current::Absent => RewindAction::Create,
            Current::Blocked => {
                out.refused
                    .push(refuse("a directory or an unreadable file is in the way"));
                continue;
            }
            Current::Content(current) => {
                let prepared = worktree.policy().prepare(&file.path, current);
                let on_disk = crate::worktree::hash_blob(&prepared);
                if on_disk == file.blob {
                    out.same += 1;
                    continue;
                }
                if worktree.stored_blob(&file.path) != Some(on_disk) {
                    out.refused
                        .push(refuse("changed since the last snapshot; take a turn first"));
                    continue;
                }
                RewindAction::Overwrite
            }
        };
        out.writes.push(RewindWrite {
            path: file.path,
            kind: file.kind,
            blob: file.blob,
            action,
        });
    }
    Ok(out)
}

/// Writes every file in `plan.writes`. A file is written next to its target and renamed over it, so a
/// crash leaves the old file or the new one.
pub fn apply(side: &SideRepo, root: &Path, plan: &RewindPlan) -> Result<usize> {
    let mut written = 0;
    for item in &plan.writes {
        write_one(side, root, item)?;
        written += 1;
    }
    Ok(written)
}

fn write_one(side: &SideRepo, root: &Path, item: &RewindWrite) -> Result<()> {
    if !parents_are_plain(root, &item.path) {
        return Err(Error::Refused(format!(
            "{} sits below a symlink",
            item.path
        )));
    }
    let target: PathBuf = root.join(item.path.as_str());
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).ctx(|| format!("creating {}", dir.display()))?;
    }
    let bytes = side.read_blob(item.blob)?;
    let temp = target.with_file_name(format!(".baton-rewind-{}", std::process::id()));
    let result = write_temp(&temp, item.kind, &bytes).and_then(|()| {
        std::fs::rename(&temp, &target).ctx(|| format!("replacing {}", target.display()))
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn write_temp(temp: &Path, kind: FileKind, bytes: &[u8]) -> Result<()> {
    match kind {
        FileKind::File | FileKind::Executable => {
            std::fs::write(temp, bytes).ctx(|| format!("writing {}", temp.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if kind == FileKind::Executable {
                    0o755
                } else {
                    0o644
                };
                std::fs::set_permissions(temp, std::fs::Permissions::from_mode(mode))
                    .ctx(|| format!("setting mode on {}", temp.display()))?;
            }
        }
        FileKind::Symlink => {
            #[cfg(unix)]
            {
                let link = String::from_utf8_lossy(bytes).into_owned();
                let _ = std::fs::remove_file(temp);
                std::os::unix::fs::symlink(link, temp)
                    .ctx(|| format!("linking {}", temp.display()))?;
            }
        }
    }
    Ok(())
}
