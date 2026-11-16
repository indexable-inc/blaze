//! A walk's memo: `u64` key to `u32` value, cleared in O(1) by a generation stamp, so a pass
//! over a large shared type visits each distinct (node, depth) once and a pass over a small
//! one pays nothing to clear. Linear probing, at most half full.

use blaze_int::Int;
use blaze_own::{Alloc, Vec};

use crate::hash;

pub(crate) struct Memo<'a> {
    keys: Vec<u64, Alloc<'a>>,
    vals: Vec<u32, Alloc<'a>>,
    gens: Vec<u32, Alloc<'a>>,
    gen_now: u32,
    len: usize,
}

impl<'a> Memo<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Memo<'a> {
        Memo { keys: Vec::new_in(alloc), vals: Vec::new_in(alloc), gens: Vec::new_in(alloc), gen_now: 1, len: 0 }
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
            for g in self.gens.iter_mut() {
                *g = 0;
            }
            self.gen_now = 1;
        }
    }

    #[inline]
    pub(crate) fn get(&self, key: u64) -> Option<u32> {
        if self.len == 0 {
            return None;
        }
        let mask = self.keys.len() - 1;
        let mut at = hash::key(key).truncate_to::<usize>() & mask;
        loop {
            if self.gens[at] != self.gen_now {
                return None;
            }
            if self.keys[at] == key {
                return Some(self.vals[at]);
            }
            at = (at + 1) & mask;
        }
    }

    /// Sets `key` to `val` (replacing what it had).
    #[inline]
    pub(crate) fn insert(&mut self, key: u64, val: u32) {
        if (self.len + 1) * 2 > self.keys.len() {
            self.grow();
        }
        let mask = self.keys.len() - 1;
        let mut at = hash::key(key).truncate_to::<usize>() & mask;
        loop {
            if self.gens[at] != self.gen_now {
                self.gens[at] = self.gen_now;
                self.keys[at] = key;
                self.vals[at] = val;
                self.len += 1;
                return;
            }
            if self.keys[at] == key {
                self.vals[at] = val;
                return;
            }
            at = (at + 1) & mask;
        }
    }

    #[cold]
    fn grow(&mut self) {
        let cap = (self.keys.len() * 2).max(64);
        let alloc = *self.keys.allocator();
        let mut keys = Vec::new_in(alloc);
        keys.resize(cap, 0);
        let mut vals = Vec::new_in(alloc);
        vals.resize(cap, 0);
        let mut gens = Vec::new_in(alloc);
        gens.resize(cap, 0);
        let old_keys = core::mem::replace(&mut self.keys, keys);
        let old_vals = core::mem::replace(&mut self.vals, vals);
        let old_gens = core::mem::replace(&mut self.gens, gens);
        let was = self.gen_now;
        self.gen_now = 1;
        self.len = 0;
        let mask = cap - 1;
        for i in 0..old_keys.len() {
            if old_gens[i] != was {
                continue;
            }
            let mut at = hash::key(old_keys[i]).truncate_to::<usize>() & mask;
            while self.gens[at] == self.gen_now {
                at = (at + 1) & mask;
            }
            self.gens[at] = self.gen_now;
            self.keys[at] = old_keys[i];
            self.vals[at] = old_vals[i];
            self.len += 1;
        }
    }
}
