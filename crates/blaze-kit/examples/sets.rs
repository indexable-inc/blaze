//! The sets' costs at three sizes of domain:
//! - `SparseSet` against a `Vec<bool>` worklist mark (a fill of 1/8 of the domain, then a clear,
//!   many times: what a per-item walk does);
//! - `IntervalSet` against `BTreeSet<u32>` (runs of 32 ids inserted, then probed);
//! - `ChunkedBitSet` against a plain `Vec<u64>` bit set (a copy and a union, as a dataflow
//!   join; half the chunks uniform).
//!
//!     cargo run --release -p blaze-kit --example sets

#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

#[expect(dead_code, reason = "the examples share the helper, each using part of it")]
mod bench;

use std::{alloc::System, collections::BTreeSet};

use blaze_int::Int;
use blaze_kit::{ChunkedBitSet, IntervalSet, SparseSet};

fn main() {
    for domain in [4_096u32, 262_144, 4_194_304] {
        let mut mix = bench::Mix(11);
        let fill: Vec<u32> = (0..domain / 8).map(|_| mix.below(u64::from(domain)).truncate_to::<u32>()).collect();
        let ops = u64::try_from(fill.len()).unwrap_or(1) * 16;

        let mut sparse: SparseSet<u32, _> = SparseSet::new_in(domain, System);
        let sparse_ns = bench::ns_per_op(ops, || {
            let mut n = 0u64;
            for _ in 0..16 {
                for &x in &fill {
                    n += u64::from(sparse.insert(x));
                }
                n += u64::from(sparse.iter().next().unwrap_or(0));
                sparse.clear();
            }
            n
        });
        let mut marks = vec![false; blaze_int::idx(domain)];
        let mut list: Vec<u32> = Vec::new();
        let marks_ns = bench::ns_per_op(ops, || {
            let mut n = 0u64;
            for _ in 0..16 {
                for &x in &fill {
                    let mark = &mut marks[blaze_int::idx(x)];
                    if !*mark {
                        *mark = true;
                        list.push(x);
                        n += 1;
                    }
                }
                // A cleared mark array costs the domain, or the list: here the domain, as a
                // `Vec<bool>` worklist mark reset with `fill` does.
                marks.fill(false);
                list.clear();
            }
            n
        });
        println!("domain {domain}: ns per insert (+ clear per round)");
        bench::row(blaze_int::idx(domain), &[("SparseSet", sparse_ns), ("Vec<bool> + list", marks_ns)]);

        let starts: Vec<u32> = (0..1024u32).map(|_| mix.below(u64::from(domain - 32)).truncate_to::<u32>()).collect();
        let probes: Vec<u32> = (0..100_000).map(|_| mix.below(u64::from(domain)).truncate_to::<u32>()).collect();
        let mut intervals: IntervalSet<u32, _> = IntervalSet::new_in(System);
        let mut tree = BTreeSet::new();
        let insert_ops = 1024u64;
        let interval_insert = bench::ns_per_op(insert_ops, || {
            intervals.clear();
            for &s in &starts {
                intervals.insert_range(s..s + 32);
            }
            u64::try_from(intervals.ranges_len()).unwrap_or(0)
        });
        let tree_insert = bench::ns_per_op(insert_ops, || {
            tree.clear();
            for &s in &starts {
                tree.extend(s..s + 32);
            }
            u64::try_from(tree.len()).unwrap_or(0)
        });
        let interval_probe = bench::ns_per_op(100_000, || probes.iter().map(|&x| u64::from(intervals.contains(x))).sum());
        let tree_probe = bench::ns_per_op(100_000, || probes.iter().map(|&x| u64::from(tree.contains(&x))).sum());
        println!("domain {domain}: ns per run of 32 inserted; ns per probe");
        bench::row(
            blaze_int::idx(domain),
            &[("IntervalSet insert", interval_insert), ("BTreeSet insert", tree_insert), ("IntervalSet probe", interval_probe), ("BTreeSet probe", tree_probe)],
        );

        // Half the chunks full, the rest scattered.
        let mut a: ChunkedBitSet<u32, _> = ChunkedBitSet::new_empty_in(domain, System);
        let mut words_a = vec![0u64; blaze_int::idx(domain).div_ceil(64)];
        for x in 0..domain / 2 {
            a.insert(x);
            words_a[blaze_int::idx(x / 64)] |= 1 << (x % 64);
        }
        let mut b = a.clone();
        let mut words_b = words_a.clone();
        for &x in &fill {
            b.insert(x);
            words_b[blaze_int::idx(x / 64)] |= 1 << (x % 64);
        }
        let joins = 100u64;
        let chunked = bench::ns_per_op(joins, || {
            let mut n = 0u64;
            for _ in 0..joins {
                let mut copy = a.clone();
                n += u64::from(copy.union(&b));
            }
            n
        });
        let plain = bench::ns_per_op(joins, || {
            let mut n = 0u64;
            for _ in 0..joins {
                let mut copy = words_a.clone();
                for (w, o) in copy.iter_mut().zip(&words_b) {
                    *w |= o;
                }
                n += copy[0];
            }
            n
        });
        println!("domain {domain}: ns per (copy + union) of a dataflow state");
        bench::row(blaze_int::idx(domain), &[("ChunkedBitSet", chunked), ("Vec<u64> bits", plain)]);
    }
}
