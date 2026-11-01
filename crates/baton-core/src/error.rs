use std::fmt::Display;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid {what}: {reason}")]
    Invalid { what: &'static str, reason: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("git: {0}")]
    Git(String),
    #[error("{path}: {reason}")]
    Config { path: PathBuf, reason: String },
    #[error("{0}")]
    Refused(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn invalid(what: &'static str, reason: impl Display) -> Self {
        Self::Invalid {
            what,
            reason: reason.to_string(),
        }
    }

    pub(crate) fn git(err: impl Display) -> Self {
        Self::Git(err.to_string())
    }

    pub(crate) fn io(context: impl Display, source: std::io::Error) -> Self {
        Self::Io {
            context: context.to_string(),
            source,
        }
    }
}

pub(crate) trait IoContext<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::io(context(), source))
    }
}
