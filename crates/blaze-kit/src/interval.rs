//! [`IntervalSet<I, A>`]: a set of ids kept as sorted, disjoint, non-touching half-open ranges:
//! liveness ranges and borrow-check regions, where members come in runs.
//!
//! The ranges are an [`InlineVec`] of two inline, as rustc's `IntervalSet` keeps a
//! `SmallVec<[(u32, u32); 2]>`: most regions are one or two runs, which costs no allocation.
//! Two ranges never touch (`a.end < b.start`), so a set has one form and equal sets are equal
//! range by range.
//!
//! # Floor
//!
//! 8 bytes per range, whatever its length. `contains` is a binary search over the ranges:
//! `log2 r` probes, one line for up to 8 ranges. `insert_range` and `remove_range` search, then
//! move the ranges after the change once (`O(log r + r)` worst case, `O(log r)` at the end,
//! where liveness adds).
//!
//! # When the plain `BitSet` (blaze-ids) is the right one instead
//!
//! When the members do not come in runs: past one range per 64 ids a bit set is smaller, and it
//! unions and intersects a word at a time where this merges ranges. A borrow-check region over
//! a long body with scattered points is a [`ChunkedBitSet`](crate::ChunkedBitSet).
//!
//! # Measured (`examples/sets.rs`, release, one Mac run, 2026-11-15)
//!
//! Against a `BTreeSet` of the ids: insert 58.7 to 68.9 ns against 2,128 to 2,156; a probe
//! 10.1 to 12.8 against 51.1 to 53.2. A second run (2026-11-15, one run, this machine):
//! insert 43 to 48 ns against 1,647 to 1,806.

use core::{alloc::Allocator, marker::PhantomData, ops::Range};

use blaze_own::AllocFailure;

use crate::{
    error::fail,
    id::Id,
    inline_vec::InlineVec,
    plant::{self, Plant},
};

/// One run: `start..end`, never empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    start: u32,
    end: u32,
}

/// A set of ids `I` as sorted disjoint ranges, in `A` (see the module).
pub struct IntervalSet<I: Id, A: Allocator> {
    runs: InlineVec<Run, 2, A>,
    _id: PhantomData<fn(I) -> I>,
}

impl<I: Id, A: Allocator> IntervalSet<I, A> {
    pub const fn new_in(alloc: A) -> IntervalSet<I, A> {
        IntervalSet { runs: InlineVec::new_in(alloc), _id: PhantomData }
    }

    /// How many ranges.
    pub fn ranges_len(&self) -> usize {
        self.runs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// How many ids are in.
    pub fn count(&self) -> u64 {
        self.runs.iter().map(|r| u64::from(r.end - r.start)).sum()
    }

    #[inline]
    pub fn contains(&self, id: I) -> bool {
        let x = id.to_u32();
        let at = self.runs.partition_point(|r| r.end <= x);
        self.runs.get(at).is_some_and(|r| r.start <= x)
    }

    /// Whether every id of `range` is in.
    pub fn contains_range(&self, range: Range<I>) -> bool {
        let (start, end) = (range.start.to_u32(), range.end.to_u32());
        if start >= end {
            return true;
        }
        let at = self.runs.partition_point(|r| r.end <= start);
        self.runs.get(at).is_some_and(|r| r.start <= start && end <= r.end)
    }

    /// Adds `id`; whether it was new.
    pub fn try_insert(&mut self, id: I) -> Result<bool, AllocFailure> {
        let x = id.to_u32();
        self.try_insert_raw(x, x + 1)
    }

    /// Adds every id of `range`; whether any was new. Refused with nothing changed.
    pub fn try_insert_range(&mut self, range: Range<I>) -> Result<bool, AllocFailure> {
        self.try_insert_raw(range.start.to_u32(), range.end.to_u32())
    }

    /// [`IntervalSet::try_insert`], panicking with the refusal.
    pub fn insert(&mut self, id: I) -> bool {
        match self.try_insert(id) {
            Ok(new) => new,
            Err(failure) => fail(failure),
        }
    }

    /// [`IntervalSet::try_insert_range`], panicking with the refusal.
    pub fn insert_range(&mut self, range: Range<I>) -> bool {
        match self.try_insert_range(range) {
            Ok(new) => new,
            Err(failure) => fail(failure),
        }
    }

    fn try_insert_raw(&mut self, start: u32, end: u32) -> Result<bool, AllocFailure> {
        if start >= end {
            return Ok(false);
        }
        // The first range that ends at or after `start` (it touches or overlaps), and the
        // first that starts after `end` (nor does any after it).
        let first = self.runs.partition_point(|r| r.end < start);
        let past = if plant::on(Plant::IntervalMissesAdjacent) {
            self.runs.partition_point(|r| r.start < end)
        } else {
            self.runs.partition_point(|r| r.start <= end)
        };
        if first == past {
            self.runs.try_insert(first, Run { start, end }).map_err(|refused| refused.why)?;
            return Ok(true);
        }
        let (old_start, old_end) = (self.runs[first].start, self.runs[past - 1].end);
        let merged = Run { start: start.min(old_start), end: end.max(old_end) };
        let grew = past - first > 1 || merged != self.runs[first];
        self.runs[first] = merged;
        self.runs.remove_range(first + 1..past);
        Ok(grew)
    }

    /// Takes `id` out; whether it was in.
    pub fn try_remove(&mut self, id: I) -> Result<bool, AllocFailure> {
        let x = id.to_u32();
        self.try_remove_raw(x, x + 1)
    }

    /// Takes every id of `range` out; whether any was in. Cutting a range in two may need room
    /// for one more: refused with nothing changed.
    pub fn try_remove_range(&mut self, range: Range<I>) -> Result<bool, AllocFailure> {
        self.try_remove_raw(range.start.to_u32(), range.end.to_u32())
    }

    /// [`IntervalSet::try_remove_range`], panicking with the refusal.
    pub fn remove_range(&mut self, range: Range<I>) -> bool {
        match self.try_remove_range(range) {
            Ok(was) => was,
            Err(failure) => fail(failure),
        }
    }

    fn try_remove_raw(&mut self, start: u32, end: u32) -> Result<bool, AllocFailure> {
        if start >= end {
            return Ok(false);
        }
        // The ranges `first..past` overlap `start..end`.
        let first = self.runs.partition_point(|r| r.end <= start);
        let past = self.runs.partition_point(|r| r.start < end);
        if first >= past {
            return Ok(false);
        }
        let head = self.runs[first];
        let tail = self.runs[past - 1];
        let left = (head.start < start).then_some(Run { start: head.start, end: start });
        let right = (tail.end > end).then_some(Run { start: end, end: tail.end });
        match (left, right) {
            (Some(left), Some(right)) if past - first == 1 => {
                self.runs.try_insert(first + 1, right).map_err(|refused| refused.why)?;
                self.runs[first] = left;
            }
            (left, right) => {
                let mut at = first;
                for piece in [left, right].into_iter().flatten() {
                    self.runs[at] = piece;
                    at += 1;
                }
                self.runs.remove_range(at..past);
            }
        }
        Ok(true)
    }

    /// Adds every id of `other`; whether any was new. Refused part way, it holds a subset of
    /// the union that includes what it held.
    pub fn try_union<B: Allocator>(&mut self, other: &IntervalSet<I, B>) -> Result<bool, AllocFailure> {
        let mut grew = false;
        for run in other.runs.iter() {
            grew |= self.try_insert_raw(run.start, run.end)?;
        }
        Ok(grew)
    }

    /// Empties the set, keeping its room.
    pub fn clear(&mut self) {
        self.runs.clear();
    }

    /// The ranges, ascending.
    pub fn ranges(&self) -> impl DoubleEndedIterator<Item = Range<I>> + '_ {
        self.runs.iter().map(|r| I::from_u32(r.start)..I::from_u32(r.end))
    }

    /// The ids, ascending.
    pub fn iter(&self) -> impl Iterator<Item = I> + '_ {
        self.runs.iter().flat_map(|r| (r.start..r.end).map(I::from_u32))
    }

    /// The first id at or after `id` that is in.
    pub fn next_from(&self, id: I) -> Option<I> {
        let x = id.to_u32();
        let at = self.runs.partition_point(|r| r.end <= x);
        self.runs.get(at).map(|r| I::from_u32(r.start.max(x)))
    }

    /// The ranges' bytes, for a cost check: `8 * ranges` past the inline two.
    pub fn bytes(&self) -> usize {
        self.runs.capacity().saturating_mul(8)
    }
}

impl<I: Id, A: Allocator> PartialEq for IntervalSet<I, A> {
    fn eq(&self, other: &Self) -> bool {
        self.runs.as_slice() == other.runs.as_slice()
    }
}

impl<I: Id, A: Allocator> Eq for IntervalSet<I, A> {}

impl<I: Id + core::fmt::Debug, A: Allocator> core::fmt::Debug for IntervalSet<I, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.ranges()).finish()
    }
}
