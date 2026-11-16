//! From a lock to what to fetch (sans-IO): each registry crate with the SHA-256 the lock
//! states, each git commit once, and every package that cannot be fetched and verified named
//! with why.
//!
//! What becomes part of a URL is checked here, whatever the lock's reader allowed: a crate's
//! name is `[A-Za-z0-9_-]{1,64}`, a version `[0-9A-Za-z.+-]{1,64}`, a git repository an
//! `https://` URL of `[A-Za-z0-9._~/:-]` with no `..`, no credentials and no query. A commit is
//! 40 lowercase hex digits.

#![blaze::sans_io]

use blaze_git_fetch::Oid;
use blaze_ids::{ContentId, Hasher};
use blaze_manifest::lock::{Checksum, Lock, LockId, LockSource};
use blaze_own::{Alloc, String, Vec};
use blaze_text::{Put, PutDisplay as _, Show, Writes as _};

/// The record key's domain of a registry crate's files. A change to the record's form
/// ([`crate::pack`]) or to what is stripped is a new domain.
pub const CRATE_DOMAIN: &str = "blaze-fetch: a registry crate's files, pack v3";
/// The record key's domain of a git commit's files.
pub const GIT_DOMAIN: &str = "blaze-fetch: a git commit's files, pack v3";

/// One thing to fetch.
#[derive(Clone, Debug)]
pub enum Unit<'l, 'a> {
    /// A crate of crates.io: `name-version.crate`, whose SHA-256 the lock states.
    Crate { package: LockId, name: &'l str, version: String<Alloc<'a>>, sha256: [u8; 32] },
    /// One commit of a repository, the packages of the lock it holds counted.
    Git { repo: &'l str, commit: Oid, packages: u32 },
}

impl Unit<'_, '_> {
    /// The record's key: the archive's identity and the form it is unpacked into, from the
    /// lock's text alone, before any work (rule 65).
    pub fn key(&self) -> ContentId {
        self.source().key()
    }

    /// What the unit's record holds the sources of.
    pub fn source(&self) -> SourceId<'_> {
        match self {
            Unit::Crate { name, version, sha256, .. } => SourceId::Crate { name, version: version.as_str(), sha256: *sha256 },
            Unit::Git { commit, .. } => SourceId::Git { commit: *commit },
        }
    }
}

/// A package's sources as the lock pins them: what a stored record is found by
/// ([`SourceId::key`]) and checked against when it is opened
/// ([`crate::PackView::open_source`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceId<'k> {
    /// A crates.io crate: its name, version and the `.crate`'s SHA-256 the lock states.
    Crate { name: &'k str, version: &'k str, sha256: [u8; 32] },
    /// A git commit (every package of the lock in it shares one record).
    Git { commit: Oid },
}

impl SourceId<'_> {
    /// The key of the record holding these sources, from the lock's text alone.
    pub fn key(&self) -> ContentId {
        match self {
            SourceId::Crate { name, version, sha256 } => {
                Hasher::new(CRATE_DOMAIN).part(name.as_bytes()).part(version.as_bytes()).part(sha256).finish()
            }
            SourceId::Git { commit } => Hasher::new(GIT_DOMAIN).part(&commit.0).finish(),
        }
    }
}

impl Show for Unit<'_, '_> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            Unit::Crate { name, version, .. } => {
                out.str("crate ").str(name).str(" ").str(version);
            }
            Unit::Git { repo, commit, .. } => {
                out.str("git ").str(repo).str(" at ").put_display(commit);
            }
        }
    }
}

/// A package that is not fetched, and why: each is named, none trusted silently.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Skip<'l> {
    /// A registry package the lock gives no checksum for.
    NoChecksum { name: &'l str },
    /// A package of another registry than crates.io: its index is not read (no credentials
    /// exist here, and none are looked for).
    OtherRegistry { name: &'l str, index: &'l str },
    /// A git source whose URL is not `https://` (an `ssh://`, `http://` or `file://` one).
    GitNotHttps { name: &'l str, repo: &'l str },
}

impl Show for Skip<'_> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            Skip::NoChecksum { name } => out.str(name).str(": the lock states no checksum, so nothing could verify it"),
            Skip::OtherRegistry { name, index } => out.str(name).str(": of another registry (").str(index).str("), not read"),
            Skip::GitNotHttps { name, repo } => out.str(name).str(": git source ").str(repo).str(" is not https"),
        };
    }
}

/// A lock whose text cannot be made into a URL or a key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanRefused<'l> {
    Name { line: u32, name: &'l str },
    Version { line: u32, name: &'l str },
    Checksum { line: u32, name: &'l str },
    Repo { line: u32, name: &'l str },
    Commit { line: u32, name: &'l str },
}

impl Show for PlanRefused<'_> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        let (line, name, what) = match *self {
            PlanRefused::Name { line, name } => (line, name, "a crate name that is not [A-Za-z0-9_-]{1,64}"),
            PlanRefused::Version { line, name } => (line, name, "a version that is not [0-9A-Za-z.+-]{1,64}"),
            PlanRefused::Checksum { line, name } => (line, name, "a checksum that is not 64 lowercase hex digits"),
            PlanRefused::Repo { line, name } => (line, name, "a git repository URL with a character, `..`, `@` or `?` refused"),
            PlanRefused::Commit { line, name } => (line, name, "a git commit that is not 40 lowercase hex digits"),
        };
        out.str("Cargo.lock line ").int(line).str(", package ").str(name).str(": ").str(what);
    }
}

/// What to fetch for a lock.
pub struct Plan<'l, 'a> {
    pub units: Vec<Unit<'l, 'a>, Alloc<'a>>,
    pub skipped: Vec<Skip<'l>, Alloc<'a>>,
    /// Packages of the lock, and of them workspace members and path dependencies.
    pub packages: u32,
    pub paths: u32,
}

/// The plan of `lock`.
pub fn plan<'l, 'a>(lock: &'l Lock<'_>, alloc: Alloc<'a>) -> Result<Plan<'l, 'a>, PlanRefused<'l>> {
    let mut units = Vec::new_in(alloc);
    let mut skipped = Vec::new_in(alloc);
    let mut paths = 0u32;
    // Git units by commit, to count each commit's packages once: few, so searched in turn.
    for (at, package) in lock.packages.iter().enumerate() {
        let name = package.name.as_str();
        let line = package.line;
        match &package.source {
            LockSource::Path => paths += 1,
            LockSource::Registry { url, .. } => {
                if !package.source.is_crates_io() {
                    skipped.push(Skip::OtherRegistry { name, index: url.as_str() });
                    continue;
                }
                let hex = match &package.checksum {
                    Checksum::Sha256(hex) => hex.as_str(),
                    Checksum::Absent => {
                        skipped.push(Skip::NoChecksum { name });
                        continue;
                    }
                };
                if !crate_name(name) {
                    return Err(PlanRefused::Name { line, name });
                }
                let mut version = String::new_in(alloc);
                version.put_display(&package.version);
                if !version_text(&version) {
                    return Err(PlanRefused::Version { line, name });
                }
                let sha256 = hex32(hex.as_bytes()).ok_or(PlanRefused::Checksum { line, name })?;
                units.push(Unit::Crate { package: LockId(at as u32), name, version, sha256 });
            }
            LockSource::Git { url, commit, .. } => {
                let repo = url.as_str();
                if !repo.starts_with("https://") {
                    skipped.push(Skip::GitNotHttps { name, repo });
                    continue;
                }
                if !repo_url(repo) {
                    return Err(PlanRefused::Repo { line, name });
                }
                let commit = commit_id(commit.as_str()).ok_or(PlanRefused::Commit { line, name })?;
                let held = units.iter_mut().find_map(|unit| match unit {
                    Unit::Git { commit: c, packages, .. } if *c == commit => Some(packages),
                    _ => None,
                });
                match held {
                    Some(packages) => *packages += 1,
                    None => units.push(Unit::Git { repo, commit, packages: 1 }),
                }
            }
        }
    }
    Ok(Plan { units, skipped, packages: lock.packages.len() as u32, paths })
}

fn crate_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn version_text(version: &str) -> bool {
    (1..=64).contains(&version.len())
        && version.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'+' || b == b'-')
}

/// An `https://` URL made only of unreserved characters, `/` and `:`, with no `..`, no
/// credentials (`@`), no query and no fragment: one that names a repository and nothing else.
fn repo_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    (1..=512).contains(&rest.len())
        && !rest.starts_with('/')
        && !rest.contains("..")
        && !rest.contains("//")
        && rest.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'/' | b':' | b'-'))
}

fn commit_id(hex: &str) -> Option<Oid> {
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    Oid::parse_hex(hex.as_bytes())
}

/// 64 lowercase hex digits as 32 bytes.
pub(crate) fn hex32(hex: &[u8]) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (at, pair) in hex.chunks_exact(2).enumerate() {
        out[at] = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(out)
}
