//! [`Map`]: text keys, unique, in insertion order, with a sorted view.
//!
//! Entries are one column in insertion order; beside it, `order` holds the entries' indices
//! sorted by key bytes (which is code-point order), searched by halving. So a lookup is
//! O(log n), the sorted view costs nothing to read, and an insert moves only `usize`s of the
//! index. Writers choose which order they write and say so: JSON's default and deterministic
//! CBOR want the sorted one (CBOR's by encoded key, which a writer derives from this one), TOML
//! and a manifest built in code want insertion order.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt, mem};

use blaze_own::{
    String, Vec,
    planted::{NoPlant, Planted},
};

use crate::{DataPlant, Value};

pub struct Map<A: Allocator> {
    entries: Vec<(String<A>, Value<A>), A>,
    /// Indices into `entries`, sorted by key.
    order: Vec<usize, A>,
}

/// An insert refused because the key is there: the entry handed back, and where the key was
/// first (its index in insertion order).
pub struct Taken<A: Allocator> {
    pub key: String<A>,
    pub value: Value<A>,
    pub first: usize,
}

impl<A: Allocator> Map<A> {
    pub fn new_in(alloc: A) -> Map<A>
    where
        A: Clone,
    {
        Map { entries: Vec::new_in(alloc.clone()), order: Vec::new_in(alloc) }
    }

    pub fn with_capacity_in(capacity: usize, alloc: A) -> Map<A>
    where
        A: Clone,
    {
        Map {
            entries: Vec::with_capacity_in(capacity, alloc.clone()),
            order: Vec::with_capacity_in(capacity, alloc),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `Ok(slot in order)` for a present key, `Err(slot to insert at)` for a missing one.
    fn find(&self, key: &str) -> Result<usize, usize> {
        self.order.binary_search_by(|&i| self.entries[i].0.as_str().cmp(key))
    }

    pub fn get(&self, key: &str) -> Option<&Value<A>> {
        self.find(key).ok().map(|slot| &self.entries[self.order[slot]].1)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value<A>> {
        match self.find(key) {
            Ok(slot) => {
                let index = self.order[slot];
                Some(&mut self.entries[index].1)
            }
            Err(_) => None,
        }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.find(key).is_ok()
    }

    /// The insertion index of `key`'s entry.
    pub fn position(&self, key: &str) -> Option<usize> {
        self.find(key).ok().map(|slot| self.order[slot])
    }

    /// `key` set to `value`: a new key goes last in insertion order; a present one keeps its
    /// place and its old value is handed back.
    pub fn insert(&mut self, key: String<A>, value: Value<A>) -> Option<Value<A>> {
        self.insert_planted(key, value, NoPlant)
    }

    /// [`Map::insert`] with a control's plant: [`DataPlant::DuplicateOnHit`] adds a present key
    /// a second time. Shipped code calls [`Map::insert`].
    pub fn insert_planted(
        &mut self,
        key: String<A>,
        value: Value<A>,
        plant: impl Planted<DataPlant>,
    ) -> Option<Value<A>> {
        match self.find(key.as_str()) {
            Ok(slot) if !plant.has(DataPlant::DuplicateOnHit) => {
                let index = self.order[slot];
                Some(mem::replace(&mut self.entries[index].1, value))
            }
            Ok(slot) | Err(slot) => {
                self.order.insert(slot, self.entries.len());
                self.entries.push((key, value));
                None
            }
        }
    }

    /// `key` set to `value` only if it is not there: the readers' duplicate check.
    pub fn insert_new(&mut self, key: String<A>, value: Value<A>) -> Result<(), Taken<A>> {
        match self.find(key.as_str()) {
            Ok(slot) => Err(Taken { key, value, first: self.order[slot] }),
            Err(slot) => {
                self.order.insert(slot, self.entries.len());
                self.entries.push((key, value));
                Ok(())
            }
        }
    }

    /// The entry under `key` taken out; later entries move up one place in insertion order.
    pub fn remove(&mut self, key: &str) -> Option<Value<A>> {
        let slot = self.find(key).ok()?;
        let index = self.order.remove(slot);
        for i in self.order.iter_mut() {
            if *i > index {
                *i -= 1;
            }
        }
        Some(self.entries.remove(index).1)
    }

    /// Entries in insertion order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &Value<A>)> + '_ {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Values in insertion order, mutably.
    pub fn values_mut(&mut self) -> impl ExactSizeIterator<Item = (&str, &mut Value<A>)> + '_ {
        self.entries.iter_mut().map(|(k, v)| (k.as_str(), v))
    }

    /// Entries sorted by key bytes (code-point order).
    pub fn sorted(&self) -> impl ExactSizeIterator<Item = (&str, &Value<A>)> + '_ {
        self.order.iter().map(|&i| {
            let (k, v) = &self.entries[i];
            (k.as_str(), v)
        })
    }

    /// The entry at `index` in insertion order.
    pub fn entry_at(&self, index: usize) -> Option<(&str, &Value<A>)> {
        self.entries.get(index).map(|(k, v)| (k.as_str(), v))
    }

    /// The entry at `index` in the sorted view: a walk with its own stack keeps an index, not
    /// an iterator.
    pub fn sorted_at(&self, index: usize) -> Option<(&str, &Value<A>)> {
        let &i = self.order.get(index)?;
        let (k, v) = &self.entries[i];
        Some((k.as_str(), v))
    }

    /// All entries taken out, leaving this map empty in the same allocator.
    pub fn take(&mut self) -> Map<A>
    where
        A: Clone,
    {
        Map { entries: self.entries.split_off(0), order: self.order.split_off(0) }
    }

    /// The last entry in insertion order taken out, the sorted index dropped: only for a drop,
    /// which takes every entry and needs no lookup meanwhile.
    pub(crate) fn pop_for_drop(&mut self) -> Option<(String<A>, Value<A>)> {
        self.order.clear();
        self.entries.pop()
    }

    /// An entry put back during a drop, not indexed (see `pop_for_drop`).
    pub(crate) fn push_for_drop(&mut self, entry: (String<A>, Value<A>)) {
        self.entries.push(entry);
    }

    pub fn copy_in<B: Allocator + Clone>(&self, alloc: B) -> Map<B> {
        let mut entries = Vec::with_capacity_in(self.entries.len(), alloc.clone());
        for (k, v) in self.entries.iter() {
            entries.push((String::from_str_in(k.as_str(), alloc.clone()), v.copy_in(alloc.clone())));
        }
        let mut order = Vec::with_capacity_in(self.order.len(), alloc);
        order.extend_from_slice(self.order.as_slice());
        Map { entries, order }
    }
}

impl<A: Allocator + Clone> Clone for Map<A> {
    fn clone(&self) -> Map<A> {
        Map { entries: self.entries.clone(), order: self.order.clone() }
    }
}

/// By content: the same keys with equal values, whatever the insertion orders.
impl<A: Allocator, B: Allocator> PartialEq<Map<B>> for Map<A> {
    fn eq(&self, other: &Map<B>) -> bool {
        self.len() == other.len()
            && self.sorted().zip(other.sorted()).all(|((ka, va), (kb, vb))| ka == kb && va == vb)
    }
}

impl<A: Allocator> Eq for Map<A> {}

impl<A: Allocator> fmt::Debug for Map<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (i, (k, v)) in self.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{k:?}: {v:?}")?;
        }
        f.write_str("}")
    }
}
