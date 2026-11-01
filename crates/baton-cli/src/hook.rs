//! The agent end-of-turn hook. Runs before anything else in `main`: no config, no logging, no gix, no
//! tokio. It must never block an agent or say anything, so every path ends in `exit(0)`.

use crate::endpoint;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

const WATCHDOG: Duration = Duration::from_millis(50);

fn agent_name_is_plain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn read_stdin() -> Vec<u8> {
    let mut payload = Vec::new();
    let _ = std::io::stdin()
        .lock()
        .take(endpoint::MAX_PAYLOAD as u64)
        .read_to_end(&mut payload);
    payload
}

fn send(agent: &str, payload: &[u8]) {
    if let Ok(mut conn) = endpoint::connect() {
        let _ = conn.write_all(&endpoint::hook_frame(agent, payload));
    }
}

fn body(agent: Option<String>) {
    let Some(agent) = agent.filter(|a| agent_name_is_plain(a)) else {
        return;
    };
    let (tx, rx) = mpsc::channel();
    // stdin may never close (a terminal, a stuck parent), and a blocked read cannot be interrupted, so
    // it lives on its own thread and the watchdog abandons it.
    if std::thread::Builder::new()
        .spawn(move || {
            let _ = tx.send(read_stdin());
        })
        .is_err()
    {
        return;
    }
    if let Ok(payload) = rx.recv_timeout(WATCHDOG) {
        send(&agent, &payload);
    }
}

pub fn run(agent: Option<String>) -> ! {
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::thread::Builder::new().spawn(|| {
        std::thread::sleep(WATCHDOG);
        std::process::exit(0);
    });
    let _ = std::panic::catch_unwind(|| body(agent));
    std::process::exit(0);
}
