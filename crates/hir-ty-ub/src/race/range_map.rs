//! The two maps the race detector keeps per allocation, ported from Miri's
//! `src/tools/miri/src/data_structures/`:
//!
//! - [`RangeMap`] (`dedup_range_map.rs`): every byte of an allocation to a value, kept as
//!   sorted runs that split on access and merge back when neighbours are equal;
//! - [`RangeObjectMap`] (`range_object_map.rs`): disjoint ranges to objects that never split
//!   (the store buffer of each atomic location).
//!
//! Both are one sorted `Vec` searched by binary search, as Miri's are.

use std::ops::Range;

use crate::ids::Span;

#[derive(Clone, Debug)]
struct Elem<T> {
    /// Never empty.
    start: u64,
    end: u64,
    data: T,
}

/// Bytes `0..size` to values, by runs (Miri: `DedupRangeMap`).
#[derive(Clone, Debug)]
pub(super) struct RangeMap<T> {
    v: Vec<Elem<T>>,
}

impl<T> RangeMap<T> {
    /// `size` bytes, each `init`.
    pub(super) fn new(size: u64, init: T) -> RangeMap<T> {
        let v = if size > 0 { vec![Elem { start: 0, end: size, data: init }] } else { Vec::new() };
        RangeMap { v }
    }

    pub(super) fn size(&self) -> u64 {
        self.v.last().map_or(0, |x| x.end)
    }

    /// The element holding `offset`, which must be in bounds.
    fn find_offset(&self, offset: u64) -> usize {
        self.v
            .binary_search_by(|elem| {
                if offset < elem.start {
                    std::cmp::Ordering::Greater
                } else if offset >= elem.end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .unwrap()
    }

    /// The runs overlapping `range`, without splitting them.
    pub(super) fn iter(&self, range: Span) -> impl Iterator<Item = (Range<u64>, &T)> {
        let slice: &[Elem<T>] = if range.len == 0 {
            &[]
        } else {
            let first_idx = self.find_offset(range.start);
            &self.v[first_idx..]
        };
        let end = range.end();
        assert!(end <= self.size(), "iterating beyond the bounds of this RangeMap");
        slice.iter().take_while(move |elem| elem.start < end).map(|elem| (elem.start..elem.end, &elem.data))
    }

    /// Splits the element at `index` so the second part starts at `split_offset`; whether it had
    /// to.
    fn split_index(&mut self, index: usize, split_offset: u64) -> bool
    where
        T: Clone,
    {
        let elem = &mut self.v[index];
        if split_offset == elem.start || split_offset == elem.end {
            return false;
        }
        debug_assert!(
            (elem.start..elem.end).contains(&split_offset),
            "the `split_offset` is not in the element to be split"
        );
        let second = Elem { start: split_offset, end: elem.end, data: elem.data.clone() };
        elem.end = split_offset;
        self.v.insert(index + 1, second);
        true
    }

    /// The runs of exactly `range`, split at its edges so a change stays inside it, merging
    /// equal neighbours on the way (Miri: `DedupRangeMap::iter_mut`).
    pub(super) fn iter_mut(&mut self, range: Span) -> impl Iterator<Item = (Range<u64>, &mut T)>
    where
        T: Clone + PartialEq,
    {
        let offset = range.start;
        let len = range.len;
        let slice: &mut [Elem<T>] = if len == 0 {
            &mut []
        } else {
            let mut first_idx = self.find_offset(offset);
            if self.split_index(first_idx, offset) {
                first_idx += 1;
            }
            let first_idx = first_idx;
            // Find the end with a linear scan (the iteration does one anyway), merging runs of
            // equal elements on the way; stop merging after a few failures.
            let mut equal_since_idx = first_idx;
            let mut successful_merge_count = 3usize;
            let mut end_idx = first_idx;
            loop {
                let done = self.v[end_idx].end >= offset + len;
                end_idx += 1;
                debug_assert!(
                    done || end_idx < self.v.len(),
                    "iter_mut: end-offset {} is out-of-bounds",
                    offset + len
                );
                if successful_merge_count > 0
                    && (done || self.v[end_idx].data != self.v[equal_since_idx].data)
                {
                    let removed_elems = end_idx - equal_since_idx - 1;
                    if removed_elems > 0 {
                        let equal_until = self.v[end_idx - 1].end;
                        self.v[equal_since_idx].end = equal_until;
                        self.v.drain(equal_since_idx + 1..end_idx);
                        end_idx -= removed_elems;
                        successful_merge_count += removed_elems;
                    } else {
                        successful_merge_count -= 1;
                    }
                    equal_since_idx = end_idx;
                }
                if done {
                    break;
                }
            }
            let end_idx = end_idx - 1;
            self.split_index(end_idx, offset + len);
            &mut self.v[first_idx..=end_idx]
        };
        slice.iter_mut().map(|elem| (elem.start..elem.end, &mut elem.data))
    }

    #[cfg(test)]
    pub(super) fn runs(&self) -> usize {
        self.v.len()
    }
}

/// Where an access falls among the objects of a [`RangeObjectMap`] (Miri: `AccessType`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Overlap {
    /// Exactly one object's range.
    Perfect(usize),
    /// No object; one would go at this position.
    Empty(usize),
    /// Part of one or more objects.
    Imperfect(Range<usize>),
}

#[derive(Clone, Debug)]
struct Object<T> {
    range: Span,
    data: T,
}

/// Disjoint ranges to objects that keep their range until removed (Miri: `RangeObjectMap`).
#[derive(Clone, Debug)]
pub(super) struct RangeObjectMap<T> {
    v: Vec<Object<T>>,
}

impl<T> Default for RangeObjectMap<T> {
    fn default() -> Self {
        RangeObjectMap { v: Vec::new() }
    }
}

impl<T> RangeObjectMap<T> {
    /// The object holding `offset`, or where one holding it would go.
    fn find_offset(&self, offset: u64) -> Result<usize, usize> {
        self.v.binary_search_by(|elem| {
            if offset < elem.range.start {
                std::cmp::Ordering::Greater
            } else if offset >= elem.range.end() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
    }

    /// How an access of `range` (never empty) overlaps the objects.
    pub(super) fn overlap(&self, range: Span) -> Overlap {
        debug_assert!(!range.is_empty());
        match self.find_offset(range.start) {
            Ok(pos) => {
                let elem = &self.v[pos];
                if elem.range == range {
                    Overlap::Perfect(pos)
                } else {
                    let end_pos = match self.find_offset(range.end() - 1) {
                        Ok(inclusive_pos) => inclusive_pos + 1,
                        Err(exclusive_pos) => exclusive_pos,
                    };
                    Overlap::Imperfect(pos..end_pos)
                }
            }
            Err(pos) => match self.find_offset(range.end() - 1) {
                Err(end_pos) => {
                    if pos == end_pos {
                        Overlap::Empty(pos)
                    } else {
                        Overlap::Imperfect(pos..end_pos)
                    }
                }
                Ok(end_pos) => Overlap::Imperfect(pos..end_pos + 1),
            },
        }
    }

    /// Puts an object at `pos`, which [`RangeObjectMap::overlap`] gave.
    pub(super) fn insert_at_pos(&mut self, pos: usize, range: Span, data: T) {
        self.v.insert(pos, Object { range, data });
        if pos > 0 {
            assert!(self.v[pos - 1].range.end() <= range.start);
        }
        if pos < self.v.len() - 1 {
            assert!(range.end() <= self.v[pos + 1].range.start);
        }
    }

    pub(super) fn remove_pos_range(&mut self, pos_range: Range<usize>) {
        self.v.drain(pos_range);
    }

    pub(super) fn remove_from_pos(&mut self, pos: usize) {
        self.v.remove(pos);
    }

    pub(super) fn get_mut(&mut self, pos: usize) -> &mut T {
        &mut self.v[pos].data
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.v.len()
    }
}
