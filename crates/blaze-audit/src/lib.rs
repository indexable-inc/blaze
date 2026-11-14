//! `blaze audit`, sans-IO: what a workspace builds from, checked default-deny.
//!
//! Two questions, each a pure function of bytes and facts a driver hands in:
//!
//! - **Is every source pinned by a strong digest?** ([`source`]) A registry package by its
//!   `Cargo.lock` sha256; a git package by the BLAKE3 of its tree ([`tree`]), recorded in
//!   `blaze.lock` ([`pins`]), never by its commit alone (SHA-1); a path outside the workspace
//!   only when the workspace names it in `allow-unpinned-paths`. Rule [`WEAK_SOURCE_PIN`],
//!   deny by default. The build's load calls [`source::require_git`] and its git fetcher
//!   [`source::verify_git`], so an unpinned or altered git tree is refused before it is built,
//!   not only when someone runs `blaze audit`.
//! - **Is any locked package known bad?** ([`advisory`], [`version`], [`yanked`]) Each
//!   package version against a RustSec advisory database, a pinned input read from disk, and
//!   against the registry's yanked versions where blaze has the index. [`policy`] decides
//!   what is an error, and what a workspace accepted, with a reason and an expiry.
//!
//! [`audit`] puts them together into one [`report::Report`]: one finding per (package
//! version, reason), in a fixed order, as text or as JSON under [`report::VERSION`].

#![forbid(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

pub mod advisory;
pub mod audit;
pub mod cargo_lock;
pub mod date;
pub mod fetch;
pub mod pins;
pub mod policy;
pub mod report;
pub mod source;
pub mod tree;
pub mod version;
pub mod yanked;

#[cfg(test)]
mod tests;

pub use crate::audit::{Input, audit};

/// A dependency source not pinned by a strong digest: a git package without its tree's
/// BLAKE3 in `blaze.lock`, or whose tree is not that digest; a registry package without its
/// sha256; a commit that is not a whole id; the advisory database unpinned.
pub const WEAK_SOURCE_PIN: &str = "weak_source_pin";
/// A path dependency outside the workspace, which no digest pins, not named in
/// `allow-unpinned-paths`.
pub const UNPINNED_PATH: &str = "unpinned_path";
/// A locked version an advisory says is vulnerable.
pub const VULNERABLE: &str = "vulnerable";
/// A locked version an advisory says is unsound: safe code can cause undefined behavior.
pub const UNSOUND: &str = "unsound";
/// A locked package an advisory says is unmaintained.
pub const UNMAINTAINED: &str = "unmaintained";
/// An informational advisory.
pub const NOTICE: &str = "notice";
/// A locked version its registry yanked.
pub const YANKED: &str = "yanked";
/// An acceptance that matched no finding.
pub const STALE_ACCEPTANCE: &str = "stale_acceptance";
