use crate::util::Res;
use gix::dir::walk::{Action, Delegate};
use gix::dir::{EntryRef, entry};
use ignore::WalkBuilder;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

pub struct Stat {
    pub path: String,
    pub size: u64,
    pub mtime: SystemTime,
}

#[derive(Default)]
pub struct Sweep {
    pub files: Vec<Stat>,
    /// Ignored directories the engine did not descend into. Only the gix engine reports them.
    pub skipped: Vec<String>,
}

fn rel_string(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn builder(root: &Path) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    b.hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .follow_links(false)
        .filter_entry(|e| e.file_name() != ".git");
    b
}

fn stat_of(root: &Path, e: &ignore::DirEntry) -> Option<Stat> {
    if e.file_type().is_some_and(|t| t.is_dir()) {
        return None;
    }
    let md = e.metadata().ok()?;
    Some(Stat {
        path: rel_string(root, e.path()),
        size: md.len(),
        mtime: md.modified().ok()?,
    })
}

pub fn ignore_serial(root: &Path) -> Res<Sweep> {
    let mut out = Sweep::default();
    for e in builder(root).build() {
        if let Some(s) = stat_of(root, &e?) {
            out.files.push(s);
        }
    }
    Ok(out)
}

pub fn ignore_parallel(root: &Path) -> Res<Sweep> {
    let files = Mutex::new(Vec::new());
    builder(root).threads(8).build_parallel().run(|| {
        let files = &files;
        Box::new(move |e| {
            if let Some(s) = e.ok().and_then(|e| stat_of(root, &e)) {
                files.lock().expect("walk mutex").push(s);
            }
            ignore::WalkState::Continue
        })
    });
    Ok(Sweep {
        files: files.into_inner().expect("walk mutex"),
        skipped: Vec::new(),
    })
}

#[derive(Default)]
struct Collect {
    files: Vec<String>,
    skipped: Vec<String>,
}

impl Delegate for Collect {
    fn emit(&mut self, entry: EntryRef<'_>, _collapsed: Option<entry::Status>) -> Action {
        let path = entry.rela_path.to_string();
        match entry.status {
            entry::Status::Ignored(_) => self.skipped.push(path),
            entry::Status::Untracked
                if !matches!(
                    entry.disk_kind,
                    Some(entry::Kind::Directory | entry::Kind::Repository)
                ) =>
            {
                self.files.push(path)
            }
            _ => {}
        }
        Action::Continue(())
    }
}

/// `repo` is opened once by the caller; the empty index means every non-ignored path is untracked.
/// `stat` adds the lstat per file that size and mtime need; without it the walk alone is timed.
pub fn gix_dirwalk(repo: &gix::Repository, root: &Path, stat: bool) -> Res<Sweep> {
    let index = gix::index::State::new(repo.object_hash());
    let options = repo
        .dirwalk_options()?
        .emit_untracked(gix::dir::walk::EmissionMode::Matching)
        .emit_ignored(Some(gix::dir::walk::EmissionMode::CollapseDirectory))
        .emit_tracked(false)
        .emit_pruned(false)
        .recurse_repositories(false);
    let mut delegate = Collect::default();
    repo.dirwalk(
        &index,
        std::iter::empty::<&str>(),
        &std::sync::atomic::AtomicBool::new(false),
        options,
        &mut delegate,
    )?;
    let mut out = Sweep {
        files: Vec::with_capacity(delegate.files.len()),
        skipped: delegate.skipped,
    };
    for path in delegate.files {
        if !stat {
            out.files.push(Stat {
                size: 0,
                mtime: SystemTime::UNIX_EPOCH,
                path,
            });
            continue;
        }
        let md = std::fs::symlink_metadata(root.join(&path))?;
        out.files.push(Stat {
            size: md.len(),
            mtime: md.modified()?,
            path,
        });
    }
    Ok(out)
}
