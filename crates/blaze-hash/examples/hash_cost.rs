//! What a lookup costs on each table blaze has for plain keys: [`PlainMap`] (`hash_plain`, the
//! key's bytes in one step), [`HashMap`] (`FixedState`, the key fed through `Hash`) and std's
//! `HashMap` on its default SipHash-1-3 under a random key, for `u32`, `u64`, `[u8; 16]` and
//! a 32-byte digest like `blaze_ids::ContentId`, at 10^3, 10^5 and 10^7 entries; ns per hit,
//! and per miss from a disjoint set of keys.
//!
//! `cargo run --release -p blaze-hash --example hash_cost`. The 10^7 rows hold, per table,
//! about 10^7 x (key + 4 B value) / 0.875 load, rounded up to a power of two: 2^24 slots, so
//! 134 MB of `u32`, 201 MB of `u64`, 335 MB of `[u8; 16]` and 604 MB of digests, plus the keys
//! asked with (the same again for hits and misses). Three tables of the largest kind at once
//! are under 2 GB; each is dropped before the next is made.
//!
//! The digest is a local type, not `ContentId`: blaze-ids depends on this crate, so as a
//! dev-dependency it would bring a second copy of it whose `PlainKey` is another trait.

#![feature(allocator_api)]

use std::{
    alloc::Global,
    collections::HashMap as StdMap,
    hash::Hash,
    hint::black_box,
    time::Instant,
};

use blaze_hash::{HashMap, PlainKey, PlainMap};

/// A 32-byte digest, uniform, as `blaze_ids::ContentId` is.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
struct Digest([u8; 32]);

// SAFETY: a transparent wrapper of a plain array with derived `Eq`.
unsafe impl PlainKey for Digest {
    const UNIFORM: bool = true;
}

/// A key made from the `n`th output of a fixed-seed generator.
trait Key: PlainKey + Hash {
    const NAME: &'static str;
    fn make(word: u64) -> Self;
}

impl Key for u32 {
    const NAME: &'static str = "u32";
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
        let mut at = word;
        for chunk in key.chunks_exact_mut(8) {
            at = splitmix(at);
            chunk.copy_from_slice(&at.to_le_bytes());
        }
        Digest(key)
    }
}

/// splitmix64's step: a fixed, seedable stream of distinct-looking words.
fn splitmix(word: u64) -> u64 {
    let mut z = word.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// `n` keys to hold and `n` disjoint keys to miss with: held keys come from even words and
/// misses from odd ones. For `u32` the held keys are sequential ids (`0, 2, 4, ...`), the shape
/// an index has; the wider keys take a mixed word, as a digest does.
fn keys<K: Key>(n: usize) -> (Vec<K>, Vec<K>) {
    let word = |at: u64| if K::NAME == "u32" { at } else { splitmix(at ^ 0x5eed) & !1 | (at & 1) };
    let held = (0..n as u64).map(|at| K::make(word(2 * at))).collect();
    let missed = (0..n as u64).map(|at| K::make(word(2 * at + 1))).collect();
    (held, missed)
}

/// ns per call of `look` over `keys`, at least ~10^7 calls in all.
fn time(keys: &[impl Copy], mut look: impl FnMut(usize) -> bool) -> f64 {
    let rounds = (10_000_000 / keys.len()).max(1);
    let start = Instant::now();
    let mut found = 0usize;
    for _ in 0..rounds {
        for at in 0..keys.len() {
            found += usize::from(look(black_box(at)));
        }
    }
    black_box(found);
    start.elapsed().as_nanos() as f64 / (rounds * keys.len()) as f64
}

fn row<K: Key>(n: usize) {
    let (held, missed) = keys::<K>(n);

    let mut plain: PlainMap<K, u32, Global> = PlainMap::plain_in(Global);
    plain.extend(held.iter().enumerate().map(|(at, key)| (*key, at as u32)));
    let plain_hit = time(&held, |at| plain.get(&held[at]).is_some());
    let plain_miss = time(&missed, |at| plain.get(&missed[at]).is_some());
    assert_eq!(plain.len(), n, "{}: the held keys are distinct", K::NAME);
    drop(plain);

    let mut fixed: HashMap<K, u32, Global> = HashMap::new_in(Global);
    fixed.extend(held.iter().enumerate().map(|(at, key)| (*key, at as u32)));
    let fixed_hit = time(&held, |at| fixed.get(&held[at]).is_some());
    let fixed_miss = time(&missed, |at| fixed.get(&missed[at]).is_some());
    drop(fixed);

    let std: StdMap<K, u32> = held.iter().enumerate().map(|(at, key)| (*key, at as u32)).collect();
    let std_hit = time(&held, |at| std.get(&held[at]).is_some());
    let std_miss = time(&missed, |at| std.get(&missed[at]).is_some());
    drop(std);

    println!(
        "{:<9} {:>9} | {:>6.2} {:>6.2} | {:>6.2} {:>6.2} | {:>6.2} {:>6.2}",
        K::NAME,
        n,
        plain_hit,
        plain_miss,
        fixed_hit,
        fixed_miss,
        std_hit,
        std_miss
    );
}

fn main() {
    println!("ns per lookup: hit and miss");
    println!("{:<9} {:>9} | {:^13} | {:^13} | {:^13}", "key", "entries", "PlainMap", "HashMap", "std");
    for n in [1_000, 100_000, 10_000_000] {
        row::<u32>(n);
        row::<u64>(n);
        row::<[u8; 16]>(n);
        row::<Digest>(n);
    }
}
