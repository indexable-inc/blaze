//! The one hash map and set (blaze-hash's: hashbrown on foldhash with one fixed seed), in the
//! allocator their type names, with fallible growth only (docs/v2/plan/design/fallible-bstd.md,
//! decision D29): [`Map::insert`] and [`Set::insert`] reserve their room first and return
//! [`OutOfMemory`] naming the arena when there is none. Reading is hashbrown's, through
//! `Deref` (`get`, `contains_key`, `iter`, `len`); hashbrown's own `insert` and `entry`, which
//! end the process when the allocator says no, are not reachable through these types.
//!
//! Shared between threads, the map is papaya's ([`SharedMap`]): it takes no allocator and has
//! no fallible form, so its growth is the global heap's (fallible-bstd.md, "What is built, and
//! what is left").

use core::{
    alloc::Allocator,
    borrow::Borrow,
    fmt,
    hash::Hash,
    ops::Deref,
};

pub use blaze_hash::{Equivalent, SharedByHash, SharedMap};

use crate::mem::{Named, OutOfMemory};

/// A map of ordinary keys in the allocator `A`.
pub struct Map<K, V, A: Allocator> {
    /// Grown only by `try_reserve`; inserted into only with room reserved.
    inner: blaze_hash::HashMap<K, V, A>,
}

impl<K, V, A: Allocator> Map<K, V, A> {
    /// An empty map in `alloc`; nothing is allocated until it grows.
    pub fn new_in(alloc: A) -> Map<K, V, A> {
        Map { inner: blaze_hash::HashMap::new_in(alloc) }
    }

    /// Drops every entry, keeping the room.
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Only the entries `keep` says yes to.
    pub fn retain(&mut self, keep: impl FnMut(&K, &mut V) -> bool) {
        self.inner.retain(keep);
    }

    pub fn iter_mut(&mut self) -> blaze_hash::hash_map::IterMut<'_, K, V> {
        self.inner.iter_mut()
    }
}

impl<K: Eq + Hash, V, A: Allocator> Map<K, V, A> {
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.get_mut(key)
    }

    /// Takes the entry of `key` out.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.remove(key)
    }
}

impl<K: Eq + Hash, V, A: Allocator + Named> Map<K, V, A> {
    /// What a growth that found no room says.
    fn out_of_memory(&self, entries: usize) -> OutOfMemory {
        OutOfMemory {
            arena: self.inner.allocator().name(),
            asked: entries.saturating_mul(size_of::<(K, V)>()),
        }
    }

    /// Room for `additional` more entries.
    pub fn reserve(&mut self, additional: usize) -> Result<(), OutOfMemory> {
        match self.inner.try_reserve(additional) {
            Ok(()) => Ok(()),
            Err(_) => Err(self.out_of_memory(additional)),
        }
    }

    /// Puts `value` under `key` and gives back what was there. When the arena has no room for
    /// one more entry the map is as it was, and `key` and `value` are dropped.
    pub fn insert(&mut self, key: K, value: V) -> Result<Option<V>, OutOfMemory> {
        self.reserve(1)?;
        // Room for one more entry was just reserved, so hashbrown's insert does not allocate
        // and cannot reach `handle_alloc_error` (hashbrown has no insert that says so in its
        // type; fallible-bstd.md, "The floor").
        #[expect(
            clippy::disallowed_methods,
            reason = "hashbrown's insert, after the reserve above made room: the one place it is called"
        )]
        Ok(self.inner.insert(key, value))
    }

    /// The value under `key`, made by `make` and put there when there is none.
    pub fn get_or_insert_with(
        &mut self,
        key: K,
        make: impl FnOnce() -> V,
    ) -> Result<&mut V, OutOfMemory> {
        self.reserve(1)?;
        // As in `insert`: the room is reserved, so the entry's insert does not allocate.
        #[expect(
            clippy::disallowed_methods,
            reason = "hashbrown's entry, after the reserve above made room"
        )]
        Ok(self.inner.entry(key).or_insert_with(make))
    }
}

impl<K, V, A: Allocator> Deref for Map<K, V, A> {
    type Target = blaze_hash::HashMap<K, V, A>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<K: fmt::Debug, V: fmt::Debug, A: Allocator> fmt::Debug for Map<K, V, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

/// A set of ordinary keys in the allocator `A`: a [`Map`] to nothing.
pub struct Set<T, A: Allocator> {
    inner: Map<T, (), A>,
}

impl<T, A: Allocator> Set<T, A> {
    /// An empty set in `alloc`; nothing is allocated until it grows.
    pub fn new_in(alloc: A) -> Set<T, A> {
        Set { inner: Map::new_in(alloc) }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.inner.keys()
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

impl<T: Eq + Hash, A: Allocator> Set<T, A> {
    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.contains_key(value)
    }

    /// Takes `value` out; whether it was there.
    pub fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.remove(value).is_some()
    }
}

impl<T: Eq + Hash, A: Allocator + Named> Set<T, A> {
    /// Adds `value`; whether it was new. When the arena has no room the set is as it was.
    pub fn insert(&mut self, value: T) -> Result<bool, OutOfMemory> {
        Ok(self.inner.insert(value, ())?.is_none())
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for Set<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}
