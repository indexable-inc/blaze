//! A walk's memo: `u64` key to `u32` value, cleared in O(1) by a generation stamp, so a pass
//! over a large shared type visits each distinct (node, depth) once and a pass over a small
//! one pays nothing to clear. Linear probing, at most half full; an entry is one 16-byte slot
//! (key, value, generation), so a probe touches one cache line.

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
        let cap = (self.slots.len() * 2).max(64);
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
