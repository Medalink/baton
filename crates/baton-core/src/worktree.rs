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
    /// The path the tree holds the file under: the disk's spelling when it was last stored.
    path: RelPath,
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
    /// Untracked, and the name looks like a secret's. Holds the built-in rule that matched. Not saved: the
    /// next scan finds the file again and says the same.
    Secret(&'static str),
}

struct Skipped {
    path: String,
    why: SkipKind,
}

/// Every update names a file by its key (`Case::key`), so two spellings of one file are one entry.
enum Update {
    Set(String, Tracked),
    Clear(String),
    Skip(String, Skipped),
    Unskip(String),
}

/// A dirty path as `prepare` resolved it against the disk.
struct Pending {
    /// What the dirty set held, to put back if the plan is abandoned.
    taken: String,
    /// The path spelled as the disk spells it; as given when nothing is there.
    spelling: String,
    key: String,
    on_disk: bool,
    /// False for a path whose file an earlier one in the batch already named under another spelling.
    first: bool,
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
    /// Keyed by `Case::key`.
    tracked: BTreeMap<String, Tracked>,
    skipped: BTreeMap<String, Skipped>,
    /// What loading the policy ignored and which paths it newly left out, until `take_notices` hands it to the log.
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
    bytes.extend_from_slice(crate::secret_names::describe().as_bytes());
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

/// A second policy for `root`, for a reader that must treat content the way a snapshot does.
pub fn open_policy(root: &Path, dirs: &Dirs) -> Result<StorePolicy> {
    StorePolicy::open(root, &load_settings(root, dirs)?)
}

/// The same policy with the fingerprint of everything it depends on, for a reader that caches what it
/// derived from stored content.
pub(crate) fn open_policy_fingerprinted(root: &Path, dirs: &Dirs) -> Result<(StorePolicy, String)> {
    let settings = load_settings(root, dirs)?;
    let policy = StorePolicy::open(root, &settings)?;
    let print = fingerprint(root, dirs, &settings, &policy);
    Ok((policy, print))
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
    real(root)
}

/// Where a path sits in git: the worktree it is in (none inside a bare repository) and the repository's
/// common git dir, which every linked worktree of one repository shares.
#[derive(Clone, Debug)]
pub struct RepoLocation {
    pub workdir: Option<PathBuf>,
    pub common_dir: PathBuf,
}

/// Reads only. Both paths are real paths without the Windows verbatim prefix, so they compare equal to
/// the spellings agents report.
pub fn repo_of(path: &Path) -> Result<RepoLocation> {
    let repo = gix::discover(path).map_err(Error::git)?;
    Ok(RepoLocation {
        workdir: repo.workdir().map(real).transpose()?,
        common_dir: real(repo.common_dir())?,
    })
}

fn real(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .map(|p| crate::paths::simplified(&p))
        .ctx(|| format!("resolving {}", path.display()))
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
    let config = repo.config_snapshot();
    let url = |name: &str| {
        config
            .string(format!("remote.{name}.url").as_str())
            .map(|v| v.to_string())
    };
    // Only remotes the repo's own config names count: one in the user's global config would key the
    // same repo differently, or not at all, on another machine. `origin` wins; without it, a repo whose
    // only remote is named otherwise (`github`, `upstream`) still has one obvious key.
    let local: std::collections::BTreeSet<String> = config
        .plumbing()
        .sections_by_name_and_filter("remote", |meta| {
            matches!(
                meta.source,
                gix::config::Source::Local | gix::config::Source::Worktree
            )
        })
        .into_iter()
        .flatten()
        .filter_map(|section| section.header().subsection_name().map(|n| n.to_string()))
        .collect();
    if local.contains("origin")
        && let Some(origin) = url("origin")
    {
        return ProjectKey::parse(&normalize_remote(&origin));
    }
    let urls: Vec<String> = local.iter().filter_map(|name| url(name)).collect();
    match urls.as_slice() {
        [only] => ProjectKey::parse(&normalize_remote(only)),
        [] => Err(Error::Refused(
            "no project key: set `project` in .baton/config.toml or add an `origin` remote".into(),
        )),
        _ => Err(Error::Refused(format!(
            "no project key: {} remotes and none named `origin`; set `project` in .baton/config.toml \
             or name one of them `origin`",
            urls.len()
        ))),
    }
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
            .values()
            .filter_map(|Skipped { path, why }| match why {
                SkipKind::Size | SkipKind::Secret(_) => {
                    RelPath::parse(path).ok().map(|p| SkippedPath::of(&p))
                }
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
        let case = self.policy.case();
        let mut tracked = BTreeMap::new();
        for e in saved.entries {
            let Ok(path) = RelPath::parse(&e.path) else {
                continue;
            };
            let entry = Tracked {
                kind: e.kind.into(),
                blob: e.blob,
                size: e.size,
                mtime_ns: e.mtime_ns,
                racy: e.racy,
                path,
            };
            // Two spellings of one file mean the mirror predates this repo folding case, and the tree
            // holds both; keeping one would leave the other in it for good.
            if tracked
                .insert(case.key(entry.path.as_str()), entry)
                .is_some()
            {
                return;
            }
        }
        self.tree = saved.tree;
        self.tracked = tracked;
        self.skipped = saved
            .skipped
            .into_iter()
            .map(|path| {
                let skip = Skipped {
                    path,
                    why: SkipKind::Size,
                };
                (case.key(&skip.path), skip)
            })
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
                .values()
                .map(|t| SavedEntry {
                    path: t.path.as_str().to_owned(),
                    kind: t.kind.into(),
                    blob: t.blob,
                    size: t.size,
                    mtime_ns: t.mtime_ns,
                    racy: t.racy,
                })
                .collect(),
            skipped: self
                .skipped
                .values()
                .filter(|s| s.why == SkipKind::Size)
                .map(|s| s.path.clone())
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
        if policy.case() != self.policy.case() {
            // The mirror's keys mean something else now, so start from an empty tree and read everything.
            self.tracked.clear();
            self.skipped.clear();
            self.tree = TreeId::empty();
        }
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
        let walked: HashSet<String> = found.iter().map(|p| self.key(p)).collect();
        let below = prefix.map(|p| format!("{}/", self.key(p)));
        let head = self.policy.rules().tracked();
        let mut listings = Listings::new();
        for (path, key) in head.iter() {
            if below.as_ref().is_some_and(|b| !key.starts_with(b)) || walked.contains(key) {
                continue;
            }
            let Some(spelling) = self.resolve(path, &mut listings) else {
                continue;
            };
            let on_disk = std::fs::symlink_metadata(self.root.join(&spelling))
                .ok()
                .and_then(|m| file_kind(&m));
            if on_disk.is_some() {
                found.push(spelling);
            }
        }
        found.retain(|p| !self.policy.excluded_by_config(p) && RelPath::parse(p).is_ok());
        Ok(found)
    }

    /// Every path whose state on disk differs from the mirror: new files, vanished files, files spelled
    /// differently from how the tree holds them, and files whose size or timestamp changed or that were
    /// too recent to trust when last hashed.
    pub fn rescan(&self) -> Result<Vec<String>> {
        let mut dirty = Vec::new();
        let mut found: HashSet<String> = HashSet::new();
        for path in self.scan(None)? {
            let key = self.key(&path);
            let unchanged = self
                .tracked
                .get(&key)
                .is_some_and(|t| t.path.as_str() == path && self.fresh(&path, t));
            if !unchanged {
                dirty.push(path);
            }
            found.insert(key);
        }
        let stored = self.tracked.iter().map(|(k, t)| (k, t.path.as_str()));
        let skipped = self.skipped.iter().map(|(k, s)| (k, s.path.as_str()));
        for (key, path) in stored.chain(skipped) {
            if !found.contains(key) {
                dirty.push(path.to_owned());
            }
        }
        Ok(dirty)
    }

    /// What identifies `path`: two spellings of one file have one key.
    pub(crate) fn key(&self, path: &str) -> String {
        self.policy.case().key(path)
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
        // Two events for one file can name it in two cases, and a stat accepts either: spell each path as
        // the disk does and keep one per file.
        let mut listings = Listings::new();
        let mut named: HashSet<String> = HashSet::new();
        let pending: Vec<Pending> = batch
            .into_iter()
            .map(|taken| {
                let found = self.resolve(&taken, &mut listings);
                let spelling = found.clone().unwrap_or_else(|| taken.clone());
                let key = self.key(&spelling);
                Pending {
                    first: named.insert(key.clone()),
                    on_disk: found.is_some(),
                    taken,
                    spelling,
                    key,
                }
            })
            .collect();
        let asked: Vec<(&str, bool)> = pending
            .iter()
            .filter(|p| p.first)
            .map(|p| (p.spelling.as_str(), false))
            .collect();
        let mut admitted = self.admits(&asked)?.into_iter();

        let mut plan = Plan {
            blobs: Vec::new(),
            edits: Vec::new(),
            updates: Vec::new(),
            taken: Vec::new(),
        };
        let mut queued: HashSet<BlobId> = HashSet::new();
        let mut bytes = 0u64;
        let mut nested: HashMap<String, bool> = HashMap::new();
        let mut rest = pending.into_iter();
        for item in rest.by_ref() {
            plan.taken.push(item.taken.clone());
            if item.first {
                let admitted = admitted.next().unwrap_or(false);
                bytes += self.inspect(&item, admitted, &mut nested, &mut queued, &mut plan)?;
            }
            if bytes >= limits.bytes {
                break;
            }
        }
        dirty.paths.extend(rest.map(|item| item.taken));
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
                dirty
                    .paths
                    .extend(self.stored_under(&dir).into_iter().map(|(_, path)| path));
                continue;
            }
            let found = self.scan(Some(&dir))?;
            let found_keys: HashSet<String> = found.iter().map(|p| self.key(p)).collect();
            dirty.paths.extend(
                self.stored_under(&dir)
                    .into_iter()
                    .filter(|(key, _)| !found_keys.contains(key))
                    .map(|(_, path)| path),
            );
            dirty.paths.extend(found);
        }
        Ok(())
    }

    /// `(key, path as stored)` of every file stored or skipped below `dir`.
    fn stored_under(&self, dir: &str) -> Vec<(String, String)> {
        let base = self.key(dir);
        let range = format!("{base}/")..format!("{base}0");
        let stored = self
            .tracked
            .range(range.clone())
            .map(|(k, t)| (k.clone(), t.path.as_str().to_owned()));
        let skipped = self
            .skipped
            .range(range)
            .map(|(k, s)| (k.clone(), s.path.clone()));
        stored.chain(skipped).collect()
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

    /// The path spelled as the disk spells it, or `None` when nothing is there. A stat cannot tell: on a
    /// case-insensitive file system it succeeds for any spelling, so the directory listings decide. When
    /// the repo ignores case a different spelling still names the file; otherwise it names nothing.
    fn resolve(&self, path: &str, listings: &mut Listings) -> Option<String> {
        let fold = self.policy.case() == Case::Fold;
        if !fold && !cfg!(any(windows, target_os = "macos")) {
            return Some(path.to_owned());
        }
        let mut spelled = String::with_capacity(path.len());
        for part in path.split('/') {
            let names = listings
                .entry(spelled.clone())
                .or_insert_with(|| {
                    std::fs::read_dir(self.root.join(&spelled))
                        .ok()
                        .map(|entries| {
                            entries
                                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                                .collect()
                        })
                })
                .as_ref()?;
            let name = if names.contains(part) {
                part
            } else if fold {
                names.iter().find(|n| n.eq_ignore_ascii_case(part))?
            } else {
                return None;
            };
            if !spelled.is_empty() {
                spelled.push('/');
            }
            spelled.push_str(name);
        }
        Some(spelled)
    }

    /// Decides one path. Returns the number of bytes it queued.
    fn inspect(
        &self,
        item: &Pending,
        admitted: bool,
        nested: &mut HashMap<String, bool>,
        queued: &mut HashSet<BlobId>,
        plan: &mut Plan,
    ) -> Result<u64> {
        let Pending {
            spelling,
            key,
            on_disk,
            ..
        } = item;
        let Ok(rel) = RelPath::parse(spelling) else {
            return Ok(0);
        };
        if !admitted || !on_disk {
            self.forget(key, plan);
            return Ok(0);
        }
        let tracked = self.tracked.get(key);
        let meta = std::fs::symlink_metadata(self.root.join(spelling)).ok();
        let kind = meta.as_ref().and_then(file_kind);

        let (Some(meta), Some(kind)) = (meta, kind) else {
            self.forget(key, plan);
            return Ok(0);
        };
        if self.inside_nested_repo(spelling, nested) {
            self.forget(key, plan);
            return Ok(0);
        }
        let skip = |why| {
            Update::Skip(
                key.clone(),
                Skipped {
                    path: spelling.clone(),
                    why,
                },
            )
        };
        if self.policy.holds_secret(spelling) {
            self.forget(key, plan);
            plan.updates.push(skip(SkipKind::Name));
            return Ok(0);
        }
        if !self.head_files.contains(spelling)
            && let Some(rule) = self.policy.secret_named(spelling)
        {
            self.forget(key, plan);
            plan.updates.push(skip(SkipKind::Secret(rule)));
            return Ok(0);
        }
        if self.policy.exceeds_size_limit(meta.len()) {
            self.forget(key, plan);
            plan.updates.push(skip(SkipKind::Size));
            return Ok(0);
        }
        let (size, mtime) = (meta.len(), mtime_ns(&meta));
        let respelled = tracked.is_some_and(|t| t.path != rel);
        let same_stat =
            tracked.filter(|t| !t.racy && t.size == size && t.mtime_ns == mtime && t.kind == kind);
        if same_stat.is_some() && !respelled {
            return Ok(0);
        }
        // A rename of an unchanged file reuses the stored blob instead of reading the file again.
        let (blob, content, racy) = match same_stat {
            Some(t) => (t.blob, None, false),
            None => {
                let Some(raw) = read_content(&self.root.join(spelling), kind) else {
                    // Vanished between the stat and the read, or unreadable: leave what is stored alone.
                    return Ok(0);
                };
                let content = self.policy.prepare(&rel, raw);
                let name = spelling.rsplit('/').next().unwrap_or_default();
                if crate::names::check_file(name.as_bytes(), kind.entry_kind() as u32, &content)
                    .is_err()
                {
                    self.forget(key, plan);
                    return Ok(0);
                }
                let racy = now_ns().saturating_sub(mtime) < RACY_WINDOW_NS;
                (hash_blob(&content), Some(content), racy)
            }
        };
        let entry = Tracked {
            path: rel.clone(),
            kind,
            blob,
            size,
            mtime_ns: mtime,
            racy,
        };
        let new_blob = tracked.is_none_or(|t| t.blob != blob);
        let mut queued_bytes = 0;
        if respelled || new_blob || tracked.is_some_and(|t| t.kind != kind) {
            if let Some(old) = tracked.filter(|_| respelled) {
                plan.edits.push(TreeEdit {
                    path: old.path.clone(),
                    to: None,
                });
            }
            plan.edits.push(TreeEdit {
                path: rel,
                to: Some((kind, blob)),
            });
            if let Some(content) = content.filter(|_| new_blob)
                && queued.insert(blob)
            {
                queued_bytes = content.len() as u64;
                plan.blobs.push(BlobPayload {
                    id: blob,
                    bytes: content,
                });
            }
        }
        plan.updates.push(Update::Set(key.clone(), entry));
        plan.updates.push(Update::Unskip(key.clone()));
        Ok(queued_bytes)
    }

    fn forget(&self, key: &str, plan: &mut Plan) {
        if let Some(t) = self.tracked.get(key) {
            plan.edits.push(TreeEdit {
                path: t.path.clone(),
                to: None,
            });
            plan.updates.push(Update::Clear(key.to_owned()));
        }
        if self.skipped.contains_key(key) {
            plan.updates.push(Update::Unskip(key.to_owned()));
        }
    }

    /// The side repo applied `plan` and produced `tree`.
    pub fn accept(&mut self, plan: Plan, tree: TreeId) {
        for update in plan.updates {
            match update {
                Update::Set(key, tracked) => {
                    self.tracked.insert(key, tracked);
                }
                Update::Clear(key) => {
                    self.tracked.remove(&key);
                }
                Update::Skip(key, skipped) => {
                    if let SkipKind::Secret(rule) = skipped.why
                        && self
                            .skipped
                            .get(&key)
                            .is_none_or(|old| old.why != skipped.why)
                    {
                        self.notices.push(format!(
                            "{} not stored: untracked, and its name looks like a secret's (matches {rule});                              to store it, list it in [snapshot] store_secret_named",
                            skipped.path
                        ));
                    }
                    self.skipped.insert(key, skipped);
                }
                Update::Unskip(key) => {
                    self.skipped.remove(&key);
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
        self.tracked.values().map(|t| t.path.as_str())
    }

    /// What the last snapshot stored for `path`. Rewind compares contents against this instead of
    /// timestamps, which cannot tell a file written twice within one tick.
    pub fn stored_blob(&self, path: &RelPath) -> Option<BlobId> {
        self.tracked.get(&self.key(path.as_str())).map(|t| t.blob)
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

/// The names in each directory already listed, by the directory's path as spelled on disk.
type Listings = HashMap<String, Option<HashSet<String>>>;

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
