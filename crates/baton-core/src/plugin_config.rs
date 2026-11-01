//! The plug-in slots of the user config. They are read by their own loader and never by `UserConfig`,
//! so a typo in one cannot stop capture, and the repo config has no type that could hold one: nothing
//! that arrives with `git clone` can name a command.

use crate::error::{Error, IoContext, Result};
use crate::plugin::CommandSpec;
use crate::rules::{RuleConfig, RuleSet};
use crate::schema::Class;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const CLASSIFY_TIMEOUT: Duration = Duration::from_secs(10);
const DECIDE_TIMEOUT: Duration = Duration::from_secs(30);
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);
const LENS_TIMEOUT: Duration = Duration::from_secs(5);
const ADAPTER_TIMEOUT: Duration = Duration::from_secs(2);
const LONGEST_TIMEOUT: Duration = Duration::from_secs(600);

/// `"500ms"`, `"10s"` or `"2m"`. A bare number is refused, since it could mean seconds or milliseconds.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let bad = || {
        Error::invalid(
            "timeout",
            format!("{text:?} is not a duration like \"500ms\", \"10s\" or \"2m\""),
        )
    };
    let split = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (digits, unit) = text.split_at(split);
    let n: u64 = digits.parse().map_err(|_| bad())?;
    let duration = match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n.saturating_mul(60)),
        _ => return Err(bad()),
    };
    if duration.is_zero() || duration > LONGEST_TIMEOUT {
        return Err(Error::invalid(
            "timeout",
            format!("{text:?} is not between 1ms and 10m"),
        ));
    }
    Ok(duration)
}

fn timeout_or(text: Option<&str>, default: Duration) -> Result<Duration> {
    Ok(text.map(parse_duration).transpose()?.unwrap_or(default))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    command: Vec<String>,
    timeout: Option<String>,
}

impl RawCommand {
    fn build(self, default: Duration) -> Result<CommandSpec> {
        CommandSpec::new(self.command, timeout_or(self.timeout.as_deref(), default)?)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClassify {
    command: Option<Vec<String>>,
    timeout: Option<String>,
    mode: Option<Mode>,
    defaults: Option<bool>,
    #[serde(default)]
    rule: Vec<RuleConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNotify {
    on: Vec<Class>,
    command: Vec<String>,
    timeout: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLens {
    file: Option<String>,
    command: Option<Vec<String>>,
    timeout: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPlugins {
    classify: Option<RawClassify>,
    decide: Option<RawCommand>,
    #[serde(default)]
    notify: Vec<RawNotify>,
    lens: Option<RawLens>,
    #[serde(default)]
    agents: BTreeMap<String, RawCommand>,
}

/// Whether verdicts are made, and whether notifiers run on them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// No classification at all.
    Off,
    /// Verdicts are stored; no notifier runs.
    #[default]
    Shadow,
    Live,
}

pub enum Classifier {
    Rules(RuleSet),
    Command(CommandSpec),
}

pub struct Notify {
    pub on: Vec<Class>,
    pub command: CommandSpec,
}

pub enum Lens {
    /// Relative to the repository root, and never above it.
    File(PathBuf),
    Command(CommandSpec),
}

pub struct Plugins {
    pub mode: Mode,
    pub classifier: Classifier,
    pub decide: Option<CommandSpec>,
    pub notify: Vec<Notify>,
    pub lens: Option<Lens>,
    pub agents: BTreeMap<String, CommandSpec>,
}

impl Default for Plugins {
    fn default() -> Self {
        Self {
            mode: Mode::Shadow,
            classifier: Classifier::Rules(RuleSet::defaults()),
            decide: None,
            notify: Vec::new(),
            lens: None,
            agents: BTreeMap::new(),
        }
    }
}

fn lens_file(raw: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw);
    let climbs = path
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir));
    if raw.is_empty() || climbs {
        return Err(Error::invalid(
            "lens file",
            format!("{raw:?} must be a path inside the repository"),
        ));
    }
    Ok(path)
}

impl Plugins {
    /// The plug-in slots of the user config file at `path`. A file that is absent means the defaults.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).ctx(|| format!("reading {}", path.display())),
        };
        Self::parse(&text).map_err(|e| Error::Config {
            path: path.to_owned(),
            reason: e.to_string(),
        })
    }

    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawPlugins = toml::from_str(text).map_err(|e| Error::invalid("plug-ins", e))?;
        let mut plugins = Self::default();

        if let Some(classify) = raw.classify {
            plugins.mode = classify.mode.unwrap_or_default();
            plugins.classifier = match classify.command {
                Some(command) => {
                    if !classify.rule.is_empty() || classify.defaults.is_some() {
                        return Err(Error::invalid(
                            "classify",
                            "names a command, so rule and defaults would never apply",
                        ));
                    }
                    let timeout = timeout_or(classify.timeout.as_deref(), CLASSIFY_TIMEOUT)?;
                    Classifier::Command(CommandSpec::new(command, timeout)?)
                }
                None => {
                    let base = if classify.defaults.unwrap_or(true) {
                        RuleSet::defaults()
                    } else {
                        RuleSet::empty()
                    };
                    Classifier::Rules(base.with(&classify.rule)?)
                }
            };
        }
        plugins.decide = raw.decide.map(|c| c.build(DECIDE_TIMEOUT)).transpose()?;
        for notify in raw.notify {
            if notify.on.is_empty() {
                return Err(Error::invalid("notify", "`on` names no class"));
            }
            let timeout = timeout_or(notify.timeout.as_deref(), NOTIFY_TIMEOUT)?;
            plugins.notify.push(Notify {
                on: notify.on,
                command: CommandSpec::new(notify.command, timeout)?,
            });
        }
        plugins.lens = match raw.lens {
            None => None,
            Some(RawLens {
                file: Some(file),
                command: None,
                timeout: None,
            }) => Some(Lens::File(lens_file(&file)?)),
            Some(RawLens {
                file: None,
                command: Some(command),
                timeout,
            }) => {
                let timeout = timeout_or(timeout.as_deref(), LENS_TIMEOUT)?;
                Some(Lens::Command(CommandSpec::new(command, timeout)?))
            }
            Some(_) => {
                return Err(Error::invalid(
                    "lens",
                    "needs exactly one of `file` and `command` (and `timeout` only with `command`)",
                ));
            }
        };
        for (name, command) in raw.agents {
            plugins.agents.insert(name, command.build(ADAPTER_TIMEOUT)?);
        }
        Ok(plugins)
    }
}
