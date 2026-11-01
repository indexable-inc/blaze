use std::hash::{BuildHasher, Hash, Hasher};

use hegel::TestCase;
use hegel::generators as gs;

use crate::{BuildKeyHasher, ByHash, ByHashSet, FixedHasher, FixedState, HashMap, KeyHasher};

/// The groups each insertion probes in a model of hashbrown's table: as many buckets as keep it
/// at most 7/8 full (a power of two), each key put in the first free slot of the 16 from its
/// hash's low bits, else of the 16 that many slots further on, then 32 further, and so on.
/// The longest probe and the mean.
fn probes(hashes: &[u64]) -> (u32, f64) {
    let mut buckets = 16;
    while buckets * 7 / 8 < hashes.len() {
        buckets *= 2;
    }
    let mask = buckets - 1;
    let mut full = vec![false; buckets]; // alloc: the model table, once per test
    let (mut longest, mut total) = (0, 0);
    for &hash in hashes {
        let (mut at, mut stride, mut groups) = (hash as usize & mask, 0, 1);
        loop {
            if let Some(free) = (0..16).map(|it| (at + it) & mask).find(|&it| !full[it]) {
                full[free] = true;
                break;
            }
            stride += 16;
            at = (at + stride) & mask;
            groups += 1;
        }
        longest = longest.max(groups);
        total += groups;
    }
    (longest, total as f64 / hashes.len() as f64)
}

fn hashes<T: Hash>(keys: impl Iterator<Item = T>) -> Vec<u64> {
    keys.map(|it| FixedState.hash_one(it)).collect() // alloc: the keys' hashes, once per test
}

// The bounds hold a random function's spread with room: these keys take at most 12 groups and
// 1.1 on average. FxHash puts the high-bits keys 110 groups deep on average.

#[test]
fn sequential_ids_spread_over_the_table() {
    let (longest, mean) = probes(&hashes(0..57_000u32));
    assert!(
        longest <= 16 && mean < 1.2,
        "sequential ids: longest probe {longest} groups, mean {mean}"
    );
}

#[test]
fn aligned_pointers_spread_over_the_table() {
    let (longest, mean) = probes(&hashes((0..57_000usize).map(|it| 0x7f12_3456_0000 + 64 * it)));
    assert!(longest <= 16 && mean < 1.2, "pointers: longest probe {longest} groups, mean {mean}");
}

#[test]
fn keys_that_differ_in_their_high_bits_spread_over_the_table() {
    let (longest, mean) = probes(&hashes((0..3_500u64).map(|it| it << 52)));
    assert!(longest <= 16 && mean < 1.2, "high bits: longest probe {longest} groups, mean {mean}");
}

#[test]
fn the_seed_is_the_same_in_every_run() {
    assert_eq!(FixedState.hash_one(0xdead_beef_u32), 0x5c89_9416_f541_efe7);
    let mut seeded = FixedHasher::with_seed(0);
    0xdead_beef_u32.hash(&mut seeded);
    assert_eq!(seeded.finish(), 0x5c89_9416_f541_efe7);
}

#[test]
fn a_map_finds_what_it_holds() {
    let mut map: HashMap<(u32, &str), u32> = HashMap::default();
    for it in 0..1_000u32 {
        map.insert((it, "key"), it);
    }
    assert!((0..1_000u32).all(|it| map.get(&(it, "key")) == Some(&it)));
    assert_eq!(map.get(&(1_000, "key")), None);
}

/// A content ID's shape: 32 bytes, hashed as derived.
#[derive(Hash, PartialEq, Eq)]
struct Id([u8; 32]);

/// blaze-cas's and blaze-global's copy as it was, the reference.
#[derive(Default)]
struct Reference(u64);

impl Hasher for Reference {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        if let Some(word) = bytes.first_chunk::<8>() {
            self.0 ^= u64::from_le_bytes(*word);
        }
    }

    fn write_usize(&mut self, _: usize) {}
}

/// blaze-graph's `plane.rs` and blaze-cargo's `print.rs` copy for a `u64` key: the word itself.
fn word_reference(word: u64) -> u64 {
    word
}

fn key_hash_of(key: &impl Hash) -> u64 {
    BuildKeyHasher::default().hash_one(key)
}

fn reference_of(key: &impl Hash) -> u64 {
    let mut state = Reference::default();
    key.hash(&mut state);
    state.finish()
}

#[test]
fn a_content_id_hashes_as_its_first_word() {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
    bytes[8] = 0xff;
    assert_eq!(key_hash_of(&Id(bytes)), 0x0123_4567_89ab_cdef);
}

#[test]
fn a_map_on_hashes_finds_what_it_holds() {
    let mut map: ByHash<Id, u32> = ByHash::default();
    let mut set: ByHashSet<u64> = ByHashSet::default();
    for i in 0..64u32 {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&i.to_le_bytes());
        map.insert(Id(bytes), i);
        set.insert(u64::from(i) << 40);
    }
    for i in 0..64u32 {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&i.to_le_bytes());
        assert_eq!(map.get(&Id(bytes)), Some(&i));
        assert!(set.contains(&(u64::from(i) << 40)));
    }
}

#[test]
fn a_lookup_on_a_hash_allocates_nothing() {
    let mut map: ByHash<Id, u32> = ByHash::default();
    map.insert(Id([7; 32]), 7);
    blaze_alloc_count::assert_at_most(0, "a prehashed lookup", || {
        for _ in 0..100 {
            std::hint::black_box(map.get(&Id([7; 32])));
        }
    });
}

#[hegel::test]
fn hashes_as_the_copies_did(tc: TestCase) {
    let bytes: [u8; 32] = tc.draw(gs::arrays(gs::integers()));
    let word: u64 = tc.draw(gs::integers());
    assert_eq!(key_hash_of(&Id(bytes)), reference_of(&Id(bytes)));
    assert_eq!(key_hash_of(&bytes), reference_of(&bytes));
    assert_eq!(key_hash_of(&word), word_reference(word));
    assert_eq!(key_hash_of(&word), reference_of(&word));
}

#[test]
fn the_identity_hasher_takes_a_words_bits() {
    let mut state = KeyHasher::default();
    state.write_u64(0xfeed);
    assert_eq!(state.finish(), 0xfeed);
}
