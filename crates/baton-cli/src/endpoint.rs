//! Where the daemon listens, and the two frame types a client can send it.
//!
//! The hook uses this too and must not read config, so the name comes from the environment only:
//! `BATON_HOME` when set (a hermetic run gets its own endpoint), otherwise the OS user.

use baton_core::HOME_ENV;
use std::io::{self, Read, Write};
use std::time::Duration;

pub const HOOK_MAGIC: &[u8; 4] = b"BTN1";
pub const CONTROL_MAGIC: &[u8; 4] = b"BTC1";
pub const MAX_PAYLOAD: usize = 1024 * 1024;

#[cfg(windows)]
pub type Conn = std::fs::File;
#[cfg(unix)]
pub type Conn = std::os::unix::net::UnixStream;

#[cfg(windows)]
fn home_tag() -> Option<String> {
    let home = std::env::var_os(HOME_ENV).filter(|h| !h.is_empty())?;
    Some(format!(
        "{:08x}",
        baton_core::fnv1a(home.to_string_lossy().to_lowercase().as_bytes()) as u32
    ))
}

#[cfg(windows)]
pub fn pipe_name() -> String {
    let user: String = std::env::var("USERNAME")
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    match home_tag() {
        Some(tag) => format!(r"\\.\pipe\baton-{user}-{tag}"),
        None => format!(r"\\.\pipe\baton-{user}"),
    }
}

/// What `sockaddr_un.sun_path` holds, with its NUL, is 108 bytes on Linux and 104 on macOS. A longer path
/// makes `bind` and `connect` fail, so a `BATON_HOME` that deep gets a short name in the runtime directory.
#[cfg(unix)]
const SOCKET_PATH_MAX: usize = 100;

#[cfg(unix)]
pub fn socket_path() -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    let runtime_dir = || match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(runtime) => PathBuf::from(runtime).join("baton"),
        None => {
            // SAFETY: getuid has no preconditions and cannot fail.
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("baton-{uid}"))
        }
    };
    if let Some(home) = std::env::var_os(HOME_ENV).filter(|h| !h.is_empty()) {
        let path = PathBuf::from(&home).join("data").join("daemon.sock");
        if path.as_os_str().len() <= SOCKET_PATH_MAX {
            return path;
        }
        let tag = baton_core::fnv1a(home.as_bytes()) as u32;
        return runtime_dir().join(format!("{tag:08x}.sock"));
    }
    runtime_dir().join("daemon.sock")
}

/// Opens the endpoint once. Used by the hook (which must never wait) and by `ensure_daemon`.
pub fn connect() -> io::Result<Conn> {
    #[cfg(windows)]
    {
        const PIPE_BUSY: i32 = 231;
        let name = pipe_name();
        let mut tries = 0;
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&name)
            {
                Ok(file) => return Ok(file),
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) && tries < 20 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e),
            }
        }
    }
    #[cfg(unix)]
    {
        let stream = std::os::unix::net::UnixStream::connect(socket_path())?;
        stream.set_write_timeout(Some(Duration::from_millis(30)))?;
        Ok(stream)
    }
}

pub fn hook_frame(agent: &str, payload: &[u8]) -> Vec<u8> {
    let len = agent.len() + 1 + payload.len();
    let mut frame = Vec::with_capacity(8 + len);
    frame.extend_from_slice(HOOK_MAGIC);
    frame.extend_from_slice(&(len as u32).to_le_bytes());
    frame.extend_from_slice(agent.as_bytes());
    frame.push(b'\n');
    frame.extend_from_slice(payload);
    frame
}

pub fn control_frame(json: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + json.len());
    frame.extend_from_slice(CONTROL_MAGIC);
    frame.extend_from_slice(&(json.len() as u32).to_le_bytes());
    frame.extend_from_slice(json);
    frame
}

/// One request and one reply, for the commands that talk to the daemon. Blocking, with no timeout on the
/// reply because a turn legitimately waits for its commit.
pub fn call(request: &[u8]) -> io::Result<Vec<u8>> {
    let mut conn = connect()?;
    conn.write_all(&control_frame(request))?;
    let mut len = [0u8; 4];
    conn.read_exact(&mut len)?;
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    conn.read_exact(&mut body)?;
    Ok(body)
}
