//! What a lookup costs on tables whose keys hold a digest, by the hasher and the path the key
//! takes, at 10^3, 10^5 and 10^6 entries; ns per hit, asked in an order unlike the inserts':
//!
//! - `Digest` (32 bytes, `blaze_ids::ContentId`'s shape and impl) in an `OrderMap` on
//!   `FixedState`: by its bytes, its first word (the `KeyHash` seam, which `OrderMap` and the
//!   shared tables hash through since 2026-11-20), against the same key in a newtype with no
//!   `PlainKey`, fed through `Hash` (what `OrderMap` and `Pinned` did before: a length write and
//!   foldhash's long path over 32 bytes, the profile's `hash_bytes_long` and `<[u8; 32]>::hash`);
//! - `Id` (a kind, a subject digest, a 16-byte cell: `blaze_engine::CellId`'s shape) in a `Map`
//!   on `FixedState`, by `Hash` (what `blaze_core::SharedMap<CellId, ..>` does today), against
//!   one on `DigestState`, by its `DigestKey` word.
//!
//! Then ns a hash alone of each, hot, which is what the floor is stated in: a digest's word is
//! one load (about 0.3 ns), an id's three folded multiplies on a dependent chain (about 1.5 ns at
//! 3 to 4 cycles a multiply); a lookup's floor past the caches is its misses, the same on both
//! paths.
//!
//!     bash /Volumes/Projects/blaze-int-1112-notes/examplerun.sh blaze-hash cost
//!
//! or `cargo run --release -p blaze-hash --example cost`. The digests are a local type with
//! `ContentId`'s shape, not `ContentId`: blaze-ids depends on this crate, so as a
//! dev-dependency it would bring a second copy whose `PlainKey` is another trait.

#![feature(allocator_api, min_specialization)]
#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

use core::hash::BuildHasher;
use std::{hint::black_box, time::Instant};

use blaze_hash::{
    DigestKey, DigestState, FixedState, KeyHash, Map, MapOpts, OrderMap, OrderMapOpts, PlainKey,
    digest_word, fold_part,
};
use blaze_own::{Alloc, Heap, Vec};

/// A 32-byte digest, uniform, as `blaze_ids::ContentId` is.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
struct Digest([u8; 32]);

// SAFETY: a transparent wrapper of a plain array with derived `Eq`; lifetime-free, as a
// specialization trait's impl must be.
unsafe impl PlainKey for Digest {
    const UNIFORM: bool = true;
}

/// The same digest with no `PlainKey`: hashed through its derived `Hash`, the old path.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Opaque(Digest);

/// `blaze_engine::CellId`'s shape: a kind, a subject digest and a 16-byte cell.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Id {
    kind: u8,
    subject: Digest,
    cell: [u8; 16],
}

/// A cell's two words, little-endian.
fn halves(cell: &[u8; 16]) -> (u64, u64) {
    let word = |half: &[u8]| {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(half);
        u64::from_le_bytes(bytes)
    };
    (word(&cell[..8]), word(&cell[8..]))
}

impl DigestKey for Id {
    fn digest_word(&self) -> u64 {
        let (low, high) = halves(&self.cell);
        let word = fold_part(digest_word(&self.subject.0), u64::from(self.kind));
        fold_part(fold_part(word, low), high)
    }
}

/// splitmix64: the fixed stream the digests are drawn from.
fn splitmix(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn digest_of(at: u64) -> Digest {
    let mut bytes = [0u8; 32];
    for (word, chunk) in (0u64..).zip(bytes.chunks_exact_mut(8)) {
        chunk.copy_from_slice(&splitmix(at.wrapping_mul(4).wrapping_add(word)).to_le_bytes());
    }
    Digest(bytes)
}

/// The `at`th id: 64 cells a subject, numbered in the cell's low word, three kinds, the shape
/// one request's member cells have.
fn id_of(at: u64) -> Id {
    let mut cell = [0u8; 16];
    cell[..8].copy_from_slice(&(at % 64).to_le_bytes());
    let kind = u8::try_from(at % 3).expect("a remainder of three fits a byte");
    Id { kind, subject: digest_of(at / 64), cell }
}

/// `items` in an order unlike their own (a multiplicative walk of the indices), so a lookup
/// does not read the entries in the order they were put.
fn shuffled<'a, T: Copy>(items: &[T], alloc: Alloc<'a>) -> Vec<T, Alloc<'a>> {
    let n = items.len();
    let mut out = Vec::with_capacity_in(n, alloc);
    // Odd and not a multiple of five, so a unit modulo every size here (powers of ten): the
    // walk is a permutation.
    let step = 0x9e37_79b9usize;
    for at in 0..n {
        out.push(items[at.wrapping_mul(step) % n]);
    }
    out
}

/// ns a call of `look` over `keys`, best of five passes of about 2^22 calls.
fn ns_each<T>(keys: &[T], mut look: impl FnMut(&T) -> bool) -> f64 {
    let passes = ((1usize << 22) / keys.len().max(1)).max(1);
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        let mut found = 0usize;
        for _ in 0..passes {
            for key in keys {
                found += usize::from(look(black_box(key)));
            }
        }
        black_box(found);
        let calls = (passes * keys.len()) as f64;
        best = best.min(start.elapsed().as_nanos() as f64 / calls);
    }
    best
}

fn main() {
    let heap = Heap::process();
    let alloc = heap.alloc();
    println!("entries | key | table on | ns a hit | ns a hash");
    for n in [1_000u64, 100_000, 1_000_000] {
        let len = usize::try_from(n).expect("a size fits a usize");
        let mut digests = Vec::with_capacity_in(len, alloc);
        let mut ids = Vec::with_capacity_in(len, alloc);
        for at in 0..n {
            digests.push(digest_of(at));
            ids.push(id_of(at));
        }
        let (digests, ids) = (digests.as_slice(), ids.as_slice());
        let (ask_digests, ask_ids) = (shuffled(digests, alloc), shuffled(ids, alloc));

        let room = Some(len);
        let mut plain = OrderMap::new(OrderMapOpts { hasher: FixedState, alloc, capacity: room });
        let mut opaque = OrderMap::new(OrderMapOpts { hasher: FixedState, alloc, capacity: room });
        let mut by_hash: Map<Id, u32, FixedState, Alloc<'_>> =
            Map::new(MapOpts { hasher: FixedState, alloc, capacity: room });
        let mut by_word: Map<Id, u32, DigestState, Alloc<'_>> =
            Map::new(MapOpts { hasher: DigestState, alloc, capacity: room });
        for (at, (digest, id)) in (0u32..).zip(digests.iter().zip(ids)) {
            plain.insert(*digest, at);
            opaque.insert(Opaque(*digest), at);
            by_hash.insert(*id, at);
            by_word.insert(*id, at);
        }

        let rows = [
            (
                "Digest",
                "FixedState, by bytes",
                ns_each(&ask_digests, |key| plain.get(key).is_some()),
                ns_each(&ask_digests, |key| KeyHash::<Digest>::hash_key(&FixedState, key) & 1 == 0),
            ),
            (
                "Digest",
                "FixedState, by Hash (before)",
                ns_each(&ask_digests, |key| opaque.get(&Opaque(*key)).is_some()),
                ns_each(&ask_digests, |key| FixedState.hash_one(Opaque(*key)) & 1 == 0),
            ),
            (
                "Id",
                "FixedState, by Hash (today)",
                ns_each(&ask_ids, |key| by_hash.get(key).is_some()),
                ns_each(&ask_ids, |key| FixedState.hash_one(key) & 1 == 0),
            ),
            (
                "Id",
                "DigestState, by word",
                ns_each(&ask_ids, |key| by_word.get(key).is_some()),
                ns_each(&ask_ids, |key| KeyHash::<Id>::hash_key(&DigestState, key) & 1 == 0),
            ),
        ];
        for (key, table, hit, hash) in rows {
            println!("{n:>9} | {key:<6} | {table:<28} | {hit:>7.2} | {hash:>6.2}");
        }
    }
}
