//! What blaze hashes in memory with: one choice per kind of key, so no crate picks its own.
//!
//! - Ordinary keys (ids, names, tuples, paths): [`HashMap`] and [`HashSet`], hashbrown's map
//!   and set on [`FixedState`], foldhash's fast hasher with its one fixed seed, in the
//!   allocator the type names (`HashMap<K, V, &Frame>` for a task's, `HashMap<K, V, Global>`
//!   with `// alloc: <why>` for the global heap's; see [`table`]). It costs about what
//!   FxHash does per key, and it spreads sequential integers and aligned pointers over the whole
//!   word, which FxHash's single multiply does not: hashbrown places a key by the low bits of its
//!   hash and tells keys of a group apart by the top seven. The seed is fixed so that a run is
//!   reproducible; no output may depend on a map's order all the same.
//! - Keys that are hashes already (a content ID, a store key, a fingerprint): [`ByHash`] and
//!   [`ByHashSet`], on [`KeyHasher`], which takes the key's first word and hashes nothing, in
//!   a named allocator as above.
//! - A map or set shared between threads: [`SharedMap`] and [`SharedSet`] (and [`SharedByHash`]
//!   for keys that are hashes), papaya's lock-free tables on the same hashers. A read takes no
//!   lock and writes no shared memory, and nothing is held across a call, so no thread waits on
//!   another's lookup. Its values are lent (`map.pin().get(..)`), never moved out: a table whose
//!   entries are taken by value stays a locked [`HashMap`]. These name no allocator: papaya
//!   takes none, and a shared table is long-lived (a daemon's, a view's), so the global heap is
//!   its lifetime's allocator; `blaze::implicit_allocator` leaves them alone for that reason.
//! - Hashes that are persisted or compared across runs are BLAKE3, never these: a content ID
//!   is `blaze_ids::Hasher`, and a value's `Hash` is fed to it through [`Blake3Hasher`]. This
//!   crate depends on nothing of blaze's, so every crate can use it.

#![feature(allocator_api)]

use std::hash::{BuildHasher, Hasher};

mod blake3;
mod key;
pub mod table;
#[cfg(test)]
mod tests;

pub use blake3::{Blake3, Blake3Hasher};
pub use key::{BuildKeyHasher, ByHash, ByHashSet, KeyHasher};
pub use table::{Map, Set};

/// The entry and iterator types of [`HashMap`] and [`ByHash`]: hashbrown's
/// (`hash_map::Entry::Vacant(..)`), not std's.
pub use hashbrown::hash_map;
/// The iterator types of [`HashSet`] and [`ByHashSet`]: hashbrown's.
pub use hashbrown::hash_set;

/// A map of ordinary keys in the allocator `A`, which every use names: `HashMap::new_in(frame)`,
/// `HashMap::with_capacity_in(n, frame)`, `HashMap::from_iter_in(iter, frame)`.
pub type HashMap<K, V, A> = Map<K, V, FixedState, A>;

/// A set of ordinary keys in the allocator `A`, which every use names, as for [`HashMap`].
pub type HashSet<K, A> = Set<K, FixedState, A>;

/// A map of ordinary keys shared between threads, made by `SharedMap::default()`. Like the
/// other shared tables it names no allocator: papaya takes none, and a shared table is
/// long-lived, so it is on the global heap (see the crate's documentation).
pub type SharedMap<K, V> = papaya::HashMap<K, V, FixedState>;

/// A set of ordinary keys shared between threads, made by `SharedSet::default()`.
pub type SharedSet<K> = papaya::HashSet<K, FixedState>;

/// A map on hashes shared between threads, indexed by the hash itself.
pub type SharedByHash<K, V> = papaya::HashMap<K, V, BuildKeyHasher>;

/// What the shared tables are asked with: a borrowed form of a key ([`Equivalent`]), and what
/// a compare-and-swap of an entry does ([`Operation`], for `map.pin().compute(..)`).
pub use papaya::{Equivalent, Operation};

/// The `BuildHasher` of [`HashMap`] and [`HashSet`], and of any other table of ordinary keys
/// (`hashbrown`, `indexmap`, `papaya`): every hasher it builds is [`FixedHasher::default`].
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
pub struct FixedHasher(foldhash::fast::FoldHasher<'static>);

impl FixedHasher {
    /// A hasher of the family [`FixedHasher::default`] is the first of (`with_seed(0)`), for
    /// hashing one value several independent ways.
    #[inline]
    pub fn with_seed(seed: u64) -> FixedHasher {
        FixedHasher(foldhash::fast::FixedState::with_seed(seed).build_hasher())
    }
}

impl Default for FixedHasher {
    #[inline]
    fn default() -> FixedHasher {
        FixedHasher(foldhash::fast::FixedState::default().build_hasher())
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
