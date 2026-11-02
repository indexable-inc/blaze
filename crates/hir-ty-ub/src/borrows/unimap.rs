//! Dense node indices for one tree: `UniKeyMap` maps a tag to its `UniIndex` once per access,
//! and every per-node table (`UniValMap`, the node columns) is then a plain vector indexed by
//! it. Indices freed by the GC are reused.
//!
//! Miri: `tree_borrows/unimap.rs`.

use std::{fmt, hash::Hash};

use blaze_hash::HashMap;

/// A node's index in its tree.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct UniIndex {
    pub(super) idx: u32,
}

impl UniIndex {
    pub(super) fn index(self) -> usize {
        self.idx as usize
    }
}

impl fmt::Debug for UniIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.idx.fmt(f)
    }
}

/// From `K` to `UniIndex`. The keys of `mapping` and the `deassigned` indices are disjoint and
/// together are `0..mapping.len() + deassigned.len()`.
#[derive(Debug, Clone)]
pub(super) struct UniKeyMap<K> {
    mapping: HashMap<K, u32>,
    deassigned: Vec<u32>,
}

impl<K> Default for UniKeyMap<K> {
    fn default() -> Self {
        UniKeyMap { mapping: HashMap::default(), deassigned: Vec::new() }
    }
}

impl<K: Hash + Eq> UniKeyMap<K> {
    pub(super) fn len(&self) -> usize {
        self.mapping.len()
    }

    pub(super) fn contains_key(&self, key: &K) -> bool {
        self.mapping.contains_key(key)
    }

    /// A fresh index for `key`, reusing a freed one first. Panics if `key` has one.
    #[track_caller]
    pub(super) fn insert(&mut self, key: K) -> UniIndex {
        let idx = self.deassigned.pop().unwrap_or_else(|| {
            u32::try_from(self.mapping.len()).expect("UniMap ran out of useable keys")
        });
        if self.mapping.insert(key, idx).is_some() {
            panic!("this key is already assigned to a different index");
        }
        UniIndex { idx }
    }

    pub(super) fn get(&self, key: &K) -> Option<UniIndex> {
        self.mapping.get(key).map(|&idx| UniIndex { idx })
    }

    #[cfg(test)]
    pub(super) fn get_or_insert(&mut self, key: K) -> UniIndex {
        match self.get(&key) {
            Some(idx) => idx,
            None => self.insert(key),
        }
    }

    /// Frees `key`'s index. The caller removes the index from every `UniValMap` first, or a
    /// later key inherits its values.
    pub(super) fn remove(&mut self, key: &K) {
        if let Some(idx) = self.mapping.remove(key) {
            self.deassigned.push(idx);
        }
    }
}

/// From `UniIndex` to `V`.
#[derive(Debug, Clone, Eq)]
pub(super) struct UniValMap<V> {
    data: Vec<Option<V>>,
}

impl<V> Default for UniValMap<V> {
    fn default() -> Self {
        UniValMap { data: Vec::new() }
    }
}

impl<V: PartialEq> UniValMap<V> {
    /// Equality up to trailing `None`s: the same mapping. Miri: `UniValMap::equivalent`, which
    /// its `PartialEq` uses (the range map merges by it).
    pub(super) fn equivalent(&self, other: &Self) -> bool {
        let min_len = self.data.len().min(other.data.len());
        self.data[min_len..].iter().all(Option::is_none)
            && other.data[min_len..].iter().all(Option::is_none)
            && self.data[..min_len] == other.data[..min_len]
    }
}

impl<V: PartialEq> PartialEq for UniValMap<V> {
    fn eq(&self, other: &Self) -> bool {
        self.equivalent(other)
    }
}

impl<V> UniValMap<V> {
    fn extend_to_length(&mut self, len: usize) {
        if len > self.data.len() {
            self.data.resize_with(len, || None);
        }
    }

    pub(super) fn insert(&mut self, idx: UniIndex, val: V) {
        self.extend_to_length(idx.index() + 1);
        self.data[idx.index()] = Some(val);
    }

    #[inline]
    pub(super) fn get(&self, idx: UniIndex) -> Option<&V> {
        self.data.get(idx.index()).and_then(Option::as_ref)
    }

    #[inline]
    pub(super) fn get_mut(&mut self, idx: UniIndex) -> Option<&mut V> {
        self.data.get_mut(idx.index()).and_then(Option::as_mut)
    }

    pub(super) fn remove(&mut self, idx: UniIndex) -> Option<V> {
        self.data.get_mut(idx.index()).and_then(Option::take)
    }

    /// The value at `idx`, inserting `default` first if there is none. Miri:
    /// `UniValMap::entry(..).or_insert(..)`.
    #[inline]
    pub(super) fn or_insert(&mut self, idx: UniIndex, default: V) -> &mut V {
        self.extend_to_length(idx.index() + 1);
        self.data[idx.index()].get_or_insert(default)
    }

    #[cfg(test)]
    pub(super) fn raw_len(&self) -> usize {
        self.data.len()
    }

    #[cfg(test)]
    pub(super) fn extend_to_length_for_test(&mut self, len: usize) {
        self.extend_to_length(len);
    }
}
