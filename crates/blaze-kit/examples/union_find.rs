//! The cost of a unify and a find on random pairs: `UnionFind` (rank, halving) outside and
//! inside a snapshot, against a naive parent array (no rank, no compression), at three sizes.
//!
//!     cargo run --release -p blaze-kit --example union_find

#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

#[expect(dead_code, reason = "the examples share the helper, each using part of it")]
mod bench;

use std::alloc::System;

use blaze_int::Int;
use blaze_kit::{Id, UnionFind};

fn naive_root(parent: &[u32], mut at: u32) -> u32 {
    while parent[blaze_int::idx(at)] != at {
        at = parent[blaze_int::idx(at)];
    }
    at
}

fn main() {
    println!("ns per (unify + find) of a random pair");
    for size in [1_000u32, 100_000, 1_000_000] {
        let mut mix = bench::Mix(3);
        let pairs: Vec<(u32, u32)> =
            (0..size).map(|_| (mix.below(u64::from(size)).truncate_to::<u32>(), mix.below(u64::from(size)).truncate_to::<u32>())).collect();
        let ops = u64::from(size);
        let run = |snapshot: bool| {
            bench::ns_per_op(ops, || {
                let mut uf: UnionFind<u32, (), _> = UnionFind::try_with_capacity_in(blaze_int::idx(size), System).unwrap_or_else(|e| panic!("{e}"));
                for _ in 0..size {
                    uf.new_key(());
                }
                let open = snapshot.then(|| uf.snapshot());
                let mut hits = 0u64;
                for &(a, b) in &pairs {
                    let _ = uf.unify(a, b, |(), ()| Ok::<(), ()>(()));
                    hits += u64::from(uf.find(a).to_u32() == uf.find(b).to_u32());
                }
                if let Some(open) = open {
                    uf.rollback_to(open);
                }
                hits
            })
        };
        let plain = run(false);
        let logged = run(true);
        let naive = bench::ns_per_op(ops, || {
            let mut parent: Vec<u32> = (0..size).collect();
            let mut hits = 0u64;
            for &(a, b) in &pairs {
                let (ra, rb) = (naive_root(&parent, a), naive_root(&parent, b));
                parent[blaze_int::idx(rb)] = ra;
                hits += u64::from(naive_root(&parent, a) == naive_root(&parent, b));
            }
            hits
        });
        bench::row(blaze_int::idx(size), &[("UnionFind", plain), ("in a snapshot (+ rollback)", logged), ("naive", naive)]);
    }
}
