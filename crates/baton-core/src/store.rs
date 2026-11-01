//! The side repo: one bare repo per project, written only by that project's writer.
//!
//! Measured on Windows: loose objects written serially cost about 1.2 ms for a 12-object turn, a pack
//! per turn is no faster, and writes from several threads stalled once near 150 ms. So objects are
//! written loose, one at a time.

use crate::error::{Error, IoContext, Result};
use crate::id::{BlobId, FileKind, RelPath, ThreadId, TreeId, TurnId, TurnSeq, UnixMillis};
use crate::turn::{Turn, TurnDraft};
use gix::objs::Write as _;
use gix::refs::Target;
use gix::refs::transaction::PreviousValue;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const REF_PREFIX: &str = "refs/baton/";

pub struct BlobPayload {
    pub id: BlobId,
    pub bytes: Vec<u8>,
}

/// One path of a tree edit: `None` removes it.
#[derive(Clone, Debug)]
pub struct TreeEdit {
    pub path: RelPath,
    pub to: Option<(FileKind, BlobId)>,
}

#[derive(Clone, Debug)]
pub struct FlatEntry {
    pub path: RelPath,
    pub kind: FileKind,
    pub blob: BlobId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

#[derive(Clone, Debug)]
pub struct PathChange {
    pub path: RelPath,
    pub kind: ChangeKind,
}

#[derive(Clone, Debug)]
pub struct TurnRecord {
    pub id: TurnId,
    pub tree: TreeId,
    pub parent: Option<TurnId>,
    pub at: UnixMillis,
    pub turn: Turn,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StageTimes {
    pub commit: Duration,
    pub reference: Duration,
}

pub struct SideRepo {
    repo: gix::Repository,
    dir: PathBuf,
}

fn signature(at: UnixMillis) -> gix::actor::Signature {
    gix::actor::Signature {
        name: "baton".into(),
        email: "baton@localhost".into(),
        time: gix::date::Time::new(at.secs(), 0),
    }
}

impl SideRepo {
    /// For the writer: creates the repo, and clears what a crashed predecessor left behind.
    pub fn open_or_init(dir: &Path) -> Result<Self> {
        if !dir.join("HEAD").is_file() {
            std::fs::create_dir_all(dir).ctx(|| format!("creating {}", dir.display()))?;
            gix::init_bare(dir).map_err(Error::git)?;
            let config = dir.join("config");
            let mut text =
                std::fs::read_to_string(&config).ctx(|| format!("reading {}", config.display()))?;
            text.push_str("[gc]\n\tauto = 0\n[core]\n\tlogAllRefUpdates = false\n");
            std::fs::write(&config, text).ctx(|| format!("writing {}", config.display()))?;
        }
        let repo = Self::open_existing(dir)?;
        remove_stale_locks(&dir.join("refs"));
        Ok(repo)
    }

    pub fn open_existing(dir: &Path) -> Result<Self> {
        if !dir.join("HEAD").is_file() {
            return Err(Error::Refused(format!(
                "no baton history at {}",
                dir.display()
            )));
        }
        let repo = gix::open_opts(dir, gix::open::Options::isolated()).map_err(Error::git)?;
        Ok(Self {
            repo,
            dir: dir.to_owned(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn write_blobs(&self, blobs: &[BlobPayload]) -> Result<()> {
        for blob in blobs {
            // `write_blob` skips the empty blob, which would leave a tree entry with nothing behind it.
            let written = if blob.bytes.is_empty() {
                self.repo
                    .objects
                    .write_buf(gix::objs::Kind::Blob, b"")
                    .map_err(Error::git)?
            } else {
                self.repo
                    .write_blob(&blob.bytes)
                    .map_err(Error::git)?
                    .detach()
            };
            if written != blob.id.oid() {
                return Err(Error::Git(format!(
                    "blob {} was stored as {written}",
                    blob.id
                )));
            }
        }
        Ok(())
    }

    /// Edits `base` with the plumbing editor. `Repository::edit_tree` probes the object store once per
    /// entry (53 ms against 5 ms for a 3-file turn on a 9,700-file tree), so it is never used here.
    /// Every blob an edit names must already be stored; the caller wrote them with `write_blobs`.
    pub fn edit_tree(&self, base: TreeId, edits: &[TreeEdit]) -> Result<TreeId> {
        if edits.is_empty() {
            return Ok(base);
        }
        let root: gix::objs::Tree = if base == TreeId::empty() {
            gix::objs::Tree::empty()
        } else {
            self.repo
                .find_tree(base.oid())
                .map_err(Error::git)?
                .decode()
                .map_err(Error::git)?
                .into()
        };
        let mut editor =
            gix::objs::tree::Editor::new(root, &self.repo.objects, self.repo.object_hash());
        for edit in edits.iter().filter(|e| e.to.is_none()) {
            editor
                .remove(edit.path.as_str().split('/'))
                .map_err(Error::git)?;
        }
        for edit in edits {
            if let Some((kind, blob)) = edit.to {
                editor
                    .upsert(edit.path.as_str().split('/'), kind.entry_kind(), blob.oid())
                    .map_err(Error::git)?;
            }
        }
        let id = editor
            .write(|tree| self.repo.write_object(tree).map(|id| id.detach()))
            .map_err(Error::git)?;
        Ok(TreeId::from_gix(id))
    }

    pub fn tip(&self, thread: &ThreadId) -> Result<Option<TurnId>> {
        let found = self
            .repo
            .try_find_reference(thread.turn_ref().as_str())
            .map_err(Error::git)?;
        Ok(found.and_then(|r| {
            r.target()
                .try_id()
                .map(|id| TurnId::from_gix(id.to_owned()))
        }))
    }

    /// Writes the turn commit on top of the thread's current tip and moves `refs/baton/<thread>` by
    /// compare-and-swap. If another writer moved the ref meanwhile, the turn is re-based once on the new tip.
    pub fn commit_turn(
        &self,
        tree: TreeId,
        draft: &TurnDraft,
        at: UnixMillis,
    ) -> Result<(TurnRecord, StageTimes)> {
        let mut times = StageTimes::default();
        if tree == TreeId::empty() {
            // `write_object` treats the empty tree as already present, so a commit of an empty worktree
            // would point at an object the repo never stored and fail `git fsck`.
            self.repo
                .objects
                .write_buf(gix::objs::Kind::Tree, b"")
                .map_err(Error::git)?;
        }
        let name = draft.thread.turn_ref();
        let mut attempts = 0;
        loop {
            let started = Instant::now();
            let tip = self.tip(&draft.thread)?;
            let (seq, parent) = match tip {
                None => (TurnSeq::first(), None),
                Some(tip) => (self.read_turn(tip)?.turn.seq.next()?, Some(tip)),
            };
            let turn = Turn {
                seq,
                draft: draft.clone(),
            };
            let commit = gix::objs::Commit {
                tree: tree.oid(),
                parents: parent.iter().map(TurnId::oid).collect(),
                author: signature(at),
                committer: signature(at),
                encoding: None,
                message: turn.to_message().into(),
                extra_headers: Vec::new(),
            };
            let id = self
                .repo
                .write_object(&commit)
                .map_err(Error::git)?
                .detach();
            times.commit = started.elapsed();

            let started = Instant::now();
            let expected = match tip {
                None => PreviousValue::MustNotExist,
                Some(tip) => PreviousValue::MustExistAndMatch(Target::Object(tip.oid())),
            };
            let moved = self
                .repo
                .reference(name.as_str(), id, expected, "baton turn");
            times.reference = started.elapsed();
            match moved {
                Ok(_) => {
                    let record = TurnRecord {
                        id: TurnId::from_gix(id),
                        tree,
                        parent,
                        at,
                        turn,
                    };
                    return Ok((record, times));
                }
                Err(e) if attempts == 0 && self.tip(&draft.thread)? != tip => {
                    attempts += 1;
                    let _ = e;
                }
                Err(e) => return Err(Error::git(e)),
            }
        }
    }

    pub fn read_turn(&self, id: TurnId) -> Result<TurnRecord> {
        let commit = self.repo.find_commit(id.oid()).map_err(Error::git)?;
        let message = commit.message_raw().map_err(Error::git)?.to_string();
        let turn = Turn::parse_message(&message).map_err(|e| {
            Error::Refused(format!("commit {} is not a baton turn: {e}", id.short()))
        })?;
        let at = UnixMillis::from_secs(commit.time().map_err(Error::git)?.seconds);
        let tree = TreeId::from_gix(commit.tree_id().map_err(Error::git)?.detach());
        let parent = commit
            .parent_ids()
            .next()
            .map(|p| TurnId::from_gix(p.detach()));
        Ok(TurnRecord {
            id,
            tree,
            parent,
            at,
            turn,
        })
    }

    /// The thread's turns, newest first.
    pub fn turns(&self, thread: &ThreadId, limit: usize) -> Result<Vec<TurnRecord>> {
        let mut out = Vec::new();
        let mut next = self.tip(thread)?;
        while let Some(id) = next {
            if out.len() >= limit {
                break;
            }
            let record = self.read_turn(id)?;
            next = record.parent;
            out.push(record);
        }
        Ok(out)
    }

    pub fn threads(&self) -> Result<Vec<ThreadId>> {
        let platform = self.repo.references().map_err(Error::git)?;
        let mut out = Vec::new();
        for reference in platform.prefixed(REF_PREFIX).map_err(Error::git)? {
            let reference = reference.map_err(Error::git)?;
            let name = reference.name().as_bstr().to_string();
            if let Some(thread) = name
                .strip_prefix(REF_PREFIX)
                .and_then(|t| ThreadId::parse(t).ok())
            {
                out.push(thread);
            }
        }
        out.sort();
        Ok(out)
    }

    /// `thread@seq`, or a hex prefix of at least 7 digits of any turn in the repo.
    pub fn resolve_turn(
        &self,
        selector: &str,
        default_thread: Option<&ThreadId>,
    ) -> Result<TurnRecord> {
        if let Some((thread, seq)) = selector.rsplit_once('@') {
            let thread = ThreadId::parse(thread)?;
            let seq: u32 = seq.parse().map_err(|_| {
                Error::invalid("turn", format!("{selector:?}: sequence is not a number"))
            })?;
            return self
                .turns(&thread, usize::MAX)?
                .into_iter()
                .find(|t| t.turn.seq.get() == seq)
                .ok_or_else(|| Error::Refused(format!("thread {thread} has no turn {seq}")));
        }
        if let Ok(seq) = selector.parse::<u32>() {
            let thread = default_thread
                .ok_or_else(|| Error::Refused("a bare turn number needs --thread".into()))?;
            return self.resolve_turn(&format!("{thread}@{seq}"), None);
        }
        let hex = selector.to_ascii_lowercase();
        if hex.len() < 7 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::invalid(
                "turn",
                format!("{selector:?} is not thread@seq or a commit id prefix"),
            ));
        }
        let mut found: Vec<TurnRecord> = Vec::new();
        for thread in self.threads()? {
            for turn in self.turns(&thread, usize::MAX)? {
                if turn.id.to_string().starts_with(&hex) && !found.iter().any(|f| f.id == turn.id) {
                    found.push(turn);
                }
            }
        }
        match found.len() {
            1 => Ok(found.remove(0)),
            0 => Err(Error::Refused(format!("no turn matches {selector:?}"))),
            n => Err(Error::Refused(format!(
                "{selector:?} matches {n} turns; use more digits"
            ))),
        }
    }

    pub fn read_blob(&self, id: BlobId) -> Result<Vec<u8>> {
        Ok(self
            .repo
            .find_blob(id.oid())
            .map_err(Error::git)?
            .data
            .clone())
    }

    pub fn has_tree(&self, id: TreeId) -> bool {
        id == TreeId::empty() || self.repo.find_tree(id.oid()).is_ok()
    }

    /// Every file of a tree. Submodule entries are not stored by baton and are skipped.
    pub fn flatten(&self, tree: TreeId) -> Result<Vec<FlatEntry>> {
        let mut out = Vec::new();
        self.walk(
            &mut String::new(),
            Some(tree.oid()),
            &mut |path, kind, blob| {
                if let Ok(path) = RelPath::parse(path) {
                    out.push(FlatEntry { path, kind, blob });
                }
            },
        )?;
        Ok(out)
    }

    /// Paths that differ between two trees, found by skipping every subtree whose id is equal.
    pub fn diff_trees(&self, from: Option<TreeId>, to: TreeId) -> Result<Vec<PathChange>> {
        let mut out = Vec::new();
        self.diff(
            &mut String::new(),
            from.map(|t| t.oid()),
            Some(to.oid()),
            &mut out,
        )?;
        Ok(out)
    }

    fn entries(
        &self,
        tree: Option<gix::ObjectId>,
    ) -> Result<BTreeMap<String, (gix::objs::tree::EntryKind, gix::ObjectId)>> {
        let mut map = BTreeMap::new();
        let Some(id) = tree else { return Ok(map) };
        if id == gix::ObjectId::empty_tree(self.repo.object_hash()) {
            return Ok(map);
        }
        let tree = self.repo.find_tree(id).map_err(Error::git)?;
        let decoded = tree.decode().map_err(Error::git)?;
        for entry in &decoded.entries {
            map.insert(
                entry.filename.to_string(),
                (entry.mode.kind(), entry.oid.to_owned()),
            );
        }
        Ok(map)
    }

    fn walk(
        &self,
        prefix: &mut String,
        tree: Option<gix::ObjectId>,
        visit: &mut dyn FnMut(&str, FileKind, BlobId),
    ) -> Result<()> {
        use gix::objs::tree::EntryKind;
        for (name, (kind, oid)) in self.entries(tree)? {
            let len = prefix.len();
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(&name);
            match kind {
                EntryKind::Tree => self.walk(prefix, Some(oid), visit)?,
                other => {
                    if let Some(kind) = FileKind::from_entry_kind(other) {
                        visit(prefix, kind, BlobId::from_gix(oid));
                    }
                }
            }
            prefix.truncate(len);
        }
        Ok(())
    }

    fn diff(
        &self,
        prefix: &mut String,
        from: Option<gix::ObjectId>,
        to: Option<gix::ObjectId>,
        out: &mut Vec<PathChange>,
    ) -> Result<()> {
        use gix::objs::tree::EntryKind;
        if from == to {
            return Ok(());
        }
        let (old, new) = (self.entries(from)?, self.entries(to)?);
        let names: std::collections::BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        for name in names {
            let len = prefix.len();
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(name);
            let (before, after) = (old.get(name), new.get(name));
            fn is_tree(e: Option<&(EntryKind, gix::ObjectId)>) -> Option<gix::ObjectId> {
                e.filter(|(k, _)| *k == EntryKind::Tree).map(|(_, id)| *id)
            }
            let (old_tree, new_tree) = (is_tree(before), is_tree(after));
            if old_tree.is_some() || new_tree.is_some() {
                self.diff(prefix, old_tree, new_tree, out)?;
            }
            fn leaf(e: Option<&(EntryKind, gix::ObjectId)>) -> Option<&(EntryKind, gix::ObjectId)> {
                e.filter(|(k, _)| *k != EntryKind::Tree && *k != EntryKind::Commit)
            }
            let kind = match (leaf(before), leaf(after)) {
                (None, Some(_)) => Some(ChangeKind::Added),
                (Some(_), None) => Some(ChangeKind::Removed),
                (Some(a), Some(b)) if a != b => Some(ChangeKind::Modified),
                _ => None,
            };
            if let (Some(kind), Ok(path)) = (kind, RelPath::parse(prefix)) {
                out.push(PathChange { path, kind });
            }
            prefix.truncate(len);
        }
        Ok(())
    }
}

/// A crash between taking a ref lock and renaming it leaves `<ref>.lock` behind, which blocks every later
/// update of that ref. Only the daemon writes this repo and only one daemon runs, so any lock found at
/// start is stale.
fn remove_stale_locks(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => remove_stale_locks(&path),
            Ok(t) if t.is_file() && path.extension().is_some_and(|e| e == "lock") => {
                let _ = std::fs::remove_file(&path);
            }
            _ => {}
        }
    }
}
