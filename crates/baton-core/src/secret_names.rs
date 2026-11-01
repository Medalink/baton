//! Untracked files whose names say they hold a secret. Redaction finds a secret by its shape, so a bare
//! value in a file such as `keys/console-password` passes straight through it; the name is all there is
//! to go on. Git tracks the files it should keep, so a tracked file is never asked about here.

use crate::error::Result;
use crate::ignore::Case;
use crate::policy::globs;
use globset::GlobSet;

/// Names that hold a secret whatever the extension is.
const HARD: &[&str] = &[
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.kdbx",
    "*.keystore",
    "*.jks",
    "*.token",
    ".env",
    ".env.*",
    "*.env",
    ".netrc",
    "_netrc",
    ".pgpass",
    "credentials",
    "credentials.json",
    "**/.ssh/**",
];

/// Names that say "secret" in a word or a directory. Source code and prose are exempt (see `SOURCE`): a new
/// `password_reset.rs` is ordinary work, and redaction reads its contents.
const SOFT: &[&str] = &[
    "*password*",
    "*passwd*",
    "*secret*",
    "id_rsa*",
    "id_dsa*",
    "id_ecdsa*",
    "id_ed25519*",
    "**/keys/**",
    "**/secrets/**",
];

/// Never a secret: public keys and the templates that go with `.env`.
const EXEMPT: &[&str] = &[
    "*.pub",
    ".env.example",
    ".env.sample",
    ".env.dist",
    ".env.template",
];

/// Extensions of source, markup and documents, which the `SOFT` names do not apply to.
const SOURCE: &[&str] = &[
    "rs", "py", "pyi", "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "go", "java", "kt",
    "kts", "swift", "rb", "php", "cs", "fs", "vb", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "m",
    "mm", "scala", "clj", "ex", "exs", "erl", "hs", "ml", "lua", "dart", "zig", "nim", "pl", "pm",
    "r", "jl", "vue", "svelte", "astro", "html", "htm", "css", "scss", "sass", "less", "md", "mdx",
    "rst", "adoc", "tex", "sh", "bash", "zsh", "fish", "ps1", "bat", "cmd", "sql", "graphql",
    "gql", "proto", "thrift",
];

/// Everything the verdict depends on, for the policy fingerprint.
pub(crate) fn describe() -> String {
    format!("hard={HARD:?} soft={SOFT:?} exempt={EXEMPT:?} source={SOURCE:?}\n",)
}

pub(crate) struct SecretNames {
    case: Case,
    allowed: GlobSet,
    exempt: GlobSet,
    hard: GlobSet,
    soft: GlobSet,
}

impl SecretNames {
    /// `allowed` is the user's `store_secret_named`. Under `core.ignorecase` every glob and every path is
    /// folded to ASCII lower case, as git folds; otherwise both are compared exactly.
    pub(crate) fn new(case: Case, allowed: &[String]) -> Result<Self> {
        let fold =
            |patterns: &[&str]| -> Vec<String> { patterns.iter().map(|p| case.key(p)).collect() };
        let folded_allowed: Vec<String> = allowed.iter().map(|p| case.key(p)).collect();
        Ok(Self {
            case,
            allowed: globs(&folded_allowed, "store_secret_named glob", true)?,
            exempt: globs(&fold(EXEMPT), "exempt names", false)?,
            hard: globs(&fold(HARD), "secret names", false)?,
            soft: globs(&fold(SOFT), "secret names", false)?,
        })
    }

    /// The rule that makes `path` look like a secret, or `None` when it is fine to store.
    pub(crate) fn rule(&self, path: &str) -> Option<&'static str> {
        let key = self.case.key(path);
        if self.allowed.is_match(&key) || self.exempt.is_match(&key) {
            return None;
        }
        if let Some(&i) = self.hard.matches(&key).first() {
            return Some(HARD[i]);
        }
        let name = key.rsplit('/').next().unwrap_or(&key);
        let source = name
            .rsplit_once('.')
            .is_some_and(|(_, ext)| SOURCE.contains(&ext));
        if source {
            return None;
        }
        self.soft.matches(&key).first().map(|&i| SOFT[i])
    }
}
