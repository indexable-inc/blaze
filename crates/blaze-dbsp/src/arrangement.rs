use std::hash::Hash;

use blaze_hash::{HashMap, HashSet};

use crate::Weight;

/// A weighted index K -> values, each value with a sort key; `get` yields values with positive
/// weight in sort-key order. Records which keys changed since the last `take_changed`.
///
/// A value is the pair (sort key, value): an update of `v` under another sort key is another
/// row, as in differential dataflow, so a move is `-1` at the old key and `+1` at the new one.
/// Rows of equal sort key keep the order they arrived in; a caller whose order must not depend
/// on history gives each value its own sort key (a [`crate::Pos`]).
///
/// A key's rows are one `Vec` in sort order, so [`Arrangement::get`] is one probe and a scan of
/// contiguous rows; an update is a binary search and, for a new or vanished row, a shift of the
/// rows after it, as the slice published for the key is rewritten whole anyway. A key whose rows
/// all cancel goes on a free list with its buffer, which the next new key takes.
#[derive(Debug)]
pub struct Arrangement<K, S, V> {
    slot: HashMap<K, u32>,
    rows: Vec<KeyRows<K, S, V>>,
    free: Vec<u32>,
    changed: Vec<K>,
    changed_set: HashSet<K>,
}

#[derive(Debug)]
struct KeyRows<K, S, V> {
    key: K,
    entries: Vec<Entry<S, V>>,
    /// How many entries have a positive weight.
    positive: u32,
    live: bool,
}

#[derive(Debug)]
struct Entry<S, V> {
    sort: S,
    value: V,
    weight: Weight,
}

impl<K: Copy + Eq + Hash, S: Ord + Clone, V: Copy + Eq + Hash> Default for Arrangement<K, S, V> {
    fn default() -> Self {
        Arrangement::new()
    }
}

impl<K: Copy + Eq + Hash, S: Ord + Clone, V: Copy + Eq + Hash> Arrangement<K, S, V> {
    pub fn new() -> Self {
        Arrangement {
            slot: HashMap::default(),
            rows: Vec::new(), // alloc: setup: an empty buffer, grown once and kept
            free: Vec::new(), // alloc: setup: an empty buffer, grown once and kept
            changed: Vec::new(), // alloc: setup: an empty buffer, grown once and kept
            changed_set: HashSet::default(),
        }
    }

    /// Adds `w` to the weight of (`sort`, `value`) under `key`. The key counts as changed when
    /// the values [`Arrangement::get`] yields change: a row's weight crossing between positive
    /// and not.
    pub fn update(&mut self, key: K, sort: S, value: V, w: Weight) {
        if w == 0 {
            return;
        }
        let at = self.intern(key);
        let rows = &mut self.rows[at as usize];
        let start = rows.entries.partition_point(|entry| entry.sort < sort);
        let run = rows.entries[start..].iter().take_while(|entry| entry.sort == sort).count();
        let found = rows.entries[start..start + run].iter().position(|entry| entry.value == value);
        let (before, after) = match found {
            Some(offset) => {
                let entry = &mut rows.entries[start + offset];
                let before = entry.weight;
                entry.weight = before.checked_add(w).expect("an arrangement weight overflows `i32`");
                let after = entry.weight;
                if after == 0 {
                    rows.entries.remove(start + offset);
                }
                (before, after)
            }
            None => {
                rows.entries.insert(start + run, Entry { sort, value, weight: w });
                (0, w)
            }
        };
        let visible = match (before > 0, after > 0) {
            (false, true) => {
                rows.positive += 1;
                true
            }
            (true, false) => {
                rows.positive -= 1;
                true
            }
            _ => false,
        };
        let empty = rows.entries.is_empty();
        if visible && self.changed_set.insert(key) {
            self.changed.push(key);
        }
        if empty {
            self.release(at);
        }
    }

    /// The values of `key` with positive weight, in sort-key order: one probe, no allocation.
    pub fn get(&self, key: K) -> impl Iterator<Item = V> + '_ {
        self.entries_of(key).iter().filter(|entry| entry.weight > 0).map(|entry| entry.value)
    }

    /// [`Arrangement::get`] with each value's sort key and weight.
    pub fn rows(&self, key: K) -> impl Iterator<Item = (&S, V, Weight)> + '_ {
        self.entries_of(key)
            .iter()
            .filter(|entry| entry.weight > 0)
            .map(|entry| (&entry.sort, entry.value, entry.weight))
    }

    /// How many values [`Arrangement::get`] yields for `key`.
    pub fn count(&self, key: K) -> usize {
        match self.slot.get(&key) {
            Some(&at) => self.rows[at as usize].positive as usize,
            None => 0,
        }
    }

    /// Whether `key` has a value with positive weight.
    pub fn contains_key(&self, key: K) -> bool {
        self.count(key) > 0
    }

    /// The keys with a value of positive weight, in no particular order.
    pub fn keys(&self) -> impl Iterator<Item = K> + '_ {
        self.rows.iter().filter(|rows| rows.live && rows.positive > 0).map(|rows| rows.key)
    }

    /// Each key changed since the last call, once, in the order each first changed; then
    /// forgets them. `out` is appended to.
    pub fn take_changed(&mut self, out: &mut Vec<K>) {
        out.extend(self.changed.drain(..));
        self.changed_set.clear();
    }

    fn entries_of(&self, key: K) -> &[Entry<S, V>] {
        match self.slot.get(&key) {
            Some(&at) => &self.rows[at as usize].entries,
            None => &[],
        }
    }

    fn intern(&mut self, key: K) -> u32 {
        if let Some(&at) = self.slot.get(&key) {
            return at;
        }
        let at = match self.free.pop() {
            Some(at) => {
                let rows = &mut self.rows[at as usize];
                rows.key = key;
                rows.live = true;
                at
            }
            None => {
                let at = u32::try_from(self.rows.len()).expect("more than `u32::MAX` keys");
                self.rows.push(KeyRows { key, entries: Vec::new(), positive: 0, live: true }); // alloc: a key first seen; an emptied key's row and buffer are reused
                at
            }
        };
        self.slot.insert(key, at);
        at
    }

    fn release(&mut self, at: u32) {
        let rows = &mut self.rows[at as usize];
        debug_assert_eq!(rows.positive, 0);
        rows.live = false;
        self.slot.remove(&rows.key);
        self.free.push(at);
    }
}
