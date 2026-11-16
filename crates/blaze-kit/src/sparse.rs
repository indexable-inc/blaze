//! [`SparseSet<I, A>`]: a set of ids below a fixed domain with O(1) insert, remove, contains
//! and clear, iterated in insertion order: the worklist of a dataflow pass, the visited marks of
//! a walk that is restarted per item (after Briggs and Torczon, "An efficient representation
//! for sparse sets", 1993).
//!
//! `dense` holds the members in insertion order; `sparse[id]` is the position of `id` in
//! `dense`, believed only when `dense` holds `id` there. A removal leaves a tombstone in place,
//! so the order of the rest stands; when the tombstones outnumber the members (and are at least
//! [`COMPACT_FROM`]) an insert first squeezes them out, so a removal is O(1) amortized and
//! `dense` is at most twice the members plus that bound. [`SparseSet::pop_front`] (a FIFO
//! worklist) and [`SparseSet::pop_back`] (LIFO) take members from either end.
//!
//! # Floor
//!
//! Memory: 4 bytes per id of the domain (`sparse`, zeroed once when made) plus 4 per member
//! (`dense`, at most twice that with tombstones). `contains`, `insert` and `remove` touch two
//! lines (the id's `sparse` slot and its `dense` slot); `clear` touches none; iteration reads
//! only `dense`.
//!
//! # When the plain `BitSet` (blaze-ids) is the right one instead
//!
//! A bit set is a 32nd of `sparse`'s memory and combines with another set a word at a time.
//! Take it when the set lives long, is iterated in id order, is combined with other sets, or
//! holds a large part of its domain. Take this one when the set is emptied and refilled many
//! times (per item, per query), when iteration must cost the members and not the domain, or
//! when the order of insertion is the order of work.

use core::{alloc::Allocator, any::type_name, marker::PhantomData};

use blaze_int::Int;
use blaze_own::{AllocFailure, Vec};

use crate::{
    error::fail,
    id::Id,
    plant::{self, Plant},
};

/// A removed member's slot in `dense`.
const TOMB: u32 = u32::MAX;

/// The fewest tombstones that are squeezed out.
pub const COMPACT_FROM: usize = 32;

/// The largest domain: `dense` holds at most twice the members plus [`COMPACT_FROM`], and its
/// positions are 32-bit.
pub const MOST_DOMAIN: u32 = 1 << 31;

/// A set of ids `I` below a domain, in `A` (see the module).
pub struct SparseSet<I: Id, A: Allocator> {
    dense: Vec<u32, A>,
    sparse: Vec<u32, A>,
    /// `dense[..head]` are tombstones: members taken from the front.
    head: usize,
    /// Members in.
    live: usize,
    _id: PhantomData<fn(I) -> I>,
}

impl<I: Id, A: Allocator + Clone> SparseSet<I, A> {
    /// An empty set of ids below `domain` (at most [`MOST_DOMAIN`]; more is a bug and panics):
    /// `4 * domain` bytes zeroed now, the members' room as they come.
    #[track_caller]
    pub fn try_new_in(domain: u32, alloc: A) -> Result<SparseSet<I, A>, AllocFailure> {
        if domain > MOST_DOMAIN {
            panic!("blaze_kit: a `{}` of {domain} ids; at most {MOST_DOMAIN}", type_name::<Self>());
        }
        let ids = blaze_int::idx(domain);
        let mut sparse = Vec::try_with_capacity_in(ids, alloc.clone())?;
        sparse.resize(ids, 0);
        Ok(SparseSet { dense: Vec::new_in(alloc), sparse, head: 0, live: 0, _id: PhantomData })
    }

    /// [`SparseSet::try_new_in`], panicking with the refusal.
    #[track_caller]
    pub fn new_in(domain: u32, alloc: A) -> SparseSet<I, A> {
        match SparseSet::try_new_in(domain, alloc) {
            Ok(set) => set,
            Err(failure) => fail(failure),
        }
    }
}

impl<I: Id, A: Allocator> SparseSet<I, A> {
    /// The ids it can hold: `0..domain`.
    pub fn domain(&self) -> usize {
        self.sparse.len()
    }

    /// Members in.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    #[inline]
    pub fn contains(&self, id: I) -> bool {
        let raw = id.to_u32();
        match self.sparse.get(id.index()) {
            Some(&at) => self.dense.get(blaze_int::idx(at)) == Some(&raw),
            None => false,
        }
    }

    /// Adds `id`; whether it was new. An id outside the domain is a bug and panics; refused
    /// with nothing changed when `dense` cannot grow.
    #[inline]
    #[track_caller]
    pub fn try_insert(&mut self, id: I) -> Result<bool, AllocFailure> {
        let at = id.index();
        if at >= self.sparse.len() {
            panic!("blaze_kit: id {at} is outside the domain of `{}`, {}", type_name::<Self>(), self.sparse.len());
        }
        if self.contains(id) {
            return Ok(false);
        }
        let tombs = self.dense.len() - self.live;
        if tombs >= COMPACT_FROM && tombs > self.live {
            self.compact();
        }
        self.dense.try_reserve(1)?;
        // `dense` stays below `2 * MOST_DOMAIN + COMPACT_FROM`, which fits 32 bits.
        self.sparse[at] = self.dense.len().truncate_to::<u32>();
        self.dense.push(id.to_u32());
        self.live += 1;
        Ok(true)
    }

    /// [`SparseSet::try_insert`], panicking with the refusal.
    #[inline]
    #[track_caller]
    pub fn insert(&mut self, id: I) -> bool {
        match self.try_insert(id) {
            Ok(new) => new,
            Err(failure) => fail(failure),
        }
    }

    /// Takes `id` out, the order of the rest kept; whether it was in.
    #[inline]
    pub fn remove(&mut self, id: I) -> bool {
        if !self.contains(id) {
            return false;
        }
        let at = blaze_int::idx(self.sparse[id.index()]);
        self.dense[at] = TOMB;
        self.taken();
        true
    }

    /// One member fewer; an empty set drops its tombstones.
    #[inline]
    fn taken(&mut self) {
        self.live -= 1;
        if self.live == 0 {
            self.dense.clear();
            self.head = 0;
        }
    }

    /// The oldest member out (a FIFO worklist).
    #[inline]
    pub fn pop_front(&mut self) -> Option<I> {
        while self.head < self.dense.len() {
            let raw = self.dense[self.head];
            self.dense[self.head] = TOMB;
            self.head += 1;
            if raw != TOMB {
                self.taken();
                return Some(I::from_u32(raw));
            }
        }
        None
    }

    /// The newest member out (a LIFO worklist).
    #[inline]
    pub fn pop_back(&mut self) -> Option<I> {
        while let Some(raw) = self.dense.pop() {
            if self.head > self.dense.len() {
                self.head = self.dense.len();
            }
            if raw != TOMB {
                self.taken();
                return Some(I::from_u32(raw));
            }
        }
        None
    }

    /// Empties the set in O(1), keeping its room.
    #[inline]
    pub fn clear(&mut self) {
        self.dense.clear();
        self.head = 0;
        self.live = 0;
    }

    /// The members in insertion order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = I> + '_ {
        self.dense[self.head..].iter().filter(|&&raw| raw != TOMB).map(|&raw| I::from_u32(raw))
    }

    /// Squeezes the tombstones out, the order kept.
    fn compact(&mut self) {
        let mut write = 0usize;
        let last = self.dense.len();
        for read in self.head..last {
            let raw = self.dense[read];
            if raw == TOMB {
                continue;
            }
            self.dense[write] = raw;
            let stale = plant::on(Plant::CompactKeepsStalePosition) && write + 1 == self.live;
            if !stale {
                self.sparse[blaze_int::idx(raw)] = write.truncate_to::<u32>();
            }
            write += 1;
        }
        self.dense.truncate(write);
        self.head = 0;
    }
}
