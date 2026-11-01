use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 5 * 1024 * 1024;

/// A line-per-event log file. The daemon has no terminal, so this is how it says anything.
pub struct Log(Mutex<File>);

impl Log {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let too_big = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(!too_big)
            .write(true)
            .truncate(too_big)
            .open(path)?;
        Ok(Self(Mutex::new(file)))
    }

    pub fn line(&self, message: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        if let Ok(mut file) = self.0.lock() {
            let _ = writeln!(
                file,
                "{}.{:03} {message}",
                now.as_secs(),
                now.subsec_millis()
            );
        }
    }
}
