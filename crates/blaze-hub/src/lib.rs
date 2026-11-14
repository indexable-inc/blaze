//! blaze's hub, step 1 (docs/v2/plan/design/hub.md): the lane, the root and the transaction,
//! and the schema an agent speaks to them.
//!
//! - [`tree`]: a checkout as a BLAKE3 Merkle tree. A file is the VFS's own content id; a
//!   directory is a search tree whose shape is a function of its names alone; a root names
//!   the exact tree.
//! - [`git`]: git as the origin. One table of each object's two names (content id, git
//!   object id), a commit's tree imported to a root and written back byte for byte.
//! - [`hub`]: the lanes and the job ledger, a sans-IO core on an injected clock. A lane is a
//!   root; a change is one transaction to a new root; a fork copies a root; every job names
//!   the root it judges and is counted warm or cold with the reason.
//! - [`wire`]: the one versioned schema, deterministic CBOR, with a limit on everything read
//!   from a peer.
//!
//! The crate is on bstd (`crates/blaze-std`): it names `core`, `alloc`, bstd and blaze's
//! crates, never `std`, and owns no socket, file, clock or thread. What it does not have yet
//! (the driver, search, verified ranges of large files, sync by root between machines, the
//! router) is hub.md's later steps.

// Tests are built on `std` (the harness, blaze-sim).
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
// `#![blaze::sans_io]` marks each core for `blaze::sans_io_core`.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

extern crate alloc;

pub mod git;
pub mod hub;
pub mod tree;
pub mod wire;

#[cfg(test)]
mod tests;

pub use crate::{
    hub::{Config, Decision, Hub},
    tree::{Memory, Store},
};

/// A safeguard a test turns off to prove its oracle sees the bug (skill `testing`,
/// planted-bug controls). Always [`Plant::None`] outside this crate's tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "the planted bugs are made by the tests only"))]
#[cfg_attr(not(test), expect(blaze::unwired, reason = "wired: a planted-bug control only its tests make"))]
pub(crate) enum Plant {
    None,
    /// A directory's nodes end where they are full, not where a name says: an entry added
    /// at the front moves every node after it.
    NodesEndByCount,
    /// A directory emptied by a delete stays in its parent: a tree's root says how it was
    /// made.
    KeepsEmptyDirs,
    /// A lane is read and changed by whoever asks, its owner or not.
    AnyoneMayUse,
    /// A change written against a root the lane has left is applied all the same.
    ChangeIgnoresOn,
    /// A change that fails part way keeps the ops before the one that failed.
    KeepsHalfAChange,
}
