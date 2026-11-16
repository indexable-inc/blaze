//! blaze's lock-free queues (AGENTS.md rule standard-structures), the one home of each:
//!
//! - [`ArrayQueue`]: bounded, many producers and many consumers, a ring of slots each with a
//!   stamp (Vyukov's bounded queue, as `crossbeam-queue`'s). A push or a pop is one
//!   compare-and-swap of the tail or the head and one release store of the slot's stamp; a
//!   full ring refuses a push and hands the value back.
//! - [`SegQueue`]: unbounded, many producers and many consumers, a linked list of blocks of
//!   `CAP` slots (31 by default, as `crossbeam-queue`'s). A push or a pop is one
//!   compare-and-swap of the tail or the head; a block is freed by the last of its readers,
//!   which the slots' `READ` and `DESTROY` bits decide with no epoch or hazard pointer.
//!
//! Orderings, in one line: the head and tail move by `SeqCst` compare-and-swaps (so the
//! emptiness and fullness checks, a `SeqCst` fence and a relaxed load of the other end, see a
//! single order), a slot's value is published by a `Release` store or `fetch_or` of its stamp
//! or state and taken by an `Acquire` load of it, and a block is linked and published with
//! `Release` and found with `Acquire`.
//!
//! Neither blocks: a waiting receiver is a channel's (`blaze_core::chan` in blaze-core,
//! which wraps these). A crate of its own, below blaze-core, so that every crate that hands a
//! value between threads (the allocator's, the launcher's runtime) has the same two. The
//! memory of both is `Global`'s, named at each allocation.
//!
//! Under `--cfg blaze_check` the atomics are the model checker's and every slot's value is a
//! tracked place, so a race on it, or a read of it after its block is freed, is a finding;
//! `tests.rs` checks each queue and plants a bug in each the model must catch.

#![no_std]
#![feature(allocator_api)]
// `#![blaze::trust]` below names blaze's tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::trust(
    unsafe,
    reason = "lock-free queues: a slot's value is written by the one push its stamp or state gives the slot to and read by the one pop it then gives it to, and a block is freed by the one reader the slots' bits name last"
)]
#![allow(
    unsafe_code,
    reason = "a slot's value is reached through its stamp or state: each `unsafe` says what orders it"
)]

extern crate alloc;
#[cfg(any(test, blaze_check))]
extern crate std;

mod array;
mod seg;
mod sync;

#[cfg(test)]
mod tests;

pub use self::{array::ArrayQueue, seg::SegQueue};

/// The owning iterators, as crossbeam's queues have them.
pub mod iter {
    pub use crate::{array::IntoIter as ArrayIntoIter, seg::IntoIter as SegIntoIter};
}

/// A value padded to 128 bytes, the pair of lines arm64 and x86 prefetch together: the head
/// and the tail are written by different threads and must not share a line.
#[repr(align(128))]
struct Padded<T>(T);

impl<T> core::ops::Deref for Padded<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}

/// A safeguard a model test removes, to see the model catch the removal. Production code
/// passes [`Plant::None`], a constant the branch folds away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// [`ArrayQueue`]'s push publishes its slot with a relaxed store: a pop reads the value
    /// with nothing ordering the write before it (a race).
    #[cfg(test)]
    RelaxedPublish,
    /// [`SegQueue`]'s pop of a block's last slot frees the block without asking its other
    /// readers: one still reading its value reads freed memory.
    #[cfg(test)]
    FreeUnasked,
}
