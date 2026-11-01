//! The tree entries baton stores and writes to disk. The rules are git's own, ported from git v2.54.0, so
//! that every tree baton keeps passes `git fsck --strict` and nothing a peer sends can name a `.git`
//! directory on any platform git protects.
//!
//! Sources, all in git v2.54.0:
//! - `path.c`: `is_ntfs_dotgit`, `is_ntfs_dot_generic` (`.gitmodules`, `.gitattributes`)
//! - `utf8.c`: `next_hfs_char`, `is_hfs_dot_generic` (`is_hfs_dotgit`, `is_hfs_dotgitmodules`, ...)
//! - `fsck.c`: `fsck_tree`, `fsck_blob`, `fsck_gitmodules_fn`
//! - `read-cache.c`: `verify_path_internal`, `verify_dotfile`
//! - `submodule-config.c`: `check_submodule_name`, `check_submodule_url`
//! - `submodule.c`: `parse_submodule_update_type`
//! - `attr.h`: `ATTR_MAX_LINE_LENGTH`

/// Longest tree entry name, git's `max_tree_entry_len` in `fsck.c`.
pub const MAX_NAME_LEN: usize = 4096;

/// Deepest nesting of trees baton keeps: the root tree is depth 0, so a file may sit in 512 directories.
pub const MAX_TREE_DEPTH: usize = 512;

const GITATTRIBUTES_MAX_LINE: usize = 2048;

const MODE_FILE: u32 = 0o100_644;
const MODE_EXEC: u32 = 0o100_755;
const MODE_LINK: u32 = 0o120_000;
const MODE_TREE: u32 = 0o040_000;
const MODE_GITLINK: u32 = 0o160_000;

/// Files whose content git validates when it checks a tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Special {
    Gitmodules,
    Gitattributes,
}

/// Whether git accepts a tree entry with this name and raw mode. On success, says which special file
/// the entry is, when its blob must also pass `check_blob`.
pub fn check_entry(name: &[u8], mode: u32) -> Result<Option<Special>, String> {
    check_name(name)?;
    if !matches!(
        mode,
        MODE_FILE | MODE_EXEC | MODE_LINK | MODE_TREE | MODE_GITLINK
    ) {
        return Err(format!("tree entry mode {mode:o} is not one git writes"));
    }
    let special = special_of(name);
    if let Some(special) = special {
        return match mode {
            MODE_FILE | MODE_EXEC => Ok(Some(special)),
            MODE_LINK if special == Special::Gitmodules => {
                Err(".gitmodules is a symbolic link".to_owned())
            }
            MODE_LINK => Ok(None),
            _ => Err(format!("{} is not a regular file", special.file())),
        };
    }
    Ok(None)
}

/// Whether git accepts a file of this name, raw mode and content. A capture asks this before it stores
/// the file, so a turn never carries what the server would refuse.
pub fn check_file(name: &[u8], mode: u32, bytes: &[u8]) -> Result<(), String> {
    match check_entry(name, mode)? {
        Some(special) => check_blob(special, bytes),
        None => Ok(()),
    }
}

/// Whether git accepts this name as one component of any path.
pub fn check_name(name: &[u8]) -> Result<(), String> {
    let shown = || String::from_utf8_lossy(name).into_owned();
    if name.is_empty() || name.iter().any(|b| matches!(b, b'/' | 0)) {
        return Err(format!("tree entry name {:?} is not allowed", shown()));
    }
    if name == b"." || name == b".." {
        return Err(format!("tree entry name {:?} is not allowed", shown()));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "tree entry name of {} bytes is longer than {MAX_NAME_LEN}",
            name.len()
        ));
    }
    if is_hfs_dot(name, b"git") || segments(name).any(is_ntfs_dotgit) {
        return Err(format!("tree entry name {:?} names .git", shown()));
    }
    Ok(())
}

impl Special {
    fn file(self) -> &'static str {
        match self {
            Special::Gitmodules => ".gitmodules",
            Special::Gitattributes => ".gitattributes",
        }
    }
}

fn special_of(name: &[u8]) -> Option<Special> {
    let matches = |hfs: &[u8], ntfs: &[u8], short: &[u8; 6]| {
        is_hfs_dot(name, hfs) || segments(name).any(|s| is_ntfs_dot_generic(s, ntfs, short))
    };
    if matches(b"gitmodules", b"gitmodules", b"gi7eba") {
        Some(Special::Gitmodules)
    } else if matches(b"gitattributes", b"gitattributes", b"gi7d29") {
        Some(Special::Gitattributes)
    } else {
        None
    }
}

/// The name, then what follows each backslash, since NTFS reads a backslash as a separator (`fsck_tree`).
fn segments(name: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::once(name).chain(
        name.iter()
            .enumerate()
            .filter(|(_, b)| **b == b'\\')
            .map(move |(i, _)| &name[i + 1..]),
    )
}

fn at(name: &[u8], i: usize) -> u8 {
    name.get(i).copied().unwrap_or(0)
}

fn is_ntfs_dotgit(name: &[u8]) -> bool {
    let mut i = 0;
    let mut next = || {
        let c = at(name, i);
        i += 1;
        c
    };
    match next() {
        b'.' => {
            if !next().eq_ignore_ascii_case(&b'g')
                || !next().eq_ignore_ascii_case(&b'i')
                || !next().eq_ignore_ascii_case(&b't')
            {
                return false;
            }
        }
        b'g' | b'G' => {
            if !next().eq_ignore_ascii_case(&b'i')
                || !next().eq_ignore_ascii_case(&b't')
                || next() != b'~'
                || next() != b'1'
            {
                return false;
            }
        }
        _ => return false,
    }
    loop {
        match next() {
            0 | b'/' | b'\\' | b':' => return true,
            b'.' | b' ' => {}
            _ => return false,
        }
    }
}

/// `dotgit` is the lower-case name without the dot, `short` the NTFS 8.3 prefix git precomputed for it.
fn is_ntfs_dot_generic(name: &[u8], dotgit: &[u8], short: &[u8; 6]) -> bool {
    let len = dotgit.len();
    let only_spaces_and_periods = |mut i: usize| loop {
        let c = at(name, i);
        i += 1;
        if c == 0 || c == b':' {
            return true;
        }
        if c != b' ' && c != b'.' {
            return false;
        }
    };
    if at(name, 0) == b'.'
        && name
            .get(1..=len)
            .is_some_and(|s| s.eq_ignore_ascii_case(dotgit))
    {
        return only_spaces_and_periods(len + 1);
    }
    if name
        .get(..6)
        .is_some_and(|s| s.eq_ignore_ascii_case(&dotgit[..6]))
        && at(name, 6) == b'~'
        && (b'1'..=b'4').contains(&at(name, 7))
    {
        return only_spaces_and_periods(8);
    }
    let mut saw_tilde = false;
    let mut i = 0;
    while i < 8 {
        let c = at(name, i);
        if c == 0 {
            return false;
        } else if saw_tilde {
            if !c.is_ascii_digit() {
                return false;
            }
        } else if c == b'~' {
            i += 1;
            if !(b'1'..=b'9').contains(&at(name, i)) {
                return false;
            }
            saw_tilde = true;
        } else if i >= 6 || c & 0x80 != 0 || c.to_ascii_lowercase() != short[i] {
            return false;
        }
        i += 1;
    }
    only_spaces_and_periods(i)
}

/// The next code point HFS+ would compare, which skips the ones it ignores. Malformed UTF-8 reads as the
/// end, exactly as in git, so a malformed tail does not hide a `.git`.
fn next_hfs_char(rest: &mut &[u8]) -> u32 {
    loop {
        let Some(&lead) = rest.first() else {
            return 0;
        };
        let len = match lead {
            0x00..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => {
                *rest = &[];
                return 0;
            }
        };
        let Some(ch) = rest
            .get(..len)
            .and_then(|c| std::str::from_utf8(c).ok())
            .and_then(|s| s.chars().next())
        else {
            *rest = &[];
            return 0;
        };
        *rest = &rest[len..];
        let ch = ch as u32;
        if matches!(
            ch,
            0x200c..=0x200f | 0x202a..=0x202e | 0x206a..=0x206f | 0xfeff
        ) {
            continue;
        }
        return ch;
    }
}

fn is_hfs_dot(name: &[u8], needle: &[u8]) -> bool {
    let mut rest = name;
    if next_hfs_char(&mut rest) != u32::from(b'.') {
        return false;
    }
    for expected in needle {
        let c = next_hfs_char(&mut rest);
        if c > 127 || (c as u8).to_ascii_lowercase() != *expected {
            return false;
        }
    }
    matches!(next_hfs_char(&mut rest), 0 | 0x2f | 0x5c)
}

/// Whether git's fsck accepts this content for the file named `special`.
pub fn check_blob(special: Special, bytes: &[u8]) -> Result<(), String> {
    match special {
        Special::Gitattributes => {
            let text = bytes.split(|b| *b == 0).next().unwrap_or_default();
            if text
                .split(|b| *b == b'\n')
                .any(|line| line.len() >= GITATTRIBUTES_MAX_LINE)
            {
                return Err(".gitattributes has too long lines to parse".into());
            }
            Ok(())
        }
        Special::Gitmodules => check_gitmodules(bytes),
    }
}

fn check_gitmodules(bytes: &[u8]) -> Result<(), String> {
    for (var, value) in parse_config(bytes) {
        let Some(rest) = var.strip_prefix("submodule.") else {
            continue;
        };
        let Some((name, key)) = rest.rsplit_once('.') else {
            continue;
        };
        if bad_submodule_name(name) {
            return Err(format!("disallowed submodule name: {name}"));
        }
        let Some(value) = value else { continue };
        match key {
            "url" if bad_submodule_url(&value) => {
                return Err(format!("disallowed submodule url: {value}"));
            }
            "path" if value.starts_with('-') => {
                return Err(format!("disallowed submodule path: {value}"));
            }
            "update" if value.starts_with('!') => {
                return Err(format!("disallowed submodule update setting: {value}"));
            }
            _ => {}
        }
    }
    Ok(())
}

fn bad_submodule_name(name: &str) -> bool {
    name.is_empty() || name.split(['/', '\\']).any(|part| part == "..")
}

fn percent_decoded(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: Option<&u8>| b.and_then(|b| (*b as char).to_digit(16));
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (hex(bytes.get(i + 1)), hex(bytes.get(i + 2)))
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// `check_submodule_url`. The `url_normalize` step for http-like URLs is approximated by requiring a
/// scheme, a host and no control or space characters, which only ever refuses more than git does.
fn bad_submodule_url(url: &str) -> bool {
    if url.starts_with('-') {
        return true;
    }
    let relative = |s: &str, dots: &str| {
        s.starts_with(dots) && matches!(s.as_bytes().get(dots.len()), Some(b'/' | b'\\'))
    };
    if relative(url, ".") || relative(url, "..") || url.starts_with("git://") {
        if percent_decoded(url).contains(&b'\n') {
            return true;
        }
        let mut rest = url;
        let mut dotdots = 0;
        loop {
            if relative(rest, "..") {
                dotdots += 1;
                rest = &rest[3..];
            } else if relative(rest, ".") {
                rest = &rest[2..];
            } else {
                break;
            }
        }
        return dotdots > 0 && rest.starts_with([':', '/']);
    }
    let curl = ["http::", "https::", "ftp::", "ftps::"]
        .iter()
        .find_map(|p| url.strip_prefix(p))
        .or_else(|| {
            ["http://", "https://", "ftp://", "ftps://"]
                .iter()
                .any(|p| url.starts_with(p))
                .then_some(url)
        });
    let Some(curl) = curl else { return false };
    let Some((scheme, rest)) = curl.split_once("://") else {
        return true;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    scheme.is_empty()
        || host.is_empty()
        || curl.bytes().any(|b| b <= b' ' || b == 0x7f)
        || percent_decoded(curl).contains(&b'\n')
}

/// git-config syntax as `git_parse_source` reads it: `(variable, value)` pairs with the variable
/// lower-cased except for the subsection. At a syntax error it stops and keeps what came before, as
/// git does: fsck reports the error as info (`gitmodulesParse`) and still checks the earlier pairs.
/// A byte order mark is skipped, as when git reads the file from a worktree, which fsck does not do.
fn parse_config(bytes: &[u8]) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let _ = parse_pairs(bytes, &mut out);
    out
}

fn parse_pairs(bytes: &[u8], out: &mut Vec<(String, Option<String>)>) -> Option<()> {
    let mut chars = Chars { bytes, at: 0 };
    let mut stem = String::new();
    let mut comment = false;
    loop {
        let c = chars.next();
        match c {
            Eof => return Some(()),
            Byte(b'\n') => comment = false,
            _ if comment => {}
            Byte(c) if is_space(c) => {}
            Byte(b'#' | b';') => comment = true,
            Byte(b'[') => {
                stem = base_var(&mut chars)?;
                stem.push('.');
            }
            Byte(c) if c.is_ascii_alphabetic() => {
                let mut var = stem.clone();
                var.push(c.to_ascii_lowercase() as char);
                let mut c = chars.next();
                while let Byte(k) = c
                    && (k.is_ascii_alphanumeric() || k == b'-')
                {
                    var.push(k.to_ascii_lowercase() as char);
                    c = chars.next();
                }
                while matches!(c, Byte(b' ' | b'\t')) {
                    c = chars.next();
                }
                let value = if matches!(c, Byte(b'\n') | Eof) {
                    None
                } else if matches!(c, Byte(b'=')) {
                    Some(value(&mut chars)?)
                } else {
                    return None;
                };
                out.push((var, value));
            }
            _ => return None,
        }
    }
}

use Next::{Byte, Eof};

#[derive(Clone, Copy)]
enum Next {
    Byte(u8),
    Eof,
}

struct Chars<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Chars<'_> {
    /// A CRLF reads as a newline.
    fn next(&mut self) -> Next {
        let Some(&c) = self.bytes.get(self.at) else {
            return Eof;
        };
        self.at += 1;
        if c == b'\r' && self.bytes.get(self.at) == Some(&b'\n') {
            self.at += 1;
            return Byte(b'\n');
        }
        Byte(c)
    }
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// The text of `[section]` or `[section "subsection"]` after the `[`.
fn base_var(chars: &mut Chars) -> Option<String> {
    let mut name = String::new();
    loop {
        match chars.next() {
            Byte(b']') => return (!name.is_empty()).then_some(name),
            Byte(c) if is_space(c) => {
                let mut c = Byte(c);
                while let Byte(k) = c
                    && is_space(k)
                {
                    if k == b'\n' {
                        return None;
                    }
                    c = chars.next();
                }
                if !matches!(c, Byte(b'"')) {
                    return None;
                }
                name.push('.');
                let mut sub = Vec::new();
                loop {
                    match chars.next() {
                        Byte(b'\n') | Eof => return None,
                        Byte(b'"') => break,
                        Byte(b'\\') => match chars.next() {
                            Byte(b'\n') | Eof => return None,
                            Byte(e) => sub.push(e),
                        },
                        Byte(k) => sub.push(k),
                    }
                }
                name.push_str(&String::from_utf8_lossy(&sub));
                return matches!(chars.next(), Byte(b']')).then_some(name);
            }
            Byte(c) if c.is_ascii_alphanumeric() || c == b'-' || c == b'.' => {
                name.push(c.to_ascii_lowercase() as char);
            }
            _ => return None,
        }
    }
}

/// `parse_value` in `config.c`: quotes, escapes, comments and line continuations.
fn value(chars: &mut Chars) -> Option<String> {
    let mut out = Vec::new();
    let (mut quote, mut comment, mut space) = (false, false, 0);
    loop {
        let c = match chars.next() {
            Eof => b'\n',
            Byte(c) => c,
        };
        if c == b'\n' {
            return (!quote).then(|| String::from_utf8_lossy(&out).into_owned());
        }
        if comment {
            continue;
        }
        if is_space(c) && !quote {
            if !out.is_empty() {
                space += 1;
            }
            continue;
        }
        if !quote && (c == b';' || c == b'#') {
            comment = true;
            continue;
        }
        out.extend(std::iter::repeat_n(b' ', std::mem::take(&mut space)));
        if c == b'\\' {
            let e = match chars.next() {
                Eof => b'\n',
                Byte(e) => e,
            };
            match e {
                b'\n' => {}
                b't' => out.push(b'\t'),
                b'b' => out.push(8),
                b'n' => out.push(b'\n'),
                b'\\' | b'"' => out.push(e),
                _ => return None,
            }
            continue;
        }
        if c == b'"' {
            quote = !quote;
            continue;
        }
        out.push(c);
    }
}
