//! What blaze hashes in memory with: one choice per kind of key, so no crate picks its own.
//!
//! - Ordinary keys (ids, names, tuples, paths): [`HashMap`] and [`HashSet`], this crate's own
//!   SwissTable ([`hash_map`], [`hash_set`], on `blaze_swiss`'s group probe) on [`FixedState`],
//!   foldhash's fast hasher with its one fixed seed, in the allocator the type names
//!   (`HashMap<K, V, &Frame>` for a task's, `HashMap<K, V, Global>` for the global heap's). It
//!   costs about what FxHash does per key, and it spreads sequential integers and aligned
//!   pointers over the whole word, which FxHash's single multiply does not: the table places a
//!   key by the low bits of its hash and tells keys of a group apart by the top seven. The seed
//!   is fixed so that a run is reproducible; no output may depend on a map's order all the same.
//! - Keys that are hashes already (a content ID, a store key, a fingerprint): [`ByHash`] and
//!   [`ByHashSet`], on [`KeyHasher`], which takes the key's first word and hashes nothing, in
//!   a named allocator as above.
//! - Keys that are their bytes (integers, indices, digests, arrays of them, [`PlainKey`]):
//!   [`PlainMap`] and [`PlainSet`], on [`Plain`], which hashes the bytes in one step
//!   ([`hash_plain`]) with no `Hasher`; a digest's first word is its hash, as [`KeyHasher`]'s.
//! - A table whose iteration reaches an output, or whose entries are named by position:
//!   [`IndexMap`] and [`IndexSet`] ([`order`]), which iterate in insertion order whatever the
//!   hasher, the capacity or the process: dense entries and a table of `u32` indices, in a
//!   named allocator as above. They replace the `indexmap` crate in blaze's own code.
//! - A map or set shared between threads: [`SharedMap`] and [`SharedSet`] (and [`SharedByHash`]
//!   for keys that are hashes), papaya's lock-free tables on the same hashers. A read takes no
//!   lock and writes no shared memory, and nothing is held across a call, so no thread waits on
//!   another's lookup. Its values are lent (`map.pin().get(..)`), never moved out: a table whose
//!   entries are taken by value stays a locked [`HashMap`]. These name no allocator: papaya
//!   takes none, and a shared table is long-lived (a daemon's, a view's), so the global heap is
//!   its lifetime's allocator; `blaze::implicit_allocator` leaves them alone for that reason.
//! - Keys an attacker may pick (a source's names and literals, a tree's file names, a path a
//!   build script reads, a symbol's name) in a table that grows with them (an interner, an
//!   index, a linker's symbols): [`SipKey`], SipHash-1-3 under a key drawn at random for a
//!   table nothing persists ([`SipKey::random`], or a [`LazyKey`] for a table in a `static`),
//!   or derived from the table's content with BLAKE3 for one an image lays out
//!   (`blaze_ids::index_key`). A byte string is [`SipKey::hash`]ed; any other value through its
//!   `Hash` by [`SipHasher`] ([`SipKey::hash_one`], or `SipKey` as a table's `BuildHasher`). A
//!   fixed-seed hash there lets whoever picks the keys pick many with one hash, and each insert
//!   then walks them all (HashDoS): foldhash's fixed seed is public, and a word that cancels a
//!   seed word zeroes its multiply, whatever the rest. foldhash under a key drawn at run time
//!   is no cheaper than SipHash, as the fixed seed's lead is LLVM folding it into the code
//!   (`interning_cost` in hir-ty, 2026-11-03). So the hottest of these tables (hir-ty's
//!   interned kinds, the watcher's names) are [`adaptive`]: the fixed hash until a lookup walks
//!   a run no honest set of keys makes, then SipHash under a key drawn then, for good. A
//!   single-owner table of such keys is a [`KeyedMap`] or [`KeyedSet`]: it has no `new_in`,
//!   only [`Map::with_hasher_in`] with the key, so it cannot be made without one.
//! - Hashes that are persisted or compared across runs are BLAKE3, never these: a content ID
//!   is `blaze_ids::Hasher`, and a value's `Hash` is fed to it through [`Blake3Hasher`]. This
//!   crate depends only on blaze-own and blaze-swiss, which depend on no crate from outside the
//!   workspace, so every crate can use it; it is `no_std` and uses no `alloc`: memory comes from
//!   the allocator a table names.

#![no_std]
#![feature(allocator_ext)]
// `#[may_dangle]` on the table's drop, as std's and hashbrown's tables have.
#![feature(dropck_eyepatch)]
// blaze's lints (`blaze::trust`) name their tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::trust(
    unsafe,
    reason = "the table's own memory (`raw`: control words, slots initialized exactly where tagged live, one allocation in the table's allocator), a `PlainKey`'s bytes read as `u8`s (`plain`: the trait's contract says none is padding) and libc's `getentropy` for `SipKey::random`; each block names the invariant it relies on"
)]
#![cfg_attr(test, feature(hashmap_internals))]
#![cfg_attr(
    test,
    allow(
        internal_features,
        reason = "std's SipHasher13, the reference `sip`'s tests check against"
    )
)]

// Tests compare against std's tables and hashers and use its processes and clocks; the crate
// itself sees `core` only.
#[cfg(test)]
#[macro_use]
extern crate std;

use core::hash::{BuildHasher, Hasher};

pub mod adaptive;
mod blake3;
mod fold;
pub mod hash_map;
pub mod hash_set;
mod key;
pub mod order;
#[cfg(test)]
mod order_tests;
mod plain;
mod raw;
#[cfg(test)]
mod raw_tests;
mod sip;
#[cfg(test)]
mod tests;

pub use blake3::{Blake3, Blake3Hasher};
pub use key::{BuildKeyHasher, ByHash, ByHashSet, KeyHasher};
pub use sip::{LazyKey, SipHasher, SipKey};
pub use order::{OrderMap, OrderSet};
pub use plain::{KeyHash, Plain, PlainKey, hash_plain};
pub use hash_map::Map;
pub use hash_set::Set;
/// Why a `try_reserve` was refused: blaze-own's, the one allocation failure of the tree.
pub use blaze_own::AllocFailure;

/// A borrowed form of a key that a table is asked with (`map.get(&("a", 1))` for a key of
/// `(String, u32)`, through a type that compares to it); every `Q: Eq` that a key `Borrow`s is
/// one. Under the `shared` feature it is papaya's trait of the same shape, so one impl serves
/// the shared tables and these.
#[cfg(not(feature = "shared"))]
pub trait Equivalent<K: ?Sized> {
    fn equivalent(&self, key: &K) -> bool;
}

#[cfg(not(feature = "shared"))]
impl<Q: ?Sized + Eq, K: ?Sized + core::borrow::Borrow<Q>> Equivalent<K> for Q {
    #[inline]
    fn equivalent(&self, key: &K) -> bool {
        PartialEq::eq(self, key.borrow())
    }
}

/// A map of ordinary keys in the allocator `A`, which every use names: `HashMap::new_in(frame)`,
/// `HashMap::with_capacity_in(n, frame)`, `HashMap::from_iter_in(iter, frame)`.
pub type HashMap<K, V, A> = Map<K, V, FixedState, A>;

/// A set of ordinary keys in the allocator `A`, which every use names, as for [`HashMap`].
pub type HashSet<K, A> = Set<K, FixedState, A>;

/// A map of keys that are their bytes ([`PlainKey`]: integers, indices, digests, arrays of
/// them) in the allocator `A`, made by `PlainMap::plain_in(alloc)`: a lookup hashes the key's
/// bytes in one step ([`hash_plain`]) instead of feeding a `Hasher` field by field.
pub type PlainMap<K, V, A> = Map<K, V, Plain, A>;

/// A set of [`PlainKey`]s, as [`PlainMap`]: `PlainSet::plain_in(alloc)`.
pub type PlainSet<K, A> = Set<K, Plain, A>;

/// A map of ordinary keys that iterates in insertion order, in the allocator `A`, which every
/// use names: `IndexMap::new_in(frame)`, `IndexMap::with_capacity_in(n, frame)`.
pub type IndexMap<K, V, A> = OrderMap<K, V, FixedState, A>;

/// A set of ordinary keys that iterates in insertion order, in the allocator `A`.
pub type IndexSet<K, A> = OrderSet<K, FixedState, A>;

/// A map of keys an attacker may pick (names and paths from outside, a request's headers), in
/// the allocator `A`: SipHash-1-3 under a [`SipKey`] its maker hands in
/// (`KeyedMap::with_hasher_in(key, alloc)`, the key from [`SipKey::random`] or from a
/// capability that gives randomness). [`SipKey`] has no `Default`, so there is no `new_in`: a
/// keyed table cannot be made without its key. With the fixed seed of [`HashMap`], whoever
/// picks the keys can pick ones that share a hash, and each insert then walks them all.
pub type KeyedMap<K, V, A> = Map<K, V, SipKey, A>;

/// A set of keys an attacker may pick, as [`KeyedMap`].
pub type KeyedSet<K, A> = Set<K, SipKey, A>;

/// A map of ordinary keys shared between threads, made by `SharedMap::default()`. Like the
/// other shared tables it names no allocator: papaya takes none, and a shared table is
/// long-lived, so it is on the global heap (see the crate's documentation). Behind the
/// `shared` feature, which the crates that share a table turn on (the root manifest's entry).
#[cfg(feature = "shared")]
pub type SharedMap<K, V> = papaya::HashMap<K, V, FixedState>;

/// A set of ordinary keys shared between threads, made by `SharedSet::default()`.
#[cfg(feature = "shared")]
pub type SharedSet<K> = papaya::HashSet<K, FixedState>;

/// A map on hashes shared between threads, indexed by the hash itself.
#[cfg(feature = "shared")]
pub type SharedByHash<K, V> = papaya::HashMap<K, V, BuildKeyHasher>;

/// What every table is asked with under `shared`: a borrowed form of a key ([`Equivalent`]),
/// and what a compare-and-swap of an entry does ([`Operation`], for `map.pin().compute(..)`).
#[cfg(feature = "shared")]
pub use papaya::{Equivalent, Operation};

/// The `BuildHasher` of [`HashMap`] and [`HashSet`], and of any other table of ordinary keys
/// (`OrderMap`, `papaya`'s): every hasher it builds is [`FixedHasher::default`].
/// Zero-sized and `Copy`, so a table holds nothing for it, and the type is its own value
/// (`FixedState.hash_one(key)`, `HashMap::with_hasher(FixedState)`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct FixedState;

impl BuildHasher for FixedState {
    type Hasher = FixedHasher;

    #[inline]
    fn build_hasher(&self) -> FixedHasher {
        FixedHasher::default()
    }
}

/// foldhash's fast hasher with its fixed seed: what [`FixedState`] builds, and what hashes one
/// value outside a table.
#[derive(Clone)]
pub struct FixedHasher(fold::Fold);

impl FixedHasher {
    /// A hasher of the family [`FixedHasher::default`] is the first of (`with_seed(0)`), for
    /// hashing one value several independent ways.
    #[inline]
    pub fn with_seed(seed: u64) -> FixedHasher {
        FixedHasher(fold::Fold::with_seed(seed))
    }
}

impl Default for FixedHasher {
    #[inline]
    fn default() -> FixedHasher {
        FixedHasher(fold::Fold::with_seed(0))
    }
}

impl Hasher for FixedHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0.finish()
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.0.write(bytes);
    }

    #[inline]
    fn write_u8(&mut self, it: u8) {
        self.0.write_u8(it);
    }

    #[inline]
    fn write_u16(&mut self, it: u16) {
        self.0.write_u16(it);
    }

    #[inline]
    fn write_u32(&mut self, it: u32) {
        self.0.write_u32(it);
    }

    #[inline]
    fn write_u64(&mut self, it: u64) {
        self.0.write_u64(it);
    }

    #[inline]
    fn write_u128(&mut self, it: u128) {
        self.0.write_u128(it);
    }

    #[inline]
    fn write_usize(&mut self, it: usize) {
        self.0.write_usize(it);
    }
}
