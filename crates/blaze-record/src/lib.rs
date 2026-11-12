//! A record with its reads (docs/v2/plan/design/end-state.md, "One primitive first"): a result
//! kept in a write-once content-addressed store as `base key → read-lists → answer`.
//!
//! - The **base key** is the result's own content identity, computable before the work.
//! - A **read-list** ([`write_list`], [`write_list_with`], [`ListRef`]) names what one run of the work read, without
//!   the values: filed under the base, in a slot of the base's chain ([`Slot`], [`list_key`]).
//! - An **answer** is filed under a read-list's bytes and the digest each of its reads had
//!   ([`AnswerKey`], [`Digest`]; a read that found nothing is [`Digest::Nothing`]).
//!
//! [`lookup`] takes the base, reads the lists filed under it, asks its caller for the digest
//! each named read has *now*, and looks for an answer under those: a hit is the answer. On a
//! miss the caller runs the work, writes the list of what it read, places it ([`Filing`]) and
//! keeps the answer ([`keep_answer`]). A caller whose answer is not one record, or whose reads
//! are valued a list at a time, searches the same lists itself with [`walk`].
//!
//! The crate owns the key derivations (two domains, both in [`keys`]), the list's encoding and
//! layout word, and the chain. It does no IO and holds no clock: the store is a [`Store`] its
//! caller hands in.
//!
//! # The chain, and its bound
//!
//! A write-once store never replaces a value, so a fixed row of slots under a base, once all
//! hold other lists, could never take another: the base was not stored again. The chain is
//! generations of [`Bounds::width`] slots, `list_key(base, generation, n)`:
//!
//! - A generation is **full** when its last slot holds a list. Slots of a generation fill in
//!   order, and generation `g + 1` is written only by a filer that saw `g` full.
//! - A list is placed in generation 0 while it has room; after that in the **open** generation,
//!   the first that is not full (found by doubling, then halving, over the last slots: at most
//!   `2 log2(generations)` reads).
//! - A lookup searches generation 0, the open generation and the one before it: at most
//!   `3 * width - 1` lists, however long the chain. A list in an older generation is not
//!   searched again: that is the eviction, and it overwrites nothing. Its answers are keyed by
//!   the list's bytes, not its slot, so a list filed again finds them.
//! - The bound: [`Bounds::generations`]. With every generation full, [`Filing::place`] says
//!   [`Placed::Full`] and [`lookup`] says `full` in its miss: the caller counts it and names
//!   the base.
//!
//! What is promised, and tested in `tests.rs`: a result recomputed and filed is found by the
//! next lookup of the same base under the same read values, unless the placing said
//! [`Placed::Full`]. For a base with at
//! most `width` lists the slots, reads and puts are those of the four-slot form this replaces.
//!
//! Which generation a list lands in is a matter of the store's history (as which slot was
//! before); nothing correct depends on it: a lookup that searches other lists only misses.
//!
//! A read-list keeps its reads in its filer's order, and the order is part of the list: a
//! filer that wants one list per set of reads sorts them.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
// `#![blaze::sans_io]` marks a sans-IO core for `blaze::sans_io_core`; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

extern crate alloc;

mod chain;
mod keys;
mod list;
#[cfg(test)]
mod tests;

pub use chain::{
    Bounds, Doing, Error, Filing, Found, Kept, Kind, Placed, Store, Walked, keep_answer, lookup,
    walk,
};
pub use keys::{AnswerKey, Digest, Slot, list_key};
pub use list::{LAYOUT, ListError, ListRef, Reads, write_list, write_list_with};
