//! The hook's side of the local IPC and a listener that stamps when a whole frame arrived.
use std::io::Write;
use std::sync::mpsc;
use std::time::Instant;

pub const MAGIC: &[u8; 4] = b"BTN1";

/// `BTN1 | u32 LE length | agent | \n | payload`, the frame the real hook writes.
pub fn frame(agent: &str, payload: &[u8]) -> Vec<u8> {
    let body_len = agent.len() + 1 + payload.len();
    let mut out = Vec::with_capacity(8 + body_len);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(body_len as u32).to_le_bytes());
    out.extend_from_slice(agent.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(payload);
    out
}

pub fn default_endpoint(tag: &str) -> String {
    if cfg!(windows) {
        format!(r"\\.\pipe\baton-bench-{tag}")
    } else {
        std::env::temp_dir()
            .join(format!("baton-bench-{tag}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(windows)]
pub fn send(endpoint: &str, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(endpoint)?
        .write_all(bytes)
}

#[cfg(unix)]
pub fn send(endpoint: &str, bytes: &[u8]) -> std::io::Result<()> {
    std::os::unix::net::UnixStream::connect(endpoint)?.write_all(bytes)
}

/// Frames received, each stamped when its last byte arrived.
pub struct Listener {
    pub received: mpsc::Receiver<(Instant, usize)>,
}

#[cfg(windows)]
pub fn listen(endpoint: &str) -> std::io::Result<Listener> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND, ReadFile,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    let name: Vec<u16> = endpoint.encode_utf16().chain([0]).collect();
    let create = move |first: bool| {
        let flags = PIPE_ACCESS_INBOUND
            | if first {
                FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                0
            };
        // SAFETY: `name` is NUL terminated and outlives the call; a null security descriptor means the default.
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                0,
                65536,
                0,
                std::ptr::null(),
            )
        };
        (h != INVALID_HANDLE_VALUE).then_some(h as usize)
    };
    let first = create(true).ok_or_else(std::io::Error::last_os_error)?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut pending = first;
        loop {
            let h = pending as windows_sys::Win32::Foundation::HANDLE;
            // SAFETY: `h` is a pipe handle this thread owns; a client that connected first is reported as an error code.
            let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            // The next instance exists before this client is served, so a fast second client never finds no pipe.
            let Some(next) = create(false) else { return };
            if connected {
                let mut buf = vec![0u8; 65536];
                let mut have = 0usize;
                loop {
                    let mut got = 0u32;
                    // SAFETY: the pointer and length describe the unused tail of `buf`.
                    let ok = unsafe {
                        ReadFile(
                            h,
                            buf[have..].as_mut_ptr(),
                            (buf.len() - have) as u32,
                            &mut got,
                            std::ptr::null_mut(),
                        )
                    };
                    if ok == 0 || got == 0 {
                        break;
                    }
                    have += got as usize;
                    if have >= 8
                        && have >= 8 + u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize
                    {
                        break;
                    }
                }
                let _ = tx.send((Instant::now(), have));
            }
            // SAFETY: `h` is still the handle created above and is not used afterwards.
            unsafe {
                DisconnectNamedPipe(h);
                CloseHandle(h);
            }
            pending = next;
        }
    });
    Ok(Listener { received: rx })
}

#[cfg(unix)]
pub fn listen(endpoint: &str) -> std::io::Result<Listener> {
    use std::io::Read;
    let _ = std::fs::remove_file(endpoint);
    let socket = std::os::unix::net::UnixListener::bind(endpoint)?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in socket.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = Vec::new();
            let _ = stream.read_to_end(&mut buf);
            let _ = tx.send((Instant::now(), buf.len()));
        }
    });
    Ok(Listener { received: rx })
}
