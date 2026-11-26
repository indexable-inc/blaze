//! Rows of bits in one flat array: the one set structure of every dataflow here.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;

use crate::flow::{Block, Flow};

/// Block-grained rows of bits (`flow.blocks.len()` rows of `cols` bits each, row-major,
/// `cols.div_ceil(64)` words a row).
///
/// Row count is derived exclusively from [`Flow::blocks`] (or zero via [`Self::empty_in`]), and
/// every row operation is indexed by typed [`Block`], making dense statement-grained (`stmts × N`)
/// bit matrices unrepresentable in production dataflows.
pub struct BlockBits<A: Allocator> {
    words: usize,
    data: Vec<u64, A>,
}

impl<A: Allocator> BlockBits<A> {
    #[inline]
    pub fn new_in<F: Allocator>(flow: &Flow<F>, cols: usize, alloc: A) -> BlockBits<A> {
        let rows = flow.blocks.len();
        let words = cols.div_ceil(64);
        let mut data = Vec::with_capacity_in(rows * words, alloc);
        data.resize(rows * words, 0);
        BlockBits { words, data }
    }

    #[inline]
    pub fn empty_in(alloc: A) -> BlockBits<A> {
        BlockBits { words: 0, data: Vec::new_in(alloc) }
    }

    #[inline]
    pub fn words(&self) -> usize {
        self.words
    }

    #[inline]
    pub fn row(&self, b: Block) -> &[u64] {
        let r = b.index();
        &self.data[r * self.words..(r + 1) * self.words]
    }

    #[inline]
    pub fn row_mut(&mut self, b: Block) -> &mut [u64] {
        let r = b.index();
        &mut self.data[r * self.words..(r + 1) * self.words]
    }

    #[inline]
    pub fn get(&self, b: Block, c: usize) -> bool {
        self.data.get(b.index() * self.words + c / 64).is_some_and(|w| (w >> (c % 64)) & 1 == 1)
    }

    /// Sets the bit; says whether it was clear.
    #[inline]
    pub fn set(&mut self, b: Block, c: usize) -> bool {
        let Some(w) = self.data.get_mut(b.index() * self.words + c / 64) else {
            return false;
        };
        let was = *w;
        *w |= 1 << (c % 64);
        *w != was
    }

    #[inline]
    pub fn clear(&mut self, b: Block, c: usize) {
        if let Some(w) = self.data.get_mut(b.index() * self.words + c / 64) {
            *w &= !(1 << (c % 64));
        }
    }

    /// Row `dst` |= row `src`; says whether `dst` grew.
    #[inline]
    pub fn union_rows(&mut self, dst: Block, src: Block) -> bool {
        let (d_off, s_off) = (dst.index() * self.words, src.index() * self.words);
        let mut grew = false;
        for i in 0..self.words {
            let s = self.data[s_off + i];
            let d = &mut self.data[d_off + i];
            let was = *d;
            *d |= s;
            grew |= *d != was;
        }
        grew
    }
}

/// Dense raw-indexed `rows × cols` bit matrix, gated to `#[cfg(test)]` reference oracles so
/// production analyses cannot allocate `stmts × N` or `origins × origins` dense tables.
#[cfg(test)]
pub struct Bits<A: Allocator> {
    words: usize,
    data: Vec<u64, A>,
}

#[cfg(test)]
impl<A: Allocator> Bits<A> {
    pub fn new_in(rows: usize, cols: usize, alloc: A) -> Bits<A> {
        let words = cols.div_ceil(64);
        let mut data = Vec::with_capacity_in(rows * words, alloc);
        data.resize(rows * words, 0);
        Bits { words, data }
    }

    pub fn words(&self) -> usize {
        self.words
    }

    pub fn row(&self, r: usize) -> &[u64] {
        &self.data[r * self.words..(r + 1) * self.words]
    }

    pub fn row_mut(&mut self, r: usize) -> &mut [u64] {
        &mut self.data[r * self.words..(r + 1) * self.words]
    }

    pub fn get(&self, r: usize, c: usize) -> bool {
        self.data.get(r * self.words + c / 64).is_some_and(|w| (w >> (c % 64)) & 1 == 1)
    }

    /// Sets the bit; says whether it was clear.
    pub fn set(&mut self, r: usize, c: usize) -> bool {
        let Some(w) = self.data.get_mut(r * self.words + c / 64) else {
            return false;
        };
        let was = *w;
        *w |= 1 << (c % 64);
        *w != was
    }

    pub fn clear(&mut self, r: usize, c: usize) {
        if let Some(w) = self.data.get_mut(r * self.words + c / 64) {
            *w &= !(1 << (c % 64));
        }
    }

    /// Row `dst` |= row `src`; says whether `dst` grew.
    pub fn union_rows(&mut self, dst: usize, src: usize) -> bool {
        let mut grew = false;
        for i in 0..self.words {
            let s = self.data[src * self.words + i];
            let d = &mut self.data[dst * self.words + i];
            let was = *d;
            *d |= s;
            grew |= *d != was;
        }
        grew
    }
}

/// `dst |= src`; says whether `dst` grew.
pub fn union_into(dst: &mut [u64], src: &[u64]) -> bool {
    let mut grew = false;
    for (d, s) in dst.iter_mut().zip(src) {
        let was = *d;
        *d |= *s;
        grew |= *d != was;
    }
    grew
}

/// `dst &= src`; says whether `dst` shrank.
pub fn intersect_into(dst: &mut [u64], src: &[u64]) -> bool {
    let mut shrank = false;
    for (d, s) in dst.iter_mut().zip(src) {
        let was = *d;
        *d &= *s;
        shrank |= *d != was;
    }
    shrank
}

pub fn test(row: &[u64], c: usize) -> bool {
    row.get(c / 64).is_some_and(|w| (w >> (c % 64)) & 1 == 1)
}

pub fn put(row: &mut [u64], c: usize, on: bool) {
    if let Some(w) = row.get_mut(c / 64) {
        if on {
            *w |= 1 << (c % 64);
        } else {
            *w &= !(1 << (c % 64));
        }
    }
}

/// Bounded, duplicate-free priority worklist over dense ranks `0..len`, popping the smallest
/// queued rank in `O(log_64 len)` word operations.
pub(crate) struct BitWorklist<A: Allocator> {
    levels: Vec<usize, A>,
    bits: Vec<u64, A>,
}

impl<A: Allocator + Copy> BitWorklist<A> {
    pub(crate) fn new_in(len: usize, alloc: A) -> Self {
        let mut levels = Vec::new_in(alloc);
        let mut bits = Vec::new_in(alloc);
        if len == 0 {
            return Self { levels, bits };
        }
        let mut count = len;
        loop {
            let start = bits.len();
            levels.push(start);
            let words = count.div_ceil(64);
            bits.resize(start + words, 0u64);
            if words == 1 {
                break;
            }
            count = words;
        }
        Self { levels, bits }
    }

    #[inline]
    pub(crate) fn push(&mut self, mut index: usize) {
        for &start in &self.levels {
            let Some(word) = self.bits.get_mut(start + index / 64) else {
                break;
            };
            let was = *word;
            *word |= 1u64 << (index % 64);
            if was != 0 {
                break;
            }
            index /= 64;
        }
    }

    #[inline]
    pub(crate) fn pop_min(&mut self) -> Option<usize> {
        if self.levels.is_empty() {
            return None;
        }
        let mut index = 0usize;
        for &start in self.levels.iter().rev() {
            let &word = self.bits.get(start + index)?;
            if word == 0 {
                return None;
            }
            let tz = usize::try_from(word.trailing_zeros()).ok()?;
            index = index * 64 + tz;
        }
        let rank = index;
        for &start in &self.levels {
            let Some(word) = self.bits.get_mut(start + index / 64) else {
                break;
            };
            *word &= !(1u64 << (index % 64));
            if *word != 0 {
                break;
            }
            index /= 64;
        }
        Some(rank)
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::Inline;

    use super::BlockBits;
    use crate::flow::{Block, FlowBuilder, Span};

    #[test]
    fn block_bits_operations() {
        let inline = Inline::<4096>::new();
        let empty = BlockBits::empty_in(&inline);
        assert_eq!(empty.words(), 0);
        assert!(!empty.get(Block(0), 0));

        let mut b = FlowBuilder::new_in(1, 0, &inline);
        let bb0 = b.block();
        let bb1 = b.block();
        b.start(bb0);
        b.goto(bb1, Span::default());
        b.start(bb1);
        b.ret(Span::default());
        let flow = b.finish().expect("well-formed flow");

        let mut bits = BlockBits::new_in(&flow, 70, &inline);
        assert_eq!(bits.words(), 2);
        assert!(bits.set(bb0, 3));
        assert!(!bits.set(bb0, 3));
        assert!(bits.set(bb0, 67));
        assert!(bits.get(bb0, 3) && bits.get(bb0, 67));
        assert!(!bits.get(bb1, 3));
        assert!(bits.union_rows(bb1, bb0));
        assert!(!bits.union_rows(bb1, bb0));
        assert!(bits.get(bb1, 3) && bits.get(bb1, 67));
        assert_eq!(bits.row(bb0), bits.row(bb1));
        bits.clear(bb1, 3);
        assert!(!bits.get(bb1, 3) && bits.get(bb1, 67));
        bits.row_mut(bb1).fill(0);
        assert!(!bits.get(bb1, 67));
    }
}
