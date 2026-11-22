use std::{
    alloc::Global,
    hash::{BuildHasher, Hash, Hasher},
    vec::Vec,
};

use blaze_sim::Config;

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
#[cfg_attr(miri, ignore = "a statistic of the safe hash over 57,000 keys: no unsafe code")]
fn sequential_ids_spread_over_the_table() {
    let (longest, mean) = probes(&hashes(0..57_000u32));
    assert!(
        longest <= 16 && mean < 1.2,
        "sequential ids: longest probe {longest} groups, mean {mean}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "a statistic of the safe hash over 57,000 keys: no unsafe code")]
fn aligned_pointers_spread_over_the_table() {
    let (longest, mean) = probes(&hashes((0..57_000usize).map(|it| 0x7f12_3456_0000 + 64 * it)));
    assert!(longest <= 16 && mean < 1.2, "pointers: longest probe {longest} groups, mean {mean}");
}

#[test]
#[cfg_attr(miri, ignore = "a statistic of the safe hash over 3,500 keys: no unsafe code")]
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

/// The port of foldhash's fast hasher (`fold`) gives foldhash 0.2.0's hashes under its fixed
/// seed, `with_seed` and the default alike, for any run of writes: byte strings of every length
/// past each of its paths (16, 128 and 256 bytes) and integers of every width, which fill and
/// fold its sponge at every offset.
#[test]
fn fold_is_foldhash() {
    blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 256 }), |tape| {
        let seed: u64 = if tape.coin("default seed") { 0 } else { tape.int("seed") };
        let mut ours = FixedHasher::with_seed(seed);
        let mut theirs = foldhash::fast::FixedState::with_seed(seed).build_hasher();
        let mut bytes = [0u8; 600];
        tape.fill(&mut bytes, "bytes");
        for _ in 0..tape.below(24, "writes") {
            match tape.below(7, "write") {
                0 => {
                    let len = if tape.one_in(3, "long") {
                        tape.index(bytes.len() + 1, "long length")
                    } else {
                        tape.index(33, "short length")
                    };
                    let from = tape.index(bytes.len() - len + 1, "offset");
                    ours.write(&bytes[from..from + len]);
                    theirs.write(&bytes[from..from + len]);
                }
                1 => {
                    let it = tape.int::<u8>("u8");
                    ours.write_u8(it);
                    theirs.write_u8(it);
                }
                2 => {
                    let it = tape.int::<u16>("u16");
                    ours.write_u16(it);
                    theirs.write_u16(it);
                }
                3 => {
                    let it = tape.int::<u32>("u32");
                    ours.write_u32(it);
                    theirs.write_u32(it);
                }
                4 => {
                    let it = tape.int::<u64>("u64");
                    ours.write_u64(it);
                    theirs.write_u64(it);
                }
                5 => {
                    let it = u128::from(tape.int::<u64>("u128 high")) << 64
                        | u128::from(tape.int::<u64>("u128 low"));
                    ours.write_u128(it);
                    theirs.write_u128(it);
                }
                _ => {
                    let it = tape.int::<u64>("usize") as usize;
                    ours.write_usize(it);
                    theirs.write_usize(it);
                }
            }
            assert_eq!(ours.finish(), theirs.finish(), "seed {seed:#x}");
        }
        assert_eq!(
            FixedState.hash_one(&bytes[..]),
            foldhash::fast::FixedState::default().hash_one(&bytes[..])
        );
    });
}

#[test]
fn hashes_as_the_copies_did() {
    blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 256 }), |tape| {
        let bytes: [u8; 32] = tape.array("bytes", |tape| tape.int::<u8>("byte"));
        let word: u64 = tape.int("word");
        assert_eq!(key_hash_of(&Id(bytes)), reference_of(&Id(bytes)));
        assert_eq!(key_hash_of(&bytes), reference_of(&bytes));
        assert_eq!(key_hash_of(&word), word_reference(word));
        assert_eq!(key_hash_of(&word), reference_of(&word));
    });
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
    for round in 0..if cfg!(miri) { 40 } else { 2000 } {
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
#[cfg_attr(miri, ignore = "a statistic of the safe hash in tables of up to 2^16 slots: no unsafe code")]
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

/// A map of `held` keys below `range` (some removed again, so dead slots stand in probes),
/// each key's value `value(key)`.
fn map_of<V>(
    tape: &blaze_sim::Tape,
    range: u64,
    value: impl Fn(u64) -> V,
) -> HashMap<u64, V, Global> {
    let mut map = HashMap::new_in(Global);
    for _ in 0..tape.below(if cfg!(miri) { 60 } else { 500 }, "held") {
        let key = tape.below(range, "key");
        if tape.one_in(5, "remove") {
            map.remove(&key);
        } else {
            map.insert(key, value(key));
        }
    }
    map
}

/// `count` keys to ask, from twice the held range (so about half are absent), every batch
/// holding a duplicate.
fn asked(tape: &blaze_sim::Tape, range: u64, count: usize) -> Vec<u64> {
    let mut keys: Vec<u64> = (0..count).map(|_| tape.below(2 * range, "asked")).collect();
    if count > 1 {
        let to = tape.index(count, "duplicate to");
        keys[to] = keys[tape.index(count, "duplicate from")];
    }
    keys
}

/// `get_many` and `get_many_into` answer what `get` answers of each key, duplicates and absent
/// keys among them: on entries narrow enough that a group's slots are prefetched with its word
/// (`u64` to `u32`) and wide enough that only the matched slot is (`u64` to 64 bytes), on a
/// set, on borrowed `str` keys, and across the slice form's batch boundaries.
#[test]
fn get_many_is_get_of_each() {
    blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 256 }), |tape| {
        let range = 1 + tape.below(400, "range");

        let narrow = map_of(tape, range, |key| key as u32);
        let keys = asked(tape, range, 8);
        let many = narrow.get_many(core::array::from_fn::<_, 8, _>(|at| &keys[at]));
        for (at, key) in keys.iter().enumerate() {
            assert_eq!(many[at], narrow.get(key), "narrow: key {key} of {keys:?}");
        }
        let keys = asked(tape, range, tape.index(40, "slice length"));
        let mut out = std::vec![None; keys.len()];
        narrow.get_many_into(&keys, &mut out);
        for (at, key) in keys.iter().enumerate() {
            assert_eq!(out[at], narrow.get(key), "narrow slice: key {key} at {at}");
        }

        let wide = map_of(tape, range, |key| [key; 8]);
        let keys = asked(tape, range, 5);
        let many = wide.get_many(core::array::from_fn::<_, 5, _>(|at| &keys[at]));
        for (at, key) in keys.iter().enumerate() {
            assert_eq!(many[at], wide.get(key), "wide: key {key} of {keys:?}");
        }
        let keys = asked(tape, range, tape.index(40, "wide slice length"));
        let mut out = std::vec![None; keys.len()];
        wide.get_many_into(&keys, &mut out);
        for (at, key) in keys.iter().enumerate() {
            assert_eq!(out[at], wide.get(key), "wide slice: key {key} at {at}");
        }

        let mut set: crate::HashSet<u64, Global> = crate::HashSet::new_in(Global);
        set.extend(narrow.keys().copied());
        let keys = asked(tape, range, 4);
        let many = set.get_many(core::array::from_fn::<_, 4, _>(|at| &keys[at]));
        let mut out = std::vec![None; keys.len()];
        set.get_many_into(&keys, &mut out);
        for (at, key) in keys.iter().enumerate() {
            assert_eq!(many[at], set.get(key), "set: key {key} of {keys:?}");
            assert_eq!(out[at], set.get(key), "set slice: key {key} of {keys:?}");
        }

        let mut names: HashMap<std::string::String, u64, Global> = HashMap::new_in(Global);
        names.extend(narrow.keys().map(|&key| (std::format!("k{key}"), key)));
        let asked_names: Vec<std::string::String> =
            asked(tape, range, 3).iter().map(|key| std::format!("k{key}")).collect();
        let many = names.get_many(core::array::from_fn::<_, 3, _>(|at| asked_names[at].as_str()));
        for (at, name) in asked_names.iter().enumerate() {
            assert_eq!(many[at], names.get(name.as_str()), "names: {name} of {asked_names:?}");
        }
    });
}

/// `get_or_insert_with` on a tape of `str` keys (with removals between, so dead slots and
/// full tables are met) is `get`, then `insert` on a miss, against a map driven that way;
/// its `make` runs once per key it puts, and never for a held key. The set's form likewise.
#[test]
fn get_or_insert_with_is_get_then_insert() {
    blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 256 }), |tape| {
        let mut ours: HashMap<std::string::String, u64, Global> = HashMap::new_in(Global);
        let mut model: HashMap<std::string::String, u64, Global> = HashMap::new_in(Global);
        let mut set: crate::HashSet<std::string::String, Global> = crate::HashSet::new_in(Global);
        let (mut made, mut put, mut set_made) = (0usize, 0usize, 0usize);
        let range = 1 + tape.below(200, "range");
        for step in 0..tape.below(if cfg!(miri) { 80 } else { 600 }, "steps") {
            let name = std::format!("k{}", tape.below(range, "key"));
            if tape.one_in(6, "remove") {
                assert_eq!(ours.remove(name.as_str()), model.remove(name.as_str()));
                set.remove(name.as_str());
                continue;
            }
            let expected = match model.get(name.as_str()) {
                Some(&held) => held,
                None => {
                    model.insert(name.clone(), step);
                    put += 1;
                    step
                }
            };
            let got = ours.get_or_insert_with(name.as_str(), |key| {
                made += 1;
                (std::string::String::from(key), step)
            });
            assert_eq!(*got, expected, "{name} at step {step}");
            let held = set.get_or_insert_with(name.as_str(), |key| {
                set_made += 1;
                std::string::String::from(key)
            });
            assert_eq!(held, &name);
            assert_eq!(ours.len(), model.len());
        }
        assert_eq!(made, put, "`make` once per key put, never on a hit");
        assert_eq!(set_made, put, "the set's `make` likewise");
        for (key, value) in model.iter() {
            assert_eq!(ours.get(key.as_str()), Some(value));
        }
        let tried = ours.try_get_or_insert_with("absent-key", |key| (std::string::String::from(key), 7));
        assert_eq!(tried.ok().map(|value| *value), Some(7));
    });
}

/// The keys a case inserts: up to 200, from a small range so that some repeat.
fn keys_of(tape: &blaze_sim::Tape) -> Vec<u64> {
    let count = tape.index(201, "keys");
    (0..count).map(|_| tape.below(300, "key")).collect() // alloc: a case's keys
}

/// D126: `Map::new` and `Set::new` make what the constructors they replace made. With
/// `capacity: None` it is `new_in`; with `Some(n)` it is `with_capacity_in(n, ..)`: the same
/// capacity, and after the same inserts the same contents in the same iteration order, so the
/// sweep from one to the other changes no program's output.
#[test]
fn new_with_options_is_the_old_constructors() {
    use crate::{Map, MapOpts, Set, SetOpts};
    blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 256 }), |tape| {
        let keys = keys_of(tape);
        let room = if tape.coin("reserve") { Some(tape.index(500, "capacity")) } else { None };

        let mut new: Map<u64, u64, FixedState, Global> =
            Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: room });
        let mut old: HashMap<u64, u64, Global> = match room {
            None => HashMap::new_in(Global),
            Some(n) => HashMap::with_capacity_in(n, Global),
        };
        assert_eq!(new.capacity(), old.capacity(), "capacity {room:?}");
        if room.is_none() {
            assert_eq!(new.capacity(), 0, "`capacity: None` reserves nothing");
        }
        let mut set: Set<u64, FixedState, Global> =
            Set::new(SetOpts { hasher: FixedState, alloc: Global, capacity: room });
        let mut old_set: crate::HashSet<u64, Global> = match room {
            None => crate::HashSet::new_in(Global),
            Some(n) => crate::HashSet::with_capacity_in(n, Global),
        };
        assert_eq!(set.capacity(), old_set.capacity(), "set capacity {room:?}");
        for (step, &key) in keys.iter().enumerate() {
            assert_eq!(new.insert(key, step as u64), old.insert(key, step as u64));
            assert_eq!(set.insert(key), old_set.insert(key));
        }
        assert_eq!(new.capacity(), old.capacity());
        assert!(new.iter().eq(old.iter()), "the same pairs in the same order");
        assert!(set.iter().eq(old_set.iter()), "the same keys in the same order");

        let tried: Map<u64, u64, FixedState, Global> =
            Map::try_new(MapOpts { hasher: FixedState, alloc: Global, capacity: room })
                .unwrap_or_else(|failure| panic!("{room:?}: {failure:?}"));
        assert_eq!(tried.capacity(), old_capacity(room));
    });
}

/// The capacity `with_capacity_in(n, ..)` gives, or `new_in`'s for `None`.
fn old_capacity(room: Option<usize>) -> usize {
    match room {
        None => HashMap::<u64, u64, Global>::new_in(Global).capacity(),
        Some(n) => HashMap::<u64, u64, Global>::with_capacity_in(n, Global).capacity(),
    }
}

/// D125: a keyed table is made the same way, its key as the hasher; two tables under two
/// different keys hold the same contents after the same inserts (their order may differ: it is
/// the key's).
#[test]
fn a_keyed_table_takes_its_key_as_the_hasher() {
    use crate::{Map, MapOpts, Set, SetOpts, SipKey};
    blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 128 }), |tape| {
        let keys = keys_of(tape);
        let first = SipKey::new(tape.int("k0"), tape.int("k1"));
        let mut second = SipKey::new(tape.int("k2"), tape.int("k3"));
        if second == first {
            second = SipKey::new(first.words()[0] ^ 1, first.words()[1]);
        }
        let mut a: Map<u64, u64, SipKey, Global> =
            Map::new(MapOpts { hasher: first, alloc: Global, capacity: None });
        let mut b: Map<u64, u64, SipKey, Global> =
            Map::new(MapOpts { hasher: second, alloc: Global, capacity: Some(keys.len()) });
        let mut set: Set<u64, SipKey, Global> =
            Set::new(SetOpts { hasher: second, alloc: Global, capacity: None });
        for (step, &key) in keys.iter().enumerate() {
            assert_eq!(a.insert(key, step as u64), b.insert(key, step as u64));
            set.insert(key);
        }
        assert_eq!(a.len(), b.len());
        assert_eq!(set.len(), a.len());
        assert!(a.iter().all(|(key, value)| b.get(key) == Some(value) && set.contains(key)));
        let mut a_pairs: Vec<(u64, u64)> = a.iter().map(|(k, v)| (*k, *v)).collect(); // alloc: the test's copy
        let mut b_pairs: Vec<(u64, u64)> = b.iter().map(|(k, v)| (*k, *v)).collect(); // alloc: the test's copy
        a_pairs.sort_unstable();
        b_pairs.sort_unstable();
        assert_eq!(a_pairs, b_pairs);
    });
}
