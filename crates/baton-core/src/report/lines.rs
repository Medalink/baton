//! Line-level comparison for the report: which significant lines a change added and removed.
//!
//! A line is compared by its trimmed text, so a re-indent or a CRLF checkout is not a change. Lines
//! with fewer than three bytes or no letter or digit (`}`, `);`, a blank) are dropped before the diff:
//! they appear everywhere and would make unrelated edits look like the same line. A line that one change
//! both adds and removes (a move, a re-indent) is cancelled out of both sides.

use crate::paths::fnv1a;
use similar::{Algorithm, DiffTag, capture_diff_slices_deadline};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// A side bigger than this is not read for lines.
pub(super) const TEXT_MAX: u64 = 1 << 20;
const LINES_MAX: usize = 100_000;
const BINARY_PROBE: usize = 8 * 1024;
const DIFF_BUDGET: Duration = Duration::from_millis(250);

/// One side of a changed file.
pub(super) enum Side {
    Absent,
    Bytes(Vec<u8>),
    /// Present but over `TEXT_MAX`.
    Big,
}

impl Side {
    pub(super) fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

#[derive(Default)]
pub(super) struct LineDelta {
    pub added: Vec<u64>,
    pub removed: Vec<u64>,
    pub added_n: u32,
    pub removed_n: u32,
    /// The diff ran out of time and kept a coarser edit script than the minimum.
    pub cut: bool,
}

impl LineDelta {
    pub(super) fn is_empty(&self) -> bool {
        self.added_n == 0 && self.removed_n == 0
    }
}

fn text(side: &Side) -> Option<String> {
    match side {
        Side::Absent => Some(String::new()),
        Side::Big => None,
        Side::Bytes(bytes) => {
            if bytes[..bytes.len().min(BINARY_PROBE)].contains(&0) {
                None
            } else {
                Some(String::from_utf8_lossy(bytes).into_owned())
            }
        }
    }
}

fn significant(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| l.len() >= 3 && l.chars().any(char::is_alphanumeric))
        .collect()
}

fn hash(line: &str) -> u64 {
    fnv1a(line.as_bytes())
}

/// The significant lines of a file as hashes, or none when it is binary, too big or has too many lines.
pub(super) fn hashes(side: &Side) -> Option<HashSet<u64>> {
    let text = text(side)?;
    let lines = significant(&text);
    (lines.len() <= LINES_MAX).then(|| lines.into_iter().map(hash).collect())
}

/// What changed between two sides, or none when either is binary, too big or has too many lines.
pub(super) fn diff(old: &Side, new: &Side) -> Option<LineDelta> {
    let (old, new) = (text(old)?, text(new)?);
    let (a, b) = (significant(&old), significant(&new));
    if a.len() > LINES_MAX || b.len() > LINES_MAX {
        return None;
    }
    let mut delta = LineDelta::default();
    let deadline = Instant::now() + DIFF_BUDGET;
    for op in capture_diff_slices_deadline(Algorithm::Myers, &a, &b, Some(deadline)) {
        let (tag, from, to) = op.as_tag_tuple();
        if matches!(tag, DiffTag::Delete | DiffTag::Replace) {
            delta.removed.extend(a[from].iter().map(|l| hash(l)));
        }
        if matches!(tag, DiffTag::Insert | DiffTag::Replace) {
            delta.added.extend(b[to].iter().map(|l| hash(l)));
        }
    }
    delta.cut = Instant::now() >= deadline;
    delta.added_n = u32::try_from(delta.added.len()).unwrap_or(u32::MAX);
    delta.removed_n = u32::try_from(delta.removed.len()).unwrap_or(u32::MAX);
    delta.added.sort_unstable();
    delta.added.dedup();
    delta.removed.sort_unstable();
    delta.removed.dedup();
    let both: HashSet<u64> = delta
        .added
        .iter()
        .filter(|h| delta.removed.binary_search(h).is_ok())
        .copied()
        .collect();
    if !both.is_empty() {
        delta.added.retain(|h| !both.contains(h));
        delta.removed.retain(|h| !both.contains(h));
    }
    Some(delta)
}
