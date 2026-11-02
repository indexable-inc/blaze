//! A map from the byte offsets of an allocation to data, stored as sorted ranges of equal
//! data: ranges split when part of one is changed and opportunistically merge back.
//!
//! Miri: `src/tools/miri/src/data_structures/dedup_range_map.rs` (`DedupRangeMap`), ported
//! with its merge heuristic unchanged, since where ranges split decides the offset an error
//! names.

use std::ops::Range;

#[derive(Clone, Debug)]
struct Elem<T> {
    /// Never empty.
    range: Range<u64>,
    data: T,
}

#[derive(Clone, Debug)]
pub(super) struct RangeMap<T> {
    v: Vec<Elem<T>>,
}

impl<T> RangeMap<T> {
    /// `0..size` mapped to `init`.
    pub(super) fn new(size: u64, init: T) -> RangeMap<T> {
        let v = if size > 0 { vec![Elem { range: 0..size, data: init }] } else { Vec::new() };
        RangeMap { v }
    }

    /// Back to `RangeMap::new(size, init)`, keeping the buffer.
    pub(super) fn reset(&mut self, size: u64, init: T) {
        self.v.clear();
        if size > 0 {
            self.v.push(Elem { range: 0..size, data: init });
        }
    }

    pub(super) fn size(&self) -> u64 {
        self.v.last().map_or(0, |x| x.range.end)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.v.len()
    }

    fn find_offset(&self, offset: u64) -> usize {
        self.v
            .binary_search_by(|elem| {
                if offset < elem.range.start {
                    std::cmp::Ordering::Greater
                } else if offset >= elem.range.end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .unwrap()
    }

    /// Everything in `offset..offset + len`, without splitting the edges.
    pub(super) fn iter(&self, offset: u64, len: u64) -> impl Iterator<Item = (Range<u64>, &T)> {
        let slice: &[Elem<T>] = if len == 0 { &[] } else { &self.v[self.find_offset(offset)..] };
        let end = offset + len;
        assert!(end <= self.size(), "iterating beyond the bounds of this RangeMap");
        slice.iter().take_while(move |elem| elem.range.start < end).map(|elem| (elem.range.clone(), &elem.data))
    }

    pub(super) fn iter_mut_all(&mut self) -> impl Iterator<Item = (Range<u64>, &mut T)> {
        self.v.iter_mut().map(|elem| (elem.range.clone(), &mut elem.data))
    }

    pub(super) fn iter_all(&self) -> impl Iterator<Item = (Range<u64>, &T)> {
        self.v.iter().map(|elem| (elem.range.clone(), &elem.data))
    }

    /// Splits the element at `index` so the second part starts at `split_offset`; whether a
    /// split was needed.
    fn split_index(&mut self, index: usize, split_offset: u64) -> bool
    where
        T: Clone,
    {
        let elem = &mut self.v[index];
        if split_offset == elem.range.start || split_offset == elem.range.end {
            return false;
        }
        debug_assert!(elem.range.contains(&split_offset));
        let second_range = split_offset..elem.range.end;
        elem.range.end = split_offset;
        let second = Elem { range: second_range, data: elem.data.clone() };
        self.v.insert(index + 1, second);
        true
    }

    /// Everything in `offset..offset + len`, split so changes stay inside it, merging equal
    /// neighbours on the way (Miri: `DedupRangeMap::iter_mut`).
    pub(super) fn iter_mut(
        &mut self,
        offset: u64,
        len: u64,
    ) -> impl Iterator<Item = (Range<u64>, &mut T)>
    where
        T: Clone + PartialEq,
    {
        let slice: &mut [Elem<T>] = if len == 0 {
            &mut []
        } else {
            let mut first_idx = self.find_offset(offset);
            if self.split_index(first_idx, offset) {
                first_idx += 1;
            }
            let first_idx = first_idx;
            let mut equal_since_idx = first_idx;
            // Miri's cutoff: stop trying to merge after too many failures.
            let mut successful_merge_count = 3usize;
            let mut end_idx = first_idx;
            loop {
                let done = self.v[end_idx].range.end >= offset + len;
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
                        let equal_until = self.v[end_idx - 1].range.end;
                        self.v[equal_since_idx].range.end = equal_until;
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
        slice.iter_mut().map(|elem| (elem.range.clone(), &mut elem.data))
    }

    /// Merges every run of equal neighbours (after a GC).
    pub(super) fn merge_adjacent_thorough(&mut self)
    where
        T: PartialEq,
    {
        let clean = Vec::with_capacity(self.v.len());
        for elem in std::mem::replace(&mut self.v, clean) {
            if let Some(prev) = self.v.last_mut()
                && prev.data == elem.data
            {
                assert_eq!(prev.range.end, elem.range.start);
                prev.range.end = elem.range.end;
                continue;
            }
            self.v.push(elem);
        }
    }
}
