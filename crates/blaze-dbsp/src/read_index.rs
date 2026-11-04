use std::hash::Hash;

use blaze_hash::HashMap;
use smallvec::SmallVec;
use std::alloc::Global;

/// [`ReadIndex::forget_many`] sweeps once the readers it forgets hold at least one in this many
/// of the edges: a one-at-a-time forget costs about three probes at random per edge it drops,
/// a sweep a pass over every edge and a probe per edge left in a key row that lost some.
const SWEEP_SHARE: usize = 4;

/// Readers of keys, both ways: what woke whom. K: the key read ((module, name), negative reads
/// included); R: a reader handle. Adding is idempotent; `forget(r)` drops every key of `r`.
///
/// Each side interns its handles to dense `u32` rows, each row an inline list of the other
/// side's handles; a pair map gives each edge's place in both lists, so adding, and removing in
/// [`ReadIndex::forget`], are `O(1)` (a swap-remove and one fix-up per side). A row left empty
/// goes on a free list with its buffer, which the next new handle takes.
#[derive(Debug)]
pub struct ReadIndex<K, R> {
    by_key: Side<K, R>,
    by_reader: Side<R, K>,
    /// (key row, reader row) -> (place in the key's list, place in the reader's list).
    pairs: HashMap<(u32, u32), (u32, u32), Global>,
}

#[derive(Debug)]
struct Side<A, B> {
    slot: HashMap<A, u32, Global>,
    rows: Vec<Row<A, B>>,
    free: Vec<u32>,
}

#[derive(Debug)]
struct Row<A, B> {
    owner: A,
    /// The other side's handle and its row.
    items: SmallVec<[(B, u32); 4]>,
}

impl<A: Copy + Eq + Hash, B: Copy> Side<A, B> {
    fn new() -> Self {
        Side { slot: HashMap::new_in(Global), rows: Vec::new(), free: Vec::new() } // alloc: setup: empty buffers, grown once and kept
    }

    fn intern(&mut self, a: A) -> u32 {
        if let Some(&at) = self.slot.get(&a) {
            return at;
        }
        let at = match self.free.pop() {
            Some(at) => {
                self.rows[at as usize].owner = a;
                at
            }
            None => {
                let at = u32::try_from(self.rows.len()).expect("more than `u32::MAX` handles");
                self.rows.push(Row { owner: a, items: SmallVec::new() });
                at
            }
        };
        self.slot.insert(a, at);
        at
    }

    fn items(&self, a: A) -> &[(B, u32)] {
        match self.slot.get(&a) {
            Some(&at) => &self.rows[at as usize].items,
            None => &[],
        }
    }

    /// Swap-removes item `at` of row `row`: the row of the item moved into its place, if any.
    fn remove_at(&mut self, row: u32, at: u32) -> Option<u32> {
        let items = &mut self.rows[row as usize].items;
        items.swap_remove(at as usize);
        items.get(at as usize).map(|moved| moved.1)
    }

    fn release(&mut self, row: u32) {
        let entry = &mut self.rows[row as usize];
        debug_assert!(entry.items.is_empty());
        self.slot.remove(&entry.owner);
        self.free.push(row);
    }
}

impl<K: Copy + Eq + Hash, R: Copy + Eq + Hash> Default for ReadIndex<K, R> {
    fn default() -> Self {
        ReadIndex::new()
    }
}

impl<K: Copy + Eq + Hash, R: Copy + Eq + Hash> ReadIndex<K, R> {
    pub fn new() -> Self {
        ReadIndex { by_key: Side::new(), by_reader: Side::new(), pairs: HashMap::new_in(Global) }
    }

    pub fn add(&mut self, key: K, reader: R) {
        let k = self.by_key.intern(key);
        let r = self.by_reader.intern(reader);
        let blaze_hash::hash_map::Entry::Vacant(edge) = self.pairs.entry((k, r)) else {
            return;
        };
        let key_items = &mut self.by_key.rows[k as usize].items;
        let in_key = key_items.len() as u32;
        key_items.push((reader, r));
        let reader_items = &mut self.by_reader.rows[r as usize].items;
        let in_reader = reader_items.len() as u32;
        reader_items.push((key, k));
        edge.insert((in_key, in_reader));
    }

    pub fn forget(&mut self, reader: R) {
        let Some(&r) = self.by_reader.slot.get(&reader) else { return };
        let items = std::mem::take(&mut self.by_reader.rows[r as usize].items);
        for &(_, k) in &items {
            let (in_key, _) =
                self.pairs.remove(&(k, r)).expect("a read index edge without its pair");
            if let Some(moved) = self.by_key.remove_at(k, in_key) {
                self.pairs.get_mut(&(k, moved)).expect("a read index edge without its pair").0 =
                    in_key;
            }
            if self.by_key.rows[k as usize].items.is_empty() {
                self.by_key.release(k);
            }
        }
        let row = &mut self.by_reader.rows[r as usize];
        row.items = items;
        row.items.clear();
        self.by_reader.release(r);
    }

    /// [`ReadIndex::forget`] of each of `readers`. While they hold less than
    /// `1 / SWEEP_SHARE` of the edges it forgets them one at a time; past that it drops their
    /// edges in one pass over the pair map and the key rows, and fixes the places of the edges
    /// left in the key rows that lost some. Forgetting most of the index at once (hir-ty's
    /// impls maintainer resetting every crate a proc macro reached) is then a sweep, not three
    /// probes at random per edge, and leaves the pair map with no tombstones to probe past.
    pub fn forget_many(&mut self, readers: impl IntoIterator<Item = R>) {
        // alloc: once per call, a flag per reader row
        let mut dropped = vec![false; self.by_reader.rows.len()];
        // alloc: once per call, the rows of the readers forgotten
        let mut gone: Vec<u32> = Vec::new();
        let mut edges = 0usize;
        for reader in readers {
            let Some(&r) = self.by_reader.slot.get(&reader) else { continue };
            if !std::mem::replace(&mut dropped[r as usize], true) {
                gone.push(r);
                edges += self.by_reader.rows[r as usize].items.len();
            }
        }
        if gone.is_empty() {
            return;
        }
        if edges.saturating_mul(SWEEP_SHARE) < self.pairs.len() {
            for r in gone {
                let reader = self.by_reader.rows[r as usize].owner;
                self.forget(reader);
            }
            return;
        }
        self.sweep(&dropped, &gone);
    }

    /// Drops every edge of the reader rows `gone` (flagged in `dropped`) in one pass.
    fn sweep(&mut self, dropped: &[bool], gone: &[u32]) {
        let ReadIndex { by_key, by_reader, pairs } = self;
        pairs.retain(|&(_, r), _| !dropped[r as usize]);
        // alloc: once per sweep, the key rows that lost an edge and kept others
        let mut changed: Vec<u32> = Vec::new();
        for k in 0..by_key.rows.len() {
            let items = &mut by_key.rows[k].items;
            let before = items.len();
            if before == 0 {
                continue;
            }
            items.retain(|&mut (_, r)| !dropped[r as usize]);
            if items.len() == before {
                continue;
            }
            if items.is_empty() {
                by_key.release(k as u32);
            } else {
                changed.push(k as u32);
            }
        }
        // A surviving reader's row is untouched (only whole readers go, and a key row released
        // above held only theirs); the edges left in a changed key row moved up in it.
        for &k in &changed {
            for (at, &(_, r)) in by_key.rows[k as usize].items.iter().enumerate() {
                pairs.get_mut(&(k, r)).expect("a read index edge without its pair").0 = at as u32;
            }
        }
        for &r in gone {
            by_reader.rows[r as usize].items.clear();
            by_reader.release(r);
        }
        // Into a table of its own size: the one the removals left holds their tombstones.
        let len = pairs.len();
        pairs.shrink_to(len);
    }

    /// Drops the one edge `key` -> `reader`, if it is there.
    pub fn remove(&mut self, key: K, reader: R) {
        let (Some(&k), Some(&r)) = (self.by_key.slot.get(&key), self.by_reader.slot.get(&reader))
        else {
            return;
        };
        let Some((in_key, in_reader)) = self.pairs.remove(&(k, r)) else { return };
        if let Some(moved) = self.by_key.remove_at(k, in_key) {
            self.pairs.get_mut(&(k, moved)).expect("a read index edge without its pair").0 = in_key;
        }
        if let Some(moved) = self.by_reader.remove_at(r, in_reader) {
            self.pairs.get_mut(&(moved, r)).expect("a read index edge without its pair").1 =
                in_reader;
        }
        if self.by_key.rows[k as usize].items.is_empty() {
            self.by_key.release(k);
        }
        if self.by_reader.rows[r as usize].items.is_empty() {
            self.by_reader.release(r);
        }
    }

    /// The readers of `key`, in no particular order: one probe, no allocation.
    pub fn readers(&self, key: K) -> impl Iterator<Item = R> + '_ {
        self.by_key.items(key).iter().map(|&(reader, _)| reader)
    }

    /// The keys `reader` read, in no particular order: one probe, no allocation.
    pub fn keys(&self, reader: R) -> impl Iterator<Item = K> + '_ {
        self.by_reader.items(reader).iter().map(|&(key, _)| key)
    }

    pub fn contains(&self, key: K, reader: R) -> bool {
        match (self.by_key.slot.get(&key), self.by_reader.slot.get(&reader)) {
            (Some(&k), Some(&r)) => self.pairs.contains_key(&(k, r)),
            _ => false,
        }
    }

    /// How many (key, reader) edges there are.
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Drops every edge, keeping the buffers.
    pub fn clear(&mut self) {
        self.by_key.clear();
        self.by_reader.clear();
        self.pairs.clear();
    }
}

impl<A, B> Side<A, B> {
    fn clear(&mut self) {
        self.slot.clear();
        self.rows.clear();
        self.free.clear();
    }
}
