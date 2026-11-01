use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub fn bench_root() -> PathBuf {
    std::env::temp_dir().join("baton-bench")
}

pub fn fresh_dir(parent: &Path, name: &str) -> Res<PathBuf> {
    let dir = parent.join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Git never takes optional locks or refreshes the index of a repository this tool only reads.
pub fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

pub fn git_ok(dir: &Path, args: &[&str]) -> Res<Output> {
    let out = git(dir).args(args).output()?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(out)
}

pub fn arg_path(args: &[String], what: &str) -> Res<PathBuf> {
    args.first()
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing <{what}>").into())
}

/// Deterministic xorshift so every run writes the same bytes.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Source-like text: compresses about as well as real code does.
    pub fn text(&mut self, len: usize) -> Vec<u8> {
        const WORDS: [&str; 16] = [
            "fn ",
            "let ",
            "self.",
            "return ",
            "match ",
            "&mut ",
            "Vec<u8>",
            "Result<(), E>",
            "{\n",
            "}\n",
            "    ",
            "// ",
            "value",
            "path",
            "turn",
            "snapshot",
        ];
        let mut out = Vec::with_capacity(len + 16);
        while out.len() < len {
            out.extend_from_slice(WORDS[(self.next() % 16) as usize].as_bytes());
            if self.next().is_multiple_of(7) {
                out.extend_from_slice(format!("{}", self.next() % 1000).as_bytes());
            }
        }
        out.truncate(len);
        out
    }
}
