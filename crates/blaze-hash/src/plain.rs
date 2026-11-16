//! Keys hashed by their bytes, in one step: [`PlainKey`] and [`hash_plain`], which every
//! [`HashMap`](crate::HashMap) and [`HashSet`](crate::HashSet) of such keys takes with no
//! constructor of its own, through a specialization of [`KeyHash`] for [`FixedState`].
//!
//! A `Hash` impl feeds a `Hasher` one field at a time, and each field is a call, a sponge
//! update and a branch on the sponge's fill. A key that is its bytes (an integer, an index, a
//! digest, an array of them) needs none of it: [`hash_plain`] reads the bytes once and mixes
//! them with one widening multiply, or for a key that is a hash already takes its first word.
//! The table hashes through one seam, [`KeyHash`] (rule 78): any `BuildHasher` over a `Hash`
//! key by default, and for [`FixedState`] alone, a [`PlainKey`] by its bytes. A seeded hasher
//! ([`SipKey`](crate::SipKey), so [`KeyedMap`](crate::KeyedMap)) keeps its seeded hash, and
//! [`ByHash`](crate::ByHash) its identity fold: neither is specialized.
//!
//! # Within `min_specialization`
//!
//! Each specializing impl here is in `min_specialization`'s sound subset: it is "always
//! applicable", which is what makes choosing it independent of lifetimes (the unsound part of
//! full `specialization`, where an impl could apply for `'static` and not for `'a`, and
//! codegen, which erases lifetimes, could not tell). Concretely: its self type is the concrete
//! [`FixedState`]; its one type parameter appears once in the impl header (no repeated
//! parameter, which would mean "when these two are equal", a condition on lifetimes too); it
//! has no lifetime parameter and no lifetime bound; and its only bound beyond what the base
//! impl asks is [`PlainKey`], a `#[rustc_specialization_trait]`, whose every impl must itself
//! be always applicable: each is lifetime-free and says nothing of `'static`.
//!
//! # What specialization does not buy here
//!
//! - Byte equality: none is added. std compares an integer in one instruction already and a
//!   `[u8; N]` by `memcmp`, through its own specializations of `PartialEq`. A table-side
//!   specialization of "the probe `Q` equals the key `K`, so compare bytes" names `K` twice
//!   (`Q = K`), a repeated parameter, which `min_specialization` refuses.
//! - A layout per key type (`u32` keys in their own column, a digest's slot with no stored
//!   hash): picking the storage type by the key needs a specialized associated type (`default
//!   type Slot = ...`), which only full `specialization` has, and full specialization is
//!   unsound (the lifetime case above). Not built. A digest key still hashes nothing: its
//!   first word is its hash ([`UNIFORM`](PlainKey::UNIFORM)).
//!
//! # A borrowed form must hash alike
//!
//! A table asked with `Q` hashes the probe by `KeyHash<Q>`. For a `PlainKey` key, `Q` is the
//! key's own type (`Borrow<T> for T`), or a slice `[T]` of an array key (`Borrow<[T]> for
//! [T; N]`), which [`KeyHash<[T]>`](KeyHash) for `FixedState` hashes by the same bytes, the
//! same way. A hand-written `Equivalent` from another type to a `PlainKey` key is hashed by
//! its `Hash` and does not meet the key's byte hash: such a key wants a non-`PlainKey` key
//! type, or the probe made into the key.

use core::{
    hash::{BuildHasher, Hash, Hasher},
    mem::size_of,
};

use crate::{FixedHasher, FixedState};

/// A key whose bytes are its value, so a table may hash the bytes instead of calling `Hash`.
///
/// # Safety
///
/// An implementor promises, for every value of the type:
///
/// - none of its bytes is padding or uninitialized: [`hash_plain`] reads all
///   `size_of::<Self>()` of them as `u8`s;
/// - it holds no pointer or reference, nor anything whose bytes depend on where something is
///   in memory: an address would make the hash, and so a table's order, differ between runs
///   and machines (rule 25);
/// - two values are equal (`==`) exactly when their bytes are, so equal keys hash alike and
///   the hash agrees with `Eq` and `Hash`. Floats are not plain: `-0.0 == 0.0` with other
///   bytes, and `NaN != NaN` with the same.
///
/// The condition is "each value has one byte pattern", not "every bit pattern is a value":
/// `bool` and `char` have bit patterns that are no value, and are plain all the same. Tuples
/// and other `repr(Rust)` structs are not implemented here: their layout may hold padding.
/// A `#[repr(transparent)]` wrapper of a plain type, with `Eq` derived, is plain.
///
/// A specialization trait (`#[rustc_specialization_trait]`): a crate implementing it turns on
/// `#![feature(min_specialization)]`, and every impl is always applicable: no lifetime
/// parameter or bound, no type parameter twice, bounds only on other `PlainKey`s.
#[rustc_specialization_trait]
pub unsafe trait PlainKey: Copy + Eq + Hash {
    /// Whether the key's bytes are uniform already (a digest, a fingerprint): when it is and
    /// the key is at least eight bytes, its first eight bytes are the hash, unmixed, as
    /// [`KeyHasher`](crate::KeyHasher) takes them. Only for keys whose every bit is as good as
    /// random: the table places a key by the low bits of its hash and tells keys of a group
    /// apart by the top seven.
    const UNIFORM: bool = false;
}

macro_rules! plain {
    ($($ty:ty),* $(,)?) => {$(
        // SAFETY: an integer, `bool` or `char` has no padding and holds no address, and two
        // are equal exactly when their bytes are.
        unsafe impl PlainKey for $ty {}
    )*};
}

plain!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, bool, char);

// SAFETY: an array's stride is its element's size, so it has no padding between elements and
// none of its own; it holds what its elements hold, and two arrays are equal exactly when each
// element pair is, so exactly when their bytes are. Uniform when its elements are.
unsafe impl<T: PlainKey, const N: usize> PlainKey for [T; N] {
    const UNIFORM: bool = T::UNIFORM;
}

/// The bytes of `key`, all `size_of::<K>()` of them.
#[inline(always)]
fn bytes_of<K: PlainKey>(key: &K) -> &[u8] {
    // SAFETY: `key` is a live shared reference, so its `size_of::<K>()` bytes are readable and
    // unchanged for the borrow; `PlainKey` promises none is padding or uninitialized, so each
    // is an initialized `u8`, and `u8` has alignment 1.
    unsafe { core::slice::from_raw_parts((key as *const K).cast::<u8>(), size_of::<K>()) }
}

/// What the short path xors into a key's word: foldhash's `ARBITRARY8`, a word of pi
/// (`fold::ARBITRARY8`).
const PLAIN_XOR: u64 = 0x9216_d5d9_8979_fb1b;
/// What the short path multiplies by: foldhash's `ARBITRARY0`, pi's first word, odd. Of the
/// pairs of foldhash's twelve pi words, this one with [`PLAIN_XOR`] spread the keys of
/// `tests::spread` best in their worst case (a scan of all 132 pairs, 2026-11-15: 16 keys in
/// the fullest of 4096 buckets of mean 8, sequential keys shifted by 0 to 48 bits); foldhash's
/// own pair for a `u64` (`ARBITRARY3`, `ARBITRARY6`) put 28 in one.
const PLAIN_MUL: u64 = 0x243f_6a88_85a3_08d3;

/// The 64 by 64 to 128-bit product, its halves xored: the low half carries the low input bits
/// up, the high half brings every input bit down, so both the table's index (the low bits) and
/// its tag (the top seven) depend on every bit of the key.
#[inline(always)]
const fn folded_multiply(x: u64, y: u64) -> u64 {
    let full = (x as u128).wrapping_mul(y as u128);
    (full as u64) ^ ((full >> 64) as u64)
}

/// The short path's mixer: a word of at most eight key bytes, zero-extended.
#[inline(always)]
const fn mix(word: u64) -> u64 {
    folded_multiply(word ^ PLAIN_XOR, PLAIN_MUL)
}

/// `key`'s hash from its bytes, with no `Hasher` built:
///
/// - a [`UNIFORM`](PlainKey::UNIFORM) key of eight bytes or more: its first eight bytes,
///   little-endian, as [`KeyHasher`](crate::KeyHasher) takes a hash's;
/// - a key of at most eight bytes: those bytes zero-extended to a word, then one folded
///   multiply;
/// - a wider key: [`FixedHasher`] fed its bytes in one write, foldhash's long path.
///
/// Which path is decided by the type, so each instantiation compiles to one of them. Not for
/// anything persisted or compared across runs: that is BLAKE3 (the crate's documentation).
#[inline(always)]
pub fn hash_plain<K: PlainKey>(key: &K) -> u64 {
    let bytes = bytes_of(key);
    if K::UNIFORM
        && let Some(word) = bytes.first_chunk::<8>()
    {
        return u64::from_le_bytes(*word);
    }
    if size_of::<K>() <= 8 {
        let mut word = [0u8; 8];
        word[..bytes.len()].copy_from_slice(bytes);
        return mix(u64::from_le_bytes(word));
    }
    let mut hasher = FixedHasher::default();
    hasher.write(bytes);
    hasher.finish()
}

/// What hashes a table's key, the one seam its hashing is chosen at: any `BuildHasher` over a
/// `Hash` key (`FixedState`, `BuildKeyHasher`, `SipKey`, `adaptive`'s), and [`Plain`] over a
/// [`PlainKey`]. A table asked with a borrowed form `Q` needs `KeyHash<Q>` too.
pub trait KeyHash<Q: ?Sized> {
    fn hash_key(&self, key: &Q) -> u64;
}

impl<Q: Hash + ?Sized, S: BuildHasher> KeyHash<Q> for S {
    #[inline(always)]
    fn hash_key(&self, key: &Q) -> u64 {
        self.hash_one(key)
    }
}

/// The hasher of a table of [`PlainKey`]s: [`hash_plain`]. Zero-sized and `Copy`, its own
/// value (`Map::plain_in(alloc)`). Not a `BuildHasher`, so a table on it hashes no key by
/// `Hash`, and its [`KeyHash`] impl is the only one.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Plain;

impl<K: PlainKey> KeyHash<K> for Plain {
    #[inline(always)]
    fn hash_key(&self, key: &K) -> u64 {
        hash_plain(key)
    }
}

#[cfg(test)]
mod tests {
    use std::{alloc::Global, collections::BTreeMap, vec::Vec};

    use blaze_sim::{Config, Tape};

    use super::{PLAIN_MUL, PLAIN_XOR, PlainKey, folded_multiply, hash_plain, mix};
    use crate::{FixedHasher, PlainMap, hash_map::Entry};
    use core::hash::Hasher;

    /// A digest-like key, uniform, as `blaze_ids::ContentId` is.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
    #[repr(transparent)]
    struct Digest([u8; 32]);

    // SAFETY: a transparent wrapper of a plain array with derived `Eq`.
    unsafe impl PlainKey for Digest {
        const UNIFORM: bool = true;
    }

    /// A key wider than a word: foldhash's long path, the same as a `FixedHasher` fed its
    /// bytes in one write.
    #[test]
    fn a_wide_key_is_fixed_hashers_one_write() {
        let wide: [u8; 24] = core::array::from_fn(|at| (at as u8).wrapping_mul(37));
        let mut fixed = FixedHasher::default();
        fixed.write(&wide);
        assert_eq!(hash_plain(&wide), fixed.finish());

        let pair = [0x0123_4567_89ab_cdefu64, 42];
        let mut fixed = FixedHasher::default();
        fixed.write(&[pair[0].to_ne_bytes(), pair[1].to_ne_bytes()].concat());
        assert_eq!(hash_plain(&pair), fixed.finish());
    }

    /// A uniform key is its first word, as `KeyHasher` takes it; a short key is its word mixed.
    #[test]
    fn the_short_and_uniform_paths() {
        let digest = Digest(core::array::from_fn(|at| at as u8 + 1));
        assert_eq!(hash_plain(&digest), u64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(hash_plain(&7u32), mix(7));
        assert_eq!(hash_plain(&[1u8, 2]), mix(0x0201));
        assert_eq!(hash_plain(&true), mix(1));
        assert_eq!(hash_plain(&u64::MAX), mix(u64::MAX));
    }

    /// The fullest of 4096 index buckets and of the 128 tags when `mix` hashes `keys`: what
    /// the table sees, as it places a key by the low bits and tags it by the top seven.
    fn spread(mix: fn(u64) -> u64, keys: impl Iterator<Item = u64>) -> (usize, usize) {
        let (mut index, mut tag) = (vec![0usize; 1 << 12], vec![0usize; 128]);
        for key in keys {
            let hash = mix(key);
            index[(hash & 0xfff) as usize] += 1;
            tag[(hash >> 57) as usize] += 1;
        }
        (index.into_iter().max().unwrap_or(0), tag.into_iter().max().unwrap_or(0))
    }

    /// Keys of each shape the tree has (sequential, and sequential in a high field) whose
    /// spread is over the bound: no index bucket past three times its mean of 8, no tag past
    /// 1.5 times its mean of 256. Empty when `mix` spreads them all.
    fn over_bound(mix: fn(u64) -> u64) -> Vec<(u32, usize, usize)> {
        const KEYS: u64 = 1 << 15;
        let mut over = Vec::new();
        for shift in [0, 16, 32, 48] {
            let (index, tag) = spread(mix, (0..KEYS).map(|key| key << shift));
            if index > 3 * 8 || tag > 256 * 3 / 2 {
                over.push((shift, index, tag));
            }
        }
        over
    }

    // Arithmetic on 2^17 keys and no unsafe code: nothing for Miri to check, and slow there.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn sequential_and_shifted_keys_spread() {
        assert_eq!(over_bound(mix), [], "(shift, fullest bucket, fullest tag) over the bound");
        // A `u32` key is its word zero-extended: sequential, and in the top half.
        for shift in [0, 16] {
            let (index, tag) = spread(|key| hash_plain(&(key as u32)), (0..1 << 15).map(|key| key << shift));
            assert!(index <= 3 * 8 && tag <= 256 * 3 / 2, "u32 keys << {shift}: {index}, {tag}");
        }
    }

    /// The planted bug: a mixer that keeps only the low half of the product puts every key
    /// that differs only above bit 12 in one bucket. The checker must say so.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn the_spread_check_catches_a_low_half_mixer() {
        fn low_half(word: u64) -> u64 {
            (word ^ PLAIN_XOR).wrapping_mul(PLAIN_MUL)
        }
        let over = over_bound(low_half);
        let shifts: Vec<u32> = over.iter().map(|(shift, ..)| *shift).collect();
        assert_eq!(shifts, [16, 32, 48], "the low-half mixer's spread: {over:?}");
        // And the high half is what the real mixer adds.
        assert_ne!(folded_multiply(1 << 48, PLAIN_MUL), (1u64 << 48).wrapping_mul(PLAIN_MUL));
    }

    /// One case: a `PlainMap` and a `BTreeMap` given the same inserts, removes, gets and entry
    /// updates agree at every step and at the end.
    fn agrees_with_btree<K: PlainKey + Ord + core::fmt::Debug>(tape: &Tape, key: impl Fn(&Tape) -> K) {
        let mut map: PlainMap<K, u32, Global> = PlainMap::plain_in(Global);
        let mut model = BTreeMap::new();
        let ops = tape.range(0..if cfg!(miri) { 64 } else { 512 }, "ops");
        for _ in 0..ops {
            let key = key(tape);
            let value = tape.below(1 << 20, "value") as u32;
            match tape.below(4, "op") {
                0 => assert_eq!(map.insert(key, value), model.insert(key, value), "insert {key:?}"),
                1 => assert_eq!(map.remove(&key), model.remove(&key), "remove {key:?}"),
                2 => assert_eq!(map.get(&key), model.get(&key), "get {key:?}"),
                _ => match map.entry(key) {
                    Entry::Occupied(mut held) => {
                        *held.get_mut() += value;
                        let want = model.get_mut(&key).expect("the model holds an occupied key");
                        *want += value;
                        assert_eq!(*held.get(), *want, "entry {key:?}");
                    }
                    Entry::Vacant(free) => {
                        assert_eq!(model.insert(key, value), None, "a vacant entry {key:?}");
                        free.insert(value);
                    }
                },
            }
            assert_eq!(map.len(), model.len());
        }
        let mut held: Vec<(K, u32)> = map.iter().map(|(key, value)| (*key, *value)).collect();
        held.sort();
        assert_eq!(held, model.into_iter().collect::<Vec<_>>());
    }

    const CASES: u32 = if cfg!(miri) { 16 } else { 256 };

    #[test]
    fn a_plain_map_of_u32_is_a_btree_map() {
        // Keys from a small range, so removes and entries meet held keys.
        blaze_sim::check(Config::cases(CASES), |tape: &Tape| {
            agrees_with_btree(tape, |tape| tape.below(64, "key") as u32)
        });
    }

    #[test]
    fn a_plain_map_of_byte_arrays_is_a_btree_map() {
        blaze_sim::check(Config::cases(CASES), |tape: &Tape| {
            agrees_with_btree(tape, |tape| {
                let mut key = [0u8; 16];
                key[0] = tape.below(64, "key") as u8;
                key[15] = tape.below(2, "key tail") as u8;
                key
            })
        });
    }
}
