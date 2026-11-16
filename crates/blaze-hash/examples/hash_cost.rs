//! What a lookup costs on a [`HashMap`] of plain keys, which hashes the key's bytes in one step
//! (`hash_plain`, by `KeyHash`'s specialization for `FixedState`), against the generic path:
//! the same key in a newtype that is not a `PlainKey` ([`Opaque`], fed through `Hash` on the
//! same `FixedState`), and std's `HashMap` on its default SipHash-1-3 under a random key. Keys
//! are `u32`, `u64`, `[u8; 16]` and a 32-byte digest like `blaze_ids::ContentId`, at 10^3,
//! 10^5 and 10^7 entries; ns per hit, and per miss from a disjoint set of keys.
//!
//! `cargo run --release -p blaze-hash --example hash_cost`. The 10^7 rows hold, per table,
//! about 10^7 x (key + 4 B value) / 0.875 load, rounded up to a power of two: 2^24 slots, so
//! 134 MB of `u32`, 201 MB of `u64`, 335 MB of `[u8; 16]` and 604 MB of digests, plus the keys
//! asked with (the same again for hits and misses). One table is held at a time, so the
//! largest row peaks near 1.3 GB.
//!
//! The digest is a local type with `ContentId`'s shape and impl, not `ContentId` itself:
//! blaze-ids depends on this crate, so as a dev-dependency it would bring a second copy of it
//! whose `PlainKey` is another trait.

#![feature(allocator_api, min_specialization)]

use std::{
    alloc::Global,
    collections::HashMap as StdMap,
    hash::Hash,
    hint::black_box,
    time::Instant,
};

use blaze_hash::{HashMap, PlainKey};

/// A 32-byte digest, uniform, as `blaze_ids::ContentId` is.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
struct Digest([u8; 32]);

// SAFETY: a transparent wrapper of a plain array with derived `Eq`; lifetime-free, as a
// specialization trait's impl must be.
unsafe impl PlainKey for Digest {
    const UNIFORM: bool = true;
}

/// The generic path: a key that is not a `PlainKey`, so a `HashMap` of it hashes through its
/// derived `Hash`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Opaque<K>(K);

/// A key made from one word of a fixed-seed stream.
trait Key: PlainKey {
    const NAME: &'static str;
    /// Whether held keys are sequential ids (`0, 2, 4, ...`), the shape an index has, rather
    /// than mixed words, the shape a digest has.
    const SEQUENTIAL: bool = false;
    fn make(word: u64) -> Self;
}

impl Key for u32 {
    const NAME: &'static str = "u32";
    const SEQUENTIAL: bool = true;
    fn make(word: u64) -> u32 {
        word as u32
    }
}

impl Key for u64 {
    const NAME: &'static str = "u64";
    fn make(word: u64) -> u64 {
        word
    }
}

impl Key for [u8; 16] {
    const NAME: &'static str = "[u8; 16]";
    fn make(word: u64) -> [u8; 16] {
        let mut key = [0; 16];
        key[..8].copy_from_slice(&word.to_le_bytes());
        key[8..].copy_from_slice(&splitmix(word).to_le_bytes());
        key
    }
}

impl Key for Digest {
    const NAME: &'static str = "digest";
    fn make(word: u64) -> Digest {
        let mut key = [0; 32];
        key[..8].copy_from_slice(&word.to_le_bytes());
        let mut at = word;
        for chunk in key[8..].chunks_exact_mut(8) {
            at = splitmix(at);
            chunk.copy_from_slice(&at.to_le_bytes());
        }
        Digest(key)
    }
}

/// splitmix64's step: a fixed stream of well-mixed words.
fn splitmix(word: u64) -> u64 {
    let mut z = word.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// `n` keys to hold and `n` disjoint keys to miss with. Each key's first word is distinct
/// between the sets: an even word for a held key and an odd one for a miss (mixed keys keep
/// the parity bit of their seed word, so the sets cannot meet).
fn keys<K: Key>(n: usize) -> (Vec<K>, Vec<K>) {
    let word =
        |at: u64| if K::SEQUENTIAL { at } else { (splitmix(at ^ 0x5eed) & !1) | (at & 1) };
    let held = (0..n as u64).map(|at| K::make(word(2 * at))).collect();
    let missed = (0..n as u64).map(|at| K::make(word(2 * at + 1))).collect();
    (held, missed)
}

/// ns per call of `look`, over at least 10^7 calls in all.
fn time(count: usize, mut look: impl FnMut(usize) -> bool) -> f64 {
    let rounds = (10_000_000 / count).max(1);
    let start = Instant::now();
    let mut found = 0usize;
    for _ in 0..rounds {
        for at in 0..count {
            found += usize::from(look(black_box(at)));
        }
    }
    black_box(found);
    start.elapsed().as_nanos() as f64 / (rounds * count) as f64
}

/// Hit and miss ns of a table that `contains` asks.
fn measure<K>(held: &[K], missed: &[K], contains: impl Fn(&K) -> bool) -> (f64, f64) {
    let hit = time(held.len(), |at| contains(&held[at]));
    let miss = time(missed.len(), |at| contains(&missed[at]));
    (hit, miss)
}

fn row<K: Key + Hash>(n: usize) {
    let (held, missed) = keys::<K>(n);
    let values = || held.iter().enumerate().map(|(at, key)| (*key, at as u32));

    let mut plain: HashMap<K, u32, Global> = HashMap::new_in(Global);
    plain.extend(values());
    assert_eq!(plain.len(), n, "{}: the held keys are distinct", K::NAME);
    let (plain_hit, plain_miss) = measure(&held, &missed, |key| plain.contains_key(key));
    drop(plain);

    let (opaque_held, opaque_missed): (Vec<_>, Vec<_>) =
        (held.iter().map(|key| Opaque(*key)).collect(), missed.iter().map(|key| Opaque(*key)).collect());
    let mut opaque: HashMap<Opaque<K>, u32, Global> = HashMap::new_in(Global);
    opaque.extend(values().map(|(key, value)| (Opaque(key), value)));
    let (opaque_hit, opaque_miss) =
        measure(&opaque_held, &opaque_missed, |key| opaque.contains_key(key));
    drop(opaque);
    drop((opaque_held, opaque_missed));

    let std: StdMap<K, u32> = values().collect();
    let (std_hit, std_miss) = measure(&held, &missed, |key| std.contains_key(key));
    drop(std);

    println!(
        "{:<9} {:>9} | {:>6.2} {:>6.2} | {:>6.2} {:>6.2} | {:>6.2} {:>6.2}",
        K::NAME,
        n,
        plain_hit,
        plain_miss,
        opaque_hit,
        opaque_miss,
        std_hit,
        std_miss
    );
}

/// The best of `ROUNDS` timings of `pass` over `count` lookups: ns per lookup.
fn best_ns(count: usize, mut pass: impl FnMut() -> usize) -> f64 {
    const ROUNDS: usize = 7;
    let mut best = f64::INFINITY;
    for _ in 0..ROUNDS {
        let start = Instant::now();
        black_box(pass());
        best = best.min(start.elapsed().as_nanos() as f64 / count as f64);
    }
    best
}

/// What one cache miss costs on this machine, over an array of `words` u64s, larger than
/// every cache: a dependent chase (the latency, one miss at a time) and independent random
/// loads with software prefetch 16 ahead (the throughput: the latency over the misses the
/// core overlaps). The latter is the floor of a lookup that misses once.
fn miss_costs(words: usize) -> (f64, f64) {
    // Sattolo's shuffle: one cycle through every word, so the chase visits them all.
    let mut next: Vec<u64> = (0..words as u64).collect();
    let mut state = 0x5eed;
    for at in (1..words).rev() {
        state = splitmix(state);
        let to = (state % at as u64) as usize;
        next.swap(at, to);
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
                blaze_swiss::prefetch(next.as_ptr().wrapping_add(ahead));
            }
            sum = sum.wrapping_add(next[pick]);
        }
        sum as usize
    });
    (latency, gather)
}

/// ns per lookup of `get_many` over `queries`, `B` at a time.
fn batched<const B: usize>(map: &HashMap<u64, u32, Global>, queries: &[u64]) -> f64 {
    best_ns(queries.len(), || {
        let mut found = 0;
        for chunk in queries.chunks_exact(B) {
            let keys: [&u64; B] = std::array::from_fn(|at| &chunk[at]);
            found += map.get_many(keys).iter().filter(|value| value.is_some()).count();
        }
        found
    })
}

/// Lookups one at a time against batches of 4 to 32 (`get_many`) and the slice form
/// (`get_many_into`, a pipeline 8 keys deep), on a `HashMap<u64, u32>` of 10^3, 10^5 and
/// 10^7 entries, with 2^20 random hits and misses; the floor is the misses a lookup takes on
/// a table larger than the cache (a hit: its home control word and its slot's line, two; a
/// miss: the word, one) times one overlapped miss, and the ratio is the best form's time
/// over it.
fn batch_rows() {
    let (latency, gather) = miss_costs(1 << 25);
    println!(
        "a miss over 256 MB: {latency:.1} ns alone, {gather:.2} ns overlapped ({:.1} misses \
         out at once); a lookup's floor on a table past the cache: 2 misses, {:.2} ns",
        latency / gather,
        2.0 * gather
    );
    println!(
        "{:>9} {:<4} | {:>7} | {:>7} {:>7} {:>7} {:>7} | {:>7} | {:>7} {:>6}",
        "entries", "", "one", "4", "8", "16", "32", "pipe8", "best/1", "/floor"
    );
    for n in [1_000, 100_000, 10_000_000] {
        let (held, missed) = keys::<u64>(n);
        let mut map: HashMap<u64, u32, Global> = HashMap::with_capacity_in(n, Global);
        map.extend(held.iter().enumerate().map(|(at, key)| (*key, at as u32)));
        let pick = |from: &[u64], salt: u64| -> Vec<u64> {
            (0..1u64 << 20).map(|at| from[(splitmix(at ^ salt) % n as u64) as usize]).collect()
        };
        for (label, queries) in [("hit", pick(&held, 1)), ("miss", pick(&missed, 2))] {
            let one = best_ns(queries.len(), || {
                queries.iter().filter(|key| map.get(*key).is_some()).count()
            });
            let many = [
                batched::<4>(&map, &queries),
                batched::<8>(&map, &queries),
                batched::<16>(&map, &queries),
                batched::<32>(&map, &queries),
            ];
            let mut out = vec![None; queries.len()];
            let slice = best_ns(queries.len(), || {
                map.get_many_into(&queries, &mut out);
                out.iter().filter(|value| value.is_some()).count()
            });
            let best = many.iter().copied().fold(slice, f64::min);
            println!(
                "{:>9} {:<4} | {:>7.2} | {:>7.2} {:>7.2} {:>7.2} {:>7.2} | {:>7.2} | {:>7.2} {:>6.2}",
                n,
                label,
                one,
                many[0],
                many[1],
                many[2],
                many[3],
                slice,
                one / best,
                best / (if label == "hit" { 2.0 } else { 1.0 } * gather)
            );
        }
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("batch") {
        println!("ns per lookup, one at a time against get_many batches");
        batch_rows();
        return;
    }
    println!("ns per lookup: hit and miss");
    println!(
        "{:<9} {:>9} | {:^13} | {:^13} | {:^13}",
        "key", "entries", "plain (spec)", "Opaque (Hash)", "std"
    );
    for n in [1_000, 100_000, 10_000_000] {
        row::<u32>(n);
        row::<u64>(n);
        row::<[u8; 16]>(n);
        row::<Digest>(n);
    }
    println!("\nns per lookup, one at a time against get_many batches");
    batch_rows();
}
