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

use core::hash::{BuildHasher, Hash, Hasher};

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

/// The short path's mixer: a word of at most eight key bytes, zero-extended. Also what
/// [`fold_part`](crate::fold_part) folds a digest key's other fields in with.
#[inline(always)]
pub(crate) const fn mix(word: u64) -> u64 {
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
    hash_bytes(bytes_of(key), K::UNIFORM)
}

/// The bytes of a slice of plain values, all `size_of_val(values)` of them.
#[inline(always)]
fn bytes_of_slice<T: PlainKey>(values: &[T]) -> &[u8] {
    // SAFETY: `values` is a live shared slice, so its `size_of_val(values)` bytes are one
    // readable allocation, unchanged for the borrow; elements sit at a stride of their size,
    // so there is no padding between them, and `PlainKey` promises none within one.
    unsafe { core::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of_val(values)) }
}

/// [`hash_plain`]'s three paths on a key's bytes: one function, so an array key and its slice
/// (the borrowed form a table may be asked with) hash alike. For a key of a fixed size the
/// length is a constant and two of the paths fold away.
#[inline(always)]
fn hash_bytes(bytes: &[u8], uniform: bool) -> u64 {
    if uniform && let Some(word) = bytes.first_chunk::<8>() {
        return u64::from_le_bytes(*word);
    }
    if bytes.len() <= 8 {
        let mut word = [0u8; 8];
        word[..bytes.len()].copy_from_slice(bytes);
        return mix(u64::from_le_bytes(word));
    }
    let mut hasher = FixedHasher::default();
    hasher.write(bytes);
    hasher.finish()
}

/// What hashes a table's key, the one seam its hashing is chosen at: any `BuildHasher` over a
/// `Hash` key (`FixedState`, `BuildKeyHasher`, `SipKey`, `adaptive`'s) by `hash_one`, and
/// [`FixedState`] over a [`PlainKey`] (or a slice of them) by [`hash_plain`]. A table asked
/// with a borrowed form `Q` needs `KeyHash<Q>` too.
pub trait KeyHash<Q: ?Sized> {
    fn hash_key(&self, key: &Q) -> u64;
}

/// The base impl, which the two below specialize for `FixedState`.
impl<Q: Hash + ?Sized, S: BuildHasher> KeyHash<Q> for S {
    #[inline(always)]
    default fn hash_key(&self, key: &Q) -> u64 {
        self.hash_one(key)
    }
}

// In min_specialization's sound subset (always applicable): the self type is the concrete
// `FixedState`; `K` appears once in the header; no lifetime parameter or bound; the one bound
// beyond the base impl's (`FixedState: BuildHasher` holds, and `K: Hash` is `PlainKey`'s
// supertrait) is `K: PlainKey`, a `#[rustc_specialization_trait]` whose impls are always
// applicable themselves. Only `FixedState`: a seeded `SipKey` keeps its seeded hash, and
// `BuildKeyHasher` its identity fold.
impl<K: PlainKey> KeyHash<K> for FixedState {
    #[inline(always)]
    fn hash_key(&self, key: &K) -> u64 {
        hash_plain(key)
    }
}

// In min_specialization's sound subset, as the impl above: concrete `FixedState`, `T` once,
// no lifetimes, the one added bound `T: PlainKey` (`[T]: Hash` follows from `T: Hash`). It
// does not overlap the impl above: `[T]` is unsized and a `PlainKey` is `Sized`. It is the
// probe side of an array key: `map.get(&bytes[..])` on a table of `[u8; 16]` hashes the
// slice's bytes exactly as `hash_plain` hashes the array's, so it finds the key.
impl<T: PlainKey> KeyHash<[T]> for FixedState {
    #[inline(always)]
    fn hash_key(&self, key: &[T]) -> u64 {
        hash_bytes(bytes_of_slice(key), T::UNIFORM)
    }
}

#[cfg(test)]
mod tests {
    use std::{alloc::Global, collections::BTreeMap, vec::Vec};

    use blaze_sim::{Config, Tape};

    use super::{KeyHash, PLAIN_MUL, PLAIN_XOR, PlainKey, folded_multiply, hash_plain, mix};
    use crate::{
        BuildKeyHasher, FixedHasher, FixedState, HashMap, HashSet, SipKey, hash_map::Entry,
    };
    use core::hash::{BuildHasher, Hasher};

    /// The specialization takes effect: a `HashMap<u32>`'s hasher hashes a key, and a probe,
    /// by `hash_plain`, not by `Hash`, and a slice probe of an array key finds it. A seeded
    /// `SipKey` and `BuildKeyHasher` are not specialized: they hash by `Hash` still.
    #[test]
    fn a_hash_map_of_plain_keys_hashes_by_bytes() {
        let mut map: HashMap<u32, u32, Global> = HashMap::new_in(Global);
        map.insert(7, 1);
        let hasher: &FixedState = map.hasher();
        assert_eq!(KeyHash::<u32>::hash_key(hasher, &7), hash_plain(&7u32));
        assert_ne!(hash_plain(&7u32), FixedState.hash_one(7u32), "the two paths differ");
        assert_eq!(map.get(&7), Some(&1));

        let key: [u8; 16] = core::array::from_fn(|at| at as u8 * 3);
        let mut set: HashSet<[u8; 16], Global> = HashSet::new_in(Global);
        set.insert(key);
        assert_eq!(KeyHash::<[u8]>::hash_key(set.hasher(), &key[..]), hash_plain(&key));
        assert!(set.contains(&key[..]), "a slice probe of an array key");
        let digest = Digest([9; 32]);
        assert_eq!(KeyHash::<[Digest]>::hash_key(&FixedState, &[digest][..]), hash_plain(&digest));

        let sip = SipKey::new(1, 2);
        assert_eq!(KeyHash::<u32>::hash_key(&sip, &7), BuildHasher::hash_one(&sip, 7u32));
        let by_hash = BuildKeyHasher::default();
        assert_eq!(KeyHash::<u64>::hash_key(&by_hash, &7), by_hash.hash_one(7u64));
    }

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

    /// One case: a `HashMap` of plain keys (so on `hash_plain`) and a `BTreeMap` given the same
    /// inserts, removes, gets and entry updates agree at every step and at the end.
    fn agrees_with_btree<K: PlainKey + Ord + core::fmt::Debug>(tape: &Tape, key: impl Fn(&Tape) -> K) {
        let mut map: HashMap<K, u32, Global> = HashMap::new_in(Global);
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
    fn a_hash_map_of_u32_is_a_btree_map() {
        // Keys from a small range, so removes and entries meet held keys.
        blaze_sim::check(Config::cases(CASES), |tape: &Tape| {
            agrees_with_btree(tape, |tape| tape.below(64, "key") as u32)
        });
    }

    #[test]
    fn a_hash_map_of_byte_arrays_is_a_btree_map() {
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
