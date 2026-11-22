//! What blaze hashes in memory with: one choice per kind of key, so no crate picks its own.
//!
//! **A table names its hasher (decision D125)**, in its type and as a value when it is made, as
//! it names its allocator, and it has one constructor, `new`, taking one options struct whose
//! every field is written (D126, rule 83 options-struct):
//!
//! ```text
//! let map: Map<K, V, FixedState, &Frame> =
//!     Map::new(MapOpts { hasher: FixedState, alloc: frame, capacity: None });
//! let set = Set::new(SetOpts { hasher: key, alloc: frame, capacity: Some(n) }); // key: SipKey
//! let ordered = OrderMap::new(OrderMapOpts { hasher: FixedState, alloc, capacity: None });
//! let shared = shared::Map::new(shared::MapOpts {
//!     name: "cas.packs", hasher: FixedState, alloc, capacity: None,
//! });
//! ```
//!
//! [`MapOpts`], [`SetOpts`], [`OrderMapOpts`], [`OrderSetOpts`] and the shared tables'
//! `shared::MapOpts` and `shared::SetOpts` (here `SharedMapOpts`, `SharedSetOpts`). A table
//! of an iterator's items is `new` then `extend`. The older constructors (`new_in`,
//! `with_capacity_in`, `from_iter_in`, `with_hasher_in`, `with_capacity_and_hasher_in`) are each
//! a call of `new` and go at the sweep, as do the aliases that hide the hasher (`HashMap`,
//! `HashSet`, `IndexMap`, `IndexSet`, `ByHash`, `ByHashSet`, `KeyedMap`, `KeyedSet`,
//! `SharedMap`, `SharedSet`, `SharedByHash`); the lint rule `ambient-hasher` finds them. Which
//! hasher:
//!
//! | Keys | Hasher | Its value | Open to |
//! |---|---|---|---|
//! | Ordinary keys: ids, names blaze made, tuples, paths of the tree | [`FixedState`] | `FixedState` (zero-sized) | HashDoS: its seed is fixed and public, so whoever picks the keys can pick many that share a hash, and each insert then walks them all |
//! | Keys an outsider picks: a source's names and literals, file names, a request's headers | [`SipKey`] | the key from the randomness grant (bstd's `Io::sip_key`), handed in by the table's maker; no `Default`, so no table of it is made without one | nothing known while the key stays secret (a keyed PRF) |
//! | Keys that are hashes already: a content ID, a store key, a fingerprint | [`BuildKeyHasher`] | `BuildKeyHasher::default()` (zero-sized) | anything, if the key is not really a hash: it takes the key's first word as its hash |
//! | Keys that hold a digest beside other fields: an engine cell (kind, subject, cell) | [`DigestState`] over a [`DigestKey`] | `DigestState` (zero-sized) | whoever writes the digested inputs, at a cost of trying inputs per key; the build's own inputs only (`digest`'s documentation) |
//!
//! [`FixedState`] holds nothing of the machine or the run (rule 25): its hasher is foldhash's
//! on constants written in `fold`, seed 0, with no address, clock, thread or process input.
//! Its hashes do depend on the target's byte order and word size (bytes are loaded in native
//! order, a `usize` is written at the target's width, and std's `Hash` writes a slice's length
//! as a `usize`), so a table's order can differ between a 64-bit host and `wasm32`: no output
//! may depend on a map's order, which an [`OrderMap`] keeps to.
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
//! - Keys that are their bytes: a [`HashMap`] or [`HashSet`] of them ([`PlainKey`]: integers,
//!   indices, digests, arrays of them) hashes those bytes in one step ([`hash_plain`]) with no
//!   `Hasher`, by a specialization of [`KeyHash`] for [`FixedState`] (`plain`'s documentation
//!   says why it is sound); a digest's first word is its hash, as [`KeyHasher`]'s.
//! - A table whose iteration reaches an output, or whose entries are named by position:
//!   [`IndexMap`] and [`IndexSet`] ([`order`]), which iterate in insertion order whatever the
//!   hasher, the capacity or the process: dense entries and a table of `u32` indices, in a
//!   named allocator as above. They replace the `indexmap` crate in blaze's own code.
//! - A map or set shared between threads: [`SharedMap`] and [`SharedSet`] (and [`SharedByHash`]
//!   for keys that are hashes), this crate's sharded tables on the same hashers ([`shared`]). A
//!   read takes no lock and writes no shared memory, and nothing is held across a call, so no
//!   thread waits on another's lookup; a write locks one shard of 64. Its values are lent
//!   (`map.pin().get(..)`), never moved out: a table whose entries are taken by value stays a
//!   locked [`HashMap`]. Each takes its allocator from its caller (`blaze_core::SharedMap::new_in(name, Global)`):
//!   a shared table is long-lived (a daemon's, a view's), so its allocator is one of its
//!   lifetime, most often the global heap, named by the caller.
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
// `KeyHash` for `FixedState` over a `PlainKey`, within the sound subset (`plain`'s
// documentation), and `PlainKey` marked as a trait that subset may specialize on.
#![feature(min_specialization, rustc_attrs)]
// blaze's lints (`blaze::trust`) name their tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
// A core: tables and hashers over memory their caller hands in. The one read of the operating
// system, `SipKey::random`'s `getentropy` (and `LazyKey::get` through it), is allowed at its
// site until its callers (intern) take a key from bstd's `Io::sip_key` (batch P4-random).
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
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
mod digest;
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
#[cfg(feature = "shared")]
#[expect(
    clippy::disallowed_types,
    reason = "the generic shared map's own home (D64): the ban keeps other crates on blaze-core's alias"
)]
pub mod shared;
mod sip;
#[cfg(test)]
mod tests;

pub use blake3::{Blake3, Blake3Hasher};
pub use digest::{DigestKey, DigestState, digest_word, fold_part};
pub use key::{BuildKeyHasher, ByHash, ByHashSet, KeyHasher};
pub use sip::{LazyKey, SipHasher, SipKey};
pub use order::{OrderMap, OrderMapOpts, OrderSet, OrderSetOpts};
pub use plain::{KeyHash, PlainKey, hash_plain};
pub use hash_map::{Map, MapOpts};
pub use hash_set::{Set, SetOpts};
/// The shared tables' options (`shared::MapOpts`, `shared::SetOpts`), named apart from the
/// single-owner ones:
/// `blaze_core::SharedMap::new(SharedMapOpts { name, hasher, alloc, capacity: None })`.
#[cfg(feature = "shared")]
pub use shared::{MapOpts as SharedMapOpts, SetOpts as SharedSetOpts};
/// Why a `try_reserve` was refused: blaze-own's, the one allocation failure of the tree.
pub use blaze_own::AllocFailure;

/// A borrowed form of a key that a table is asked with (`map.get(&("a", 1))` for a key of
/// `(String, u32)`, through a type that compares to it); every `Q: Eq` that a key `Borrow`s is
/// one. The shared tables take it too, so one impl serves them and these.
pub trait Equivalent<K: ?Sized> {
    fn equivalent(&self, key: &K) -> bool;
}

impl<Q: ?Sized + Eq, K: ?Sized + core::borrow::Borrow<Q>> Equivalent<K> for Q {
    #[inline]
    fn equivalent(&self, key: &K) -> bool {
        PartialEq::eq(self, key.borrow())
    }
}

/// A map of ordinary keys in the allocator `A`, which every use names: `HashMap::new_in(frame)`,
/// `HashMap::with_capacity_in(n, frame)`, `HashMap::from_iter_in(iter, frame)`.
///
/// D125: goes at the sweep; write the hasher: `Map<K, V, FixedState, A>`, made by
/// `Map::new(MapOpts { hasher: FixedState, alloc, capacity: None })`.
pub type HashMap<K, V, A> = Map<K, V, FixedState, A>;

/// A set of ordinary keys in the allocator `A`, which every use names, as for [`HashMap`].
///
/// D125: goes at the sweep; write the hasher: `Set<K, FixedState, A>`, made by
/// `Set::new(SetOpts { hasher: FixedState, alloc, capacity: None })`.
pub type HashSet<K, A> = Set<K, FixedState, A>;
/// A map of ordinary keys that iterates in insertion order, in the allocator `A`, which every
/// use names: `IndexMap::new_in(frame)`, `IndexMap::with_capacity_in(n, frame)`.
///
/// D125: goes at the sweep; write the hasher: `OrderMap<K, V, FixedState, A>`, made by
/// `OrderMap::new(OrderMapOpts { hasher: FixedState, alloc, capacity: None })`.
pub type IndexMap<K, V, A> = OrderMap<K, V, FixedState, A>;

/// A set of ordinary keys that iterates in insertion order, in the allocator `A`.
///
/// D125: goes at the sweep; write the hasher: `OrderSet<K, FixedState, A>`, made by
/// `OrderSet::new(OrderSetOpts { hasher: FixedState, alloc, capacity: None })`.
pub type IndexSet<K, A> = OrderSet<K, FixedState, A>;

/// A map of keys an attacker may pick (names and paths from outside, a request's headers), in
/// the allocator `A`: SipHash-1-3 under a [`SipKey`] its maker hands in
/// (`KeyedMap::with_hasher_in(key, alloc)`, the key from [`SipKey::random`] or from a
/// capability that gives randomness). [`SipKey`] has no `Default`, so there is no `new_in`: a
/// keyed table cannot be made without its key. With the fixed seed of [`HashMap`], whoever
/// picks the keys can pick ones that share a hash, and each insert then walks them all.
///
/// D125: goes at the sweep; write the hasher: `Map<K, V, SipKey, A>`, made by
/// `Map::new(MapOpts { hasher: key, alloc, capacity: None })`.
pub type KeyedMap<K, V, A> = Map<K, V, SipKey, A>;

/// A set of keys an attacker may pick, as [`KeyedMap`].
///
/// D125: goes at the sweep; write the hasher: `Set<K, SipKey, A>`.
pub type KeyedSet<K, A> = Set<K, SipKey, A>;

/// A map of ordinary keys shared between threads ([`shared`]), in the allocator its caller
/// names (one that lives as long as the table, as a shared table is long-lived: see the crate's
/// documentation), its shards locked by `L` (decision D64). Code names blaze-core's
/// instantiation, `blaze_core::SharedMap`, made by `SharedMap::new_in(name, alloc)`; the root
/// `.clippy.toml` bans naming this one elsewhere. Behind the `shared` feature, which the crates
/// that share a table turn on (the root manifest's entry).
///
/// D125: goes at the sweep with blaze-core's instantiation; write the hasher in the type, and
/// make it by `new(SharedMapOpts { name, hasher: FixedState, alloc, capacity: None })`.
#[cfg(feature = "shared")]
#[expect(clippy::disallowed_types, reason = "the generic alias blaze-core instantiates (D64)")]
pub type SharedMap<K, V, A, L> = shared::Map<K, V, FixedState, A, L>;

/// A set of ordinary keys shared between threads, as [`SharedMap`]: `blaze_core::SharedSet`.
///
/// D125: goes at the sweep, as [`SharedMap`].
#[cfg(feature = "shared")]
#[expect(clippy::disallowed_types, reason = "the generic alias blaze-core instantiates (D64)")]
pub type SharedSet<K, A, L> = shared::Set<K, FixedState, A, L>;

/// A map on hashes shared between threads, indexed by the hash itself, as [`SharedMap`]:
/// `blaze_core::SharedByHash`.
///
/// D125: goes at the sweep, as [`SharedMap`], with `BuildKeyHasher::default()` as its hasher.
#[cfg(feature = "shared")]
#[expect(clippy::disallowed_types, reason = "the generic alias blaze-core instantiates (D64)")]
pub type SharedByHash<K, V, A, L> = shared::Map<K, V, BuildKeyHasher, A, L>;

/// The `BuildHasher` of [`HashMap`] and [`HashSet`], and of any other table of ordinary keys
/// (`OrderMap`, the shared tables): every hasher it builds is [`FixedHasher::default`].
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
