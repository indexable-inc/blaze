//! A map sorted by key, kept in chunks shared by reference count: what a walk's nodes and a
//! kept `Built`'s rows are held in (docs/v2/plan/design/incremental-walk.md).
//!
//! The entries are in key order, cut into chunks of at most [`CHUNK`]; the directory is the
//! chunks in order. A lookup is two binary searches. An insert, a removal or a change touches
//! one chunk: it moves at most [`CHUNK`] entries, and the directory moves one pointer per
//! chunk after it only when a chunk is made or emptied. A copy of the map copies the
//! directory, a reference count a chunk; a chunk two maps share is copied by the first of
//! them that changes it. Iteration is in key order, the order a sorted `Vec` of the same
//! entries has, whatever sequence of inserts and removals made the map.

use std::{alloc::Global, hash::Hash, sync::Arc};

/// The most entries a chunk holds: an insert past it splits the chunk in two.
const CHUNK: usize = 64;

/// What a whole map's chunks are filled to, so the first inserts split nothing.
const FILL: usize = 48;

// clone: a chunk two maps share, copied by the one that changes it
#[derive(Clone)]
struct Chunk<K, V> {
    keys: Vec<K>,
    values: Vec<V>,
}

/// A map sorted by key ([`self`]).
pub(super) struct Rows<K, V> {
    /// In key order; none empty.
    chunks: Vec<Arc<Chunk<K, V>>>,
    len: usize,
}

impl<K, V> Default for Rows<K, V> {
    fn default() -> Self {
        // alloc: none, the directory is empty
        Rows { chunks: Vec::new(), len: 0 }
    }
}

impl<K, V> Clone for Rows<K, V> {
    fn clone(&self) -> Self {
        // clone: the directory, a reference count a chunk
        Rows { chunks: self.chunks.clone(), len: self.len }
    }
}

impl<K: Copy + Ord, V> Rows<K, V> {
    /// `entries`, which are in key order, each key once.
    pub(super) fn of_sorted(entries: impl Iterator<Item = (K, V)>) -> Self {
        let mut rows = Rows::default();
        // alloc: per map made whole, a chunk per `FILL` entries
        let mut chunk = Chunk { keys: Vec::with_capacity(CHUNK), values: Vec::with_capacity(CHUNK) };
        for (key, value) in entries {
            debug_assert!(
                chunk.keys.last().is_none_or(|last| *last < key)
                    && rows.chunks.last().is_none_or(|it: &Arc<Chunk<K, V>>| {
                        it.keys.last().is_some_and(|last| *last < key)
                    }),
                "in key order"
            );
            chunk.keys.push(key);
            chunk.values.push(value);
            rows.len += 1;
            if chunk.keys.len() == FILL {
                // alloc: as above
                let next =
                    Chunk { keys: Vec::with_capacity(CHUNK), values: Vec::with_capacity(CHUNK) };
                // alloc: a chunk's reference count
                rows.chunks.push(Arc::new(std::mem::replace(&mut chunk, next)));
            }
        }
        if !chunk.keys.is_empty() {
            // alloc: a chunk's reference count
            rows.chunks.push(Arc::new(chunk));
        }
        rows
    }

    /// The entries of `map` by key.
    pub(super) fn of(map: blaze_hash::HashMap<K, V, Global>) -> Self
    where
        K: Hash,
    {
        // alloc: per map made whole, sized by it
        let mut entries: Vec<Option<(K, V)>> = map.into_iter().map(Some).collect();
        // alloc: per map made whole, sized by it
        let mut order: Vec<u32> = (0..entries.len() as u32).collect();
        assert!(entries.len() <= u32::MAX as usize, "more than 2^32 entries");
        // Keys are unique: an unstable sort is the order.
        order.sort_unstable_by_key(|&at| entries[at as usize].as_ref().expect("not taken yet").0);
        let sorted: Vec<(K, V)> = order
            .into_iter()
            .map(|at| entries[at as usize].take().expect("each entry once"))
            // alloc: per map made whole, sized by it
            .collect();
        Rows::of_sorted(sorted.into_iter())
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The chunk whose keys `key` is among, if it is in the map: the last chunk whose first
    /// key is not after it.
    fn chunk_of(&self, key: &K) -> Option<usize> {
        self.chunks.partition_point(|chunk| chunk.keys[0] <= *key).checked_sub(1)
    }

    pub(super) fn get(&self, key: &K) -> Option<&V> {
        let chunk = &self.chunks[self.chunk_of(key)?];
        chunk.keys.binary_search(key).ok().map(|at| &chunk.values[at])
    }

    pub(super) fn contains(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// In key order.
    pub(super) fn iter(&self) -> impl Iterator<Item = (&K, &V)> + Clone {
        self.chunks.iter().flat_map(|chunk| chunk.keys.iter().zip(&chunk.values))
    }

    /// In key order.
    pub(super) fn keys(&self) -> impl Iterator<Item = &K> + Clone {
        self.chunks.iter().flat_map(|chunk| chunk.keys.iter())
    }

    /// In key order.
    pub(super) fn values(&self) -> impl Iterator<Item = &V> + Clone {
        self.chunks.iter().flat_map(|chunk| chunk.values.iter())
    }
}

impl<K: Copy + Ord, V: Clone> Rows<K, V> {
    pub(super) fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let chunk = self.chunk_of(key)?;
        let at = self.chunks[chunk].keys.binary_search(key).ok()?;
        Some(&mut Arc::make_mut(&mut self.chunks[chunk]).values[at])
    }

    /// Sets the entry of `key`: gives the value it had, if it had one.
    pub(super) fn insert(&mut self, key: K, value: V) -> Option<V> {
        let Some(first) = self.chunks.first() else {
            // alloc: the map's first chunk
            let chunk = Chunk { keys: vec![key], values: vec![value] };
            // alloc: a chunk's reference count
            self.chunks.push(Arc::new(chunk));
            self.len = 1;
            return None;
        };
        // A key before every chunk's first goes at the front of the first.
        let which = if key < first.keys[0] { 0 } else { self.chunk_of(&key).unwrap_or(0) };
        let chunk = Arc::make_mut(&mut self.chunks[which]);
        let at = match chunk.keys.binary_search(&key) {
            Ok(at) => return Some(std::mem::replace(&mut chunk.values[at], value)),
            Err(at) => at,
        };
        chunk.keys.insert(at, key);
        chunk.values.insert(at, value);
        self.len += 1;
        if chunk.keys.len() > CHUNK {
            let half = chunk.keys.len() / 2;
            // alloc: a chunk split in two, once per `CHUNK / 2` inserts into it
            let after = Chunk { keys: chunk.keys.split_off(half), values: chunk.values.split_off(half) };
            // alloc: a chunk's reference count
            self.chunks.insert(which + 1, Arc::new(after));
        }
        None
    }

    /// Takes the entry of `key` out: gives its value, if it had one.
    pub(super) fn remove(&mut self, key: &K) -> Option<V> {
        let which = self.chunk_of(key)?;
        let at = self.chunks[which].keys.binary_search(key).ok()?;
        self.len -= 1;
        if self.chunks[which].keys.len() == 1 {
            let chunk = self.chunks.remove(which);
            return Some(match Arc::try_unwrap(chunk) {
                Ok(mut chunk) => chunk.values.remove(0),
                // clone: the last value of a chunk another map still holds
                Err(shared) => shared.values[0].clone(),
            });
        }
        let chunk = Arc::make_mut(&mut self.chunks[which]);
        chunk.keys.remove(at);
        Some(chunk.values.remove(at))
    }

    /// In key order. Every chunk another map shares is copied: for a map made whole, which
    /// shares none.
    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        self.chunks.iter_mut().flat_map(|chunk| {
            let chunk = Arc::make_mut(chunk);
            chunk.keys.iter().zip(chunk.values.iter_mut())
        })
    }

    /// In key order; as [`Self::iter_mut`].
    pub(super) fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.iter_mut().map(|(_, value)| value)
    }
}

impl<K: Copy + Ord, V> std::ops::Index<&K> for Rows<K, V> {
    type Output = V;

    fn index(&self, key: &K) -> &V {
        self.get(key).expect("an entry of the map")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use hegel::generators as gs;

    use super::*;

    /// The refinement: after any sequence of inserts, removals and changes, a map in chunks
    /// holds what a `BTreeMap` given the same sequence holds, in the same order, and each
    /// call gave what the `BTreeMap`'s gave; a copy taken midway holds what the map held then.
    #[hegel::test]
    fn rows_hold_what_a_sorted_map_holds(tc: hegel::TestCase) {
        let count: usize = tc.draw(gs::integers::<usize>().max_value(600));
        let start: Vec<u16> = tc.draw(gs::vecs(gs::integers::<u16>().max_value(400)).max_size(300));
        let mut model: BTreeMap<u16, u32> = start.iter().map(|key| (*key, u32::from(*key))).collect();
        let mut rows = Rows::of_sorted(model.iter().map(|(key, value)| (*key, *value)));
        let mut copy = None;
        for step in 0..count {
            let key: u16 = tc.draw(gs::integers::<u16>().max_value(400));
            match tc.draw(gs::integers::<u8>().max_value(3)) {
                0 => assert_eq!(rows.remove(&key), model.remove(&key)),
                1 => {
                    if let Some(value) = rows.get_mut(&key) {
                        *value += 1;
                    }
                    if let Some(value) = model.get_mut(&key) {
                        *value += 1;
                    }
                }
                _ => assert_eq!(rows.insert(key, step as u32), model.insert(key, step as u32)),
            }
            assert_eq!(rows.get(&key), model.get(&key));
            if step == count / 2 {
                // clone: the test's copy of the map and its model
                copy = Some((rows.clone(), model.clone()));
            }
        }
        let held = |rows: &Rows<u16, u32>| -> Vec<(u16, u32)> {
            rows.iter().map(|(key, value)| (*key, *value)).collect()
        };
        assert_eq!(held(&rows), model.iter().map(|(key, value)| (*key, *value)).collect::<Vec<_>>());
        assert_eq!(rows.len(), model.len());
        assert!(rows.chunks.iter().all(|it| !it.keys.is_empty() && it.keys.len() <= CHUNK));
        if let Some((rows, model)) = copy {
            let model: Vec<(u16, u32)> = model.into_iter().collect();
            assert_eq!(held(&rows), model, "a copy holds what the map held when it was taken");
        }
    }

    /// The control of [`rows_hold_what_a_sorted_map_holds`]: a key before every chunk's first
    /// is found only because an insert puts it in the first chunk; one put after the chunk
    /// the search names (the planted bug: appended to the last chunk) is out of order.
    #[test]
    fn the_oracle_catches_a_key_put_out_of_order() {
        let mut rows = Rows::of_sorted((10u16..200).map(|key| (key, 0u32)));
        assert_eq!(rows.insert(3, 1), None);
        assert_eq!(rows.keys().next(), Some(&3));
        assert!(rows.keys().is_sorted());
        let last = rows.chunks.len() - 1;
        let chunk = Arc::make_mut(&mut rows.chunks[last]);
        chunk.keys.push(5);
        chunk.values.push(1);
        assert!(!rows.keys().is_sorted(), "the planted bug leaves the keys out of order");
        assert_eq!(rows.get(&5), None, "and its entry is not found");
    }

    /// An insert or a removal copies the one chunk it changes: every other chunk stays the
    /// one the copy holds.
    #[test]
    fn a_change_copies_one_chunk() {
        let rows = Rows::of_sorted((0u32..10_000).map(|key| (key, key)));
        let mut changed = rows.clone();
        changed.insert(20_000, 1);
        changed.remove(&5_000);
        *changed.get_mut(&7).expect("an entry") = 9;
        let shared = (rows.chunks.iter())
            .filter(|chunk| changed.chunks.iter().any(|it| Arc::ptr_eq(it, chunk)))
            .count();
        assert_eq!(shared, rows.chunks.len() - 3, "three chunks changed, the rest are shared");
        assert_eq!(rows.get(&5_000), Some(&5_000), "the copy holds what it held");
    }
}
