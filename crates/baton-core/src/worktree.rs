//! A worktree's view of itself: which files exist, what each one was last stored as, and the tree that
//! holds them. One `Worktree` is owned by one task, so nothing here is shared or locked.
//!
//! The watcher only says "this path may have changed". `prepare` turns those paths into blobs and tree
//! edits by looking at the disk, so a lost, duplicated or reordered event can only cost work, never
//! correctness. A full `rescan` (dirwalk plus a stat comparison) covers events the watcher dropped.

use crate::config::{RepoConfig, Settings, UserConfig};
use crate::error::{Error, IoContext, Result};
use crate::id::{BlobId, FileKind, GitSha, ProjectKey, RelPath, TreeId};
use crate::ignore::{Case, TrackedSet};
use crate::paths::Dirs;
use crate::policy::StorePolicy;
use crate::store::{BlobPayload, TreeEdit};
use crate::turn::SkippedPath;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// Beyond this many dirty paths the set stops tracking them and the next drain does a full rescan.
const DIRTY_CAP: usize = 50_000;
/// A file modified this recently when it was hashed may be modified again within the same timestamp tick.
const RACY_WINDOW_NS: i64 = 2_000_000_000;
const STATE_VERSION: u32 = 2;

#[derive(Debug, Default)]
pub struct DirtySet {
    paths: BTreeSet<String>,
    /// Paths whose creation, removal or rename was seen: if one is a directory, its whole subtree is re-read.
    structural: BTreeSet<String>,
    overflow: bool,
}

impl DirtySet {
    pub fn mark(&mut self, path: String, structural: bool) {
        if self.overflow {
            return;
        }
        if structural {
            self.structural.insert(path.clone());
        }
        self.paths.insert(path);
        if self.paths.len() > DIRTY_CAP {
            self.overflow();
        }
    }

    /// The watcher lost events, or there were too many to track.
    pub fn overflow(&mut self) {
        self.paths.clear();
        self.structural.clear();
        self.overflow = true;
    }

    pub fn is_empty(&self) -> bool {
        !self.overflow && self.paths.is_empty()
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Takes in what a full rescan found. A rescan is bounded by the worktree, so unlike `mark` it never
    /// overflows; otherwise a worktree over the cap would rescan forever and record nothing.
    fn absorb(&mut self, found: Vec<String>) {
        self.paths.extend(found);
    }
}

#[derive(Clone, Debug)]
struct Tracked {
    kind: FileKind,
    blob: BlobId,
    size: u64,
    mtime_ns: i64,
    racy: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum SavedKind {
    File,
    Executable,
    Symlink,
}

impl From<FileKind> for SavedKind {
    fn from(k: FileKind) -> Self {
        match k {
            FileKind::File => Self::File,
            FileKind::Executable => Self::Executable,
            FileKind::Symlink => Self::Symlink,
        }
    }
}

impl From<SavedKind> for FileKind {
    fn from(k: SavedKind) -> Self {
        match k {
            SavedKind::File => Self::File,
            SavedKind::Executable => Self::Executable,
            SavedKind::Symlink => Self::Symlink,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SavedEntry {
    path: String,
    kind: SavedKind,
    blob: BlobId,
    size: u64,
    mtime_ns: i64,
    racy: bool,
}

#[derive(Serialize, Deserialize)]
struct SavedState {
    version: u32,
    project: ProjectKey,
    tree: TreeId,
    entries: Vec<SavedEntry>,
    skipped: Vec<String>,
    /// What the store policy was when `entries` were read; a different policy re-reads every file.
    policy: String,
}

/// Why a path is not in the tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SkipKind {
    /// Over the size limit.
    Size,
    /// The path itself holds a secret, so it cannot be named in a tree or a trailer. Never saved to disk.
    Name,
}

enum Update {
    Set(String, Tracked),
    Clear(String),
    Skip(String, SkipKind),
    Unskip(String),
}

/// What one `prepare` call decided. Apply `blobs` then `edits` to the side repo, then call `accept`;
/// if applying failed, call `abandon` so the paths come back to be retried.
pub struct Plan {
    pub blobs: Vec<BlobPayload>,
    pub edits: Vec<TreeEdit>,
    updates: Vec<Update>,
    taken: Vec<String>,
}

impl Plan {
    /// How many dirty paths this plan consumed. Zero means the dirty set is drained.
    pub fn paths(&self) -> usize {
        self.taken.len()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ChunkLimits {
    pub files: usize,
    pub bytes: u64,
}

impl Default for ChunkLimits {
    fn default() -> Self {
        Self {
            files: 4096,
            bytes: 64 * 1024 * 1024,
        }
    }
}

pub struct Worktree {
    root: PathBuf,
    dirs: Dirs,
    project: ProjectKey,
    settings: Settings,
    policy: StorePolicy,
    policy_fingerprint: String,
    /// The files in HEAD as of the last `prepare`, to notice which ones HEAD started or stopped tracking.
    head_files: Arc<TrackedSet>,
    tracked: BTreeMap<String, Tracked>,
    skipped: BTreeMap<String, SkipKind>,
    /// What loading the policy ignored, until `take_notices` hands it to the log.
    notices: Vec<String>,
    tree: TreeId,
    state_file: PathBuf,
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn mtime_ns(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Everything a stored blob depends on besides the file's own bytes: the built-in redaction rules, the
/// merged settings, and the contents of every file that feeds them. The value files and the user's
/// ignore files live outside the worktree, so no watcher sees them change; they are read on every call.
fn fingerprint(root: &Path, dirs: &Dirs, settings: &Settings, policy: &StorePolicy) -> String {
    let mut bytes = Vec::new();
    for (name, pattern) in crate::redact::BUILTIN {
        bytes.extend_from_slice(format!("{name}={pattern}\n").as_bytes());
    }
    bytes.extend_from_slice(format!("{settings:?}\n").as_bytes());
    let mut files = vec![root.join(".baton").join("config.toml"), dirs.user_config()];
    files.extend(settings.value_files.iter().cloned());
    files.extend(policy.rules().input_files());
    for file in files {
        bytes.extend_from_slice(file.as_os_str().as_encoded_bytes());
        match std::fs::read(&file) {
            Ok(content) => {
                bytes.extend_from_slice(format!(":{}:", content.len()).as_bytes());
                bytes.extend_from_slice(&content);
            }
            Err(_) => bytes.extend_from_slice(b":missing:"),
        }
        bytes.push(b'\n');
    }
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, &bytes)
        .map_or_else(|_| String::new(), |id| id.to_string())
}

fn load_settings(root: &Path, dirs: &Dirs) -> Result<Settings> {
    let user = UserConfig::load(&dirs.user_config())?;
    let repo = RepoConfig::load(root)?;
    Ok(Settings::merge(&user, &repo))
}

pub fn find_root(path: &Path) -> Result<PathBuf> {
    let repo = gix::discover(path).map_err(Error::git)?;
    let root = repo.workdir().ok_or_else(|| {
        Error::Refused(format!("{} is not inside a git worktree", path.display()))
    })?;
    root.canonicalize()
        .map(|p| crate::paths::simplified(&p))
        .ctx(|| format!("resolving {}", root.display()))
}

/// `https://Host/Owner/Repo.git`, `git@host:Owner/Repo` and `ssh://git@host:22/Owner/Repo` all become `host/owner/repo`.
fn normalize_remote(url: &str) -> String {
    let url = url.trim();
    let (authority, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').unwrap_or((rest, "")),
        None => url.split_once(':').unwrap_or((url, "")),
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if url.contains("://") {
        host.split(':').next().unwrap_or(host)
    } else {
        host
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    format!("{host}/{path}").to_lowercase()
}

pub fn project_key(root: &Path, config: &RepoConfig) -> Result<ProjectKey> {
    if let Some(key) = &config.project {
        return Ok(key.clone());
    }
    let repo = gix::open(root).map_err(Error::git)?;
    let origin = repo
        .config_snapshot()
        .string("remote.origin.url")
        .map(|v| v.to_string())
        .ok_or_else(|| {
            Error::Refused(
                "no project key: set `project` in .baton/config.toml or add an `origin` remote"
                    .into(),
            )
        })?;
    ProjectKey::parse(&normalize_remote(&origin))
}

impl Worktree {
    /// Loads the saved mirror when its tree is still in the side repo (`tree_known`); otherwise the
    /// worktree starts empty and the first `rescan` makes every file dirty.
    pub fn open(root: &Path, dirs: &Dirs, tree_known: &dyn Fn(TreeId) -> bool) -> Result<Self> {
        let settings = load_settings(root, dirs)?;
        let repo_config = RepoConfig::load(root)?;
        let project = project_key(root, &repo_config)?;
        let policy = StorePolicy::open(root, &settings)?;
        let policy_fingerprint = fingerprint(root, dirs, &settings, &policy);
        let state_file = dirs.worktree_state_file(root);
        let mut worktree = Self {
            root: root.to_owned(),
            dirs: dirs.clone(),
            project,
            head_files: policy.rules().tracked(),
            settings,
            policy,
            policy_fingerprint,
            tracked: BTreeMap::new(),
            skipped: BTreeMap::new(),
            notices: Vec::new(),
            tree: TreeId::empty(),
            state_file,
        };
        worktree.notices = worktree.policy.notices().to_vec();
        worktree.load_state(tree_known);
        Ok(worktree)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn project(&self) -> &ProjectKey {
        &self.project
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn policy(&self) -> &StorePolicy {
        &self.policy
    }

    pub fn tree(&self) -> TreeId {
        self.tree
    }

    pub fn case(&self) -> Case {
        self.policy.case()
    }

    /// The paths the last `prepare` left out, as a turn names them: a path with a secret in it by its
    /// redacted name.
    pub fn skipped(&self) -> Vec<SkippedPath> {
        let labels: BTreeSet<SkippedPath> = self
            .skipped
            .iter()
            .filter_map(|(path, why)| match why {
                SkipKind::Size => RelPath::parse(path).ok().map(|p| SkippedPath::of(&p)),
                SkipKind::Name => {
                    let label = self.policy.redact_text(path);
                    (label != *path)
                        .then(|| SkippedPath::parse(&label).ok())
                        .flatten()
                }
            })
            .collect();
        labels.into_iter().collect()
    }

    /// What loading the settings ignored or dropped since the last call.
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    pub fn head(&self) -> Option<GitSha> {
        self.policy.rules().head()
    }

    pub fn tracked_files(&self) -> usize {
        self.tracked.len()
    }

    fn load_state(&mut self, tree_known: &dyn Fn(TreeId) -> bool) {
        let Ok(text) = std::fs::read(&self.state_file) else {
            return;
        };
        let Ok(saved) = serde_json::from_slice::<SavedState>(&text) else {
            return;
        };
        if saved.version != STATE_VERSION
            || saved.project != self.project
            || !tree_known(saved.tree)
        {
            return;
        }
        self.tree = saved.tree;
        self.tracked = saved
            .entries
            .into_iter()
            .filter(|e| RelPath::parse(&e.path).is_ok())
            .map(|e| {
                let tracked = Tracked {
                    kind: e.kind.into(),
                    blob: e.blob,
                    size: e.size,
                    mtime_ns: e.mtime_ns,
                    racy: e.racy,
                };
                (e.path, tracked)
            })
            .collect();
        self.skipped = saved
            .skipped
            .into_iter()
            .map(|p| (p, SkipKind::Size))
            .collect();
        if saved.policy != self.policy_fingerprint {
            self.invalidate();
        }
    }

    /// The stat cache says a file is unchanged, not that it was stored under today's policy. Marking every
    /// entry racy makes the next rescan read it again.
    fn invalidate(&mut self) {
        for tracked in self.tracked.values_mut() {
            tracked.racy = true;
        }
    }

    /// Writes the mirror next to the side repo, atomically, so a crash leaves the old state or the new one.
    pub fn save_state(&self) -> Result<()> {
        let saved = SavedState {
            version: STATE_VERSION,
            project: self.project.clone(),
            tree: self.tree,
            entries: self
                .tracked
                .iter()
                .map(|(path, t)| SavedEntry {
                    path: path.clone(),
                    kind: t.kind.into(),
                    blob: t.blob,
                    size: t.size,
                    mtime_ns: t.mtime_ns,
                    racy: t.racy,
                })
                .collect(),
            skipped: self
                .skipped
                .iter()
                .filter(|(_, why)| **why == SkipKind::Size)
                .map(|(path, _)| path.clone())
                .collect(),
            policy: self.policy_fingerprint.clone(),
        };
        let bytes = serde_json::to_vec(&saved).map_err(|e| Error::Git(e.to_string()))?;
        let dir = self.state_file.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).ctx(|| format!("creating {}", dir.display()))?;
        let tmp = self.state_file.with_extension("json.tmp");
        std::fs::write(&tmp, bytes).ctx(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.state_file)
            .ctx(|| format!("replacing {}", self.state_file.display()))
    }

    /// Rebuilds the policy when any file it is built from changed, and says so: every stored file must then
    /// be read again, and the set of files that belong in the tree may differ.
    fn refresh_policy(&mut self) -> Result<bool> {
        let now = fingerprint(&self.root, &self.dirs, &self.settings, &self.policy);
        if now == self.policy_fingerprint {
            return Ok(false);
        }
        let settings = load_settings(&self.root, &self.dirs)?;
        let policy = StorePolicy::open(&self.root, &settings)?;
        self.policy_fingerprint = fingerprint(&self.root, &self.dirs, &settings, &policy);
        self.notices.extend(policy.notices().iter().cloned());
        self.settings = settings;
        self.policy = policy;
        self.invalidate();
        Ok(true)
    }

    /// Whether baton stores each `(path relative to the root, is a directory)`: not under `.git`, not
    /// ignored by git unless git tracks it, not excluded by the config. This is the one answer, asked again
    /// whenever a path is read, so a stale watcher event or stat cache never overrides it.
    pub fn admits(&self, candidates: &[(&str, bool)]) -> Result<Vec<bool>> {
        let mut out = vec![false; candidates.len()];
        let mut asked: Vec<(&str, bool)> = Vec::new();
        let mut slots = Vec::new();
        for (i, (path, is_dir)) in candidates.iter().enumerate() {
            let first = path.split('/').next().unwrap_or("");
            if first.eq_ignore_ascii_case(".git") {
                continue;
            }
            asked.push((*path, *is_dir));
            slots.push(i);
        }
        for (slot, admitted) in slots.into_iter().zip(self.policy.admits(&asked)?) {
            out[slot] = admitted;
        }
        Ok(out)
    }

    /// The files that belong in the tree under `prefix` (or everywhere), spelled as they are on disk: the
    /// directory walk, plus the files HEAD tracks that the walk skipped because an ignore rule matched.
    fn scan(&self, prefix: Option<&str>) -> Result<Vec<String>> {
        let mut found = self.policy.rules().walk(prefix)?;
        let walked: HashSet<String> = found.iter().cloned().collect();
        let below = prefix.map(|p| format!("{p}/"));
        for (path, _) in self.policy.rules().tracked().iter() {
            if walked.contains(path) || below.as_ref().is_some_and(|b| !path.starts_with(b)) {
                continue;
            }
            let on_disk = std::fs::symlink_metadata(self.root.join(path))
                .ok()
                .and_then(|m| file_kind(&m));
            if on_disk.is_some() {
                found.push(path.to_owned());
            }
        }
        found.retain(|p| !self.policy.excluded_by_config(p) && RelPath::parse(p).is_ok());
        Ok(found)
    }

    /// Every path whose state on disk differs from the mirror: new files, vanished files, and files whose
    /// size or timestamp changed or that were too recent to trust when last hashed.
    pub fn rescan(&self) -> Result<Vec<String>> {
        let mut dirty = Vec::new();
        let mut found: HashSet<String> = HashSet::new();
        for path in self.scan(None)? {
            let unchanged = self
                .tracked
                .get(&path)
                .is_some_and(|t| self.fresh(&path, t));
            if !unchanged {
                dirty.push(path.clone());
            }
            found.insert(path);
        }
        for path in self.tracked.keys().chain(self.skipped.keys()) {
            if !found.contains(path) {
                dirty.push(path.clone());
            }
        }
        Ok(dirty)
    }

    fn fresh(&self, path: &str, tracked: &Tracked) -> bool {
        if tracked.racy {
            return false;
        }
        std::fs::symlink_metadata(self.root.join(path))
            .is_ok_and(|m| m.len() == tracked.size && mtime_ns(&m) == tracked.mtime_ns)
    }

    /// Takes up to `limits` paths out of `dirty` and works out what to store for them.
    pub fn prepare(&mut self, dirty: &mut DirtySet, limits: ChunkLimits) -> Result<Plan> {
        if self.refresh_policy()? {
            dirty.overflow();
        }
        self.follow_head(dirty);
        if dirty.overflow {
            let found = self.rescan()?;
            dirty.overflow = false;
            dirty.absorb(found);
        }
        self.expand_directories(dirty)?;

        let mut batch: Vec<String> = Vec::new();
        while batch.len() < limits.files {
            let Some(path) = dirty.paths.pop_first() else {
                break;
            };
            dirty.structural.remove(&path);
            batch.push(path);
        }
        let asked: Vec<(&str, bool)> = batch.iter().map(|p| (p.as_str(), false)).collect();
        let admitted = self.admits(&asked)?;

        let mut plan = Plan {
            blobs: Vec::new(),
            edits: Vec::new(),
            updates: Vec::new(),
            taken: Vec::new(),
        };
        let mut queued: HashSet<BlobId> = HashSet::new();
        let mut bytes = 0u64;
        let mut nested: HashMap<String, bool> = HashMap::new();
        let mut listings: Listings = HashMap::new();
        let mut rest = batch.into_iter().zip(admitted);
        for (path, admitted) in rest.by_ref() {
            plan.taken.push(path.clone());
            let ctx = Inspect {
                admitted,
                nested: &mut nested,
                listings: &mut listings,
            };
            bytes += self.inspect(&path, ctx, &mut queued, &mut plan)?;
            if bytes >= limits.bytes {
                break;
            }
        }
        dirty.paths.extend(rest.map(|(path, _)| path));
        Ok(plan)
    }

    /// HEAD moved: a file it started tracking is no longer subject to ignore rules, and one it stopped
    /// tracking is again. Neither changed on disk, so no watcher event says so.
    fn follow_head(&mut self, dirty: &mut DirtySet) {
        let now = self.policy.rules().tracked();
        if Arc::ptr_eq(&now, &self.head_files) {
            return;
        }
        for path in now.differing(&self.head_files) {
            dirty.mark(path.to_owned(), false);
        }
        self.head_files = now;
    }

    /// A created, removed or renamed directory is a whole subtree of changes: file events inside it may
    /// never arrive. Re-read the subtree, and treat every tracked file under it that is gone as dirty.
    fn expand_directories(&self, dirty: &mut DirtySet) -> Result<()> {
        let structural: Vec<String> = std::mem::take(&mut dirty.structural).into_iter().collect();
        for dir in structural {
            let abs = self.root.join(&dir);
            let is_dir = std::fs::symlink_metadata(&abs).is_ok_and(|m| m.is_dir());
            if !is_dir {
                for gone in self.tracked_under(&dir) {
                    dirty.paths.insert(gone);
                }
                continue;
            }
            let found: HashSet<String> = self.scan(Some(&dir))?.into_iter().collect();
            for gone in self
                .tracked_under(&dir)
                .into_iter()
                .filter(|p| !found.contains(p))
            {
                dirty.paths.insert(gone);
            }
            dirty.paths.extend(found);
        }
        Ok(())
    }

    fn tracked_under(&self, dir: &str) -> Vec<String> {
        let (from, to) = (format!("{dir}/"), format!("{dir}0"));
        self.tracked
            .range(from..to)
            .map(|(p, _)| p.clone())
            .chain(
                self.skipped
                    .keys()
                    .filter(|p| p.starts_with(&format!("{dir}/")))
                    .cloned(),
            )
            .collect()
    }

    fn inside_nested_repo(&self, path: &str, memo: &mut HashMap<String, bool>) -> bool {
        let mut dir = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            if !dir.is_empty() {
                dir.push('/');
            }
            dir.push_str(part);
            let nested = *memo
                .entry(dir.clone())
                .or_insert_with(|| self.root.join(&dir).join(".git").exists());
            if nested {
                return true;
            }
        }
        false
    }

    /// Whether every component of `path` is spelled on disk exactly as given. On a case-insensitive
    /// filesystem a stat of `Foo.txt` succeeds when the file is now `foo.txt`; only a directory listing
    /// says which name it really has.
    fn spelled_exactly(&self, path: &str, listings: &mut Listings) -> bool {
        if !cfg!(any(windows, target_os = "macos")) {
            return true;
        }
        let mut dir = String::new();
        for part in path.split('/') {
            let names = listings.entry(dir.clone()).or_insert_with(|| {
                std::fs::read_dir(self.root.join(&dir)).ok().map(|entries| {
                    entries
                        .filter_map(|e| e.ok().map(|e| e.file_name()))
                        .collect()
                })
            });
            if !names
                .as_ref()
                .is_some_and(|names| names.contains(std::ffi::OsStr::new(part)))
            {
                return false;
            }
            if !dir.is_empty() {
                dir.push('/');
            }
            dir.push_str(part);
        }
        true
    }

    /// Decides one path. Returns the number of bytes it queued.
    fn inspect(
        &self,
        path: &str,
        ctx: Inspect<'_>,
        queued: &mut HashSet<BlobId>,
        plan: &mut Plan,
    ) -> Result<u64> {
        let Inspect {
            admitted,
            nested,
            listings,
        } = ctx;
        let Ok(rel) = RelPath::parse(path) else {
            return Ok(0);
        };
        if !admitted {
            self.forget(path, &rel, plan);
            return Ok(0);
        }
        let tracked = self.tracked.get(path);
        let meta = std::fs::symlink_metadata(self.root.join(path)).ok();
        let kind = meta.as_ref().and_then(file_kind);

        let (Some(meta), Some(kind)) = (meta, kind) else {
            self.forget(path, &rel, plan);
            return Ok(0);
        };
        if !self.spelled_exactly(path, listings) || self.inside_nested_repo(path, nested) {
            self.forget(path, &rel, plan);
            return Ok(0);
        }
        if self.policy.holds_secret(path) {
            self.forget(path, &rel, plan);
            plan.updates
                .push(Update::Skip(path.to_owned(), SkipKind::Name));
            return Ok(0);
        }
        if self.policy.exceeds_size_limit(meta.len()) {
            self.forget(path, &rel, plan);
            plan.updates
                .push(Update::Skip(path.to_owned(), SkipKind::Size));
            return Ok(0);
        }
        let (size, mtime) = (meta.len(), mtime_ns(&meta));
        if let Some(t) = tracked
            && !t.racy
            && t.size == size
            && t.mtime_ns == mtime
            && t.kind == kind
        {
            return Ok(0);
        }
        let Some(raw) = read_content(&self.root.join(path), kind) else {
            // Vanished between the stat and the read, or unreadable: leave what is stored alone.
            return Ok(0);
        };
        let content = self.policy.prepare(&rel, raw);
        let blob = hash_blob(&content);
        let racy = now_ns().saturating_sub(mtime) < RACY_WINDOW_NS;
        let entry = Tracked {
            kind,
            blob,
            size,
            mtime_ns: mtime,
            racy,
        };
        let changed = tracked.is_none_or(|t| t.blob != blob || t.kind != kind);
        let mut queued_bytes = 0;
        if changed {
            plan.edits.push(TreeEdit {
                path: rel,
                to: Some((kind, blob)),
            });
            if queued.insert(blob) {
                queued_bytes = content.len() as u64;
                plan.blobs.push(BlobPayload {
                    id: blob,
                    bytes: content,
                });
            }
        }
        plan.updates.push(Update::Set(path.to_owned(), entry));
        plan.updates.push(Update::Unskip(path.to_owned()));
        Ok(queued_bytes)
    }

    fn forget(&self, path: &str, rel: &RelPath, plan: &mut Plan) {
        if self.tracked.contains_key(path) {
            plan.edits.push(TreeEdit {
                path: rel.clone(),
                to: None,
            });
            plan.updates.push(Update::Clear(path.to_owned()));
        }
        if self.skipped.contains_key(path) {
            plan.updates.push(Update::Unskip(path.to_owned()));
        }
    }

    /// The side repo applied `plan` and produced `tree`.
    pub fn accept(&mut self, plan: Plan, tree: TreeId) {
        for update in plan.updates {
            match update {
                Update::Set(path, tracked) => {
                    self.tracked.insert(path, tracked);
                }
                Update::Clear(path) => {
                    self.tracked.remove(&path);
                }
                Update::Skip(path, why) => {
                    self.skipped.insert(path, why);
                }
                Update::Unskip(path) => {
                    self.skipped.remove(&path);
                }
            }
        }
        self.tree = tree;
    }

    /// Applying `plan` failed: put its paths back so the next drain retries them.
    pub fn abandon(plan: Plan, dirty: &mut DirtySet) {
        for path in plan.taken {
            dirty.mark(path, false);
        }
    }

    pub fn tracked_paths(&self) -> impl Iterator<Item = &str> {
        self.tracked.keys().map(String::as_str)
    }

    /// What the last snapshot stored for `path`. Rewind compares contents against this instead of
    /// timestamps, which cannot tell a file written twice within one tick.
    pub fn stored_blob(&self, path: &RelPath) -> Option<BlobId> {
        self.tracked.get(path.as_str()).map(|t| t.blob)
    }

    pub fn current(&self, path: &RelPath) -> Current {
        let abs = self.root.join(path.as_str());
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Current::Absent,
            Err(_) => return Current::Blocked,
        };
        match file_kind(&meta).and_then(|kind| read_content(&abs, kind)) {
            Some(bytes) => Current::Content(bytes),
            None => Current::Blocked,
        }
    }
}

/// What is on disk at a path, as rewind needs to tell it apart.
pub enum Current {
    Absent,
    Content(Vec<u8>),
    /// A directory, a special file, or something that cannot be read.
    Blocked,
}

type Listings = HashMap<String, Option<HashSet<OsString>>>;

/// What `prepare` learned about a path before `inspect` reads it.
struct Inspect<'a> {
    admitted: bool,
    nested: &'a mut HashMap<String, bool>,
    listings: &'a mut Listings,
}

fn file_kind(meta: &Metadata) -> Option<FileKind> {
    let kind = meta.file_type();
    if kind.is_symlink() {
        return Some(FileKind::Symlink);
    }
    if !kind.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return Some(FileKind::Executable);
        }
    }
    Some(FileKind::File)
}

fn read_content(path: &Path, kind: FileKind) -> Option<Vec<u8>> {
    match kind {
        FileKind::Symlink => {
            let target = std::fs::read_link(path).ok()?;
            Some(target.to_string_lossy().replace('\\', "/").into_bytes())
        }
        FileKind::File | FileKind::Executable => std::fs::read(path).ok(),
    }
}

pub(crate) fn hash_blob(bytes: &[u8]) -> BlobId {
    let id = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
        .unwrap_or_else(|_| gix::ObjectId::null(gix::hash::Kind::Sha1));
    BlobId::from_gix(id)
}
