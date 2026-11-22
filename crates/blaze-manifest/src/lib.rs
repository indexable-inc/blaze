//! `Cargo.toml` and `.cargo/config.toml` read as the pinned nightly's cargo
//! (`pins/toolchain`) reads them, its `cargo-features` activated but its `-Z` flags not
//! read, so that blaze knows a workspace without running cargo: its packages, targets,
//! dependencies and features, and the configuration that applies. What cargo accepts reads
//! the same here; what it rejects is rejected with its message. `cargo metadata --no-deps` is
//! the check: [`metadata`] prints the same JSON, held by the tests to the output cargo printed
//! for each fixture, recorded once.
//!
//! [`layout`] is the sans-IO core the new `blaze` reads a workspace with, on blaze's own crates
//! only (`default-features = false` leaves out the rest, the `cargo` feature):
//!
//! - It starts from a package's directory, anchor-relative: `Layout::new("crates/app", alloc)`.
//! - `pass()` answers `Done(units)` or `Need(needs)`: manifest texts and directory listings by
//!   anchor-relative path. The caller answers each with `manifest(path, text)` or
//!   `listing(dir, entries)`, `None` for none there, and calls `pass()` again.
//! - `units.members`, in cargo's order, each have their `dir`, `name`, `edition`
//!   (`PackageEdition`: syntax, and `std_crate()`, `bstd` for a blaze edition), `lib` and
//!   `bins` with their `root` files, `dependencies` and `features`.
//! - Each dependency's `source` is `Member(index)`, `Path(dir)`, `Registry { req, registry }`
//!   or `Git { url, reference }`.
//! - Every package, target, dependency, feature and edition has a `span`: its manifest and the
//!   byte range of its line, for a finding to be placed on.

#![feature(allocator_api)]
// The bare crate (`default-features = false`: `layout` and `lock`, the shape every crate on the
// rebuilt path takes) is sans-IO over text on blaze-own; only the `cargo` reader of the old
// layer, which goes with that layer at the switch, is on `std`.
#![cfg_attr(not(any(test, feature = "cargo")), no_std)]

#[cfg(feature = "cargo")]
pub mod config;
#[cfg(feature = "cargo")]
pub mod dependency;
#[cfg(feature = "cargo")]
pub mod diagnostic;
#[cfg(feature = "cargo")]
pub mod error;
#[cfg(feature = "cargo")]
pub mod features;
#[cfg(feature = "cargo")]
pub mod files;
#[cfg(feature = "cargo")]
pub mod home;
pub mod layout;
pub mod lock;
// cargo's `build.rustflags` rule: the `--cfg`s a unit's cfg takes (sans-IO, no serde).
pub mod rustflags;
#[cfg(test)]
mod lock_ban_tests;
#[cfg(feature = "cargo")]
pub mod manifest;
#[cfg(feature = "cargo")]
pub mod metadata;
#[cfg(feature = "cargo")]
pub mod paths;
#[cfg(feature = "cargo")]
pub mod profiles;
#[cfg(feature = "cargo")]
pub mod schema;
#[cfg(feature = "cargo")]
mod snippet;
#[cfg(feature = "cargo")]
pub mod source;
#[cfg(feature = "cargo")]
pub mod targets;
#[cfg(feature = "cargo")]
pub mod user_config;
#[cfg(feature = "cargo")]
pub mod workspace;

#[cfg(feature = "cargo")]
use stdx::edit_distance::cargo as edit_distance;

#[cfg(feature = "cargo")]
pub use config::{Config, UserRecord};
#[cfg(feature = "cargo")]
pub use error::ManifestError;
#[cfg(feature = "cargo")]
pub use manifest::Context;
#[cfg(feature = "cargo")]
pub use workspace::Workspace;
