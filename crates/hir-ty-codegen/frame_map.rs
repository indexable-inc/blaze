//! The maps and sets one function's build keeps while it runs, in its frame
//! ([`blaze_mem::frame::task`]): dropped with the frame when the build returns, so a warm build
//! asks the global allocator for none of them.

use std::{marker::PhantomData, ops::Index};

use blaze_hash::FixedState;
use blaze_mem::Frame;
use la_arena::{Idx, RawIdx};

/// An `HashMap` in a frame. Only for a map nothing iterates where the order shows: its order
/// is the table's.
pub(crate) type FrameHashMap<'f, K, V> = hashbrown::HashMap<K, V, FixedState, &'f Frame>;

/// An `HashSet` in a frame, as [`FrameHashMap`].
pub(crate) type FrameHashSet<'f, T> = hashbrown::HashSet<T, FixedState, &'f Frame>;

pub(crate) fn hash_map<'f, K, V>(frame: &'f Frame) -> FrameHashMap<'f, K, V> {
    hashbrown::HashMap::with_hasher_in(FixedState, frame)
}

pub(crate) fn hash_map_with_capacity<'f, K, V>(capacity: usize, frame: &'f Frame) -> FrameHashMap<'f, K, V> {
    hashbrown::HashMap::with_capacity_and_hasher_in(capacity, FixedState, frame)
}

pub(crate) fn hash_set<'f, T>(frame: &'f Frame) -> FrameHashSet<'f, T> {
    hashbrown::HashSet::with_hasher_in(FixedState, frame)
}

/// A value for some of an arena's indices, by index, in a frame: `la_arena::ArenaMap`'s layout
/// (one `Option<V>` a slot, up to the largest index given) and its answers. [`FrameMap::keys`]
/// is in index order.
pub(crate) struct FrameMap<'f, K, V> {
    slots: Vec<Option<V>, &'f Frame>,
    len: usize,
    key: PhantomData<fn() -> K>,
}

impl<'f, T, V> FrameMap<'f, Idx<T>, V> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        Self::with_capacity_in(0, frame)
    }

    pub(crate) fn with_capacity_in(capacity: usize, frame: &'f Frame) -> Self {
        FrameMap { slots: Vec::with_capacity_in(capacity, frame), len: 0, key: PhantomData }
    }

    fn slot(idx: Idx<T>) -> usize {
        idx.into_raw().into_u32() as usize
    }

    /// Gives `idx` `value`, returning the value it had.
    pub(crate) fn insert(&mut self, idx: Idx<T>, value: V) -> Option<V> {
        let at = Self::slot(idx);
        if at >= self.slots.len() {
            self.slots.resize_with(at + 1, || None);
        }
        let old = self.slots[at].replace(value);
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    pub(crate) fn get(&self, idx: Idx<T>) -> Option<&V> {
        self.slots.get(Self::slot(idx)).and_then(Option::as_ref)
    }

    pub(crate) fn contains_idx(&self, idx: Idx<T>) -> bool {
        self.get(idx).is_some()
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The indices with a value, in index order.
    pub(crate) fn keys(&self) -> impl Iterator<Item = Idx<T>> + '_ {
        let given = self.slots.iter().enumerate().filter(|(_, it)| it.is_some());
        given.map(|(at, _)| Idx::from_raw(RawIdx::from(at as u32)))
    }
}

impl<T, V> Index<Idx<T>> for FrameMap<'_, Idx<T>, V> {
    type Output = V;

    #[track_caller]
    fn index(&self, idx: Idx<T>) -> &V {
        match self.get(idx) {
            Some(it) => it,
            None => panic!("no value for index {}", Self::slot(idx)),
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_mem::{Tag, frame};
    use la_arena::{Arena, ArenaMap};

    use super::*;

    /// Each index's value is the last one given, and `keys` is in index order whatever order
    /// they came in: what `ArenaMap` answers.
    #[test]
    fn a_frame_map_answers_as_an_arena_map() {
        let mut arena = Arena::new();
        let ids: Vec<Idx<u32>> = (0..64).map(|it| arena.alloc(it)).collect();
        frame::task(Tag::Codegen, |frame| {
            let mut map = FrameMap::new_in(frame);
            let mut reference = ArenaMap::default();
            for (step, &id) in ids.iter().rev().step_by(3).enumerate() {
                assert_eq!(map.insert(id, step), reference.insert(id, step));
            }
            assert_eq!(map.insert(ids[3], 99), reference.insert(ids[3], 99));
            for &id in &ids {
                assert_eq!(map.get(id), reference.get(id));
                assert_eq!(map.contains_idx(id), reference.contains_idx(id));
            }
            let keys: Vec<Idx<u32>> = map.keys().collect();
            let expected: Vec<Idx<u32>> = reference.iter().map(|(id, _)| id).collect();
            assert_eq!(keys, expected);
            assert_eq!(map.len(), expected.len());
            assert_eq!(map[ids[3]], 99);
        });
    }

    /// A warm unit's maps and sets take nothing of the global allocator.
    #[test]
    fn a_warm_frame_map_allocates_nothing() {
        let mut arena = Arena::new();
        let ids: Vec<Idx<u32>> = (0..256).map(|it| arena.alloc(it)).collect();
        let fill = |frame: &Frame| {
            let mut map = FrameMap::with_capacity_in(ids.len(), frame);
            let mut set = hash_set(frame);
            let mut by = hash_map(frame);
            for (at, &id) in ids.iter().enumerate() {
                map.insert(id, at);
                set.insert(id);
                by.insert(id, at);
            }
            (map.len(), map.keys().count(), set.len(), by.len())
        };
        let cold = frame::task(Tag::Codegen, &fill);
        let warm = blaze_alloc_count::assert_at_most(0, "a warm unit's frame maps and sets", || {
            frame::task(Tag::Codegen, &fill)
        });
        assert_eq!(cold, warm);
    }
}
