//! The tables shared between threads ([`SharedMap`], [`SharedSet`], [`SharedByHash`]):
//! papaya's, behind a pin that reads its thread in a function of its own.
//!
//! A papaya pin takes a guard of seize, its reclaimer, for the thread it is made on: the
//! thread's slot in the table's collector, found through a thread-local. A fiber can resume
//! on another thread after any wait (AGENTS.md rule 27), and the compiler, which knows no
//! function moves between threads, may read a thread-local's address once and keep it across
//! a call that waits: papaya's `pin` inlined into a function that pins, waits and pins again
//! makes the second pin with the first thread's slot (the codegen walk's count of a batch of
//! functions did, on arm64: one read of `tpidr_el0` before its loop). Two threads then count
//! guards in one slot, which is not atomic: the slot's thread is taken to be out while a
//! guard on it still reads, the table it reads is freed under it, and papaya's resize
//! unwraps a next table that is gone (`raw/mod.rs:2008`). So the pin here is
//! `#[inline(never)]`: it reads the thread-local in a call that ends before any wait, and
//! every pin names the thread it is made on. A guard taken holds no other thread-local, and it
//! still may not be held across a wait (`blaze::held_across_wait`).

#![expect(
    clippy::disallowed_types,
    reason = "the one home of papaya's tables: everywhere else names these wrappers"
)]

use std::fmt;

use papaya::{HashMapRef, HashSetRef, LocalGuard};

use crate::{BuildKeyHasher, FixedState};

/// A map of ordinary keys shared between threads, made by `SharedMap::default()`. Like the
/// other shared tables it names no allocator: papaya takes none, and a shared table is
/// long-lived, so it is on the global heap (see the crate's documentation).
pub struct SharedMap<K, V, S = FixedState>(papaya::HashMap<K, V, S>);

/// A map on hashes shared between threads, indexed by the hash itself.
pub type SharedByHash<K, V> = SharedMap<K, V, BuildKeyHasher>;

/// A set of ordinary keys shared between threads, made by `SharedSet::default()`.
pub struct SharedSet<K, S = FixedState>(papaya::HashSet<K, S>);

impl<K, V, S: Default> Default for SharedMap<K, V, S> {
    fn default() -> Self {
        SharedMap(papaya::HashMap::with_hasher(S::default()))
    }
}

impl<K, V, S> SharedMap<K, V, S> {
    /// The map pinned on the thread this runs on: what reads and writes it, held for a few
    /// operations and never across a wait. Never inlined: see the module's documentation.
    #[inline(never)]
    pub fn pin(&self) -> HashMapRef<'_, K, V, S, LocalGuard<'_>> {
        self.0.pin()
    }

    /// A guard of this map, taken as [`Self::pin`] takes its own: what the test reads the
    /// thread of, as a pin does not show its guard.
    #[cfg(test)]
    #[inline(never)]
    pub(crate) fn guard(&self) -> LocalGuard<'_> {
        self.0.guard()
    }

    /// The test's planted bug: papaya's pin inlined into its caller.
    #[cfg(test)]
    #[inline(always)]
    pub(crate) fn guard_inlined(&self) -> LocalGuard<'_> {
        self.0.guard()
    }
}

impl<K: std::hash::Hash + Eq, V, S: std::hash::BuildHasher> SharedMap<K, V, S> {
    /// How many entries it holds.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<K, V, S> fmt::Debug for SharedMap<K, V, S>
where
    K: std::hash::Hash + Eq + fmt::Debug,
    V: fmt::Debug,
    S: std::hash::BuildHasher,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.pin().iter()).finish()
    }
}

impl<K, S: Default> Default for SharedSet<K, S> {
    fn default() -> Self {
        SharedSet(papaya::HashSet::with_hasher(S::default()))
    }
}

impl<K, S> SharedSet<K, S> {
    /// The set pinned on the thread this runs on, as [`SharedMap::pin`].
    #[inline(never)]
    pub fn pin(&self) -> HashSetRef<'_, K, S, LocalGuard<'_>> {
        self.0.pin()
    }
}

impl<K: std::hash::Hash + Eq, S: std::hash::BuildHasher> SharedSet<K, S> {
    /// How many keys it holds.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<K, S> fmt::Debug for SharedSet<K, S>
where
    K: std::hash::Hash + Eq + fmt::Debug,
    S: std::hash::BuildHasher,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.pin().iter()).finish()
    }
}
