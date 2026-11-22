//! [`Carried`]: a table that outlives a snapshot.

use core::{alloc::Allocator, fmt, hash::Hash};

use blaze_hash::{FixedState, Map, MapOpts, hash_map};

use crate::Portable;

/// A table that outlives a snapshot (it is kept across `Memo::next`, or for a session): a map
/// that can only be keyed by a [`Portable`] key, so it never holds an id of a dead arena. An
/// item is its [`crate::ItemKey`] here, never its interned id.
///
/// It is the workspace's single-owner map (`blaze_hash::Map` on `FixedState`, AGENTS.md rule 12) in the
/// allocator `A`, which the maker names (rule 11): `Global` for a table that lives as long as
/// the process's database, a request's or a session's allocator for a shorter one. Shared
/// between threads, it sits behind a `blaze_core::lock::Lock` as any single-owner map does.
pub struct Carried<K: Portable, V, A: Allocator + Clone> {
    map: Map<K, V, FixedState, A>,
}

impl<K: Portable + Eq + Hash, V, A: Allocator + Clone> Carried<K, V, A> {
    /// An empty table in `alloc`. It allocates nothing until its first insert.
    pub fn new_in(alloc: A) -> Carried<K, V, A> {
        Carried { map: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }) }
    }

    /// An empty table in `alloc` with room for `capacity` entries.
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Carried<K, V, A> {
        Carried { map: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: Some(capacity) }) }
    }

    /// Puts `value` under `key`, giving back the value it held before, if any.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.map.insert(key, value)
    }

    /// The value under `key`, if any.
    pub fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    /// The value under `key`, to change in place, if any.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.map.get_mut(key)
    }

    /// Whether a value is under `key`.
    pub fn contains_key(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Takes the value under `key` out, if any.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.map.remove(key)
    }

    /// Keeps only the entries `keep` says yes to.
    pub fn retain(&mut self, keep: impl FnMut(&K, &mut V) -> bool) {
        self.map.retain(keep);
    }

    /// The entries, in no order a caller may rely on: an output or a key made from them sorts
    /// them by key first (docs/v2/plan/design/identity.md, point 5).
    pub fn iter(&self) -> hash_map::Iter<'_, K, V> {
        self.map.iter()
    }

    /// How many entries it holds.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether it holds no entry.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl<'a, K: Portable, V, A: Allocator + Clone> IntoIterator for &'a Carried<K, V, A> {
    type Item = (&'a K, &'a V);
    type IntoIter = hash_map::Iter<'a, K, V>;

    fn into_iter(self) -> hash_map::Iter<'a, K, V> {
        self.map.iter()
    }
}

impl<K: Portable + fmt::Debug, V: fmt::Debug, A: Allocator + Clone> fmt::Debug for Carried<K, V, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.map, f)
    }
}
