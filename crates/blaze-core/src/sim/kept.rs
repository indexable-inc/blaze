//! The descriptors a real driver's file threads keep open for reads into memory
//! (`crate::descriptors`), modelled: which names are open and through which cache mode, the
//! least recently read closed past the bound, and what a kept descriptor reads once another
//! process unlinked its file or renamed another over it (the bytes the file held then: the
//! inode the descriptor still holds, which no name leads to).

use std::sync::Arc;

use crate::{Cache, FileName, Opens};

/// The table: at most `bound` descriptors, past which the least recently read closes. A read
/// is never refused for room.
pub(super) struct Descriptors {
    bound: usize,
    /// In the order opened; the least recently read is found by `used`.
    entries: Vec<Entry>,
    /// Counts the reads, for which was read last.
    clock: u64,
    opened: u64,
    closed: u64,
}

struct Entry {
    name: FileName,
    cache: Cache,
    used: u64,
    /// The bytes the file held when another process unlinked it or renamed another over its
    /// name: what this descriptor reads from then on.
    orphan: Option<Arc<[u8]>>,
}

impl Descriptors {
    pub(super) fn new(bound: usize) -> Descriptors {
        assert!(bound > 0, "a table of descriptors keeps at least one");
        // alloc: none until the first read into memory.
        Descriptors { bound, entries: Vec::new(), clock: 0, opened: 0, closed: 0 }
    }

    /// The table's bound set to `bound`, closing the least recently read past it.
    pub(super) fn bound(&mut self, bound: usize) {
        assert!(bound > 0, "a table of descriptors keeps at least one");
        self.bound = bound;
        while self.entries.len() > self.bound {
            self.close_oldest();
        }
    }

    /// Whether `name` is kept open through `cache`, read now: `Some` of what it reads from (an
    /// orphan's bytes, or `None` for the file at the name).
    pub(super) fn read(&mut self, name: &FileName, cache: Cache) -> Option<Option<Arc<[u8]>>> {
        self.clock += 1;
        let now = self.clock;
        let entry = self.entries.iter_mut().find(|it| it.name == *name && it.cache == cache)?;
        entry.used = now;
        Some(entry.orphan.clone()) // clone: a refcount
    }

    /// `name` opened through `cache` and kept, the least recently read closed where the table
    /// is at its bound.
    pub(super) fn open(&mut self, name: &FileName, cache: Cache) {
        if self.entries.len() >= self.bound {
            self.close_oldest();
        }
        self.clock += 1;
        // clone: a refcount, the key it is kept under.
        self.entries.push(Entry { name: name.clone(), cache, used: self.clock, orphan: None });
        self.opened += 1;
    }

    fn close_oldest(&mut self) {
        let oldest = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, it)| it.used)
            .map(|(at, _)| at)
            .expect("a table at its bound holds some");
        self.entries.remove(oldest);
        self.closed += 1;
    }

    /// The descriptors of `name` and of everything under it closed: the driver changed what
    /// is under the name.
    pub(super) fn forget(&mut self, name: &FileName) {
        let before = self.entries.len();
        self.entries.retain(|it| !it.name.is_under(name));
        self.closed += (before - self.entries.len()) as u64;
    }

    /// The names under `name` kept open and reading the file at their name, which another
    /// process is about to unlink or rename over.
    pub(super) fn live_under(&self, name: &FileName) -> Vec<FileName> {
        // alloc: a test's change of the world, not an op.
        let mut names: Vec<FileName> = self
            .entries
            .iter()
            .filter(|it| it.orphan.is_none() && it.name.is_under(name))
            .map(|it| it.name.clone()) // clone: a refcount
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// The descriptors of `name` read `bytes` from now on: its inode, which no name leads to.
    pub(super) fn orphan(&mut self, name: &FileName, bytes: &[u8]) {
        let bytes: Arc<[u8]> = Arc::from(bytes); // alloc: a test's change of the world.
        for entry in self.entries.iter_mut().filter(|it| it.name == *name && it.orphan.is_none()) {
            entry.orphan = Some(Arc::clone(&bytes)); // clone: a refcount
        }
    }

    /// Every descriptor closed: the process ended.
    pub(super) fn crash(&mut self) {
        self.closed += self.entries.len() as u64;
        self.entries.clear();
    }

    pub(super) fn opens(&self) -> Opens {
        Opens { opened: self.opened, closed: self.closed }
    }

    /// How many are kept open now.
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}
