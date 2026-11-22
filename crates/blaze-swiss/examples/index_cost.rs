//! What a lookup in an [`Index`] costs one at a time against [`Index::find_many`] batches of
//! 4, 8, 16 and 32, on indexes of 10^3, 10^5 and 10^7 values naming a column of `u64` keys,
//! with 2^20 random hits and misses: ns per lookup. A hit reads the home group (one line) and
//! then the key in the caller's column; `find_many` prefetches every group, then (through
//! `ahead`) every first tag match's column entry, then probes. The floor on an index past the
//! cache is two misses a lookup (the group, the column entry) at one overlapped miss each,
//! measured here as random loads over 256 MB with prefetch 16 ahead; the ratio is the best
//! batch's time over it.
//!
//! `cargo run --release -p blaze-swiss --example index_cost`. The 10^7 row holds 2^22 groups
//! of 32 bytes (134 MB) and an 80 MB column; the miss measurement 256 MB before them.
//!
//! The index's memory is the process's heap, named once at the entry (`blaze_own::Heap::process`,
//! a plain binary's `fn main`), not `Global`: what is measured is the probe, not the allocator,
//! and the index allocates only while it is built, outside every timing.

#![feature(allocator_api)]

use std::{hint::black_box, time::Instant};

use blaze_own::{Heap, SharedAlloc};
use blaze_swiss::{Index, prefetch};

/// splitmix64's step: a fixed stream of well-mixed words.
fn splitmix(word: u64) -> u64 {
    let mut z = word.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The best of seven timings of `pass` over `count` lookups: ns per lookup.
fn best_ns(count: usize, mut pass: impl FnMut() -> usize) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..7 {
        let start = Instant::now();
        black_box(pass());
        best = best.min(start.elapsed().as_nanos() as f64 / count as f64);
    }
    best
}

/// A dependent chase over `words` u64s (one miss's latency) and independent random loads
/// with prefetch 16 ahead (one overlapped miss: the floor's unit).
fn miss_costs(words: usize) -> (f64, f64) {
    let mut next: Vec<u64> = (0..words as u64).collect();
    let mut state = 0x5eed;
    for at in (1..words).rev() {
        state = splitmix(state);
        next.swap(at, (state % at as u64) as usize);
    }
    let steps = 2_000_000;
    let latency = best_ns(steps, || {
        let mut at = 0usize;
        for _ in 0..steps {
            at = next[at] as usize;
        }
        at
    });
    let picks: Vec<usize> =
        (0..1u64 << 20).map(|at| (splitmix(at ^ 0xfeed) % words as u64) as usize).collect();
    let gather = best_ns(picks.len(), || {
        let mut sum = 0u64;
        for (at, &pick) in picks.iter().enumerate() {
            if let Some(&ahead) = picks.get(at + 16) {
                prefetch(next.as_ptr().wrapping_add(ahead));
            }
            sum = sum.wrapping_add(next[pick]);
        }
        sum as usize
    });
    (latency, gather)
}

/// ns per lookup of `find_many` over `queries`, `B` at a time.
fn batched<const B: usize>(index: &Index<SharedAlloc<'static>>, column: &[u64], queries: &[u64]) -> f64 {
    best_ns(queries.len(), || {
        let mut found = 0;
        for chunk in queries.chunks_exact(B) {
            let hashes: [u64; B] = std::array::from_fn(|at| splitmix(chunk[at]));
            let answers = index.find_many(
                hashes,
                |value| prefetch(column.as_ptr().wrapping_add(value as usize)),
                |at, value| column[value as usize] == chunk[at],
            );
            found += answers.iter().filter(|value| value.is_some()).count();
        }
        found
    })
}

fn main() {
    let heap = Heap::process();
    let (latency, gather) = miss_costs(1 << 25);
    println!(
        "a miss over 256 MB: {latency:.1} ns alone, {gather:.2} ns overlapped ({:.1} misses \
         out at once); a lookup's floor on an index past the cache: 2 misses, {:.2} ns",
        latency / gather,
        2.0 * gather
    );
    println!(
        "{:>9} {:<4} | {:>7} | {:>7} {:>7} {:>7} {:>7} {:>7} | {:>7} {:>6}",
        "values", "", "one", "4", "8", "16", "32", "64", "best/1", "/floor"
    );
    for n in [1_000u64, 100_000, 10_000_000] {
        // Held keys are even words of the stream, misses odd ones: the sets never meet.
        let column: Vec<u64> = (0..n).map(|at| splitmix(2 * at) & !1).collect();
        let index = Index::new_in(heap.shared());
        for (at, &key) in column.iter().enumerate() {
            index.insert(at, splitmix(key), at as u32, |value| splitmix(column[value as usize]));
        }
        let hits: Vec<u64> =
            (0..1u64 << 20).map(|at| column[(splitmix(at ^ 1) % n) as usize]).collect();
        let misses: Vec<u64> = (0..1u64 << 20).map(|at| splitmix(at ^ 2) | 1).collect();
        for (label, queries) in [("hit", hits), ("miss", misses)] {
            let one = best_ns(queries.len(), || {
                queries
                    .iter()
                    .filter(|&&key| {
                        index.find(splitmix(key), |value| column[value as usize] == key).is_some()
                    })
                    .count()
            });
            let many = [
                batched::<4>(&index, &column, &queries),
                batched::<8>(&index, &column, &queries),
                batched::<16>(&index, &column, &queries),
                batched::<32>(&index, &column, &queries),
                batched::<64>(&index, &column, &queries),
            ];
            let best = many.iter().copied().fold(f64::INFINITY, f64::min);
            // A hit misses twice (group, column entry), a miss once (the group).
            let floor = if label == "hit" { 2.0 * gather } else { gather };
            println!(
                "{:>9} {:<4} | {:>7.2} | {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>7.2} | {:>7.2} {:>6.2}",
                n,
                label,
                one,
                many[0],
                many[1],
                many[2],
                many[3],
                many[4],
                one / best,
                best / floor
            );
        }
    }
}
