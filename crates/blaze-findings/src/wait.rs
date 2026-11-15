//! How long a command waits for findings, and what its reply says.
//!
//! An agent chooses per call: the answer alone, the answer and the cheap class (the default), or
//! everything within a budget. Whatever it chooses, the answer class is always complete (a gate
//! is never answered before its check ran), and the reply says what is still pending, by lint
//! and item, with an estimate, so the agent can decide to ask again rather than guess.
//!
//! What changes in `blaze_api` (not edited here; for whoever ports it):
//!
//! - `Request::Diagnostics { wait_ms }` becomes `wait: Wait`, a CBOR enum `{"answer"}`,
//!   `{"cheap"}` (the default when absent) or `{"all": {"budget_ms": u64}}`; the CLI's
//!   `--wait SECONDS` maps to `All`, a new `--answer-only` to `Answer`.
//! - `diagnostics::Seen` gains `pending: Vec<Pending>` (lint name, item path or none for a
//!   workspace fold, class, estimate in ms) and `slow: Vec<Slow>` (pass, mean, bound), after
//!   `new` in its CBOR key order; `superseded: bool` for a reply cut short by a newer command.
//! - `diagnostics::Found::id` becomes the item's form ([`crate::finding::FindingId`]: lint,
//!   item, message, arguments, ordinal), and `Found` gains `item: Option<String>` (the item's
//!   path) and `at: Option<(u32, u32)>` (the item-relative range) beside the drawn `span`.
//! - `Request` schema 56 becomes 57.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use blaze_keyed::record::StableId;

use crate::finding::LintId;
use crate::pass::{Class, PassId, Slow};

/// How long a command holds its reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Wait {
    /// Until the answer class is done: errors and gate lints.
    Answer,
    /// Until the cheap class is done too, or its budget after the answer passed.
    #[default]
    Cheap,
    /// Until everything is done, or `budget_ns` after the command passed (never before the
    /// answer class is done).
    All { budget_ns: u64 },
}

/// One lint not yet computed for one item (`None`: a workspace fold).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pending {
    pub lint: LintId,
    pub item: Option<StableId>,
    pub class: Class,
    /// The pass's measured mean, else its declared cost.
    pub estimate_ns: u64,
}

/// A work that failed (its pass panicked or refused): reported, never retried.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Failed {
    pub pass: PassId,
    pub item: Option<StableId>,
}

/// What a command's waiter is told.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Reply {
    /// The command, by its snapshot.
    pub snapshot: u64,
    /// A newer command arrived before this one's wait was over.
    pub superseded: bool,
    /// Works of the command finished by the reply (computed or found in the store).
    pub done: u32,
    pub pending: Vec<Pending, Global>,
    pub slow: Vec<Slow, Global>,
    pub failed: Vec<Failed, Global>,
}
