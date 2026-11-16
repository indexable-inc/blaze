//! The request machine's own maps (drive.md, "Scheduling on many cores"; parallel.md change 3):
//! every lookup, insert and remove a step makes is constant time on average, so a step costs
//! the same at ten jobs and at a million.
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

pub struct Table<K, V, A: Allocator + Clone> {
    slots: Vec<Option<(K, V)>, A>,
    len: usize,
    alloc: A,
    ops: Cell<u64>,
}

impl<K: Hashed, V, A: Allocator + Clone> Table<K, V, A> {
    pub fn new_in(alloc: A) -> Table<K, V, A> {
        Table { slots: Vec::new_in(alloc.clone()), len: 0, alloc, ops: Cell::new(0) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Probes made so far, growth included.
    pub fn ops(&self) -> u64 {
        self.ops.get()
    }

    fn tick(&self) {
        self.ops.set(self.ops.get() + 1);
    }

    fn mask(&self) -> usize {
        self.slots.len() - 1
    }

    /// The key's slot, or the empty slot it would go in. The table is never full (load at most
    /// a half), so the probe ends.
    fn find(&self, key: &K) -> Result<usize, usize> {
        let mask = self.mask();
        let mut at = key.hash() as usize & mask;
        loop {
            self.tick();
            match &self.slots[at] {
                Some((held, _)) if held == key => return Ok(at),
                Some(_) => at = (at + 1) & mask,
                None => return Err(at),
            }
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        if self.len == 0 {
            return None;
        }
        let at = self.find(key).ok()?;
        self.slots[at].as_ref().map(|(_, value)| value)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        if self.len == 0 {
            return None;
        }
        let at = self.find(key).ok()?;
        self.slots[at].as_mut().map(|(_, value)| value)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        match self.find(&key) {
            Ok(at) => self.slots[at].as_mut().map(|(_, held)| core::mem::replace(held, value)),
            Err(at) => {
                self.slots[at] = Some((key, value));
                self.len += 1;
                None
            }
        }
    }

    fn grow(&mut self) {
        let size = (self.slots.len() * 2).max(16);
        let mut slots = Vec::with_capacity_in(size, self.alloc.clone());
        for _ in 0..size {
            slots.push(None);
        }
        let old = core::mem::replace(&mut self.slots, slots);
        for (key, value) in old.into_iter().flatten() {
            if let Err(at) = self.find(&key) {
                self.slots[at] = Some((key, value));
            }
        }
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        if self.len == 0 {
            return None;
        }
        let at = self.find(key).ok()?;
        let (_, value) = self.slots[at].take()?;
        self.len -= 1;
        // Backward shift: each later entry of the run moves into the hole unless its home lies
        // after the hole (cyclically), so every probe chain stays unbroken.
        let mask = self.mask();
        let mut hole = at;
        let mut next = (at + 1) & mask;
        loop {
            self.tick();
            let home = match &self.slots[next] {
                Some((held, _)) => held.hash() as usize & mask,
                None => break,
            };
            if (next.wrapping_sub(home) & mask) >= (next.wrapping_sub(hole) & mask) {
                self.slots[hole] = self.slots[next].take();
                hole = next;
            }
            next = (next + 1) & mask;
        }
        Some(value)
    }

    /// In slot order: callers that print, write or hash sort first.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> + '_ {
        self.slots.iter().filter_map(|slot| slot.as_ref().map(|(key, value)| (key, value)))
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> + '_ {
        self.iter().map(|(key, _)| key)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> + '_ {
        self.iter().map(|(_, value)| value)
    }

    /// Every entry, the table left empty.
    pub fn take_all(&mut self) -> impl Iterator<Item = (K, V)> + use<K, V, A> {
        let slots = core::mem::replace(&mut self.slots, Vec::new_in(self.alloc.clone()));
        self.len = 0;
        slots.into_iter().flatten()
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
}
