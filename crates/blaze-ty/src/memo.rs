//! A walk's maps: `u64` key to `u32` value, cleared in O(1) by a generation stamp, so a pass
//! over a small term pays nothing to clear. An entry is one 16-byte slot (key, value,
//! generation), so a probe touches one cache line.
//!
//! [`Memo`] is exact (linear probing, at most half full): for maps whose answers must not be
//! lost (canonical variables, the canonical key's back-references, fresh bound variables).
//! [`Cache`] is a fixed direct-mapped table that may lose entries: a fold's memo, where a
//! lost entry is only work done again.

use blaze_int::Int;
use blaze_own::{Alloc, Vec};

use crate::hash;

#[derive(Clone, Copy, Default)]
struct Slot {
    key: u64,
    val: u32,
    generation: u32,
}

pub(crate) struct Memo<'a> {
    slots: Vec<Slot, Alloc<'a>>,
    gen_now: u32,
    len: usize,
}

impl<'a> Memo<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Memo<'a> {
        Memo { slots: Vec::new_in(alloc), gen_now: 1, len: 0 }
    }

    /// Forgets every entry.
    #[inline]
    pub(crate) fn clear(&mut self) {
        if self.len == 0 {
            return;
        }
        self.len = 0;
        self.gen_now = self.gen_now.wrapping_add(1);
        if self.gen_now == 0 {
            for s in self.slots.iter_mut() {
                s.generation = 0;
            }
            self.gen_now = 1;
        }
    }

    #[inline]
    pub(crate) fn get(&self, key: u64) -> Option<u32> {
        if self.len == 0 {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut at = hash::key(key).truncate_to::<usize>() & mask;
        loop {
            let s = self.slots[at];
            if s.generation != self.gen_now {
                return None;
            }
            if s.key == key {
                return Some(s.val);
            }
            at = (at + 1) & mask;
        }
    }

    /// Sets `key` to `val` (replacing what it had).
    #[inline]
    pub(crate) fn insert(&mut self, key: u64, val: u32) {
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mask = self.slots.len() - 1;
        let mut at = hash::key(key).truncate_to::<usize>() & mask;
        loop {
            let s = &mut self.slots[at];
            if s.generation != self.gen_now {
                *s = Slot { key, val, generation: self.gen_now };
                self.len += 1;
                return;
            }
            if s.key == key {
                s.val = val;
                return;
            }
            at = (at + 1) & mask;
        }
    }

    #[cold]
    fn grow(&mut self) {
        self.grow_to((self.slots.len() * 2).max(64));
    }
}

/// Slots of a [`Cache`]: 64 KiB, half an Apple core's L1 data cache.
const CACHE_SLOTS: usize = 4096;

/// A fold's memo: a direct-mapped cache of [`CACHE_SLOTS`] slots, where an insert overwrites
/// what was in its slot. A lost entry costs a walk of that subtree again, never a wrong
/// answer, and the table stays in L1: an exact table of a million-node pass was 64 MiB, and
/// its get and insert were two cache misses a node, which the cost example measured as 61 of
/// the 110 ns a field of a substitution over a million fields. A subtree met again soon (the
/// sharing a DAG has: `(T, T)`, a parameter's argument used twice) is still met in the cache,
/// since nothing is inserted between its first walk's end and its second's start but the
/// first's own result.
pub(crate) struct Cache<'a> {
    slots: Vec<Slot, Alloc<'a>>,
    gen_now: u32,
}

impl<'a> Cache<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Cache<'a> {
        Cache { slots: Vec::new_in(alloc), gen_now: 1 }
    }

    /// Forgets every entry.
    #[inline]
    pub(crate) fn clear(&mut self) {
        self.gen_now = self.gen_now.wrapping_add(1);
        if self.gen_now == 0 {
            for s in self.slots.iter_mut() {
                s.generation = 0;
            }
            self.gen_now = 1;
        }
    }

    #[inline(always)]
    fn at(key: u64) -> usize {
        hash::key(key).truncate_to::<usize>() & (CACHE_SLOTS - 1)
    }

    #[inline]
    pub(crate) fn get(&self, key: u64) -> Option<u32> {
        let s = self.slots.get(Cache::at(key))?;
        (s.generation == self.gen_now && s.key == key).then_some(s.val)
    }

    #[inline]
    pub(crate) fn insert(&mut self, key: u64, val: u32) {
        if self.slots.is_empty() {
            self.slots.resize(CACHE_SLOTS, Slot::default());
        }
        self.slots[Cache::at(key)] = Slot { key, val, generation: self.gen_now };
    }
}

impl Memo<'_> {
    /// Moves every entry to a table of `cap` slots.
    #[cold]
    fn grow_to(&mut self, cap: usize) {
        let alloc = *self.slots.allocator();
        let mut slots = Vec::new_in(alloc);
        slots.resize(cap, Slot::default());
        let old = core::mem::replace(&mut self.slots, slots);
        let was = self.gen_now;
        self.gen_now = 1;
        self.len = 0;
        let mask = cap - 1;
        for s in old.iter() {
            if s.generation != was {
                continue;
            }
            let mut at = hash::key(s.key).truncate_to::<usize>() & mask;
            while self.slots[at].generation == self.gen_now {
                at = (at + 1) & mask;
            }
            self.slots[at] = Slot { key: s.key, val: s.val, generation: self.gen_now };
            self.len += 1;
        }
    }
}
