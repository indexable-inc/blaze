use std::{
    alloc::Global,
    hash::{BuildHasher, Hash, Hasher},
};

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
    let mut map: HashMap<(u32, &str), u32, Global> = HashMap::new_in(Global); // alloc: a test's map
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
    let mut map: ByHash<Id, u32, Global> = ByHash::new_in(Global); // alloc: a test's map
    let mut set: ByHashSet<u64, Global> = ByHashSet::new_in(Global); // alloc: a test's set
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
    let mut map: ByHash<Id, u32, Global> = ByHash::new_in(Global); // alloc: a test's map
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

/// The reference implementation's SipHash-2-4 vectors (key `00..0f`, message `00..i`), and
/// std's SipHash-2-4 and SipHash-1-3 on every length up to 64 under several keys: the
/// algorithm images are laid out by is SipHash's, word for word.
#[test]
fn sip_is_siphash() {
    use crate::sip::{SipKey, sip};
    let key = SipKey::new(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    let message: Vec<u8> = (0..64u8).collect(); // alloc: a test's message
    let reference = [
        0x726f_db47_dd0e_0e31,
        0x74f8_39c5_93dc_67fd,
        0x0d6c_8009_d9a9_4f5a,
        0x8567_6696_d7fb_7e2d,
    ];
    for (len, expected) in reference.into_iter().enumerate() {
        assert_eq!(sip::<2, 4>(key, &message[..len]), expected, "SipHash-2-4 of {len} bytes");
    }
    for [k0, k1] in [[0, 0], key.words(), [u64::MAX, 0x1234_5678_9abc_def0]] {
        let key = SipKey::new(k0, k1);
        for len in 0..=64 {
            let bytes = &message[..len];
            #[allow(deprecated, reason = "std's SipHash-2-4, the reference here")]
            let mut two_four = std::hash::SipHasher::new_with_keys(k0, k1);
            two_four.write(bytes);
            assert_eq!(sip::<2, 4>(key, bytes), two_four.finish(), "SipHash-2-4 of {len} bytes");
            let mut one_three = core::hash::SipHasher13::new_with_keys(k0, k1);
            one_three.write(bytes);
            assert_eq!(key.hash(bytes), one_three.finish(), "SipHash-1-3 of {len} bytes");
        }
    }
}

/// Two random keys are different, and so are the hashes they give one name.
#[test]
fn random_keys_differ() {
    let (a, b) = (crate::SipKey::random(), crate::SipKey::random());
    assert_ne!(a, b);
    assert_ne!(a.hash(b"self"), b.hash(b"self"));
}

/// [`crate::SipHasher`] fed any mix of byte strings and integers is std's SipHash-1-3 fed the
/// same, and [`crate::SipKey::hash`] of the bytes written one after another (integers
/// little-endian): a value's `Hash` is keyed exactly as its bytes are.
#[test]
fn the_streaming_hasher_is_siphash_of_the_bytes_written() {
    use crate::SipKey;
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..2000 {
        let key = SipKey::new(next(), next());
        let mut ours = key.hasher();
        let [k0, k1] = key.words();
        let mut std13 = core::hash::SipHasher13::new_with_keys(k0, k1);
        let mut bytes = Vec::new(); // alloc: a test's expected stream
        for _ in 0..(round % 9) {
            let word = next();
            match next() % 7 {
                0 => {
                    let len = (next() % 20) as usize;
                    let chunk: Vec<u8> = (0..len).map(|_| next() as u8).collect(); // alloc: a test's bytes
                    ours.write(&chunk);
                    std13.write(&chunk);
                    bytes.extend_from_slice(&chunk);
                }
                1 => {
                    ours.write_u8(word as u8);
                    std13.write_u8(word as u8);
                    bytes.push(word as u8);
                }
                2 => {
                    ours.write_u16(word as u16);
                    std13.write_u16(word as u16);
                    bytes.extend_from_slice(&(word as u16).to_le_bytes());
                }
                3 => {
                    ours.write_u32(word as u32);
                    std13.write_u32(word as u32);
                    bytes.extend_from_slice(&(word as u32).to_le_bytes());
                }
                4 => {
                    ours.write_u64(word);
                    std13.write_u64(word);
                    bytes.extend_from_slice(&word.to_le_bytes());
                }
                5 => {
                    ours.write_usize(word as usize);
                    std13.write_usize(word as usize);
                    bytes.extend_from_slice(&word.to_le_bytes());
                }
                _ => {
                    let wide = (u128::from(next()) << 64) | u128::from(word);
                    ours.write_u128(wide);
                    std13.write_u128(wide);
                    bytes.extend_from_slice(&wide.to_le_bytes());
                }
            }
            assert_eq!(
                ours.finish(),
                key.hash(&bytes),
                "round {round}: a prefix of {} bytes",
                bytes.len()
            );
        }
        assert_eq!(
            ours.finish(),
            std13.finish(),
            "round {round}: std's SipHash-1-3 of the same writes"
        );
        assert_eq!(ours.finish(), key.hash(&bytes), "round {round}: {} bytes", bytes.len());
    }
    // A derived `Hash` goes through it: `hash_one` is the hasher's.
    let key = SipKey::new(1, 2);
    let mut by_hand = key.hasher();
    (7u32, "name").hash(&mut by_hand);
    assert_eq!(key.hash_one(&(7u32, "name")), by_hand.finish());
}

/// A lazy key is drawn once: every ask after the first gives the same key.
#[test]
fn a_lazy_key_is_drawn_once() {
    static KEY: crate::LazyKey = crate::LazyKey::new();
    let first = KEY.get();
    assert_eq!(KEY.get(), first);
    assert_ne!(crate::LazyKey::new().get(), first);
}

/// What hashing a kind shaped as the solver's (a discriminant and two handles) costs under
/// each candidate: foldhash's fixed seed with the old murmur finish (what hir-ty's interner
/// used), foldhash under a random seed, and SipHash-1-3 under a random key.
#[test]
#[ignore = "a bench: cargo test -p blaze-hash --release -- --ignored --nocapture hash_cost"]
fn hash_cost() {
    use std::{hint::black_box, time::Instant};
    #[derive(Hash)]
    enum Kind {
        Adt(u32, u32),
        Array(u32, u64),
    }
    let kinds: Vec<Kind> =
        (0..1u32 << 16) // alloc: a bench's inputs
            .map(
                |i| {
                    if i % 2 == 0 { Kind::Adt(i, i / 3) } else { Kind::Array(i, u64::from(i) << 7) }
                },
            )
            .collect();
    let murmur = |hash: u64| {
        let hash = (hash ^ (hash >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd);
        let hash = (hash ^ (hash >> 33)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        hash ^ (hash >> 33)
    };
    let sip = crate::SipKey::random();
    #[allow(clippy::disallowed_types, reason = "the keyed foldhash candidate, measured here")]
    let random_fold = foldhash::fast::RandomState::default();
    fn time<K>(kinds: &[K], hash: impl Fn(&K) -> u64) -> f64 {
        let mut best = f64::MAX;
        for _ in 0..21 {
            let start = Instant::now();
            let mut acc = 0u64;
            for kind in kinds {
                acc = acc.wrapping_add(hash(black_box(kind)));
            }
            black_box(acc);
            best = best.min(start.elapsed().as_nanos() as f64 / kinds.len() as f64);
        }
        best
    }
    for round in 0..3 {
        let fixed = time(&kinds, |kind| murmur(FixedState.hash_one(kind)));
        let folded = time(&kinds, |kind| random_fold.hash_one(kind));
        let keyed = time(&kinds, |kind| sip.hash_one(kind));
        println!(
            "hash_cost round {round}: fixed foldhash + murmur {fixed:.2} ns, random-seeded foldhash {folded:.2} ns, SipHash-1-3 {keyed:.2} ns"
        );
    }
}

/// The bound [`crate::adaptive::refused_run`] derives, pinned at the sizes its doc names, and
/// never below the longest walk of honest keys: sequential integers placed by the fixed hash in
/// linear-probing tables filled to half, the most an adaptive index holds before it grows.
#[test]
fn honest_keys_never_walk_the_refused_run() {
    use crate::adaptive::refused_run;
    assert_eq!([refused_run(4), refused_run(9), refused_run(27)], [169, 187, 252]);
    assert!(
        (4..40).all(|log| refused_run(log) < refused_run(log + 1)),
        "the bound grows with the table"
    );
    for log in [10u32, 13, 16] {
        for base in [0u64, 1 << 32, 0x5eed << 40] {
            let len = 1usize << log;
            let mask = len - 1;
            let mut full = vec![false; len]; // alloc: the model table, once per size
            let mut longest = 0;
            for key in base..base + (len / 2) as u64 {
                let (mut at, mut walked) = (FixedState.hash_one(key) as usize & mask, 0);
                while full[at] {
                    at = (at + 1) & mask;
                    walked += 1;
                }
                full[at] = true;
                longest = longest.max(walked);
            }
            assert!(
                longest < refused_run(log),
                "keys from {base} in 2^{log} slots walked {longest}, the refused run is {}",
                refused_run(log)
            );
        }
    }
}
