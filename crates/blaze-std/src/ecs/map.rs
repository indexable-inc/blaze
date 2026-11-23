//! Self-contained, deterministic, `#![forbid(unsafe_code)]` sorted flat map and set over
//! `blaze_own::Vec`. Zero third-party dependencies, zero panics, zero integer casts, zero clones.

use core::alloc::Allocator;
use core::borrow::Borrow;

use blaze_own::Vec as OwnVec;

use super::error::EcsError;

/// Deterministic sorted key-value map over a contiguous `blaze_own::Vec<(K, V), A>`.
pub(crate) struct FlatMap<K, V, A: Allocator + Copy> {
    entries: OwnVec<(K, V), A>,
}

impl<K, V, A: Allocator + Copy> FlatMap<K, V, A> {
    /// Construct an empty map in `alloc`.
    #[inline]
    pub(crate) fn new_in(alloc: A) -> Self {
        Self { entries: OwnVec::new_in(alloc) }
    }

    /// Number of stored entries.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Look up a value by borrowed key `Q` in O(log N) time with zero allocation.
    #[inline]
    pub(crate) fn get<Q: Ord + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        let idx = self.entries.as_slice().binary_search_by(|(k, _)| k.borrow().cmp(key)).ok()?;
        self.entries.get(idx).map(|(_, v)| v)
    }

    /// Mutably look up a value by borrowed key `Q` in O(log N) time.
    #[inline]
    #[cfg(test)]
    pub(crate) fn get_mut<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        let idx = self.entries.as_slice().binary_search_by(|(k, _)| k.borrow().cmp(key)).ok()?;
        self.entries.get_mut(idx).map(|(_, v)| v)
    }

    /// Insert or replace `(key, value)` maintaining strict key order.
    pub(crate) fn insert(&mut self, key: K, value: V) -> Result<Option<V>, EcsError>
    where
        K: Ord,
    {
        match self.entries.as_slice().binary_search_by(|(k, _)| k.cmp(&key)) {
            Ok(idx) => {
                if let Some(entry) = self.entries.get_mut(idx) {
                    let prev = core::mem::replace(&mut entry.1, value);
                    Ok(Some(prev))
                } else {
                    Ok(None)
                }
            }
            Err(idx) => {
                self.entries.try_push((key, value)).map_err(|r| EcsError::Alloc(r.why))?;
                if let Some(tail) = self.entries.as_mut_slice().get_mut(idx..) {
                    if !tail.is_empty() {
                        tail.rotate_right(1);
                    }
                }
                Ok(None)
            }
        }
    }

    /// Remove and return the value associated with `key`, if present.
    pub(crate) fn remove<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let idx = self.entries.as_slice().binary_search_by(|(k, _)| k.borrow().cmp(key)).ok()?;
        if let Some(tail) = self.entries.as_mut_slice().get_mut(idx..) {
            if !tail.is_empty() {
                tail.rotate_left(1);
            }
        }
        self.entries.pop().map(|(_, v)| v)
    }

    /// Iterate over stored `(K, V)` pairs in deterministic sorted key order.
    #[inline]
    #[cfg(test)]
    pub(crate) fn iter(&self) -> core::slice::Iter<'_, (K, V)> {
        self.entries.as_slice().iter()
    }
}

/// Deterministic sorted key set over a contiguous `blaze_own::Vec<K, A>`.
pub(crate) struct FlatSet<K, A: Allocator + Copy> {
    entries: OwnVec<K, A>,
}

impl<K, A: Allocator + Copy> FlatSet<K, A> {
    /// Construct an empty set in `alloc`.
    #[inline]
    pub(crate) fn new_in(alloc: A) -> Self {
        Self { entries: OwnVec::new_in(alloc) }
    }

    /// Check membership in O(log N) time with zero allocation.
    #[inline]
    pub(crate) fn contains<Q: Ord + ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.entries.as_slice().binary_search_by(|k| k.borrow().cmp(key)).is_ok()
    }

    /// Insert `key` maintaining strict key order. Returns `Ok(true)` if newly inserted.
    pub(crate) fn insert(&mut self, key: K) -> Result<bool, EcsError>
    where
        K: Ord,
    {
        match self.entries.as_slice().binary_search_by(|k| k.cmp(&key)) {
            Ok(_) => Ok(false),
            Err(idx) => {
                self.entries.try_push(key).map_err(|r| EcsError::Alloc(r.why))?;
                if let Some(tail) = self.entries.as_mut_slice().get_mut(idx..) {
                    if !tail.is_empty() {
                        tail.rotate_right(1);
                    }
                }
                Ok(true)
            }
        }
    }
}
