//! Sets by row for the dataflow analyses, and the effects that change them.
//!
//! A row of up to [`DENSE_WORDS`] words is stored densely. A longer row is a slot per 64-byte
//! line: slot [`ZEROS`] a line of zeros, slot [`ONES`] a line of ones, any other slot a line of
//! a pool that grows only as lines become mixed, as rustc's `ChunkedBitSet` keeps chunks of
//! `Zeros | Ones | Mixed`, at a line rather than 2048 bits. A uniform line is never written or
//! read, so a large body's rows cost their mixed lines, not their length.
//!
//! An effect ([`Effects`]) is a row of `(word, gen, kill)` triples sorted by word: a block's
//! statements touch a few words of a set of thousands, so applying one is O(touched).

use std::ops::Range;

use crate::{
    bitset::{Transfer, range_masks},
    mem::{FVec, Frame},
};

/// The words of a cache line.
pub(super) const LINE: usize = 8;
/// The longest row stored densely: two cache lines.
pub(super) const DENSE_WORDS: usize = 16;
pub(super) type Line = [u64; LINE];
pub(super) const ZERO_LINE: Line = [0; LINE];
const ONES_LINE: Line = [!0; LINE];
const ZEROS: u32 = 0;
const ONES: u32 = 1;

pub(super) fn lines_for(words: usize) -> usize {
    words.div_ceil(LINE)
}

/// How many of line `line`'s words a row of `words` words has.
#[inline]
pub(super) fn line_len(words: usize, line: usize) -> usize {
    (words - line * LINE).min(LINE)
}

/// Line `line` of the dense row `words`, zeros past its end.
#[inline]
pub(super) fn line_of(words: &[u64], line: usize) -> Line {
    let mut out = ZERO_LINE;
    let start = line * LINE;
    let len = line_len(words.len(), line);
    out[..len].copy_from_slice(&words[start..start + len]);
    out
}

/// Rows of sets of `words` words, read and grown a line at a time.
pub(super) trait Store<'f>: Sized {
    fn new(frame: &'f Frame, rows: usize, words: usize) -> Self;

    /// Adds `bits` to line `line` of `row`, and gives the bits it did not hold.
    fn merge(&mut self, row: usize, line: usize, bits: &Line) -> Line;

    /// Moves line `line` of `row` into `out`, leaving zeros.
    fn take(&mut self, row: usize, line: usize, out: &mut Line);

    /// Row `row` as dense words.
    fn load(&self, row: usize, out: &mut [u64]);

    #[cfg(test)]
    fn contains(&self, row: usize, bit: usize) -> bool;
}

/// Rows of at most [`DENSE_WORDS`] words, one array.
pub(super) struct Dense<'f> {
    words: usize,
    data: &'f mut [u64],
}

impl<'f> Store<'f> for Dense<'f> {
    fn new(frame: &'f Frame, rows: usize, words: usize) -> Self {
        Dense { words, data: frame.alloc_slice_fill_with(rows * words, |_| 0) }
    }

    #[inline]
    fn merge(&mut self, row: usize, line: usize, bits: &Line) -> Line {
        let start = row * self.words + line * LINE;
        let len = line_len(self.words, line);
        let mut new = ZERO_LINE;
        for (at, word) in self.data[start..start + len].iter_mut().enumerate() {
            new[at] = bits[at] & !*word;
            *word |= bits[at];
        }
        new
    }

    #[inline]
    fn take(&mut self, row: usize, line: usize, out: &mut Line) {
        let start = row * self.words + line * LINE;
        let len = line_len(self.words, line);
        *out = ZERO_LINE;
        for (at, word) in self.data[start..start + len].iter_mut().enumerate() {
            out[at] = std::mem::take(word);
        }
    }

    fn load(&self, row: usize, out: &mut [u64]) {
        out.copy_from_slice(&self.data[row * self.words..(row + 1) * self.words]);
    }

    #[cfg(test)]
    fn contains(&self, row: usize, bit: usize) -> bool {
        bit / 64 < self.words && self.data[row * self.words + bit / 64] & (1 << (bit % 64)) != 0
    }
}

/// Rows of more than [`DENSE_WORDS`] words, a slot per line.
pub(super) struct Lined<'f> {
    words: usize,
    lines: usize,
    slots: &'f mut [u32],
    /// The mixed lines; `pool[ZEROS]` and `pool[ONES]` are the uniform ones, so a slot reads
    /// its line without a branch.
    pool: FVec<'f, Line>,
    /// Pool lines no slot names any more.
    free: FVec<'f, u32>,
}

impl<'f> Lined<'f> {
    fn line(&mut self, bits: Line) -> u32 {
        match self.free.pop() {
            Some(slot) => {
                self.pool[slot as usize] = bits;
                slot
            }
            None => {
                self.pool.push(bits);
                (self.pool.len() - 1) as u32
            }
        }
    }
}

impl<'f> Store<'f> for Lined<'f> {
    fn new(frame: &'f Frame, rows: usize, words: usize) -> Self {
        let lines = lines_for(words);
        let mut pool = Vec::with_capacity_in(2 + rows, frame);
        pool.push(ZERO_LINE);
        pool.push(ONES_LINE);
        Lined {
            words,
            lines,
            slots: frame.alloc_slice_fill_with(rows * lines, |_| ZEROS),
            pool,
            free: Vec::new_in(frame),
        }
    }

    #[inline]
    fn merge(&mut self, row: usize, line: usize, bits: &Line) -> Line {
        let at = row * self.lines + line;
        let slot = self.slots[at];
        if slot == ONES || *bits == ZERO_LINE {
            return ZERO_LINE;
        }
        // Only a whole line can be ones: the bits past a row's end stay zero.
        let whole = line_len(self.words, line) == LINE;
        if slot == ZEROS {
            let slot = if whole && *bits == ONES_LINE { ONES } else { self.line(*bits) };
            self.slots[at] = slot;
            return *bits;
        }
        let held = &mut self.pool[slot as usize];
        let mut new = ZERO_LINE;
        let mut all = !0u64;
        for word in 0..LINE {
            new[word] = bits[word] & !held[word];
            held[word] |= bits[word];
            all &= held[word];
        }
        if whole && all == !0 {
            self.slots[at] = ONES;
            self.free.push(slot);
        }
        new
    }

    #[inline]
    fn take(&mut self, row: usize, line: usize, out: &mut Line) {
        let at = row * self.lines + line;
        let slot = std::mem::replace(&mut self.slots[at], ZEROS);
        *out = self.pool[slot as usize];
        if slot > ONES {
            self.free.push(slot);
        }
    }

    fn load(&self, row: usize, out: &mut [u64]) {
        let slots = &self.slots[row * self.lines..(row + 1) * self.lines];
        for (line, &slot) in slots.iter().enumerate() {
            let len = line_len(self.words, line);
            out[line * LINE..line * LINE + len].copy_from_slice(&self.pool[slot as usize][..len]);
        }
    }

    #[cfg(test)]
    fn contains(&self, row: usize, bit: usize) -> bool {
        let word = bit / 64;
        word < self.words
            && self.pool[self.slots[row * self.lines + word / LINE] as usize][word % LINE]
                & (1 << (bit % 64))
                != 0
    }
}

/// The entry sets of an analysis, a row per block, in the store its size picked.
pub(super) enum Rows<'f> {
    Dense(Dense<'f>),
    Lined(Lined<'f>),
}

impl Rows<'_> {
    #[cfg(test)]
    pub(super) fn words(&self) -> usize {
        match self {
            Rows::Dense(it) => it.words,
            Rows::Lined(it) => it.words,
        }
    }

    /// Row `row` as dense words: `out` is a row's length.
    pub(super) fn load(&self, row: usize, out: &mut [u64]) {
        match self {
            Rows::Dense(it) => it.load(row, out),
            Rows::Lined(it) => it.load(row, out),
        }
    }
}

/// The effect of each block (or of anything else with a row) as `(word, gen, kill)` triples,
/// each row's sorted by word, every row's in one pair of columns. A row not composed is the
/// identity.
pub(super) struct Effects<'f> {
    words: usize,
    /// Each row's range of the columns.
    rows: &'f mut [(u32, u32)],
    word: FVec<'f, u32>,
    bits: FVec<'f, [u64; 2]>,
    /// The row being composed, densely, and the words it has touched.
    scratch: &'f mut [[u64; 2]],
    touched: FVec<'f, u32>,
}

impl<'f> Effects<'f> {
    /// The identity effect for `rows` rows over sets of `domain` bits.
    pub(super) fn new(frame: &'f Frame, rows: usize, domain: usize) -> Self {
        let words = crate::bitset::words_for(domain);
        Effects {
            words,
            rows: frame.alloc_slice_fill_with(rows, |_| (0, 0)),
            word: Vec::with_capacity_in(rows, frame),
            bits: Vec::with_capacity_in(rows, frame),
            scratch: frame.alloc_slice_fill_with(words, |_| [0; 2]),
            touched: Vec::with_capacity_in(LINE.min(words), frame),
        }
    }

    pub(super) fn words(&self) -> usize {
        self.words
    }

    /// Row `row` as a [`Transfer`]: each effect applied to it comes after those before. A row
    /// is composed once; it is stored when the returned value drops.
    pub(super) fn row(&mut self, row: usize) -> Compose<'_, 'f> {
        Compose { effects: self, row }
    }

    /// Row `row`'s words and their `[gen, kill]`, by word.
    pub(super) fn of(&self, row: usize) -> (&[u32], &[[u64; 2]]) {
        let (start, end) = self.rows[row];
        let (start, end) = (start as usize, end as usize);
        (&self.word[start..end], &self.bits[start..end])
    }

    /// The edge effect that is row `row`.
    pub(super) fn edge(&self, row: usize) -> Edge<'_> {
        let (words, bits) = self.of(row);
        Edge::Effect { words, bits }
    }

    #[cfg(test)]
    pub(super) fn apply(&self, row: usize, state: &mut [u64]) {
        let (words, bits) = self.of(row);
        for (&word, &[gen_, kill]) in words.iter().zip(bits) {
            let word = &mut state[word as usize];
            *word = (*word & !kill) | gen_;
        }
    }

    #[inline]
    fn touch(&mut self, word: usize) -> &mut [u64; 2] {
        // A touched word is never `[0, 0]` again: each change sets one of the pair.
        if self.scratch[word] == [0; 2] {
            self.touched.push(word as u32);
        }
        &mut self.scratch[word]
    }

    fn store(&mut self, row: usize) {
        self.touched.sort_unstable();
        let start = self.word.len() as u32;
        for &word in self.touched.iter() {
            self.word.push(word);
            self.bits.push(std::mem::take(&mut self.scratch[word as usize]));
        }
        self.touched.clear();
        self.rows[row] = (start, self.word.len() as u32);
    }
}

/// A row of [`Effects`] being composed.
pub(super) struct Compose<'e, 'f> {
    effects: &'e mut Effects<'f>,
    row: usize,
}

impl Transfer for Compose<'_, '_> {
    #[inline]
    fn gen_range(&mut self, range: Range<usize>) {
        range_masks(range, |word, mask| {
            let [gen_, kill] = self.effects.touch(word);
            *gen_ |= mask;
            *kill &= !mask;
        });
    }

    #[inline]
    fn kill_range(&mut self, range: Range<usize>) {
        range_masks(range, |word, mask| {
            let [gen_, kill] = self.effects.touch(word);
            *kill |= mask;
            *gen_ &= !mask;
        });
    }
}

impl Drop for Compose<'_, '_> {
    fn drop(&mut self) {
        self.effects.store(self.row);
    }
}

/// What an edge does to the set that crosses it. Each distributes over union, which the
/// difference fixpoint relies on.
#[derive(Clone, Copy)]
pub(super) enum Edge<'e> {
    Same,
    Nothing,
    /// Only the bits of the mask, a row's length, cross.
    Mask(&'e [u64]),
    /// `(set & !kill) | gen`, by word: a row of [`Effects`].
    Effect { words: &'e [u32], bits: &'e [[u64; 2]] },
}

impl Edge<'_> {
    /// The edge's effect on a dense row.
    pub(super) fn apply(self, state: &mut [u64]) {
        match self {
            Edge::Same => (),
            Edge::Nothing => state.fill(0),
            Edge::Mask(mask) => {
                for (word, mask) in state.iter_mut().zip(mask) {
                    *word &= mask;
                }
            }
            Edge::Effect { words, bits } => {
                for (&word, &[gen_, kill]) in words.iter().zip(bits) {
                    let word = &mut state[word as usize];
                    *word = (*word & !kill) | gen_;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;
    use blaze_mem::{Tag, frame};

    use super::{Dense, LINE, Line, Lined, Store, ZERO_LINE, line_of, lines_for};

    /// Both stores give back what was merged in, say which bits were new, and empty a line
    /// they give up, against a plain array.
    fn matches_an_array<'f, S: Store<'f>>(frame: &'f blaze_mem::frame::Frame, seed: u64) {
        let mut rng = SplitMix64::new(seed);
        for _ in 0..200 {
            let words = 1 + rng.index(60);
            let rows = 1 + rng.index(6);
            let mut store = S::new(frame, rows, words);
            let mut want = vec![vec![0u64; words]; rows];
            for _ in 0..rng.index(40) {
                let row = rng.index(rows);
                let line = rng.index(lines_for(words));
                let mut bits = ZERO_LINE;
                for at in 0..super::line_len(words, line) {
                    bits[at] = match rng.index(4) {
                        0 => 0,
                        1 => !0,
                        _ => rng.below(u64::MAX),
                    };
                }
                if rng.index(5) == 0 {
                    let mut out: Line = [7; LINE];
                    store.take(row, line, &mut out);
                    assert_eq!(out, line_of(&want[row], line));
                    let start = line * LINE;
                    let len = super::line_len(words, line);
                    want[row][start..start + len].fill(0);
                    continue;
                }
                let before = line_of(&want[row], line);
                let new = store.merge(row, line, &bits);
                for at in 0..LINE {
                    assert_eq!(new[at], bits[at] & !before[at]);
                }
                let start = line * LINE;
                for at in 0..super::line_len(words, line) {
                    want[row][start + at] |= bits[at];
                }
            }
            let mut out = vec![0u64; words];
            for (row, want) in want.iter().enumerate() {
                store.load(row, &mut out);
                assert_eq!(&out, want);
                for bit in 0..words * 64 {
                    assert_eq!(store.contains(row, bit), want[bit / 64] & (1 << (bit % 64)) != 0);
                }
            }
        }
    }

    #[test]
    fn dense_rows_match_an_array() {
        frame::task(Tag::Mir, |frame| matches_an_array::<Dense<'_>>(frame, 0x9e37_79b9_7f4a_7c15));
    }

    #[test]
    fn lined_rows_match_an_array() {
        frame::task(Tag::Mir, |frame| matches_an_array::<Lined<'_>>(frame, 0x2545_f491_4f6c_dd1d));
    }
}
