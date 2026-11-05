//! Fixed-size sets of small integers as `u64` words, for the dataflow analyses: one set, and a
//! matrix of them with a row per block in one allocation. Ranges are set and cleared a word at
//! a time, so a move path's subtree (a contiguous range of path ids) costs a few word ops.
//!
//! The global-heap sets of before the frame, verbatim; the free functions and [`Transfer`],
//! unchanged since, are today's, so one sequence of effects composes either side.

use std::ops::Range;

pub(crate) use crate::bitset::{Transfer, iter_words, masks, union_words, words_for};

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    pub(crate) fn new(size: usize) -> Self {
        BitSet { words: vec![0; words_for(size)] }
    }

    /// The set of every bit below `size`.
    pub(crate) fn filled(size: usize) -> Self {
        let mut out = BitSet::new(size);
        out.gen_range(0..size);
        out
    }

    pub(crate) fn contains(&self, bit: usize) -> bool {
        self.words.get(bit / 64).is_some_and(|word| word & (1 << (bit % 64)) != 0)
    }

    pub(crate) fn insert(&mut self, bit: usize) -> bool {
        let word = &mut self.words[bit / 64];
        let before = *word;
        *word |= 1 << (bit % 64);
        before != *word
    }

    pub(crate) fn remove(&mut self, bit: usize) {
        self.words[bit / 64] &= !(1 << (bit % 64));
    }

    /// Adds the bits of `other`, and says whether that added any.
    pub(crate) fn union(&mut self, other: &BitSet) -> bool {
        union_words(&mut self.words, &other.words)
    }

    pub(crate) fn clear(&mut self) {
        self.words.fill(0);
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        iter_words(&self.words)
    }

    /// The smallest bit of the set in `range`.
    pub(crate) fn first_in(&self, range: Range<usize>) -> Option<usize> {
        let mut found = None;
        masks(range, |word, mask| {
            if found.is_none() {
                let hit = self.words[word] & mask;
                if hit != 0 {
                    found = Some(word * 64 + hit.trailing_zeros() as usize);
                }
            }
        });
        found
    }

    pub(crate) fn words(&self) -> &[u64] {
        &self.words
    }

    pub(crate) fn words_mut(&mut self) -> &mut [u64] {
        &mut self.words
    }

    /// Makes this the set whose words are `words`, reusing its allocation.
    pub(crate) fn load(&mut self, words: &[u64]) {
        self.words.clear();
        self.words.extend_from_slice(words);
    }
}

impl Transfer for BitSet {
    #[inline]
    fn gen_range(&mut self, range: Range<usize>) {
        masks(range, |word, mask| self.words[word] |= mask);
    }

    #[inline]
    fn kill_range(&mut self, range: Range<usize>) {
        masks(range, |word, mask| self.words[word] &= !mask);
    }
}

/// A set of `columns` bits per row, every row in one array: the entry sets of every block,
/// `columns / 64` words a row, a row found by one multiply.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct BitMatrix {
    words: usize,
    data: Vec<u64>,
}

impl BitMatrix {
    pub(crate) fn new(rows: usize, columns: usize) -> Self {
        let words = words_for(columns);
        BitMatrix { words, data: vec![0; rows * words] }
    }

    pub(crate) fn row(&self, row: usize) -> &[u64] {
        &self.data[row * self.words..(row + 1) * self.words]
    }

    pub(crate) fn row_mut(&mut self, row: usize) -> &mut [u64] {
        &mut self.data[row * self.words..(row + 1) * self.words]
    }

    pub(crate) fn contains(&self, row: usize, bit: usize) -> bool {
        self.row(row).get(bit / 64).is_some_and(|word| word & (1 << (bit % 64)) != 0)
    }

    pub(crate) fn insert(&mut self, row: usize, bit: usize) {
        self.row_mut(row)[bit / 64] |= 1 << (bit % 64);
    }

    /// Row `into` gains the bits of row `from`; says whether it gained any.
    pub(crate) fn union_rows(&mut self, into: usize, from: usize) -> bool {
        if into == from {
            return false;
        }
        let words = self.words;
        let (into, from) = if into < from {
            let (low, high) = self.data.split_at_mut(from * words);
            (&mut low[into * words..(into + 1) * words], &high[..words])
        } else {
            let (low, high) = self.data.split_at_mut(into * words);
            (&mut high[..words], &low[from * words..(from + 1) * words])
        };
        union_words(into, from)
    }
}
