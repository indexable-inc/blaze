//! What batching with prefetch buys on the shared tables, one lookup at a time against
//! batches of 4 to 64, on 10^3, 10^5 and 10^7 entries with 2^20 random keys (2^18 for the
//! interner): ns per lookup.
//!
//! - `SharedMap<u64, u64>` (`Pinned::get` against `get_many` and `get_many_into`), hits and
//!   misses. It is measured here, not in blaze-hash's own `hash_cost`, because the shared
//!   tables are blaze-hash's `shared` feature, which this crate's dependency turns on. A hit
//!   is two dependent misses past the cache (its slot, then its boxed entry), a miss one.
//! - `HashCons` of a `u64` (`intern` of values already in, against `intern_many`): each
//!   value's BLAKE3 content ID comes first, so the probe's miss is a part of a lookup.
//!
//! The floor's unit is one overlapped miss: random loads over 256 MB with prefetch 16 ahead.
//!
//! `cargo run --release -p blaze-hashcons --example batch_cost`. The 10^7 rows hold about
//! 600 MB (boxed entries and slots; nodes and indexes), one table at a time.

use std::alloc::Global;
use std::{hint::black_box, time::Instant};

use blaze_core::SharedMap;
use blaze_hash::shared::SHARED_BATCH;
use blaze_hashcons::{Consed, HashCons};
use blaze_ids::{Domain, Hasher};

/// splitmix64's step: a fixed stream of well-mixed words.
fn splitmix(word: u64) -> u64 {
    let mut z = word.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The best of five timings of `pass` over `count` lookups: ns per lookup.
fn best_ns(count: usize, mut pass: impl FnMut() -> usize) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..5 {
        let start = Instant::now();
        black_box(pass());
        best = best.min(start.elapsed().as_nanos() as f64 / count as f64);
    }
    best
}

/// A dependent chase over `words` u64s (one miss's latency) and independent random loads
/// with prefetch 16 ahead (one overlapped miss).
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
                blaze_mem::swiss::prefetch(next.as_ptr().wrapping_add(ahead));
            }
            sum = sum.wrapping_add(next[pick]);
        }
        sum as usize
    });
    (latency, gather)
}

/// A value to intern: its word.
#[derive(PartialEq, Eq)]
struct Word(u64);

impl Consed for Word {
    fn domain() -> &'static Domain {
        static DOMAIN: Domain = Domain::new("blaze v2 hashcons batch_cost word 1");
        &DOMAIN
    }

    fn hash_content(&self, hasher: &mut Hasher) {
        hasher.update(&self.0.to_le_bytes());
    }
}

fn shared_batched<const B: usize>(map: &SharedMap<u64, u64, Global>, queries: &[u64]) -> f64 {
    let pinned = map.pin();
    best_ns(queries.len(), || {
        let mut found = 0;
        for chunk in queries.chunks_exact(B) {
            let keys: [&u64; B] = std::array::from_fn(|at| &chunk[at]);
            found += pinned.get_many(keys).iter().filter(|value| value.is_some()).count();
        }
        found
    })
}

fn intern_batched<const B: usize>(cons: &HashCons<Word>, queries: &[u64]) -> f64 {
    best_ns(queries.len(), || {
        let mut sum = 0u64;
        for chunk in queries.chunks_exact(B) {
            let values: [Word; B] = std::array::from_fn(|at| Word(chunk[at]));
            for handle in cons.intern_many(values) {
                sum = sum.wrapping_add(u64::from(handle.idx().raw()));
            }
        }
        sum as usize
    })
}

fn row(name: &str, n: u64, label: &str, one: f64, many: [f64; 5], extra: Option<f64>, floor: f64) {
    let best = many.iter().copied().fold(extra.unwrap_or(f64::INFINITY), f64::min);
    let extra = extra.map_or_else(|| "-".to_owned(), |ns| format!("{ns:.2}"));
    println!(
        "{name:<8} {n:>9} {label:<4} | {one:>7.2} | {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>7.2} | \
         {extra:>7} | {:>6.2} {:>6.2}",
        many[0],
        many[1],
        many[2],
        many[3],
        many[4],
        one / best,
        best / floor
    );
}

fn main() {
    let (latency, gather) = miss_costs(1 << 25);
    println!(
        "a miss over 256 MB: {latency:.1} ns alone, {gather:.2} ns overlapped ({:.1} misses \
         out at once)",
        latency / gather
    );
    println!(
        "{:<8} {:>9} {:<4} | {:>7} | {:>7} {:>7} {:>7} {:>7} {:>7} | {:>7} | {:>6} {:>6}",
        "table", "entries", "", "one", "4", "8", "16", "32", "64", "slice", "best/1", "/floor"
    );
    for n in [1_000u64, 100_000, 10_000_000] {
        let map: SharedMap<u64, u64, Global> = SharedMap::new_in("batch_cost", Global);
        {
            let pinned = map.pin();
            for at in 0..n {
                pinned.insert(splitmix(2 * at) & !1, at);
            }
        }
        let hits: Vec<u64> =
            (0..1u64 << 20).map(|at| splitmix(2 * (splitmix(at ^ 1) % n)) & !1).collect();
        let misses: Vec<u64> = (0..1u64 << 20).map(|at| splitmix(at ^ 2) | 1).collect();
        for (label, queries, misses_each) in [("hit", &hits, 2.0), ("miss", &misses, 1.0)] {
            let pinned = map.pin();
            let one = best_ns(queries.len(), || {
                queries.iter().filter(|key| pinned.get(*key).is_some()).count()
            });
            let many = [
                shared_batched::<4>(&map, queries),
                shared_batched::<8>(&map, queries),
                shared_batched::<16>(&map, queries),
                shared_batched::<32>(&map, queries),
                shared_batched::<64>(&map, queries),
            ];
            let mut out = vec![None; queries.len()];
            let slice = best_ns(queries.len(), || {
                pinned.get_many_into(queries, &mut out);
                out.iter().filter(|value| value.is_some()).count()
            });
            row("shared", n, label, one, many, Some(slice), misses_each * gather);
        }
        drop((map, hits, misses));

        let cons: HashCons<Word> = HashCons::new();
        for at in 0..n {
            cons.intern(Word(at));
        }
        let queries: Vec<u64> = (0..1u64 << 18).map(|at| splitmix(at ^ 3) % n).collect();
        let one = best_ns(queries.len(), || {
            let mut sum = 0u64;
            for &word in &queries {
                sum = sum.wrapping_add(u64::from(cons.intern(Word(word)).idx().raw()));
            }
            sum as usize
        });
        let many = [
            intern_batched::<4>(&cons, &queries),
            intern_batched::<8>(&cons, &queries),
            intern_batched::<16>(&cons, &queries),
            intern_batched::<32>(&cons, &queries),
            intern_batched::<64>(&cons, &queries),
        ];
        // An intern hit misses twice past the cache: the index group, then the node.
        row("hashcons", n, "hit", one, many, None, 2.0 * gather);
    }
    println!("(SharedMap's slice form batches {SHARED_BATCH} keys)");
}
