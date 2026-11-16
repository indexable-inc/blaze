//! What is left of the invalidation oracle once the query database went (2026-11-15,
//! docs/v2/plan/design/bottom.md): the pieces that never read a query's record, which the store,
//! the format and the profile use.
//!
//! - [`bytes`]: where two output files first differ, mapped, across cores; a slice by
//!   [`blaze_store_check::bytes`], the one byte comparison.
//! - [`columns`]: the exact round trip every column encoding must make.
//! - [`tree`]: the digest of a source tree, its directories walked and files hashed on every core.

#![deny(unsafe_code)]
// `#[expect(blaze::..)]` names blaze's own lints; inert under rustc.
#![feature(register_tool)]
#![register_tool(blaze)]

pub mod bytes;
pub mod columns;
pub mod tree;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// SplitMix64: the same numbers from the same seed on every machine, for the cases the tests
/// draw. What a check samples is [`blaze_store_check::Setting::sampled`]'s, the one sampler.
pub use blaze_ids::rng::SplitMix64 as Rng;

/// How many cores the work is cut across: what the system says is available, or one.
pub(crate) fn cores() -> usize {
    static CORES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CORES.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}
