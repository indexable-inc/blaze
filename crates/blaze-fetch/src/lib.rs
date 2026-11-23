//! blaze-fetch: the sources of a project's dependencies, obtained by blaze itself
//! (docs/v2/plan/design/fetch.md).
//!
//! ```text
//! Cargo.lock --plan--> units --Machine--> effects (lookup, HTTPS, put) --> one record a unit
//! ```
//!
//! - [`plan`]: from a lock ([`blaze_manifest::lock`], the one reader) to the units to fetch:
//!   each registry crate with the SHA-256 the lock states, each git commit once however many
//!   packages it holds. A package the lock cannot verify (no checksum, an unpinned or non-HTTPS
//!   git source, another registry) is named in [`plan::Skip`], never fetched and trusted.
//! - [`fetch`]: the machine (sans-IO, rule 14). It asks the store whether each unit's record is
//!   held, asks for the rest over HTTPS with a byte limit and a deadline, ONCE (D172: a
//!   failure is the unit's outcome, owed to the next command, never tried again in this one),
//!   checks every answer before a byte of it is unpacked, and asks the store to keep one record
//!   a unit.
//! - [`unpack`]: a `.crate` (gzip, then tar) or a git pack, read under limits, every path held
//!   inside the unit's folder, into [`pack`]: the record's form, its files sorted, its tree's
//!   listing digest and its own BLAKE3 in it.
//! - [`digest`] and [`tree`] (kept from the old crate): the pins blaze-audit writes
//!   (`<algo>:<hex>`) and the canonical listing a tree's pin is over, whose path rules the
//!   record uses.
//!
//! The trust steps, in order: the lock's checksum is the pin; the answer's length is held to
//! its limit by the driver and again here; its SHA-256 is compared before it is inflated; the
//! inflate and the tar reader are bounded before they write; every path is checked (inside the
//! crate's folder, no link out, no two names one on a caseless disk); only then is a record
//! made, and only a made record is put.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]


extern crate alloc;
mod casefold;
pub mod correct;
pub mod digest;
pub mod drive;
pub mod fetch;
pub mod index;
pub mod local;
pub mod pack;
pub mod peer;
pub mod plan;
pub mod tree;
pub mod unpack;

#[cfg(test)]
mod tests;

pub use crate::{
    digest::{Algo, Blake3, Digest, DigestRefused, Hashed, Hashing},
    drive::{FetchIo, Reserved, Woke},
    fetch::{Effect, Failed, Limits, Machine, Method, NetFailure, Outcome, Reply, StoreFailure, Why},
    pack::{File, Mode, PackRefused, PackView},
    plan::{Plan, PlanRefused, Skip, SourceId, Unit},
    unpack::UnpackError,
};

/// A bug planted by a test, to show its oracle catches it: only ever in this crate's tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// A `.crate` is unpacked and stored without its SHA-256 compared to the lock's.
    StoreBeforeChecksum,
    /// A tar entry outside the crate's folder is kept under its own path.
    EntryOutside,
    /// A tar entry's size, as its header says it, is reserved before it is held to a limit
    /// (and the tar reader is given no limit of its own).
    SizeFromWire,
    /// A git answer's commit is the first commit object of its pack, not the one whose id the
    /// lock pins: the server's word taken.
    CommitFromServer,
}

#[cfg(test)]
thread_local! {
    pub(crate) static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

/// Whether `plant` is planted: only ever in this crate's tests.
#[cfg(test)]
pub(crate) fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

/// Whether `plant` is planted: never, outside tests.
#[cfg(not(test))]
#[inline(always)]
pub(crate) fn planted(_plant: Plant) -> bool {
    false
}
