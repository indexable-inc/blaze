//! A small open-addressing table from a 64-bit hash to `u32` values, several values per hash
//! allowed, in the allocator it is given. What the type table hash-conses with and the driver
//! caches exports in. Iteration order is never observed: it has no iterator.

use blaze_own::Vec;
use core::alloc::Allocator;

use crate::error::{CheckError, Limit};

const EMPTY: u32 = u32::MAX;

/// Hash to values; a lookup takes the caller's equality, so equal hashes of unequal values are
/// told apart by content.
pub struct HashIndex<A: Allocator + Clone> {
    hashes: Vec<u64, A>,
    values: Vec<u32, A>,
    len: usize,
}

impl<A: Allocator + Clone> HashIndex<A> {
    /// An empty table.
    pub fn new_in(alloc: A) -> HashIndex<A> {
        HashIndex { hashes: Vec::new_in(alloc.clone()), values: Vec::new_in(alloc), len: 0 }
    }

    /// The value under `hash` for which `eq` holds.
    pub fn find(&self, hash: u64, mut eq: impl FnMut(u32) -> bool) -> Option<u32> {
        let cap = self.values.len();
        if cap == 0 {
            return None;
        }
        let mask = cap - 1;
        let mut at = (hash as usize) & mask;
        // At most `cap` probes: the table is never full (load at most one half).
        for _ in 0..cap {
            let v = self.values[at];
            if v == EMPTY {
                return None;
            }
            if self.hashes[at] == hash && eq(v) {
                return Some(v);
            }
            at = (at + 1) & mask;
        }
        None
    }

    /// Adds `value` under `hash`. Fails with [`Limit::Table`] past `u32::MAX / 2` entries.
    pub fn insert(&mut self, hash: u64, value: u32) -> Result<(), CheckError> {
        if (self.len + 1) * 2 > self.values.len() {
            self.grow()?;
        }
        let mask = self.values.len() - 1;
        let mut at = (hash as usize) & mask;
        for _ in 0..self.values.len() {
            if self.values[at] == EMPTY {
                self.values[at] = value;
                self.hashes[at] = hash;
                self.len += 1;
                return Ok(());
            }
            at = (at + 1) & mask;
        }
        Err(CheckError::Limit(Limit::Table))
    }

    fn grow(&mut self) -> Result<(), CheckError> {
        let cap = if self.values.is_empty() { 64 } else { self.values.len() * 2 };
        if cap > (u32::MAX as usize) {
            return Err(CheckError::Limit(Limit::Table));
        }
        let alloc = self.values.allocator().clone();
        let mut hashes = Vec::with_capacity_in(cap, alloc.clone());
        let mut values = Vec::with_capacity_in(cap, alloc);
        hashes.resize(cap, 0);
        values.resize(cap, EMPTY);
        let mask = cap - 1;
        for i in 0..self.values.len() {
            let v = self.values[i];
            if v == EMPTY {
                continue;
            }
            let h = self.hashes[i];
            let mut at = (h as usize) & mask;
            while values[at] != EMPTY {
                at = (at + 1) & mask;
            }
            values[at] = v;
            hashes[at] = h;
        }
        self.hashes = hashes;
        self.values = values;
        Ok(())
    }
}
