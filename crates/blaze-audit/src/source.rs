//! Rule `weak_source_pin`, deny by default: every source a build reads is pinned by a strong
//! digest of its bytes, or it is refused.
//!
//! | source                         | pinned by                                    |
//! |--------------------------------|----------------------------------------------|
//! | the toolchain's `library/`     | its digest in `blaze.lock` (`[blaze]`)        |
//! | a registry package             | its sha256 in `Cargo.lock` (`checksum`)       |
//! | a git package at a commit      | its tree's BLAKE3 in `blaze.lock` (`[git]`)   |
//! | a path inside the workspace    | the workspace itself: what is being built     |
//! | a path outside the workspace   | nothing: refused unless `allow-unpinned-paths` |
//!
//! A git commit id alone is no pin: SHA-1 collides, and a repository that is moved or
//! rewritten can serve another tree under an id. So a git package is accepted only with its
//! tree's digest recorded ([`require_git`], which the build's load calls before it asks for
//! the checkout), and the tree that was fetched must be that digest ([`verify_git`], which
//! the git fetcher calls before the checkout is used). [`check`] is the same rule over a
//! whole lock, for `blaze audit`.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::fmt;

use blaze_fetch::{Algo, Digest};

use crate::cargo_lock::{Lock, Source};
use crate::pins::{SourcePins, is_commit};
use crate::policy::Settings;
use crate::{UNPINNED_PATH, WEAK_SOURCE_PIN};

/// What to run to record a git source's pin.
pub const PIN_COMMAND: &str = "blaze audit pin";

/// A source refused, naming the package and what to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PinRefused {
    /// No digest of `url` at `commit` in `blaze.lock`.
    Missing { package: String, url: String, commit: String },
    /// The tree fetched is not the digest `blaze.lock` records.
    Mismatch { package: String, url: String, commit: String, recorded: Digest, found: Digest },
    /// The tree has no digest: why ([`crate::tree::TreeRefused`]).
    Tree { package: String, url: String, commit: String, why: String },
    /// `Cargo.lock` names the commit by less than its whole id, or names none.
    NoCommit { package: String, url: String, commit: String },
    /// A registry package without a `checksum` in `Cargo.lock`.
    NoChecksum { package: String, version: String, registry: String },
    /// A path dependency outside the workspace, not allowed unpinned.
    OutsidePath { package: String, path: String },
}

impl PinRefused {
    /// The rule that refused it.
    pub fn rule(&self) -> &'static str {
        match self {
            PinRefused::OutsidePath { .. } => UNPINNED_PATH,
            _ => WEAK_SOURCE_PIN,
        }
    }

    /// The package it is about.
    pub fn package(&self) -> &str {
        match self {
            PinRefused::Missing { package, .. }
            | PinRefused::Mismatch { package, .. }
            | PinRefused::Tree { package, .. }
            | PinRefused::NoCommit { package, .. }
            | PinRefused::NoChecksum { package, .. }
            | PinRefused::OutsidePath { package, .. } => package,
        }
    }
}

impl fmt::Display for PinRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = self.rule();
        match self {
            PinRefused::Missing { package, url, commit } => write!(
                f,
                "`{package}` comes from git `{url}` at {commit}, which `blaze.lock` does not pin: a commit id \
                 (SHA-1) is no pin (rule `{rule}`); run `{PIN_COMMAND}` to record its tree's BLAKE3, and review \
                 the diff of `blaze.lock`"
            ),
            PinRefused::Mismatch { package, url, commit, recorded, found } => write!(
                f,
                "`{package}` comes from git `{url}` at {commit}, whose tree is {found}, not the {recorded} \
                 `blaze.lock` pins (rule `{rule}`): the repository served another tree under that commit; find \
                 out why before pinning it again with `{PIN_COMMAND}`"
            ),
            PinRefused::Tree { package, url, commit, why } => write!(
                f,
                "`{package}` comes from git `{url}` at {commit}, whose tree is refused (rule `{rule}`): {why}"
            ),
            PinRefused::NoCommit { package, url, commit } => write!(
                f,
                "`{package}` comes from git `{url}` at `{commit}`, which is no whole commit id (rule `{rule}`): \
                 lock it again so `Cargo.lock` names the commit, then run `{PIN_COMMAND}`"
            ),
            PinRefused::NoChecksum { package, version, registry } => write!(
                f,
                "`{package} {version}` comes from `{registry}` without a `checksum` in `Cargo.lock` (rule \
                 `{rule}`): nothing pins its bytes; lock it again with a registry that serves checksums"
            ),
            PinRefused::OutsidePath { package, path } => write!(
                f,
                "`{package}` is a path dependency at `{path}`, outside the workspace, which no digest pins (rule \
                 `{rule}`): move it into the workspace, or name it in `allow-unpinned-paths` of \
                 `[workspace.metadata.blaze.audit]`"
            ),
        }
    }
}

/// The digest `blaze.lock` pins `url` at `commit` by, or why `package` is refused: the check
/// the load makes before it asks for a git package's checkout.
pub fn require_git<'a>(pins: &'a SourcePins, package: &str, url: &str, commit: &str) -> Result<&'a Digest, PinRefused> {
    let named = || (package.to_owned(), url.to_owned(), commit.to_owned());
    if !is_commit(commit) {
        let (package, url, commit) = named();
        return Err(PinRefused::NoCommit { package, url, commit });
    }
    match pins.git(url, commit) {
        Some(pin) => Ok(&pin.tree),
        None => {
            let (package, url, commit) = named();
            Err(PinRefused::Missing { package, url, commit })
        }
    }
}

/// Whether the tree fetched, `found` (in the recorded digest's algorithm), is the one pinned.
pub fn verify_git(recorded: &Digest, found: &Digest, package: &str, url: &str, commit: &str) -> Result<(), PinRefused> {
    if recorded == found {
        return Ok(());
    }
    Err(PinRefused::Mismatch {
        package: package.to_owned(),
        url: url.to_owned(),
        commit: commit.to_owned(),
        recorded: recorded.clone(),
        found: found.clone(),
    })
}

/// The digest of a git tree, where a driver has it: what [`check`] compares a pin against.
pub trait Trees {
    /// The digest in `algo` of `url`'s tree at `commit`, `Ok(None)` when it is not on disk
    /// (so not verified, though the pin is still required), or why it has none.
    fn tree(&mut self, url: &str, commit: &str, algo: Algo) -> Result<Option<Digest>, String>;
}

/// A path dependency outside the workspace, as the driver found it.
#[derive(Clone, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub struct OutsidePath {
    pub package: String,
    pub path: String,
}

/// One source the check passed, and how it was pinned: for the report's account of what was
/// checked.
#[derive(Clone, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Pinned {
    /// A git tree that matched its pin, or whose pin is recorded but whose tree is not on disk.
    Git { package: String, url: String, commit: String, tree: Digest, verified: bool },
    /// An outside path allowed unpinned.
    AllowedPath { package: String, path: String },
}

/// Every source of `lock` and every outside path dependency, checked: the refusals, and the
/// git trees and paths it passed, each sorted.
pub fn check(
    lock: &Lock,
    outside: &[OutsidePath],
    pins: &SourcePins,
    settings: &Settings,
    trees: &mut dyn Trees,
) -> (Vec<PinRefused, Global>, Vec<Pinned, Global>) {
    let mut refused = Vec::new_in(Global);
    let mut passed = Vec::new_in(Global);
    for package in &lock.packages {
        match &package.source {
            Source::Path => {}
            Source::Registry { url, checksum } => {
                let strong = checksum.as_deref().is_some_and(|it| {
                    it.len() == 64 && it.bytes().all(|b| b.is_ascii_hexdigit())
                });
                if !strong {
                    refused.push(PinRefused::NoChecksum {
                        package: package.name.clone(),
                        version: package.version.to_string(),
                        registry: url.clone(),
                    });
                }
            }
            Source::Git { url, commit, .. } => {
                let recorded = match require_git(pins, &package.name, url, commit) {
                    Ok(it) => it,
                    Err(why) => {
                        refused.push(why);
                        continue;
                    }
                };
                let pinned = |verified| Pinned::Git {
                    package: package.name.clone(),
                    url: url.clone(),
                    commit: commit.clone(),
                    tree: recorded.clone(),
                    verified,
                };
                match trees.tree(url, commit, recorded.algo()) {
                    Ok(None) => passed.push(pinned(false)),
                    Ok(Some(found)) => match verify_git(recorded, &found, &package.name, url, commit) {
                        Ok(()) => passed.push(pinned(true)),
                        Err(why) => refused.push(why),
                    },
                    Err(why) => refused.push(PinRefused::Tree {
                        package: package.name.clone(),
                        url: url.clone(),
                        commit: commit.clone(),
                        why,
                    }),
                }
            }
        }
    }
    for path in outside {
        if settings.allows_unpinned_path(&path.package) {
            passed.push(Pinned::AllowedPath { package: path.package.clone(), path: path.path.clone() });
        } else {
            refused.push(PinRefused::OutsidePath { package: path.package.clone(), path: path.path.clone() });
        }
    }
    refused.sort_by(|a, b| (a.package(), a.to_string()).cmp(&(b.package(), b.to_string())));
    refused.dedup();
    passed.sort();
    passed.dedup();
    (refused, passed)
}
