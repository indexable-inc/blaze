//! The arena of a code memory: ranges for code placed while the program runs, handed out and
//! given back in constant time (TLSF, Masmano et al. 2004: free ranges in lists by a two-level
//! size class, found through two bitmaps, neighbours merged on give-back).

use std::collections::HashMap;
use std::hash::BuildHasherDefault;

use crate::CodeRange;

/// Every range starts and ends on this: the alignment a function's code wants at least.
pub(crate) const GRAIN: u64 = 16;
const SECOND: usize = 16;
const SECOND_BITS: u32 = 4;
const FIRST: usize = 64;
const NONE: u32 = u32::MAX;

/// A hasher for offsets, which are distinct multiples of [`GRAIN`]: one multiply.
#[derive(Default)]
pub(crate) struct OffsetHasher(u64);

impl std::hash::Hasher for OffsetHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 << 8 | u64::from(*byte)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (value >> 4).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

type Map = HashMap<u64, u32, BuildHasherDefault<OffsetHasher>>;

#[derive(Clone, Copy)]
struct Block {
    offset: u64,
    len: u64,
    prev: u32,
    next: u32,
}

/// Not a bump arena (`blaze_mem::Arena`): ranges of code memory are given back one by one and
/// reused, so it is a TLSF heap over offsets.
pub(crate) struct CodeHeap {
    blocks: Vec<Block>,
    /// Indices of `blocks` not in use.
    spare: Vec<u32>,
    heads: Vec<u32>,
    first: u64,
    second: [u16; FIRST],
    by_start: Map,
    by_end: Map,
    free: u64,
}

/// The class of a free range of `len` bytes: the list it is kept in.
fn class(len: u64) -> (usize, usize) {
    let first = 63 - len.leading_zeros();
    let second = if first < SECOND_BITS {
        (len as usize) & (SECOND - 1)
    } else {
        ((len >> (first - SECOND_BITS)) as usize) & (SECOND - 1)
    };
    (first as usize, second)
}

/// The smallest class whose every range is at least `len` bytes.
fn class_at_least(len: u64) -> (usize, usize) {
    let first = 63 - len.leading_zeros();
    let rounded = if first < SECOND_BITS {
        len
    } else {
        len.saturating_add((1u64 << (first - SECOND_BITS)) - 1)
    };
    class(rounded)
}

impl CodeHeap {
    /// A heap of the `len` bytes from `start`, all free.
    pub(crate) fn new(start: u64, len: u64) -> CodeHeap {
        let mut arena = CodeHeap {
            blocks: Vec::new(),
            spare: Vec::new(),
            heads: vec![NONE; FIRST * SECOND],
            first: 0,
            second: [0; FIRST],
            by_start: Map::default(),
            by_end: Map::default(),
            free: 0,
        };
        let start = start.next_multiple_of(GRAIN);
        let len = blaze_mem::align::align_down_u64(len, GRAIN);
        if len != 0 {
            arena.insert(start, len);
        }
        arena
    }

    /// The free bytes.
    pub(crate) fn free(&self) -> u64 {
        self.free
    }

    /// The longest free range.
    pub(crate) fn largest(&self) -> u64 {
        if self.first == 0 {
            return 0;
        }
        let first = 63 - self.first.leading_zeros() as usize;
        let second = 15 - self.second[first].leading_zeros() as usize;
        let mut at = self.heads[first * SECOND + second];
        let mut most = 0;
        while at != NONE {
            most = most.max(self.blocks[at as usize].len);
            at = self.blocks[at as usize].next;
        }
        most
    }

    /// `len` bytes (rounded up to [`GRAIN`]) aligned to `align`, or `None` when no free range
    /// is long enough.
    pub(crate) fn take(&mut self, len: u64, align: u64) -> Option<CodeRange> {
        let len = len.checked_next_multiple_of(GRAIN)?;
        let align = align.max(GRAIN);
        let need = len.checked_add(align - GRAIN)?;
        let index = self.find(need)?;
        let block = self.blocks[index as usize];
        self.unlink(index);
        let offset = block.offset.next_multiple_of(align);
        let head = offset - block.offset;
        let tail = block.len - head - len;
        if head != 0 {
            self.insert(block.offset, head);
        }
        if tail != 0 {
            self.insert(offset + len, tail);
        }
        Some(CodeRange { offset, len })
    }

    /// Gives back `range`, as [`CodeHeap::take`] gave it, merged with free neighbours.
    pub(crate) fn give(&mut self, range: CodeRange) {
        let mut offset = range.offset;
        let mut len = range.len.next_multiple_of(GRAIN);
        if len == 0 {
            return;
        }
        if let Some(&before) = self.by_end.get(&offset) {
            let block = self.blocks[before as usize];
            self.unlink(before);
            offset = block.offset;
            len += block.len;
        }
        if let Some(&after) = self.by_start.get(&(offset + len)) {
            let block = self.blocks[after as usize];
            self.unlink(after);
            len += block.len;
        }
        self.insert(offset, len);
    }

    /// A free block of at least `need` bytes.
    fn find(&self, need: u64) -> Option<u32> {
        let (first, second) = class_at_least(need);
        if first >= FIRST {
            return None;
        }
        let seconds = u32::from(self.second[first]) & (u32::MAX << second);
        let (first, second) = if seconds != 0 {
            (first, seconds.trailing_zeros() as usize)
        } else {
            let firsts = self.first & u64::MAX.checked_shl(first as u32 + 1).unwrap_or(0);
            if firsts == 0 {
                return self.find_exact(need);
            }
            let first = firsts.trailing_zeros() as usize;
            (first, (self.second[first] as u32).trailing_zeros() as usize)
        };
        let head = self.heads[first * SECOND + second];
        (head != NONE).then_some(head)
    }

    /// The class `need` falls in, searched whole: its ranges are not all long enough, but one
    /// may be. Only when every larger class is empty.
    fn find_exact(&self, need: u64) -> Option<u32> {
        let (first, second) = class(need);
        if first >= FIRST {
            return None;
        }
        let mut at = self.heads[first * SECOND + second];
        while at != NONE {
            if self.blocks[at as usize].len >= need {
                return Some(at);
            }
            at = self.blocks[at as usize].next;
        }
        None
    }

    fn insert(&mut self, offset: u64, len: u64) {
        let (first, second) = class(len);
        let list = first * SECOND + second;
        let block = Block { offset, len, prev: NONE, next: self.heads[list] };
        let index = match self.spare.pop() {
            Some(index) => {
                self.blocks[index as usize] = block;
                index
            }
            None => {
                self.blocks.push(block);
                (self.blocks.len() - 1) as u32
            }
        };
        if block.next != NONE {
            self.blocks[block.next as usize].prev = index;
        }
        self.heads[list] = index;
        self.first |= 1 << first;
        self.second[first] |= 1 << second;
        self.by_start.insert(offset, index);
        self.by_end.insert(offset + len, index);
        self.free += len;
    }

    fn unlink(&mut self, index: u32) {
        let block = self.blocks[index as usize];
        let (first, second) = class(block.len);
        let list = first * SECOND + second;
        if block.prev != NONE {
            self.blocks[block.prev as usize].next = block.next;
        } else {
            self.heads[list] = block.next;
        }
        if block.next != NONE {
            self.blocks[block.next as usize].prev = block.prev;
        }
        if self.heads[list] == NONE {
            self.second[first] &= !(1 << second);
            if self.second[first] == 0 {
                self.first &= !(1 << first);
            }
        }
        self.by_start.remove(&block.offset);
        self.by_end.remove(&(block.offset + block.len));
        self.spare.push(index);
        self.free -= block.len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference: a sorted list of free ranges, first fit by address.
    struct Model {
        free: Vec<(u64, u64)>,
    }

    impl Model {
        fn give(&mut self, offset: u64, len: u64) {
            self.free.push((offset, len));
            self.free.sort_unstable();
            let mut merged: Vec<(u64, u64)> = Vec::new();
            for &(offset, len) in &self.free {
                match merged.last_mut() {
                    Some(last) if last.0 + last.1 == offset => last.1 += len,
                    _ => merged.push((offset, len)),
                }
            }
            self.free = merged;
        }

        fn total(&self) -> u64 {
            self.free.iter().map(|it| it.1).sum()
        }

        fn contains(&self, offset: u64, len: u64) -> bool {
            self.free.iter().any(|&(at, n)| offset >= at && offset + len <= at + n)
        }

        fn take(&mut self, offset: u64, len: u64) {
            let index = self.free.iter().position(|&(at, n)| offset >= at && offset + len <= at + n).unwrap();
            let (at, n) = self.free.remove(index);
            if offset > at {
                self.free.push((at, offset - at));
            }
            if offset + len < at + n {
                self.free.push((offset + len, at + n - offset - len));
            }
            self.free.sort_unstable();
        }
    }

    #[test]
    fn matches_a_sorted_free_list() {
        for seed in 1..40u64 {
            let mut rng = blaze_ids::rng::SplitMix64::new(seed.wrapping_mul(0x2545_f491_4f6c_dd1d));
            let start = 0x10000;
            let len = 1 << 20;
            let mut arena = CodeHeap::new(start, len);
            let mut model = Model { free: vec![(start, len)] };
            let mut taken: Vec<CodeRange> = Vec::new();
            for _ in 0..2000 {
                if !rng.one_in(3) || taken.is_empty() {
                    let want = rng.below(4096) + 1;
                    let align = 1 << (rng.below(8) + 4);
                    if let Some(range) = arena.take(want, align) {
                        assert!(range.len >= want && range.offset.is_multiple_of(align));
                        assert!(model.contains(range.offset, range.len), "{range:?} was not free");
                        model.take(range.offset, range.len);
                        taken.push(range);
                    } else {
                        let need = want.next_multiple_of(GRAIN) + align.max(GRAIN) - GRAIN;
                        assert!(model.free.iter().all(|it| it.1 < need), "a range of {need} was free and not found");
                    }
                } else {
                    let index = rng.index(taken.len());
                    let range = taken.swap_remove(index);
                    arena.give(range);
                    model.give(range.offset, range.len);
                }
                assert_eq!(arena.free(), model.total());
            }
            for range in taken.drain(..) {
                arena.give(range);
                model.give(range.offset, range.len);
            }
            assert_eq!(model.free, vec![(start, len)]);
            assert_eq!(arena.largest(), len);
        }
    }

    #[test]
    fn classes_order_by_size() {
        let mut last = (0, 0);
        for len in (GRAIN..1 << 16).step_by(GRAIN as usize) {
            let at = class(len);
            assert!(at >= last);
            last = at;
            let (first, second) = class_at_least(len);
            let smallest_of = if first < SECOND_BITS as usize {
                second as u64
            } else {
                (1u64 << first) | ((second as u64) << (first - SECOND_BITS as usize))
            };
            assert!(smallest_of >= len, "class ({first}, {second}) holds ranges shorter than {len}");
        }
    }
}
