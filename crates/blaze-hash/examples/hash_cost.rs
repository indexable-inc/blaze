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

fn main() {
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
}
