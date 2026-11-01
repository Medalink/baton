use std::time::{SystemTime, UNIX_EPOCH};

/// One line on stderr, stamped with the Unix time in microseconds so a run's logs from several
/// processes on one machine can be laid on a single timeline.
pub fn log(message: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    eprintln!("{}.{:06} {message}", now.as_secs(), now.subsec_micros());
}
