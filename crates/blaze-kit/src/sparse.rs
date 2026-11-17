//! [`SparseSet<I, A>`]: a set of ids below a fixed domain with O(1) insert, remove and contains,
//! a clear that costs the members, and iteration in insertion order: the worklist of a dataflow
//! pass, the visited marks of a walk that is restarted per item (after Briggs and Torczon, "An
//! efficient representation for sparse sets", 1993, with membership moved to a bit array).
//!
//! `dense` holds the members in insertion order. Membership is a bit per id of the domain;
//! `pos[id]` is the position of `id` in `dense`, written on insert and read only by a removal.
//! A removal leaves a tombstone in `dense`, so the order of the rest stands; when the
//! tombstones outnumber the members (and are at least [`COMPACT_FROM`]) an insert first
//! squeezes them out, so a removal is O(1) amortized and `dense` is at most twice the members
//! plus that bound. [`SparseSet::pop_front`] (a FIFO worklist) and [`SparseSet::pop_back`]
//! (LIFO) take members from either end.
//!
//! # Floor
//!
//! Memory: a bit per id of the domain plus 4 bytes (`pos`, zeroed once when made), and 4 per
//! member (`dense`, at most twice that with tombstones). `contains` reads one bit: one line of
//! an array a 32nd the size of `pos`, so it stays in cache where `pos` would not (512 KB of
//! bits for a 4M domain, against 16 MB of positions). An insert reads that bit and stores the
//! position (a store, which does not wait for its line) and the member; a removal reads the
//! bit and the position and writes the tombstone. A clear walks the members to clear their
//! bits, or the bit array when that is smaller.
//!
//! # Measured (`examples/sets.rs`, release, one Mac run, 2026-11-15)
//!
//! Before (Briggs and Torczon as published: `contains` read `sparse[id]` then `dense` at it):
//! 10.7 ns an insert at a 4M domain against 2.67 for a `Vec<bool>` mark and a list. Cause
//! (from the sizes, not profiled): every insert's membership test read the 16 MB position
//! array at a random place, a cache miss, where the marks are 4 MB and a bit array 512 KB.
//! Fix: membership is the bit array; the position array is only stored to on insert and read
//! on removal. After (2026-11-15, one run, this machine): 1.91 ns an insert against 1.03 for
//! the `Vec<bool>` and list at a 262,144 domain; 3.48 against 1.49 at 4,194,304. About twice
//! the plain structure on insert: the position store is the price of removal by id.
//!
//! # When it is the right choice, and when the plain one is
//!
//! Right: a sparse subset of a large domain that is emptied and refilled many times (a clear
//! costs its members, not the domain), iterated in insertion order (the order of work), and
//! whose members are also removed one at a time by id, in O(1) with the order of the rest
//! kept. Only removal by id needs `pos`, and `pos` is what an insert pays for.
//!
//! Wrong: a set that is only inserted into, cleared or popped as a worklist, never removed
//! from by id. There a mark per id and a list beside it (a [`TableSet`](crate::TableSet) and a
//! `Vec`) is half the insert cost above, with the same clear by walking the list. A set that
//! lives long, is iterated in id order, is combined with other sets or holds much of its
//! domain is a plain bit set ([`TableSet`](crate::TableSet), or a
//! [`ChunkedBitSet`](crate::ChunkedBitSet) when copied and combined).

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

/// A set of ids `I` below a domain, in `A` (see the module): insert, remove and contains O(1), a bit and 4 bytes an id of the domain plus 4 a member.
pub struct SparseSet<I: Id, A: Allocator> {
    dense: Vec<u32, A>,
    /// A bit per id of the domain: whether it is in.
    bits: Vec<u64, A>,
    /// The position in `dense` of each id that is in; anything for the rest.
    pos: Vec<u32, A>,
    /// `dense[..head]` are tombstones: members taken from the front.
    head: usize,
    /// Members in.
    live: usize,
    _id: PhantomData<fn(I) -> I>,
}

impl<I: Id, A: Allocator + Clone> SparseSet<I, A> {
    /// An empty set of ids below `domain` (at most [`MOST_DOMAIN`]; more is a bug and panics):
    /// `4.125 * domain` bytes zeroed now, the members' room as they come.
    #[track_caller]
    pub fn try_new_in(domain: u32, alloc: A) -> Result<SparseSet<I, A>, AllocFailure> {
        if domain > MOST_DOMAIN {
            panic!("blaze_kit: a `{}` of {domain} ids; at most {MOST_DOMAIN}", type_name::<Self>());
        }
        let ids = blaze_int::idx(domain);
        let mut bits = Vec::try_with_capacity_in(ids.div_ceil(64), alloc.clone())?;
        bits.resize(ids.div_ceil(64), 0);
        let mut pos = Vec::try_with_capacity_in(ids, alloc.clone())?;
        pos.resize(ids, 0);
        Ok(SparseSet { dense: Vec::new_in(alloc), bits, pos, head: 0, live: 0, _id: PhantomData })
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
        self.pos.len()
    }

    /// Members in.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    #[inline]
    fn bit(&self, at: usize) -> bool {
        self.bits.get(at / 64).is_some_and(|word| word >> (at % 64) & 1 != 0)
    }

    #[inline]
    fn set_bit(&mut self, at: usize, on: bool) {
        let word = &mut self.bits[at / 64];
        let mask = 1u64 << (at % 64);
        if on { *word |= mask } else { *word &= !mask }
    }

    #[inline]
    pub fn contains(&self, id: I) -> bool {
        self.bit(id.index())
    }

    /// Adds `id`; whether it was new. An id outside the domain is a bug and panics; refused
    /// with nothing changed when `dense` cannot grow.
    #[inline]
    #[track_caller]
    pub fn try_insert(&mut self, id: I) -> Result<bool, AllocFailure> {
        let at = id.index();
        if at >= self.pos.len() {
            panic!("blaze_kit: id {at} is outside the domain of `{}`, {}", type_name::<Self>(), self.pos.len());
        }
        if self.bit(at) {
            return Ok(false);
        }
        let tombs = self.dense.len() - self.live;
        if tombs >= COMPACT_FROM && tombs > self.live {
            self.compact();
        }
        self.dense.try_reserve(1)?;
        self.set_bit(at, true);
        // `dense` stays below `2 * MOST_DOMAIN + COMPACT_FROM`, which fits 32 bits.
        self.pos[at] = self.dense.len().truncate_to::<u32>();
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
        let at = id.index();
        if !self.bit(at) {
            return false;
        }
        self.set_bit(at, false);
        let slot = blaze_int::idx(self.pos[at]);
        self.dense[slot] = TOMB;
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
                self.set_bit(blaze_int::idx(raw), false);
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
                self.set_bit(blaze_int::idx(raw), false);
                self.taken();
                return Some(I::from_u32(raw));
            }
        }
        None
    }

    /// Empties the set, keeping its room: clears the members' bits, or the whole bit array
    /// when that is fewer words.
    #[inline]
    pub fn clear(&mut self) {
        if self.dense.len() - self.head > self.bits.len() {
            self.bits.iter_mut().for_each(|word| *word = 0);
        } else {
            for k in self.head..self.dense.len() {
                let raw = self.dense[k];
                if raw != TOMB {
                    self.set_bit(blaze_int::idx(raw), false);
                }
            }
        }
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
                self.pos[blaze_int::idx(raw)] = write.truncate_to::<u32>();
            }
            write += 1;
        }
        self.dense.truncate(write);
        self.head = 0;
    }
}
