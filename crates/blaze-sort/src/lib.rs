//! Sorting, searching and comparing, the algorithm chosen by the element type
//! (docs/v2/plan/design/std.md, "The fast path is chosen by the type").
//!
//! The one rule: a fast path is taken only where no code can tell it from the general one,
//! the same result for every input. "Plain old data" is not that condition for a sort: a
//! `(key, payload)` struct with `Ord` on the key alone is plain, and stability shows on it.
//! Stability is unobservable exactly where values that compare equal are one value: integers,
//! `bool`, `char`, floats under their total order, and arrays and tuples of those, which is
//! [`RadixKey`]; a struct is one when its type says so ([`radix_key!`]).
//!
//! | operation | general form (core) | here, and when |
//! |---|---|---|
//! | [`sort`] | `sort_unstable` | core's unstable sort on the keys' integer images, `T: RadixKey`; no allocation (the in-place radix sort lost, see below) |
//! | [`sort_in`] | `sort`, `sort_unstable` | least-significant-digit radix sort through scratch from the named allocator, `T: RadixKey` |
//! | [`sort_by_key_in`] | `sort_by_key` (stable) | stable radix sort of (key image, index) pairs, then the values permuted in place: any `T`, `K: RadixKey`; `key` called once a value |
//! | [`sort_stable_in`] | `sort` | [`sort_in`] when `T: RadixKey` (specialized); else a bottom-up merge sort, scratch from the named allocator |
//! | [`sort_unstable`] | `sort_unstable` | [`sort`] when `T: RadixKey` (specialized); else core's |
//! | [`lower_bound`], [`binary_search`] | `partition_point`, `binary_search` | halving by the images with a conditional select a step, `T: RadixKey` |
//! | [`position`] | `iter().position` | 64 bytes a step by vector compare, `T: PlainKey` of 1, 2, 4 or 8 bytes (specialized) |
//! | [`contains`] | `contains` | core's: its vector loop tied or beat the lane search (measured, `contains`'s notes) |
//! | [`eq`] | `==` | one block compare of the bytes, `T: PlainKey` (specialized) |
//! | [`cmp`] | `Ord::cmp` | the first differing byte by vector compare, then that element's `cmp`, `T: Ord + PlainKey` (specialized) |
//! | [`is_sorted`], [`dedup`], [`min`], [`max`] | `is_sorted`, `Vec::dedup`, `iter().min()`, `iter().max()` | branch-free over blocks of 64 images, `T: RadixKey` |
//!
//! Every digit position on which all keys agree is skipped (ids that fit 20 bits in a `u64`
//! cost three passes, not eight), and input already in order either way is found in one pass
//! that stops at the first key out of order both ways.
//!
//! # How the type decides, and why each tool
//!
//! - A specialization trait (`#[rustc_specialization_trait]`) where a generic front door
//!   (`sort_stable_in::<T: Ord>`, `position::<T: PartialEq>`) must choose for the caller:
//!   [`RadixKey`] here, blaze-hash's `PlainKey` (byte equality) for the compares. Each
//!   specializing impl adds only that bound, in `min_specialization`'s sound subset.
//! - A `const { .. }` branch on the type's properties (`T::BYTES`, `size_of`, `align_of`)
//!   where the choice is between kernels for one bound: the word a key's image is sorted in
//!   (`u32`, `u64`, `u128`, or bytes), the lane width of a search. The dead arms are removed.
//! - Not reflection: whether `Ord` is derived or hand-written, which is the property, is not
//!   in a type's shape (`key`'s notes).
//!
//! # Floors and measurements (rule 67)
//!
//! `examples/cost.rs`, best of 5 rounds, ns a key on this Mac, core against blaze (ratio is
//! core/blaze, above 1 is faster), random keys:
//!
//! | row | 4096 | 1e5 | 1e7 |
//! |---|---|---|---|
//! | `sort_in` u32 | 4.32 / 2.32 (1.86x) | 6.51 / 2.28 (2.85x) | 10.23 / 2.40 (4.27x) |
//! | `sort_in` u64 | 4.30 / 4.48 (0.96x) | 6.58 / 4.61 (1.43x) | 10.00 / 4.95 (2.02x) |
//! | `sort_in` u64, 16 bits used | 4.29 / 1.27 (3.38x) | 6.40 / 1.31 (4.90x) | 6.29 / 1.42 (4.43x) |
//! | `sort_in` (u32,u32) | 6.69 / 5.32 (1.26x) | 8.95 / 5.13 (1.74x) | 13.34 / 5.37 (2.49x) |
//! | `sort_in` f64 | 6.14 / 7.26 (0.85x) | 8.45 / 7.35 (1.15x) | 12.80 / 7.37 (1.74x) |
//! | `sort_stable_in` u32 against core's `sort` | 4.66 / 2.36 (1.97x) | 7.84 / 2.31 (3.40x) | 13.57 / 2.48 (5.46x) |
//! | `sort_by_key_in` u32 | 5.00 / 2.93 (1.71x) | 7.95 / 3.39 (2.34x) | 14.22 / 5.61 (2.54x) |
//! | `binary_search` (u32,u32), ns a probe | 15.76 / 9.90 (1.59x) | 31.55 / 17.86 (1.77x) | 74.95 / 40.94 (1.83x) |
//! | `min` f64 | 3.06 / 0.15 (20x) | 3.20 / 0.16 (21x) | 3.08 / 0.16 (20x) |
//!
//! Floors. A radix pass reads and writes every key once, scattered over 256 buckets; the floor
//! of a sorted pass is a sequential read, which `is_sorted` measures at 0.04 ns a u32 key, so
//! the u32 sort's 0.60 ns a key a pass (2.40 over 4 passes at 1e7) is 15x the read floor and
//! bound by the scatter's writes, not the count. `sort_by_key_in` sorts 8-byte (image, index)
//! pairs and gathers the values once: its floor is the pairs' sort plus one gather, and at 1e7
//! it costs 5.61 against `sort_in`'s 2.40 for the same keys alone. `cmp`, `eq` and `is_sorted`
//! are at the read floor (0.04 ns a u32 key at 4096). A search's floor is `log2 n` dependent
//! loads: 23.79 ns a probe at 1e7 u32 against core's 22.24.
//!
//! Where blaze is behind core, kept and stated: [`binary_search`] on unsigned keys (0.86x to
//! 0.98x: core's search has no image to compute); [`min`]/[`max`] on unsigned keys below 4096
//! (0.36x at 256 u32); [`sort_by_key_in`] below a few hundred values (0.51x at 16 u32: the
//! pair buffer's allocation and the gather are fixed costs); [`sort_in`] on 8-digit keys at
//! 4096 (0.96x u64), the price of the varying-digit pass in front of [`lsd_from`].

#![no_std]
#![feature(min_specialization, rustc_attrs, portable_simd)]
#![allow(internal_features, reason = "`min_specialization` and `rustc_specialization_trait`, within the subset std itself uses (the specializing impls' notes)")]
// `#![blaze::sans_io]` on each module and `#![blaze::trust]` below name blaze's tool; inert
// under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::trust(
    unsafe,
    reason = "the radix scatters write each key at its digit's next slot unchecked (sound by `RadixKey`'s promise that the image is pure); the merge and the permutation move values through raw pointers with a guard that refills the gap; plain keys are read as bytes or lanes (`PlainKey`'s promise of no padding and byte equality); each block names what it relies on"
)]
#![expect(
    blaze::as_cast,
    reason = "a key's image is truncated to the word it is sorted in and a digit to a byte, and a `u32` index widened; each cast is that truncation or widening"
)]

#[cfg(test)]
extern crate std;

mod compare;
mod key;
mod merge;
mod plant;
mod radix;
mod search;
mod sort;
#[cfg(test)]
mod tests;

pub use compare::{cmp, dedup, eq, is_sorted, max, min};
pub use key::RadixKey;
pub use search::{binary_search, contains, lower_bound, position};
pub use sort::{
    LSD_FROM, TINY, kernels, lsd_from, sort, sort_by_key_in, sort_in, sort_stable_by_in, sort_stable_in, sort_unstable,
    sort_unstable_by,
};
