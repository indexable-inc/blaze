//! A tree of files as one object: its **listing**, the canonical encoding a git source's
//! digest is over ([`crate::Source::Git`]) and a fetch graph's steps will hand on
//! ([`crate::graph::Tree`]). Nix's NAR serialises a tree for its recursive hashes the same
//! way in spirit (names sorted, the executable bit and symlinks kept, nothing else); this one
//! is text, and lists each file by its BLAKE3 instead of inlining it, so each file is an object
//! of its own in the store and two trees share the files they share.
//!
//! The encoding, `blaze tree 2`:
//!
//! ```text
//! blaze tree 2\n
//! <kind> <blake3 hex of the bytes> <length> <path>\n   one line a file, sorted by the path's bytes
//! ```
//!
//! `length` is the file's length in decimal, with no leading zero: with it a lookup knows
//! which files are kept as chunks ([`crate::store::CHUNK_LEN`]) without reading any file.
//!
//! `kind` is `file`, `exec` (a file with any executable bit) or `link` (a symlink, its bytes
//! where it points). A path is `/`-separated UTF-8 with no empty, `.`, `..` or `.git` part and
//! no newline; directories are not listed (an empty one is not in the tree), and no path is
//! twice or both a file and a directory, compared without regard to ASCII case, so that no
//! listing written out on a case-insensitive file system (APFS, NTFS) puts a file under
//! another file or a symlink. [`parse`] refuses all of it as [`listing`] does. A change to any
//! of this is `blaze tree 3`.

use std::fmt::{self, Write as _};

use crate::digest::Blake3;

/// The listing format's name and version, its first line. Every index record's key covers it
/// ([`crate::Origin::key`]): a step's output under a new format is a new record, never a
/// second value under the old one's key.
pub(crate) const HEADER: &str = "blaze tree 2\n";

/// What a listed path is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    File,
    Exec,
    Link,
}

impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Exec => "exec",
            Kind::Link => "link",
        }
    }

    fn named(name: &str) -> Option<Kind> {
        [Kind::File, Kind::Exec, Kind::Link].into_iter().find(|it| it.name() == name)
    }
}

/// One line of a listing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub blake3: Blake3,
    pub len: u64,
}

/// Why a tree has no listing, or a listing is not one: each names the path or line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TreeRefused {
    /// A path with an empty, `.`, `..` or `.git` part, a newline, or a leading `/`.
    Path {
        path: String,
        why: &'static str,
    },
    /// Two entries, or two directories entries are under, whose paths are one path to a
    /// case- and normalization-insensitive disk ([`fold`]) but are spelled differently (or the
    /// same: an entry listed twice). `path` is one spelling.
    Twice {
        path: String,
    },
    /// `path` is a file and `under` is listed beneath it ([`fold`]ed alike).
    Under {
        path: String,
        under: String,
    },
    /// A listing's line `line` (from 1) is not `<kind> <hex> <len> <path>`, or is out of order.
    Line {
        line: usize,
        why: &'static str,
    },
    NoHeader,
}

impl fmt::Display for TreeRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TreeRefused::Path { path, why } => write!(f, "the tree's path {path:?} {why}"),
            TreeRefused::Twice { path } => {
                write!(
                    f,
                    "the tree has the path {path:?} twice, or spelled two ways that a case-insensitive disk takes for one name"
                )
            }
            TreeRefused::Under { path, under } => {
                write!(f, "the tree's {under:?} is under {path:?}, which is a file")
            }
            TreeRefused::Line { line, why } => write!(f, "line {line} of the tree's listing {why}"),
            TreeRefused::NoHeader => {
                write!(f, "the listing does not start `{}`", HEADER.trim_end())
            }
        }
    }
}

impl std::error::Error for TreeRefused {}

/// Why `path` can't be listed, if it can't.
fn bad_path(path: &str) -> Option<&'static str> {
    if path.contains('\n') {
        return Some("has a newline");
    }
    if path.starts_with('/') {
        return Some("is absolute");
    }
    path.split('/').find_map(|part| match part {
        "" => Some("has an empty part"),
        "." | ".." => Some("has a `.` or `..` part"),
        _ if part.eq_ignore_ascii_case(".git") => {
            Some("is in a `.git`, which is the repository's, not the tree's")
        }
        _ => None,
    })
}

/// `path` folded for a caseless, normalization-insensitive compare: Unicode's canonical
/// caseless match (Unicode 16, section 3.13, D145): the path in canonical decomposition (NFD),
/// each character then replaced by its full case folding (statuses C and F of
/// CaseFolding.txt, [`crate::casefold`]: `ſ` and `s`, `ß`, `ẞ` and `ss`, `ﬀ` and `ff`, `ς`
/// and `σ` are one), then in NFD again. Two names this folds equal are taken as one name on
/// a case-insensitive disk (APFS, NTFS). APFS compares by its own tables, which are not proven
/// equal to this; the aim is to fold at least as much as it does, so a pair it takes as one
/// is refused here. Folding is per character, so a `/` stays where it was and a folded path's
/// directories are the folded directories.
pub(crate) fn fold(path: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    let mut folded = String::with_capacity(path.len()); // alloc: once a path compared
    for it in path.nfd() {
        match crate::casefold::FOLD.binary_search_by_key(&it, |row| row.0) {
            Ok(at) => folded.push_str(crate::casefold::FOLD[at].1),
            Err(_) => folded.push(it),
        }
    }
    folded.nfd().collect() // alloc: as above
}

/// Every rule of a tree over `entries`, sorted by path: no bad path, no two paths that are one
/// path to a case-insensitive disk (entries or the directories they are under, [`fold`]), and
/// no path under a file.
fn check(entries: &[Entry]) -> Result<(), TreeRefused> {
    for entry in entries {
        if let Some(why) = bad_path(&entry.path) {
            return Err(TreeRefused::Path { path: entry.path.clone(), why }); // clone: error path
        }
    }
    // Every name the tree makes on disk, an entry or a directory it is under, as (folded,
    // spelled), sorted: one folded name spelled two ways (`Dir/a` and `dir/b`, `a` and `A`) is
    // one name on a case-insensitive disk, and an entry listed twice is one name twice.
    let mut names: Vec<(String, &str, bool)> = Vec::new(); // alloc: once a tree
    for entry in entries {
        for (at, _) in entry.path.match_indices('/') {
            names.push((fold(&entry.path[..at]), &entry.path[..at], true));
        }
        names.push((fold(&entry.path), &entry.path, false));
    }
    names.sort_unstable();
    names.dedup_by(|b, a| a.0 == b.0 && a.1 == b.1 && a.2 && b.2);
    for pair in names.windows(2) {
        let (folded_a, a, a_dir): (&str, &str, bool) = (&pair[0].0, pair[0].1, pair[0].2);
        let (folded_b, b, b_dir): (&str, &str, bool) = (&pair[1].0, pair[1].1, pair[1].2);
        if folded_a != folded_b {
            continue;
        }
        // A file and a directory of one folded name: something is listed under a file.
        if a_dir != b_dir {
            let (file, dir) = if a_dir { (b, a) } else { (a, b) };
            let under = entries
                .iter()
                .find(|it| it.path.starts_with(dir) && it.path[dir.len()..].starts_with('/'));
            return Err(TreeRefused::Under {
                path: file.to_owned(), // alloc: error path
                under: under.map_or_else(|| dir.to_owned(), |it| it.path.clone()), // alloc: error path
            });
        }
        return Err(TreeRefused::Twice { path: b.to_owned() }); // alloc: error path
    }
    Ok(())
}

/// The listing of `entries`, sorted by path first.
pub fn listing(entries: &mut [Entry]) -> Result<Vec<u8>, TreeRefused> {
    entries.sort_unstable_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    check(entries)?;
    let mut out = String::from(HEADER); // alloc: the listing, once a tree
    for entry in entries.iter() {
        out.push_str(entry.kind.name());
        out.push(' ');
        out.push_str(&entry.blake3.hex());
        out.push(' ');
        _ = write!(out, "{}", entry.len);
        out.push(' ');
        out.push_str(&entry.path);
        out.push('\n');
    }
    Ok(out.into_bytes())
}

/// The entries of a listing [`listing`] wrote, in its order; anything else is refused.
pub fn parse(bytes: &[u8]) -> Result<Vec<Entry>, TreeRefused> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| TreeRefused::Line { line: 0, why: "is not UTF-8" })?;
    let body = text.strip_prefix(HEADER).ok_or(TreeRefused::NoHeader)?;
    let mut entries: Vec<Entry> = Vec::new(); // alloc: once a tree read
    for (at, line) in body.split_terminator('\n').enumerate() {
        let line_no = at + 2;
        let refused = |why| TreeRefused::Line { line: line_no, why };
        let mut parts = line.splitn(3, ' ');
        let kind = parts
            .next()
            .and_then(Kind::named)
            .ok_or(refused("has no kind of file, exec or link"))?;
        let hex = parts.next().ok_or(refused("has no BLAKE3"))?;
        let blake3 = blaze_format::hex::decode_lower::<32>(hex)
            .ok_or(refused("has no lower-case BLAKE3 of 64 digits"))?;
        let rest = parts.next().ok_or(refused("has no length"))?;
        let (len, path) = rest.split_once(' ').ok_or(refused("has no path"))?;
        if len.is_empty()
            || !len.bytes().all(|it| it.is_ascii_digit())
            || (len.len() > 1 && len.starts_with('0'))
        {
            return Err(refused("has no length in decimal digits with no leading zero"));
        }
        let len: u64 = len.parse().map_err(|_| refused("has a length past 64 bits"))?;
        if bad_path(path).is_some() {
            return Err(refused("has a path a tree can't have"));
        }
        if entries.last().is_some_and(|it| it.path.as_bytes() >= path.as_bytes()) {
            return Err(refused("is not after the line before it"));
        }
        entries.push(Entry { path: path.to_owned(), kind, blake3: Blake3(blake3), len }); // alloc: once an entry
    }
    if !text.ends_with('\n') {
        return Err(TreeRefused::Line {
            line: body.lines().count() + 1,
            why: "does not end in a newline",
        });
    }
    // The rules a listing is written under, over what was read: a listing that is not one
    // [`listing`] could write is never written out (a file under a symlink).
    check(&entries)?;
    Ok(entries)
}
