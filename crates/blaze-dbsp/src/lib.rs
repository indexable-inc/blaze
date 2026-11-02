//! blaze v2's delta engine (docs/v2/plan/design/dbsp.md §3, notes in
//! docs/v2/plan/lanes/dbsp/engine.md): the generic pieces the write-side maintainers of name
//! resolution, impl tables and trait candidate sets are made of.
//!
//! - [`ZSet`]: a multiset with integer weights, DBSP's unit of change.
//! - [`diff_seq`]: the delta between two op logs, as removed and added positions.
//! - [`ReadIndex`]: who read which key, both ways: what a changed binding wakes.
//! - [`Arrangement`]: a weighted index from a key to its values in sort order, differential
//!   dataflow's arrangement, with the keys whose visible values changed.
//! - [`closure`]: DRed's over-delete region.
//! - [`Worklist`]: a FIFO with set semantics, the semi-naive frontier.
//! - [`SliceHasher`]: a published slice's content hash.
//! - [`Work`]: the counters the cost tests assert.
//!
//! Every structure is flat: Hash maps to `u32` rows, buffers kept and reused, and no
//! allocation on a lookup.

#![forbid(unsafe_code)]

mod arrangement;
mod closure;
mod diff;
mod hash;
mod pos;
mod read_index;
mod work;
mod worklist;
mod zset;

#[cfg(test)]
mod tests;

pub use crate::{
    arrangement::Arrangement,
    closure::{Closure, closure},
    diff::{SeqDiff, diff_seq},
    hash::SliceHasher,
    pos::Pos,
    read_index::ReadIndex,
    work::Work,
    worklist::Worklist,
    zset::ZSet,
};

/// A Z-set weight: how many times a fact is in, negative for a retraction.
pub type Weight = i32;

/// Counts allocations so the lookups' tests hold them to a budget.
#[cfg(all(test, not(blaze_check)))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
