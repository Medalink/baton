//! Where the daemon listens, and the two frame types a client can send it.
//!
//! The hook uses this too and must not read config, so the name comes from the environment only. On
//! Windows that is `BATON_HOME` when set (a hermetic run gets its own pipe), otherwise the OS user. On unix
//! it is the data dir, which `BATON_HOME`, `XDG_DATA_HOME` and `HOME` decide.

#[cfg(windows)]
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
/// makes `bind` and `connect` fail, so a data dir that deep gets a short name in a fixed directory.
#[cfg(unix)]
const SOCKET_PATH_MAX: usize = 100;

/// Where the short sockets live. Fixed, not read from `TMPDIR` or `XDG_RUNTIME_DIR`, so every process
/// agrees on it.
#[cfg(unix)]
const SHORT_SOCKET_ROOT: &str = "/tmp";

/// The data dir spelled one way: symlinks resolved, no trailing slash. When it does not exist yet, its
/// nearest existing ancestor is resolved and the rest appended, so the answer is the same before and after
/// the daemon creates it.
#[cfg(unix)]
fn canonical_data_dir() -> io::Result<std::path::PathBuf> {
    let data = baton_core::Dirs::resolve()
        .map_err(|e| io::Error::other(e.to_string()))?
        .data()
        .to_owned();
    let mut missing = Vec::new();
    let mut ancestor = data.as_path();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                resolved.extend(missing.iter().rev());
                return Ok(resolved);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                match (ancestor.file_name(), ancestor.parent()) {
                    (Some(name), Some(parent)) => {
                        missing.push(name.to_owned());
                        ancestor = parent;
                    }
                    _ => return Err(e),
                }
            }
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("cannot resolve the data dir {}: {e}", data.display()),
                ));
            }
        }
    }
}

/// The daemon's socket: `daemon.sock` in the canonical data dir, the dir that holds the single-instance
/// lock, so every process that agrees on the lock agrees on the socket whatever `XDG_RUNTIME_DIR`, `TMPDIR`
/// or spelling of `BATON_HOME` it has. A data dir too deep for `sun_path` gets
/// `/tmp/baton-<uid>/<32 hex of SHA-256 of the data dir>.sock` instead. Pure: it creates nothing.
#[cfg(unix)]
pub fn socket_path() -> io::Result<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    let data = canonical_data_dir()?;
    let plain = data.join("daemon.sock");
    if plain.as_os_str().len() <= SOCKET_PATH_MAX {
        return Ok(plain);
    }
    let digest = Sha256::digest(data.as_os_str().as_bytes());
    let tag: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let short = std::path::Path::new(SHORT_SOCKET_ROOT)
        .join(format!("baton-{uid}"))
        .join(format!("{tag}.sock"));
    if short.as_os_str().len() > SOCKET_PATH_MAX {
        return Err(io::Error::other(format!(
            "no socket path fits sun_path ({SOCKET_PATH_MAX} bytes): {} is too long for the data dir {}",
            short.display(),
            data.display()
        )));
    }
    Ok(short)
}

/// Daemon side: makes the directory that holds `socket` fit to hold it. In the data dir that means mode
/// 0700. The short-socket directory may be shared, so it is created 0700 if absent and must then be a real
/// directory (checked with lstat, so a symlink fails) owned by this user with no group or other access.
#[cfg(unix)]
pub fn prepare_socket_dir(socket: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let dir = socket
        .parent()
        .ok_or_else(|| io::Error::other("the socket path has no directory"))?;
    if socket.file_name() == Some(std::ffi::OsStr::new("daemon.sock")) {
        std::fs::create_dir_all(dir)?;
        return std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let refuse = |why: &str| {
        io::Error::other(format!(
            "the socket directory {} is unusable: {why}; remove or fix it, or move BATON_HOME to a shorter path",
            dir.display()
        ))
    };
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(refuse(&e.to_string())),
    }
    let meta = std::fs::symlink_metadata(dir).map_err(|e| refuse(&e.to_string()))?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !meta.file_type().is_dir() {
        Err(refuse("it is not a directory (a symlink or file is there)"))
    } else if meta.uid() != uid {
        Err(refuse(&format!(
            "it is owned by uid {}, not {uid}",
            meta.uid()
        )))
    } else if meta.mode() & 0o077 != 0 {
        Err(refuse(&format!(
            "its mode is {:o}, and group or other can access it",
            meta.mode() & 0o7777
        )))
    } else {
        Ok(())
    }
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
        let stream = std::os::unix::net::UnixStream::connect(socket_path()?)?;
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
