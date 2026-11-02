//! Weak memory: store buffers that let an atomic load read an older store, as C++20 allows,
//! ported from Miri's `src/tools/miri/src/concurrency/weak_memory.rs` (itself after "Dynamic
//! Race Detection for C++", Lidbury and Donaldson, POPL 2017, and tsan11).
//!
//! Each atomic location of an allocation (a range written atomically, made on its first atomic
//! store and dropped at the next non-atomic or imperfectly overlapping write) keeps its last
//! [`STORE_BUFFER_LIMIT`] stores in modification order. A load picks, at random, one store it may
//! read under the coherence rules (CoWR, CoRR), the SC fence rules (C++17 §32.4 paragraphs 4
//! and 5) and Miri's C++20 rules for SC accesses (coherence-ordered-before). It never makes a
//! behavior the C++20 model forbids, and misses some it allows.
//!
//! Load information is kept per store and thread as the first load's timestamp only, as tsan11
//! and Miri do; it is a short inline list rather than Miri's hash map, since few threads load
//! one store.

use std::collections::VecDeque;

use blaze_ids::rng::SplitMix64;
use smallvec::SmallVec;

use crate::ids::Span;

use super::{
    cells::ThreadClockSet,
    range_map::{Overlap, RangeObjectMap},
    vclock::{VClock, VTimestamp, VectorIdx},
};

/// The stores kept per location; more would show more weak behavior and cost more (Miri and
/// tsan11 both pick 128).
pub(super) const STORE_BUFFER_LIMIT: usize = 128;

/// A store's bytes; atomics are at most 16 bytes, so they never leave the inline buffer.
pub(super) type Value = SmallVec<[u8; 16]>;

/// The stores of one atomic location, oldest first (Miri: `StoreBuffer`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StoreBuffer {
    buffer: VecDeque<StoreElement>,
}

/// One store (Miri: `StoreElement`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StoreElement {
    store_thread: VectorIdx,
    store_timestamp: VTimestamp,
    /// What loading this store acquires.
    pub(super) sync_clock: VClock,
    /// The store is SC, or happens before or precedes in modification order an SC store.
    is_seqcst: bool,
    /// `None`: the bytes were not all initialized.
    pub(super) val: Option<Value>,
    load_info: LoadInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct LoadInfo {
    /// The time of each thread's first load of this store.
    timestamps: SmallVec<[(VectorIdx, VTimestamp); 4]>,
    /// An SC load read this store: later SC loads may not read an earlier one.
    sc_loaded: bool,
}

impl LoadInfo {
    #[inline]
    fn any_before(&self, clock: &VClock) -> bool {
        self.timestamps.iter().any(|&(load_index, load_timestamp)| load_timestamp <= clock[load_index])
    }
}

impl StoreBuffer {
    /// A buffer holding only the location's value before its first atomic store, which is
    /// non-atomic: it acquires nothing (Miri: `StoreBuffer::new`).
    pub(super) fn new(init: Option<&[u8]>) -> Self {
        let mut buffer = VecDeque::new();
        buffer.push_back(StoreElement {
            store_thread: VectorIdx(0),
            store_timestamp: VTimestamp::ZERO,
            sync_clock: VClock::default(),
            val: init.map(Value::from_slice),
            is_seqcst: false,
            load_info: LoadInfo::default(),
        });
        Self { buffer }
    }

    pub(super) fn element(&self, pos: usize) -> &StoreElement {
        &self.buffer[pos]
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.buffer.len()
    }

    /// An RMW or failed compare-exchange reads the latest store (Miri:
    /// `read_from_last_store`).
    pub(super) fn read_from_last_store(&mut self, index: VectorIdx, clocks: &ThreadClockSet, is_seqcst: bool) {
        if let Some(store_elem) = self.buffer.back_mut() {
            store_elem.load_impl(index, clocks, is_seqcst);
        }
    }

    /// Visits, newest first, each store a load with `clocks` may read, until `visit` says stop
    /// (Miri: the candidate iterator of `fetch_store`).
    fn candidates(&self, is_seqcst: bool, clocks: &ThreadClockSet, mut visit: impl FnMut(usize) -> bool) {
        let mut found_sc = false;
        for pos in (0..self.buffer.len()).rev() {
            let store_elem = &self.buffer[pos];
            let keep_searching = if store_elem.store_timestamp <= clocks.clock[store_elem.store_thread] {
                // CoWR: a store that happens before the load hides everything earlier in
                // modification order (C++20 §6.9.2.2 [intro.races] paragraph 18).
                false
            } else if store_elem.load_info.any_before(&clocks.clock) {
                // CoRR: so does a store read by a load that happens before this one
                // (paragraph 16).
                false
            } else if store_elem.store_timestamp <= clocks.write_seqcst[store_elem.store_thread]
                && store_elem.is_seqcst
            {
                // A load after an SC fence can't read before the last SC store before that fence
                // (C++17 §32.4 [atomics.order] paragraph 4).
                false
            } else if is_seqcst && store_elem.store_timestamp <= clocks.read_seqcst[store_elem.store_thread] {
                // An SC load can read only the last store before the last SC fence
                // (paragraph 5).
                false
            } else if is_seqcst && store_elem.load_info.sc_loaded {
                // An SC load can't read before a store an earlier SC load read
                // (coherence-ordered-before, C++20 §31.4 paragraphs 3.1, 3.3 and 4.1).
                false
            } else {
                true
            };

            // An SC load ignores all but the last store marked SC.
            let include = if is_seqcst && store_elem.is_seqcst {
                let include = !found_sc;
                found_sc = true;
                include
            } else {
                true
            };
            if include && visit(pos) {
                return;
            }
            if !keep_searching {
                return;
            }
        }
    }

    /// Picks the store a load reads, uniformly among those it may: its position and whether it
    /// is the latest (Miri: `fetch_store`).
    pub(super) fn fetch_store(
        &self,
        is_seqcst: bool,
        clocks: &ThreadClockSet,
        rng: &mut SplitMix64,
    ) -> (usize, bool) {
        let mut count = 0usize;
        self.candidates(is_seqcst, clocks, |_| {
            count += 1;
            false
        });
        assert!(count > 0, "store buffer cannot be empty");
        let mut skip = rng.index(count);
        let mut chosen = usize::MAX;
        self.candidates(is_seqcst, clocks, |pos| {
            if skip == 0 {
                chosen = pos;
                true
            } else {
                skip -= 1;
                false
            }
        });
        (chosen, chosen == self.buffer.len() - 1)
    }

    /// Records a load of the store at `pos` and returns its bytes (Miri: `load_impl`).
    pub(super) fn load(&mut self, pos: usize, index: VectorIdx, clocks: &ThreadClockSet, is_seqcst: bool) -> &StoreElement {
        let store_elem = &mut self.buffer[pos];
        store_elem.load_impl(index, clocks, is_seqcst);
        store_elem
    }

    /// Appends a store (Miri: `store_impl`). An SC store marks SC every store that happens
    /// before it, and, for C++20, every store read by a load that happens before it.
    pub(super) fn store(
        &mut self,
        val: Option<&[u8]>,
        index: VectorIdx,
        thread_clock: &VClock,
        is_seqcst: bool,
        sync_clock: VClock,
    ) {
        let store_elem = StoreElement {
            store_thread: index,
            store_timestamp: thread_clock[index],
            sync_clock,
            val: val.map(Value::from_slice),
            is_seqcst,
            load_info: LoadInfo::default(),
        };
        if self.buffer.len() >= STORE_BUFFER_LIMIT {
            self.buffer.pop_front();
        }
        self.buffer.push_back(store_elem);
        if is_seqcst {
            for elem in self.buffer.iter_mut().rev() {
                if elem.store_timestamp <= thread_clock[elem.store_thread]
                    || elem.load_info.any_before(thread_clock)
                {
                    elem.is_seqcst = true;
                }
            }
        }
    }
}

impl StoreElement {
    /// Only each thread's first load is kept: a later one by the same thread has a greater
    /// timestamp, so it can't happen before a load the first one doesn't (Miri: `load_impl`).
    fn load_impl(&mut self, index: VectorIdx, clocks: &ThreadClockSet, is_seqcst: bool) {
        self.load_info.sc_loaded |= is_seqcst;
        if !self.load_info.timestamps.iter().any(|&(i, _)| i == index) {
            self.load_info.timestamps.push((index, clocks.clock[index]));
        }
    }
}

/// The store buffers of one allocation (Miri: `StoreBufferAlloc`).
#[derive(Debug, Clone, Default)]
pub(super) struct StoreBuffers {
    buffers: RangeObjectMap<StoreBuffer>,
}

impl StoreBuffers {
    /// A non-atomic write happens after every atomic write it doesn't race with, so the
    /// location shows no weak behavior until the next atomic store (Miri: `non_atomic_write`).
    pub(super) fn non_atomic_write(&mut self, range: Span) {
        if range.is_empty() {
            return;
        }
        match self.buffers.overlap(range) {
            Overlap::Perfect(pos) => self.buffers.remove_from_pos(pos),
            // The race check made sure this write is synchronized with them.
            Overlap::Imperfect(pos_range) => self.buffers.remove_pos_range(pos_range),
            Overlap::Empty(_) => {}
        }
    }

    /// The buffer of exactly `range`, if it has one: without one there was no atomic store
    /// there, so nothing outdated to read (Miri: `get_store_buffer`).
    pub(super) fn find(&self, range: Span) -> Option<usize> {
        match self.buffers.overlap(range) {
            Overlap::Perfect(pos) => Some(pos),
            Overlap::Empty(_) | Overlap::Imperfect(_) => None,
        }
    }

    pub(super) fn get_mut(&mut self, pos: usize) -> &mut StoreBuffer {
        self.buffers.get_mut(pos)
    }

    /// The buffer of `range`, made with `init` if missing; buffers it overlaps imperfectly are
    /// dropped, the race check having ordered them before this access (Miri:
    /// `get_or_create_store_buffer_mut`).
    pub(super) fn find_or_create(&mut self, range: Span, init: Option<&[u8]>) -> usize {
        match self.buffers.overlap(range) {
            Overlap::Perfect(pos) => pos,
            Overlap::Empty(pos) => {
                self.buffers.insert_at_pos(pos, range, StoreBuffer::new(init));
                pos
            }
            Overlap::Imperfect(pos_range) => {
                let start = pos_range.start;
                self.buffers.remove_pos_range(pos_range);
                self.buffers.insert_at_pos(start, range, StoreBuffer::new(init));
                start
            }
        }
    }

    #[cfg(test)]
    pub(super) fn count(&self) -> usize {
        self.buffers.len()
    }
}
