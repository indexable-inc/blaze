//! `StaticSearch::lower_bound` against `slice::partition_point` (`binary_search`'s form) on
//! random probes, at three sizes: inside L1/L2, about the last-level cache, and well past it.
//!
//!     cargo run --release -p blaze-kit --example static_search

#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

#[expect(dead_code, reason = "the examples share the helper, each using part of it")]
mod bench;

use std::alloc::System;

use blaze_int::Int;
use blaze_kit::StaticSearch;

fn main() {
    println!("ns per lower bound of a random probe");
    for size in [1_000usize, 10_000, 30_000, 100_000, 300_000, 1_000_000, 4_000_000, 64_000_000] {
        let mut mix = bench::Mix(13);
        let mut keys: Vec<u32> = (0..size).map(|_| mix.next().truncate_to::<u32>()).collect();
        keys.sort_unstable();
        keys.dedup();
        let set = StaticSearch::try_new_in(&keys, System).unwrap_or_else(|e| panic!("{e}"));
        let probes: Vec<u32> = (0..1_000_000).map(|_| mix.next().truncate_to::<u32>()).collect();
        let ops = 1_000_000u64;
        let tree = bench::ns_per_op(ops, || probes.iter().map(|&x| u64::try_from(set.lower_bound(x)).unwrap_or(0)).fold(0, u64::wrapping_add));
        let batched = bench::ns_per_op(ops, || {
            let (eights, _) = probes.as_chunks::<8>();
            let mut sum = 0u64;
            for xs in eights {
                for at in set.lower_bounds(xs) {
                    sum = sum.wrapping_add(u64::try_from(at).unwrap_or(0));
                }
            }
            sum
        });
        let binary =
            bench::ns_per_op(ops, || probes.iter().map(|&x| u64::try_from(keys.partition_point(|&k| k < x)).unwrap_or(0)).fold(0, u64::wrapping_add));
        bench::row(keys.len(), &[("StaticSearch", tree), ("lower_bounds x8", batched), ("partition_point", binary), ("bytes", format!("{} vs {}", set.bytes(), keys.len() * 4))]);
    }
}
