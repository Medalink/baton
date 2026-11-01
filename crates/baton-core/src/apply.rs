//! Writing a stored tree into a directory that is not an enrolled worktree: `baton pull` and
//! `baton follow --into`. It keeps rewind's rules. It never writes through a symlink, never writes a
//! file whose stored copy had a secret cut out of it, and never overwrites a file that differs from
//! what baton put there before.

use crate::error::{IoContext, Result};
use crate::id::{BlobId, FileKind, RelPath, TreeId};
use crate::redact::REDACTION_PREFIX;
use crate::rewind::{Refusal, parents_are_plain, write_file};
use crate::store::SideRepo;
use crate::worktree::hash_blob;
use std::collections::HashSet;
use std::path::Path;

/// What the target directory is known to hold.
pub enum Guard<'a> {
    /// A directory `git worktree add` just made. Everything in it is git's checkout, so any file of the
    /// tree may be overwritten, and `git_paths` (what git checked out) that the tree lacks are removed,
    /// except `keep`, the paths baton did not store.
    Fresh {
        git_paths: &'a [String],
        keep: &'a HashSet<String>,
    },
    /// The directory holds this tree, as an earlier apply left it. Only paths that differ between it and
    /// the new tree are touched, and only when they still hold what that tree had.
    Holds(TreeId),
}

#[derive(Debug, Default)]
pub struct Applied {
    pub written: usize,
    pub removed: usize,
    pub same: usize,
    pub refused: Vec<Refusal>,
}

enum Disk {
    Absent,
    Blob(BlobId),
    Blocked,
}

fn on_disk(root: &Path, path: &RelPath) -> Disk {
    if !parents_are_plain(root, path) {
        return Disk::Blocked;
    }
    let target = root.join(path.as_str());
    match std::fs::symlink_metadata(&target) {
        Err(_) => Disk::Absent,
        Ok(meta) if meta.is_file() => match std::fs::read(&target) {
            Ok(bytes) => Disk::Blob(hash_blob(&bytes)),
            Err(_) => Disk::Blocked,
        },
        Ok(_) => Disk::Blocked,
    }
}

fn refusal(path: &RelPath, reason: &'static str) -> Refusal {
    Refusal {
        path: path.clone(),
        reason,
    }
}

/// Makes `root` hold `to`, as far as `guard` allows. A refused path is reported and left alone; the
/// rest still apply.
pub fn apply_tree(side: &SideRepo, root: &Path, to: TreeId, guard: &Guard<'_>) -> Result<Applied> {
    let mut out = Applied::default();
    match guard {
        Guard::Fresh { git_paths, keep } => {
            let files = side.flatten(to)?;
            let wanted: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
            for file in &files {
                place(side, root, &file.path, file.kind, file.blob, None, &mut out)?;
            }
            for path in *git_paths {
                if wanted.contains(path.as_str()) || keep.contains(path) {
                    continue;
                }
                if let Ok(path) = RelPath::parse(path) {
                    remove(root, &path, None, &mut out)?;
                }
            }
        }
        Guard::Holds(from) => {
            for change in side.diff_trees(Some(*from), to)? {
                let old = side.entry_at(*from, &change.path)?.map(|(_, blob)| blob);
                match side.entry_at(to, &change.path)? {
                    Some((kind, blob)) => {
                        place(side, root, &change.path, kind, blob, Some(old), &mut out)?;
                    }
                    None => remove(root, &change.path, Some(old), &mut out)?,
                }
            }
        }
    }
    Ok(out)
}

/// `expected` is what the path must hold now for the write to be safe: `None` accepts anything,
/// `Some(None)` requires the path to be absent, `Some(Some(blob))` requires that content.
fn place(
    side: &SideRepo,
    root: &Path,
    path: &RelPath,
    kind: FileKind,
    blob: BlobId,
    expected: Option<Option<BlobId>>,
    out: &mut Applied,
) -> Result<()> {
    if kind == FileKind::Symlink && cfg!(windows) {
        out.refused
            .push(refusal(path, "symlinks are not restored on Windows"));
        return Ok(());
    }
    let current = on_disk(root, path);
    match (&current, expected) {
        (Disk::Blocked, _) => {
            out.refused.push(refusal(
                path,
                "a symlink, a directory or an unreadable file is in the way",
            ));
            return Ok(());
        }
        (Disk::Blob(have), _) if *have == blob => {
            out.same += 1;
            return Ok(());
        }
        (Disk::Absent, Some(Some(_))) => {
            out.refused
                .push(refusal(path, "deleted here since it was last applied"));
            return Ok(());
        }
        (Disk::Blob(have), Some(want)) if want != Some(*have) => {
            out.refused
                .push(refusal(path, "changed here since it was last applied"));
            return Ok(());
        }
        _ => {}
    }
    let stored = side.read_blob(blob)?;
    if stored
        .windows(REDACTION_PREFIX.len())
        .any(|w| w == REDACTION_PREFIX)
    {
        out.refused
            .push(refusal(path, "the stored copy has a secret cut out of it"));
        return Ok(());
    }
    write_file(root, path, kind, &stored)?;
    out.written += 1;
    Ok(())
}

fn remove(
    root: &Path,
    path: &RelPath,
    expected: Option<Option<BlobId>>,
    out: &mut Applied,
) -> Result<()> {
    match (on_disk(root, path), expected) {
        (Disk::Absent, _) => {}
        (Disk::Blocked, _) => out
            .refused
            .push(refusal(path, "a symlink or a directory is in the way")),
        (Disk::Blob(have), Some(want)) if want != Some(have) => out
            .refused
            .push(refusal(path, "changed here since it was last applied")),
        (Disk::Blob(_), _) => {
            let target = root.join(path.as_str());
            std::fs::remove_file(&target).ctx(|| format!("removing {}", target.display()))?;
            out.removed += 1;
        }
    }
    Ok(())
}
