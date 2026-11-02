//! Which bytes of an allocation carry a pointer's provenance: a port of Miri's `ProvenanceMap`
//! (`compiler/rustc_middle/src/mir/interpret/allocation/provenance_map.rs`): whole pointers as
//! sorted `(offset, Prov)`, and the bytes of pointers broken up by a partial overwrite or copy
//! as sorted `(offset, Frag)`.
//!
//! Blaze's fragments differ from Miri's `PointerFrag` in one way: they hold no copy of the
//! pointer's bytes, since the evaluator owns the bytes. Fragments of two pointers with the same
//! provenance, put back together at matching positions, read as one pointer where Miri (which
//! compares the bytes too) reads none; any other mix reads as no provenance in both.

use std::ops::Range;

use crate::ids::Prov;

/// One byte of a pointer: its position in the pointer and the pointer's provenance.
// Miri: provenance_map.rs, `PointerFrag`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Frag {
    pub(super) idx: u8,
    pub(super) prov: Prov,
}

// Miri: provenance_map.rs, `ProvenanceMap`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ProvMap {
    /// Whole pointers: each entry covers `ptr_size` bytes from its offset.
    ptrs: Vec<(u64, Prov)>,
    /// Fragments, one per byte.
    bytes: Option<Box<Vec<(u64, Frag)>>>,
}

/// The entries of `map` with keys in `range`.
#[inline]
fn range_of<V>(map: &[(u64, V)], range: Range<u64>) -> &[(u64, V)] {
    let lo = map.partition_point(|(k, _)| *k < range.start);
    let hi = lo + map[lo..].partition_point(|(k, _)| *k < range.end);
    &map[lo..hi]
}

#[inline]
fn range_is_empty<V>(map: &[(u64, V)], range: Range<u64>) -> bool {
    let lo = map.partition_point(|(k, _)| *k < range.start);
    map.get(lo).is_none_or(|(k, _)| *k >= range.end)
}

fn remove_range<V>(map: &mut Vec<(u64, V)>, range: Range<u64>) {
    let lo = map.partition_point(|(k, _)| *k < range.start);
    let hi = lo + map[lo..].partition_point(|(k, _)| *k < range.end);
    map.drain(lo..hi);
}

/// Inserts sorted `items` whose keys lie in a stretch of `map` holding no keys, with one move of
/// what follows (Miri's `SortedMap::insert_presorted`).
fn insert_presorted<V: Copy>(map: &mut Vec<(u64, V)>, items: &[(u64, V)]) {
    let Some(&(first, _)) = items.first() else { return };
    let at = map.partition_point(|(k, _)| *k < first);
    debug_assert!(map.get(at).is_none_or(|(k, _)| *k > items[items.len() - 1].0));
    map.splice(at..at, items.iter().copied());
}

impl ProvMap {
    pub(super) fn new() -> ProvMap {
        ProvMap { ptrs: Vec::new(), bytes: None }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.ptrs.is_empty() && self.bytes.as_ref().is_none_or(|b| b.is_empty())
    }

    // Miri: provenance_map.rs, `adjusted_range_ptrs`.
    #[inline]
    fn adjusted_range_ptrs(range: Range<u64>, ptr_size: u64) -> Range<u64> {
        range.start.saturating_sub(ptr_size - 1)..range.end
    }

    /// Whole pointers overlapping `range`; for an empty range, those across `start - 1` and
    /// `start`.
    // Miri: provenance_map.rs, `range_ptrs_get`.
    #[inline]
    pub(super) fn range_ptrs(&self, range: Range<u64>, ptr_size: u64) -> &[(u64, Prov)] {
        range_of(&self.ptrs, Self::adjusted_range_ptrs(range, ptr_size))
    }

    // Miri: provenance_map.rs, `range_ptrs_is_empty`.
    #[inline]
    fn range_ptrs_is_empty(&self, range: Range<u64>, ptr_size: u64) -> bool {
        range_is_empty(&self.ptrs, Self::adjusted_range_ptrs(range, ptr_size))
    }

    /// Fragments in `range`.
    // Miri: provenance_map.rs, `range_bytes_get`.
    #[inline]
    pub(super) fn range_bytes(&self, range: Range<u64>) -> &[(u64, Frag)] {
        match &self.bytes {
            Some(bytes) => range_of(bytes, range),
            None => &[],
        }
    }

    // Miri: provenance_map.rs, `range_bytes_is_empty`.
    #[inline]
    fn range_bytes_is_empty(&self, range: Range<u64>) -> bool {
        self.bytes.as_ref().is_none_or(|bytes| range_is_empty(bytes, range))
    }

    // Miri: provenance_map.rs, `get_ptr`.
    #[inline]
    pub(super) fn get_ptr(&self, offset: u64) -> Option<Prov> {
        self.ptrs.binary_search_by_key(&offset, |(k, _)| *k).ok().map(|i| self.ptrs[i].1)
    }

    // Miri: provenance_map.rs, `get_byte`.
    #[inline]
    fn get_byte(&self, offset: u64) -> Option<&Frag> {
        let bytes = self.bytes.as_ref()?;
        bytes.binary_search_by_key(&offset, |(k, _)| *k).ok().map(|i| &bytes[i].1)
    }

    /// The provenance of the pointer at `offset`, from a whole pointer there or from fragments
    /// that put one back together; `None` when there is none or the bytes don't add up (Miri's
    /// `OFFSET_IS_ADDR`: such a read is an integer, not an error).
    // Miri: provenance_map.rs, `read_ptr`.
    pub(super) fn read_ptr(&self, offset: u64, ptr_size: u64) -> Option<Prov> {
        if let Some(prov) = self.get_ptr(offset) {
            return Some(prov);
        }
        let range = offset..offset + ptr_size;
        let no_ptrs = self.range_ptrs_is_empty(range.clone(), ptr_size);
        if no_ptrs && self.range_bytes_is_empty(range) {
            return None;
        }
        if !no_ptrs {
            return None;
        }
        let mut expected = None;
        for idx in 0..ptr_size {
            let frag = self.get_byte(offset + idx)?;
            if frag.prov == Prov::Wildcard {
                continue;
            }
            if u64::from(frag.idx) != idx {
                return None;
            }
            match expected {
                Some(expected) if frag.prov != expected => return None,
                Some(_) => {}
                None => expected = Some(frag.prov),
            }
        }
        Some(expected.unwrap_or(Prov::Wildcard))
    }

    // Miri: provenance_map.rs, `range_empty`.
    pub(super) fn range_empty(&self, range: Range<u64>, ptr_size: u64) -> bool {
        self.range_ptrs_is_empty(range.clone(), ptr_size) && self.range_bytes_is_empty(range)
    }

    /// Every provenance stored here.
    // Miri: provenance_map.rs, `provenances`.
    pub(super) fn provenances(&self) -> impl Iterator<Item = Prov> + '_ {
        let bytes = self.bytes.iter().flat_map(|b| b.iter().map(|(_, frag)| frag.prov));
        self.ptrs.iter().map(|(_, p)| *p).chain(bytes)
    }

    // Miri: provenance_map.rs, `insert_ptr`.
    pub(super) fn insert_ptr(&mut self, offset: u64, prov: Prov, ptr_size: u64) {
        debug_assert!(self.range_empty(offset..offset + ptr_size, ptr_size));
        let at = self.ptrs.partition_point(|(k, _)| *k < offset);
        self.ptrs.insert(at, (offset, prov));
    }

    /// Inserts sorted whole pointers into a cleared range.
    pub(super) fn insert_ptrs_presorted(&mut self, items: &[(u64, Prov)]) {
        insert_presorted(&mut self.ptrs, items);
    }

    /// Inserts sorted fragments into a cleared range.
    pub(super) fn insert_bytes_presorted(&mut self, items: &[(u64, Frag)]) {
        if items.is_empty() {
            return;
        }
        insert_presorted(self.bytes.get_or_insert_with(Box::default), items);
    }

    /// The fragments of the pointer at `ptr_pos` whose positions are in `pos_range`.
    // Miri: provenance_map.rs, `ptr_fragments`.
    fn ptr_fragments(
        pos_range: Range<u64>,
        ptr_pos: u64,
        prov: Prov,
        ptr_size: u64,
    ) -> impl Iterator<Item = (u64, Frag)> {
        let lo = pos_range.start.max(ptr_pos);
        let hi = pos_range.end.min(ptr_pos + ptr_size);
        (lo..hi).map(move |pos| (pos, Frag { idx: (pos - ptr_pos) as u8, prov }))
    }

    /// Removes the provenance of `range`; pointers that stick out of it keep theirs, bytewise,
    /// on the bytes outside.
    // Miri: provenance_map.rs, `clear`.
    pub(super) fn clear(&mut self, range: Range<u64>, ptr_size: u64) {
        let start = range.start;
        let end = range.end;
        if start >= end {
            return;
        }
        if let Some(bytes) = self.bytes.as_mut() {
            remove_range(bytes, start..end);
        }
        let ptrs_range = Self::adjusted_range_ptrs(range, ptr_size);
        if range_is_empty(&self.ptrs, ptrs_range.clone()) {
            return;
        }
        let ptrs = range_of(&self.ptrs, ptrs_range.clone());
        let &(first, first_prov) = ptrs.first().expect("the range has a pointer");
        let &(last, last_prov) = ptrs.last().expect("the range has a pointer");
        if first < start {
            let bytes = self.bytes.get_or_insert_with(Box::default);
            for (pos, frag) in Self::ptr_fragments(0..start, first, first_prov, ptr_size) {
                let at = bytes.partition_point(|(k, _)| *k < pos);
                bytes.insert(at, (pos, frag));
            }
        }
        if last + ptr_size > end {
            let bytes = self.bytes.get_or_insert_with(Box::default);
            for (pos, frag) in Self::ptr_fragments(end..u64::MAX, last, last_prov, ptr_size) {
                let at = bytes.partition_point(|(k, _)| *k < pos);
                bytes.insert(at, (pos, frag));
            }
        }
        remove_range(&mut self.ptrs, ptrs_range);
    }

    /// The provenance of `range`, offsets from its start, into `ptrs` and `bytes` (cleared
    /// first): whole pointers inside it, and fragments of what is in it only in part.
    // Miri: provenance_map.rs, `prepare_copy`, into buffers the caller keeps rather than a fresh
    // `ProvenanceCopy`.
    pub(super) fn prepare_copy(
        &self,
        range: Range<u64>,
        ptr_size: u64,
        ptrs: &mut Vec<(u64, Prov)>,
        bytes: &mut Vec<(u64, Frag)>,
    ) {
        ptrs.clear();
        bytes.clear();
        let shift = range.start;
        let size = range.end - range.start;
        if size >= ptr_size {
            let adjusted_end = range.end - (ptr_size - 1);
            ptrs.extend(
                range_of(&self.ptrs, range.start..adjusted_end).iter().map(|&(o, p)| (o - shift, p)),
            );
        }
        let begin_overlap = self.range_ptrs(range.start..range.start, ptr_size).first().copied();
        let end_overlap = self.range_ptrs(range.end..range.end, ptr_size).first().copied();
        if begin_overlap.is_none() && end_overlap.is_none() && self.bytes.is_none() {
            return;
        }
        if let Some((pos, prov)) = begin_overlap {
            let end = (pos + ptr_size).min(range.end);
            for (pos, frag) in Self::ptr_fragments(range.start..end, pos, prov, ptr_size) {
                bytes.push((pos - shift, frag));
            }
        }
        bytes.extend(self.range_bytes(range.clone()).iter().map(|&(o, f)| (o - shift, f)));
        if let Some((pos, prov)) = end_overlap
            && begin_overlap.is_none_or(|(begin, _)| begin != pos)
        {
            assert!(pos >= range.start);
            for (pos, frag) in Self::ptr_fragments(pos..range.end, pos, prov, ptr_size) {
                let pos = pos - shift;
                debug_assert!(bytes.last().is_none_or(|(o, _)| *o < pos));
                bytes.push((pos, frag));
            }
        }
    }

    /// Writes `repeat` copies of what `prepare_copy` made, each of `size` bytes, from `start`,
    /// into a range already clear of provenance.
    // Miri: provenance_map.rs, `apply_copy`.
    pub(super) fn apply_copy(
        &mut self,
        ptrs: &[(u64, Prov)],
        bytes: &[(u64, Frag)],
        start: u64,
        size: u64,
        repeat: u64,
    ) {
        if !ptrs.is_empty() {
            let at = self.ptrs.partition_point(|(k, _)| *k < start);
            let items = (0..repeat)
                .flat_map(|chunk| ptrs.iter().map(move |&(o, p)| (o + start + chunk * size, p)));
            self.ptrs.splice(at..at, items);
        }
        if !bytes.is_empty() {
            let map = self.bytes.get_or_insert_with(Box::default);
            let at = map.partition_point(|(k, _)| *k < start);
            let items = (0..repeat)
                .flat_map(|chunk| bytes.iter().map(move |&(o, f)| (o + start + chunk * size, f)));
            map.splice(at..at, items);
        }
    }
}
