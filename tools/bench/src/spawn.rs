use crate::ipc;
use crate::stats;
use crate::util::Res;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const RUNS: usize = 100;
const PAYLOAD_BYTES: usize = 2048;
const WATCHDOG: Duration = Duration::from_millis(50);
const FRAME_BYTES: usize = 8 + "claude\n".len() + PAYLOAD_BYTES;

/// What the real hook does, as the first thing `main` runs: read stdin on a helper thread, write one
/// frame to the endpoint, exit 0 whatever happens. `--noop` exits at once, to price process creation alone.
pub fn hookprobe(args: &[String]) -> ! {
    if args.get(1).map(String::as_str) != Some("--noop") {
        let _ = std::panic::catch_unwind(|| probe_body(args));
    }
    std::process::exit(0)
}

fn probe_body(args: &[String]) {
    let Some(endpoint) = args.first() else { return };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut payload = Vec::new();
        let _ = std::io::stdin().take(1 << 20).read_to_end(&mut payload);
        let _ = tx.send(payload);
    });
    let Ok(payload) = rx.recv_timeout(WATCHDOG) else {
        return;
    };
    let _ = ipc::send(endpoint, &ipc::frame("claude", &payload));
}

struct Once {
    exit: Duration,
    receipt: Option<Duration>,
}

fn once(exe: &std::path::Path, endpoint: &str, listener: &ipc::Listener, noop: bool) -> Res<Once> {
    let payload = vec![b'x'; PAYLOAD_BYTES];
    while listener.received.try_recv().is_ok() {}
    let start = Instant::now();
    let mut cmd = Command::new(exe);
    cmd.arg("hookprobe").arg(endpoint);
    if noop {
        cmd.arg("--noop");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let _ = stdin.write_all(&payload);
    }
    let status = child.wait()?;
    let exit = start.elapsed();
    if !status.success() {
        return Err(format!("hookprobe exited with {status}").into());
    }
    let receipt = if noop {
        None
    } else {
        match listener.received.recv_timeout(Duration::from_millis(500)) {
            Ok((at, bytes)) if bytes == FRAME_BYTES => Some(at.duration_since(start)),
            Ok((_, bytes)) => {
                return Err(format!("listener got {bytes} bytes, expected {FRAME_BYTES}").into());
            }
            Err(_) => None,
        }
    };
    Ok(Once { exit, receipt })
}

pub fn run() -> Res<()> {
    let exe = std::env::current_exe()?;
    let endpoint = ipc::default_endpoint(&std::process::id().to_string());
    let listener = ipc::listen(&endpoint)?;

    for _ in 0..10 {
        once(&exe, &endpoint, &listener, false)?;
        once(&exe, &endpoint, &listener, true)?;
    }
    let (mut to_receipt, mut hook_exit, mut noop_exit) = (Vec::new(), Vec::new(), Vec::new());
    let mut lost = 0;
    for _ in 0..RUNS {
        let h = once(&exe, &endpoint, &listener, false)?;
        hook_exit.push(h.exit);
        match h.receipt {
            Some(r) => to_receipt.push(r),
            None => lost += 1,
        }
        noop_exit.push(once(&exe, &endpoint, &listener, true)?.exit);
    }

    println!(
        "spawn: {PAYLOAD_BYTES} bytes of stdin, {FRAME_BYTES} byte frame, endpoint {endpoint}, exe {} KiB",
        std::fs::metadata(&exe)?.len() / 1024
    );
    stats::header();
    if to_receipt.is_empty() {
        println!("no frame was received");
    } else {
        stats::row("spawn -> listener got whole frame", &mut to_receipt);
    }
    stats::row("spawn -> hook exited", &mut hook_exit);
    stats::row("spawn -> --noop exited", &mut noop_exit);
    println!("frames lost: {lost} of {RUNS}");
    Ok(())
}
