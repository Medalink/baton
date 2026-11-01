//! Config files are untrusted input: every field is parsed into a branded or validated type here.
//! `RepoConfig` has no command field anywhere in its tree, so a config that arrives with `git clone`
//! cannot name a program to run, and `deny_unknown_fields` rejects one that tries.

use crate::error::{Error, IoContext, Result};
use crate::id::{MachineId, ProjectKey};
use crate::redact::RuleSpec;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) const DEFAULT_STORE_WHOLE: &[&str] = &[
    "*.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "go.sum",
    "bun.lockb",
];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSnapshot {
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    store_whole: Vec<String>,
    max_file_bytes: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMachine {
    id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUserRedact {
    #[serde(default)]
    value_files: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    url: Option<String>,
    token: Option<String>,
    token_file: Option<String>,
    ca_file: Option<String>,
}

/// The plug-in tables of the user config. `plugin_config` reads them; `UserConfig` only has to let them
/// pass, so a mistake in one never stops capture.
type PluginTable = Option<toml::Value>;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    dead_code,
    reason = "parsed so the file loads; `Plugins` reads these tables"
)]
struct RawUser {
    classify: PluginTable,
    decide: PluginTable,
    notify: PluginTable,
    lens: PluginTable,
    agents: PluginTable,
    #[serde(default)]
    machine: RawMachine,
    server: Option<RawServer>,
    #[serde(default)]
    snapshot: RawSnapshot,
    #[serde(default)]
    redact: RawUserRedact,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepoRedact {
    #[serde(default)]
    rule: Vec<RuleSpec>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepo {
    project: Option<String>,
    // Tables only the user config may hold. They parse, so a clone that carries one still records, and
    // they are never read: `RepoConfig` has no field for a command, a rule or a lens, and so cannot
    // hand one on.
    classify: PluginTable,
    decide: PluginTable,
    notify: PluginTable,
    lens: PluginTable,
    agents: PluginTable,
    #[serde(default)]
    snapshot: RawSnapshot,
    #[serde(default)]
    redact: RawRepoRedact,
}

#[derive(Clone, Debug, Default)]
pub struct SnapshotConfig {
    pub exclude: Vec<String>,
    pub store_whole: Vec<String>,
    pub max_file_bytes: Option<u64>,
}

impl From<RawSnapshot> for SnapshotConfig {
    fn from(raw: RawSnapshot) -> Self {
        Self {
            exclude: raw.exclude,
            store_whole: raw.store_whole,
            max_file_bytes: raw.max_file_bytes,
        }
    }
}

/// `~/.config/baton/config.toml`. Only this file can name `value_files`: literal secrets differ per
/// machine, so blob ids of a file containing one can differ between machines.
#[derive(Clone, Debug, Default)]
pub struct UserConfig {
    pub machine: Option<MachineId>,
    pub server: Option<ServerConfig>,
    pub snapshot: SnapshotConfig,
    pub value_files: Vec<PathBuf>,
}

/// `ws://host:port/path` or `wss://host:port/path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerUrl {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl ServerUrl {
    pub fn parse(raw: &str) -> Result<Self> {
        let bad = |why: &str| Error::invalid("server url", format!("{raw:?} {why}"));
        let (tls, rest) = match (raw.strip_prefix("ws://"), raw.strip_prefix("wss://")) {
            (Some(rest), _) => (false, rest),
            (_, Some(rest)) => (true, rest),
            _ => return Err(bad("must start with ws:// or wss://")),
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let default_port = if tls { 443 } else { 80 };
        let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
            let (host, after) = inner
                .split_once(']')
                .ok_or_else(|| bad("has an open '['"))?;
            match after.strip_prefix(':') {
                Some(port) => (host, port.parse().map_err(|_| bad("has a bad port"))?),
                None if after.is_empty() => (host, default_port),
                None => return Err(bad("has text after the address")),
            }
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host, port.parse().map_err(|_| bad("has a bad port"))?),
                None => (authority, default_port),
            }
        };
        if host.is_empty() || host.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(bad("has no host"));
        }
        Ok(Self {
            tls,
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        })
    }

    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn url(&self) -> String {
        let scheme = if self.tls { "wss" } else { "ws" };
        format!("{scheme}://{}{}", self.authority(), self.path)
    }
}

/// Where the device token comes from. The file form is read at each connect, so rotating a token
/// needs no restart.
#[derive(Clone)]
pub enum Token {
    Inline(String),
    File(PathBuf),
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inline(_) => f.write_str("Token::Inline(..)"),
            Self::File(path) => write!(f, "Token::File({})", path.display()),
        }
    }
}

impl Token {
    pub fn read(&self) -> Result<String> {
        let raw = match self {
            Self::Inline(token) => token.clone(),
            Self::File(path) => std::fs::read_to_string(path)
                .ctx(|| format!("reading the token file {}", path.display()))?,
        };
        let token = raw.trim();
        if token.is_empty() || token.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(Error::invalid("token", "is empty or has whitespace"));
        }
        Ok(token.to_owned())
    }
}

/// `[server]` of the user config. Only that file may name a server, so a cloned repo cannot point the
/// daemon at one.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub url: ServerUrl,
    pub token: Token,
    /// A private CA to trust for `wss://`, on top of the system roots.
    pub ca_file: Option<PathBuf>,
}

impl ServerConfig {
    fn from_raw(raw: RawServer, path: &Path) -> Result<Option<Self>> {
        let Some(url) = raw.url else {
            return Ok(None);
        };
        let token = match (raw.token, raw.token_file) {
            (Some(token), None) => Token::Inline(token),
            (None, Some(file)) => Token::File(expand_home(&file)),
            _ => {
                return Err(config_error(
                    path,
                    "[server] needs exactly one of token and token_file",
                ));
            }
        };
        Ok(Some(Self {
            url: ServerUrl::parse(&url).map_err(|e| config_error(path, e))?,
            token,
            ca_file: raw.ca_file.as_deref().map(expand_home),
        }))
    }
}

/// `<repo>/.baton/config.toml`, shared through git, so identical on every machine.
#[derive(Clone, Debug, Default)]
pub struct RepoConfig {
    pub project: Option<ProjectKey>,
    pub snapshot: SnapshotConfig,
    pub rules: Vec<RuleSpec>,
    /// What the file held that only the user config may hold; the daemon logs it.
    pub notices: Vec<String>,
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).ctx(|| format!("reading {}", path.display())),
    }
}

fn config_error(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::Config {
        path: path.to_owned(),
        reason: e.to_string(),
    }
}

fn expand_home(raw: &str) -> PathBuf {
    match raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        Some(rest) => dirs::home_dir().map_or_else(|| PathBuf::from(raw), |h| h.join(rest)),
        None => PathBuf::from(raw),
    }
}

impl UserConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let Some(text) = read_optional(path)? else {
            return Ok(Self::default());
        };
        let raw: RawUser = toml::from_str(&text).map_err(|e| config_error(path, e))?;
        Ok(Self {
            machine: raw
                .machine
                .id
                .as_deref()
                .map(MachineId::parse)
                .transpose()?,
            server: raw
                .server
                .map(|server| ServerConfig::from_raw(server, path))
                .transpose()?
                .flatten(),
            snapshot: raw.snapshot.into(),
            value_files: raw
                .redact
                .value_files
                .iter()
                .map(|p| expand_home(p))
                .collect(),
        })
    }
}

impl RepoConfig {
    pub fn load(worktree_root: &Path) -> Result<Self> {
        let path = worktree_root.join(".baton").join("config.toml");
        let Some(text) = read_optional(&path)? else {
            return Ok(Self::default());
        };
        let raw: RawRepo = toml::from_str(&text).map_err(|e| config_error(&path, e))?;
        let notices = [
            ("classify", &raw.classify),
            ("decide", &raw.decide),
            ("notify", &raw.notify),
            ("lens", &raw.lens),
            ("agents", &raw.agents),
        ]
        .into_iter()
        .filter(|(_, table)| table.is_some())
        .map(|(name, _)| {
            format!(".baton/config.toml sets [{name}], which only the user config may; ignored")
        })
        .collect();
        Ok(Self {
            notices,
            project: raw.project.as_deref().map(ProjectKey::parse).transpose()?,
            snapshot: raw.snapshot.into(),
            rules: raw.redact.rule,
        })
    }
}

/// The user and repo configs merged: exclusions add up, the size limit is the smaller, and only the user's
/// `store_whole` counts. A repo config arrives with `git clone`, so it may add redaction and take files
/// out of the store, never put a file in unredacted.
#[derive(Clone, Debug)]
pub struct Settings {
    pub machine: MachineId,
    pub exclude: Vec<String>,
    pub store_whole: Vec<String>,
    pub max_file_bytes: u64,
    pub rules: Vec<RuleSpec>,
    pub value_files: Vec<PathBuf>,
    /// What the merge ignored; the daemon logs these.
    pub notices: Vec<String>,
}

impl Settings {
    pub fn merge(user: &UserConfig, repo: &RepoConfig) -> Self {
        let mut exclude = user.snapshot.exclude.clone();
        exclude.extend(repo.snapshot.exclude.iter().cloned());
        let mut store_whole: Vec<String> = DEFAULT_STORE_WHOLE
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        store_whole.extend(user.snapshot.store_whole.iter().cloned());
        let mut notices = repo.notices.clone();
        if !repo.snapshot.store_whole.is_empty() {
            notices.push(
                ".baton/config.toml sets store_whole, which only the user config may; ignored"
                    .into(),
            );
        }
        let limits = [user.snapshot.max_file_bytes, repo.snapshot.max_file_bytes];
        Self {
            notices,
            machine: user.machine.clone().unwrap_or_else(MachineId::from_host),
            exclude,
            store_whole,
            max_file_bytes: limits
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(DEFAULT_MAX_FILE_BYTES),
            rules: repo.rules.clone(),
            value_files: user.value_files.clone(),
        }
    }
}
