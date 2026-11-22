//! The drive's map keyed by content ids (drive.md, "Scheduling on many cores"; parallel.md
//! change 3), used by the engine's code pool: every lookup, insert and remove is constant time
//! on average, so a step costs the same at ten jobs and at a million.
//!
//! [`Table`] is open addressing with linear probing and backward-shift removal (no tombstones),
//! at most half full, doubling when it would pass that. Its keys are content ids or counters, so
//! the hash is the key's own bits ([`Hashed`]): no seed, no hasher state, and the layout is a
//! function of the insertions alone (rule 25: nothing printed or stored follows it; what is
//! written sorts first).
//!
//! Every probe is counted ([`Table::ops`]): the linearity test reads the machine's bookkeeping
//! as a count, not a time.

use core::alloc::Allocator;
use core::cell::Cell;

use blaze_own::Vec;

use crate::key::{Key, Name};

/// A key whose bits are already uniform, or made so by one multiply.
pub trait Hashed: Copy + Eq {
    fn hash(&self) -> u64;
}

fn word(bytes: &[u8; 32]) -> u64 {
    u64::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
}

/// Fibonacci hashing: a counter's low bits spread over the word.
pub(crate) fn spread(x: u64) -> u64 {
    x.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29)
}

impl Hashed for Name {
    fn hash(&self) -> u64 {
        word(&self.subject.0) ^ spread(self.kind.index() as u64)
    }
}

impl Hashed for Key {
    fn hash(&self) -> u64 {
        word(&self.0.0)
    }
}

impl Hashed for u32 {
    fn hash(&self) -> u64 {
        spread(u64::from(*self))
    }
}

impl Hashed for u64 {
    fn hash(&self) -> u64 {
        spread(*self)
    }
}

/// Control byte marking an unoccupied hash bucket; occupied buckets hold a 7-bit H2 hash tag in
/// `0x00..=0x7F`, so probes filter mismatches in L1 cache without touching entries.
const EMPTY: u8 = 0x80;

#[inline]
fn h2(hash: u64) -> u8 {
    (hash >> 57) as u8
}

pub struct Table<K, V, A: Allocator + Clone> {
    /// One byte per bucket: `EMPTY` or the 7-bit H2 fingerprint of the occupant's hash.
    ctrl: Vec<u8, A>,
    /// Bucket-to-entry index into `entries` (valid where `ctrl[i] != EMPTY`).
    indices: Vec<u32, A>,
    /// Densely packed live `(K, V)` entries (zero `Option` discriminant or empty-slot waste).
    entries: Vec<(K, V), A>,
    alloc: A,
    ops: Cell<u64>,
}

impl<K: Hashed, V, A: Allocator + Clone> Table<K, V, A> {
    pub fn new_in(alloc: A) -> Table<K, V, A> {
        Table {
            ctrl: Vec::new_in(alloc.clone()),
            indices: Vec::new_in(alloc.clone()),
            entries: Vec::new_in(alloc.clone()),
            alloc,
            ops: Cell::new(0),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Probes made so far, growth included.
    pub fn ops(&self) -> u64 {
        self.ops.get()
    }

    fn tick(&self) {
        self.ops.set(self.ops.get() + 1);
    }

    fn mask(&self) -> usize {
        self.ctrl.len() - 1
    }

    /// The key's bucket, or the empty bucket it would go in. The table stays at or below 75% load,
    /// so every probe terminates.
    fn find(&self, key: &K) -> Result<usize, usize> {
        let h = key.hash();
        let tag = h2(h);
        let mask = self.mask();
        let mut at = h as usize & mask;
        loop {
            self.tick();
            let c = self.ctrl[at];
            if c == EMPTY {
                return Err(at);
            }
            if c == tag {
                let idx = self.indices[at] as usize;
                if self.entries[idx].0 == *key {
                    return Ok(at);
                }
            }
            at = (at + 1) & mask;
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        if self.entries.is_empty() {
            return None;
        }
        let at = self.find(key).ok()?;
        Some(&self.entries[self.indices[at] as usize].1)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        if self.entries.is_empty() {
            return None;
        }
        let at = self.find(key).ok()?;
        let idx = self.indices[at] as usize;
        Some(&mut self.entries[idx].1)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        // 75% load factor (`(len + 1) / cap > 3 / 4`).
        if (self.entries.len() + 1) * 4 > self.ctrl.len() * 3 {
            self.grow();
        }
        match self.find(&key) {
            Ok(at) => {
                let idx = self.indices[at] as usize;
                Some(core::mem::replace(&mut self.entries[idx].1, value))
            }
            Err(at) => {
                let idx = u32::try_from(self.entries.len()).expect("table exceeds u32::MAX entries");
                self.ctrl[at] = h2(key.hash());
                self.indices[at] = idx;
                self.entries.push((key, value));
                None
            }
        }
    }

    fn grow(&mut self) {
        let size = (self.ctrl.len() * 2).max(16);
        let mut ctrl = Vec::with_capacity_in(size, self.alloc.clone());
        ctrl.resize(size, EMPTY);
        let mut indices = Vec::with_capacity_in(size, self.alloc.clone());
        indices.resize(size, 0);
        let mask = size - 1;
        // Re-index existing entries in place without moving any `(K, V)` values.
        for (idx, (key, _)) in self.entries.iter().enumerate() {
            let h = key.hash();
            let mut at = h as usize & mask;
            loop {
                self.tick();
                if ctrl[at] == EMPTY {
                    ctrl[at] = h2(h);
                    indices[at] = idx as u32;
                    break;
                }
                at = (at + 1) & mask;
            }
        }
        self.ctrl = ctrl;
        self.indices = indices;
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        if self.entries.is_empty() {
            return None;
        }
        let at = self.find(key).ok()?;
        let removed_idx = self.indices[at] as usize;
        // Backward shift over the compact ctrl/index arrays: each later bucket of the run moves
        // into the hole unless its home lies after the hole (cyclically), keeping probe chains
        // unbroken with no tombstones and without moving `(K, V)` payloads.
        let mask = self.mask();
        let mut hole = at;
        let mut next = (at + 1) & mask;
        loop {
            self.tick();
            let c = self.ctrl[next];
            if c == EMPTY {
                break;
            }
            let idx = self.indices[next] as usize;
            let home = self.entries[idx].0.hash() as usize & mask;
            if (next.wrapping_sub(home) & mask) >= (next.wrapping_sub(hole) & mask) {
                self.ctrl[hole] = c;
                self.indices[hole] = idx as u32;
                hole = next;
            }
            next = (next + 1) & mask;
        }
        self.ctrl[hole] = EMPTY;

        let (_, value) = self.entries.remove_swap(removed_idx);
        if removed_idx < self.entries.len() {
            // The former last entry moved into `removed_idx`: patch its bucket index in place.
            let old_idx = self.entries.len() as u32;
            let h = self.entries[removed_idx].0.hash();
            let tag = h2(h);
            let mut probe = h as usize & mask;
            loop {
                self.tick();
                if self.ctrl[probe] == tag && self.indices[probe] == old_idx {
                    self.indices[probe] = removed_idx as u32;
                    break;
                }
                probe = (probe + 1) & mask;
            }
        }
        Some(value)
    }

    /// In slot order: callers that print, write or hash sort first.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> + '_ {
        self.entries.iter().map(|(key, value)| (key, value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> + '_ {
        self.iter().map(|(key, _)| key)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> + '_ {
        self.iter().map(|(_, value)| value)
    }

    /// Every entry, the table left empty.
    pub fn take_all(&mut self) -> impl Iterator<Item = (K, V)> + use<K, V, A> {
        self.ctrl = Vec::new_in(self.alloc.clone());
        self.indices = Vec::new_in(self.alloc.clone());
        core::mem::replace(&mut self.entries, Vec::new_in(self.alloc.clone())).into_iter()
    }
}

/// A set over [`Table`].
pub struct Set<K, A: Allocator + Clone>(Table<K, (), A>);

impl<K: Hashed, A: Allocator + Clone> Set<K, A> {
    pub fn new_in(alloc: A) -> Set<K, A> {
        Set(Table::new_in(alloc))
    }

    /// Whether `key` was not in the set.
    pub fn insert(&mut self, key: K) -> bool {
        self.0.insert(key, ()).is_none()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.0.contains_key(key)
    }

    pub fn remove(&mut self, key: &K) -> bool {
        self.0.remove(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &K> + '_ {
        self.0.keys()
    }

    pub fn ops(&self) -> u64 {
        self.0.ops()
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;
    use std::collections::BTreeMap;
    use std::format;

    use blaze_sim::{Broke, Config as SimConfig, check};

    use super::Table;

    /// A model check against `BTreeMap`: any sequence of inserts and removes over a small key
    /// space (so chains collide and wrap) leaves the same map.
    #[test]
    fn the_table_is_a_map() {
        check(SimConfig::cases(500), |tape| {
            let mut table: Table<u32, u32, Global> = Table::new_in(Global);
            let mut model = BTreeMap::new();
            for step in 0..tape.index(400, "steps") as u32 {
                let key = tape.index(64, "key") as u32;
                match tape.one_in(3, "remove") {
                    true => {
                        if table.remove(&key) != model.remove(&key) {
                            return Err(Broke::Oracle(format!("remove {key} differs")));
                        }
                    }
                    false => {
                        if table.insert(key, step) != model.insert(key, step) {
                            return Err(Broke::Oracle(format!("insert {key} differs")));
                        }
                    }
                }
                for probe in 0..64 {
                    if table.get(&probe) != model.get(&probe) {
                        return Err(Broke::Oracle(format!("get {probe} differs after step {step}")));
                    }
                }
            }
            match table.len() == model.len() {
                true => Ok(()),
                false => Err(Broke::Oracle(format!("len {} against {}", table.len(), model.len()))),
            }
        });
    }

    #[test]
    fn compact_table_respects_75_percent_load_and_take_all() {
        let mut table: Table<u64, u64, Global> = Table::new_in(Global);
        // 12 entries fit in a 16-bucket table at 75% load factor without growing to 32.
        for i in 0..12u64 {
            assert_eq!(table.insert(i, i * 10), None);
        }
        assert_eq!(table.ctrl.len(), 16, "12 entries fit at 75% load factor");
        assert_eq!(table.len(), 12);
        // 13th entry grows to 32.
        assert_eq!(table.insert(12, 120), None);
        assert_eq!(table.ctrl.len(), 32);
        for i in 0..13u64 {
            assert_eq!(table.get(&i), Some(&(i * 10)));
        }
        let mut drained: std::vec::Vec<(u64, u64)> = table.take_all().collect();
        drained.sort_unstable();
        assert_eq!(drained.len(), 13);
        assert!(table.is_empty());
        assert_eq!(table.get(&0), None);
    }
}
