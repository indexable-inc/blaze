//! blaze's specialized collections: the structures a compiler and its runtime need that Rust's
//! std lacks, each blaze's own code (no third-party crate), each in an allocator its caller
//! names, each with its floor stated in its doc (bytes per element, cache lines per operation)
//! and a cost example under `examples/` that prints nanoseconds per operation at three sizes
//! against the std or naive form.
//!
//! | type | what | instead of |
//! |---|---|---|
//! | [`Id`], [`id!`], [`NonMax`] | a typed 32-bit id; `Option<Id>` is 4 bytes | `usize` indices into anything |
//! | [`Table`], [`TableSet`], [`TableMap`], [`table::Branded`] | a vector indexed only by its own id; a bit per id; a dense side table; reads proven in range once | `Vec<T>[usize]`, `la_arena`, `index_vec` |
//! | [`InlineVec`] | `N` values inline, then spilled to the allocator | `smallvec` |
//! | [`UnionFind`] | union by rank, path halving, values per root, snapshots and rollback | `ena` |
//! | [`SparseSet`] | O(1) insert, remove, contains, clear; iteration in insertion order | hand-rolled worklists |
//! | [`IntervalSet`] | sorted disjoint ranges | rustc's `IntervalSet` |
//! | [`ChunkedBitSet`] | a bit set in 2,048-bit chunks, uniform chunks one word, mixed chunks shared between copies | rustc's `ChunkedBitSet` |
//! | [`Heap4`] | a 4-ary min-heap in one vector | `BinaryHeap<Reverse<T>>` |
//! | [`TimerWheel`] | a hierarchical timer wheel, O(1) insert and cancel by a generational handle | a heap of deadlines |
//! | [`codec`], [`Dict`] | zigzag, LEB128 and a delta column on them; a dictionary column builder (bit-packing and Parquet's hybrid live in blaze-parquet) | ad hoc encoders, `IndexSet` interners |
//! | [`StaticSearch`] | a read-only sorted set laid out as an implicit B-tree of 16 keys a node | `binary_search` |
//!
//! # Rules every type here keeps
//!
//! - The allocator is a type parameter with no default and every constructor takes it
//!   (`new_in`, `try_with_capacity_in`); no `Default`, no `FromIterator` (rule 11).
//! - Every allocating method has a `try_` form returning the refusal (an
//!   [`AllocFailure`](blaze_own::AllocFailure), or a [`Refused`](blaze_own::Refused) with the
//!   value handed back) and leaving the value as it was; the plain form panics with it, which
//!   fails one job (rule 20). Held by `every_failure` tests.
//! - Where an index is stored it is a 32-bit [`Id`]; integer conversions go through
//!   `blaze-int`, never a bare `as`.
//! - Each structure has a model-based property test on `blaze_sim::check` against the obvious
//!   slow structure, and a planted bug its oracle catches (`src/tests/`).
//!
//! # Unsafe code
//!
//! [`InlineVec`] (its union of inline slots and a heap block), [`ChunkedBitSet`] (its counted
//! chunk blocks), [`Heap4`] (the hole in a sift) and [`table::Branded`] (one unchecked read,
//! proven in range by the brand) hold the crate's `unsafe`; each block names the invariant it
//! relies on. Everything else is safe code over blaze-own's vectors.
//!
//! # Callers
//!
//! Moved (2026-11-15): blaze-graph's batch order and blaze-protocol's node order (Kahn's ready
//! set) and blaze-profile's replay events to [`Heap4`]; blaze-parquet's thrift zigzag and
//! varint to [`codec`]; blaze-typeck's toy variable table to [`UnionFind`]. Left for when their
//! crates come back from the parked list (the root manifest's `exclude`): blaze-codegen's
//! `BinaryHeap` (src/driver/support.rs) to [`Heap4`], and blaze-link's `SmallVec`s
//! (symbols.rs, macho.rs, native.rs, target.rs, lib.rs) to [`InlineVec`].

#![no_std]
#![feature(allocator_ext, core_intrinsics)]
#![allow(
    internal_features,
    reason = "`core_intrinsics` for `prefetch_read_data` alone, which has no stable form: \
              `StaticSearch::lower_bounds` prefetches each probe's next node (measured in \
              search.rs's doc)"
)]
// blaze's lints (`blaze::trust`) name their tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::trust(
    unsafe,
    reason = "the collections' own memory: `InlineVec`'s union of inline slots and a heap block, `ChunkedBitSet`'s counted chunk blocks, `Heap4`'s sift hole and `Branded`'s read proven in range; each block names the invariant it relies on"
)]

// Tests compare against std's collections and keep the planted bug in a thread-local.
#[cfg(test)]
extern crate std;

// `macro_use`, first: the crate's own code (its tests) names `id!` by textual scope; a path to
// an exported macro is refused in a crate whose root carries a tool attribute, as blaze-own
// found.
#[macro_use]
mod id;
#[macro_use]
pub mod newtype;
mod error;

pub mod chunked;
pub mod codec;
pub mod dict;
pub mod heap4;
pub mod inline_vec;
pub mod interval;
pub mod search;
pub mod sparse;
pub mod table;
pub mod union_find;
pub mod wheel;

// A switch that plants a known bug, for the oracles' controls (rule 53); `false` outside tests.
mod plant;

#[cfg(test)]
mod tests;

pub use chunked::ChunkedBitSet;
pub use dict::Dict;
pub use heap4::Heap4;
pub use id::{Id, NonMax};
pub use inline_vec::InlineVec;
pub use interval::IntervalSet;
pub use search::StaticSearch;
pub use sparse::SparseSet;
pub use table::{Table, TableMap, TableSet};
pub use union_find::UnionFind;
pub use wheel::TimerWheel;
