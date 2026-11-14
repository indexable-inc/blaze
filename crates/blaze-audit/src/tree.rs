//! The digest of a source tree: what pins a git checkout (and the advisory database) where a
//! commit id cannot. A git commit is named by SHA-1, which is broken for collisions (SHAttered,
//! 2017; chosen-prefix, 2020): two repositories, or one moved, can show two trees under one
//! commit id. So the tree itself is hashed, by blaze-fetch's one canonical listing.
//!
//! **What is hashed, exactly.** The listing `blaze tree 2` ([`blaze_fetch::tree`]) of the
//! tree's files: one line a file, sorted by the path's bytes, each `<kind> <BLAKE3 of its
//! bytes> <length> <path>`, where kind is `file`, `exec` (any executable bit) or `link` (a
//! symlink, its bytes where it points); paths `/`-separated UTF-8 relative to the tree's root.
//! Directories are not listed; nothing of a `.git` (the repository's own, not the tree's) is
//! listed, nor the files at the root a fetcher writes ([`MARKERS`]). The digest is the pin's
//! algorithm over the listing's bytes: BLAKE3 for what blaze records, SHA-256 or SHA-512 where
//! a person wrote one ([`blaze_fetch::Algo`]; md5 and SHA-1 are refused by name when a pin is
//! read). It is the digest blaze-fetch's `Source::Git` is pinned by, so one tree has one digest
//! wherever blaze names it.
//!
//! Beyond the listing's own rules (no `..` part, no two paths one name on a case-insensitive
//! disk, nothing under a file), a tree whose symlink points out of it is refused
//! ([`TreeRefused::Escapes`]): a build that follows it would read a file no digest covers.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::fmt;

use blaze_fetch::tree::{Entry, listing};
use blaze_fetch::{Algo, Blake3, Digest};

pub use blaze_fetch::tree::Kind;

/// Files at a tree's root that are a fetcher's, not the commit's: cargo's `.cargo-ok` and the
/// mark blaze writes into a package it keeps on disk (`.blaze-ok`). Never hashed.
pub const MARKERS: [&str; 2] = [".cargo-ok", ".blaze-ok"];

/// The most files a tree may have: the largest real crate repositories hold some tens of
/// thousands; a million is a tree no build reads, refused before it is listed.
pub const MOST_FILES: usize = 1 << 20;

/// The most bytes a tree's files may hold together: 4 GiB, past any source tree, so a tree
/// that would take the machine's memory to hash is refused by name.
pub const MOST_BYTES: u64 = 4 << 30;

/// One file of a tree, as a driver read it.
#[derive(Clone, Copy, Debug)]
pub struct TreeFile<'a> {
    /// `/`-separated, relative to the tree's root.
    pub path: &'a str,
    pub kind: Kind,
    /// Its bytes; a symlink's are where it points.
    pub bytes: &'a [u8],
}

/// Why a tree has no digest.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TreeRefused {
    /// The symlink `path` points at `target`, which is outside the tree.
    Escapes { path: String, target: String },
    /// The tree breaks a rule of the listing ([`blaze_fetch::tree::TreeRefused`]).
    Listing(String),
    TooMany { files: usize },
    TooBig { bytes: u64 },
}

impl fmt::Display for TreeRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TreeRefused::Escapes { path, target } => write!(
                f,
                "the symlink `{path}` points at `{target}`, which is outside the tree: a build that follows it reads what no digest covers"
            ),
            TreeRefused::Listing(why) => f.write_str(why),
            TreeRefused::TooMany { files } => {
                write!(f, "the tree has {files} files, more than the {MOST_FILES} a source tree may")
            }
            TreeRefused::TooBig { bytes } => {
                write!(f, "the tree's files hold {bytes} bytes, more than the {MOST_BYTES} a source tree may")
            }
        }
    }
}

/// Whether `path` is left out of every tree's digest: in a `.git`, or a root [`MARKERS`] file.
pub fn skipped(path: &str) -> bool {
    MARKERS.contains(&path) || path.split('/').any(|part| part.eq_ignore_ascii_case(".git"))
}

/// Whether the symlink at `path` pointing at `target` leaves the tree: an absolute target, a
/// Windows drive or a backslash, or more `..` than the directories `path` is under.
pub fn escapes(path: &str, target: &str) -> bool {
    if target.starts_with('/') || target.contains('\\') || target.contains(':') {
        return true;
    }
    let mut depth = path.matches('/').count() as i64;
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => depth += 1,
        }
    }
    false
}

/// The digest in `algo` of the tree of `files`, in any order: see the module's text for what
/// is hashed.
pub fn digest(files: &[TreeFile<'_>], algo: Algo) -> Result<Digest, TreeRefused> {
    if files.len() > MOST_FILES {
        return Err(TreeRefused::TooMany { files: files.len() });
    }
    let mut bytes = 0u64;
    let mut entries: Vec<Entry, Global> = Vec::with_capacity_in(files.len(), Global);
    for file in files.iter().filter(|it| !skipped(it.path)) {
        bytes = bytes.saturating_add(file.bytes.len() as u64);
        if bytes > MOST_BYTES {
            return Err(TreeRefused::TooBig { bytes });
        }
        if file.kind == Kind::Link {
            let target = String::from_utf8_lossy(file.bytes);
            if escapes(file.path, &target) {
                return Err(TreeRefused::Escapes { path: file.path.to_owned(), target: target.into_owned() });
            }
        }
        entries.push(Entry {
            path: file.path.to_owned(),
            kind: file.kind,
            blake3: Blake3::of(file.bytes),
            len: file.bytes.len() as u64,
        });
    }
    let listed = listing(&mut entries).map_err(|it| TreeRefused::Listing(it.to_string()))?;
    Ok(Digest::of(algo, &listed))
}
