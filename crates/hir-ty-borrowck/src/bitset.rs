//! The analyses' sets: `blaze_ids::bits` sets over words in the body's frame, fixed in size
//! (no capacity word and no growth), so a clean body takes no global memory. Ranges are set
//! and cleared a word at a time, so a move path's subtree (a contiguous range of path ids)
//! costs a few word ops.

use std::ops::Range;

#[cfg(test)]
use blaze_ids::bits::Ones;
use blaze_ids::bits::{self as shared, BitSlice, BitSliceMut, Bits, WordsMut};
#[cfg(test)]
pub(super) use blaze_ids::bits::range_masks as masks;
pub(super) use blaze_ids::bits::{range_masks, words_for};

use crate::mem::Frame;

/// A set of bits in the frame.
pub(super) type BitSet<'f> = BitSliceMut<'f>;

/// Rows of one width in the frame, a row found by one multiply: the entry sets of every block.
pub(super) type BitMatrix<'f> = shared::BitMatrix<usize, &'f mut [u64]>;

/// An empty set of `len` bits in `frame`.
pub(super) fn bits(frame: &Frame, len: usize) -> BitSet<'_> {
    Bits::from_words(frame.alloc_slice_fill_with(words_for(len), |_| 0))
}

/// The set of every bit below `len`, in `frame`.
pub(super) fn filled_bits(frame: &Frame, len: usize) -> BitSet<'_> {
    let mut out = bits(frame, len);
    out.insert_range(0..len);
    out
}

/// An empty matrix of `rows` rows of `columns` bits in `frame`.
pub(super) fn matrix(frame: &Frame, rows: usize, columns: usize) -> BitMatrix<'_> {
    let data = frame.alloc_slice_fill_with(rows * words_for(columns), |_| 0);
    shared::BitMatrix::from_words(rows, columns, data)
}

/// The bits set in `words`, ascending: a row a store loaded into a word buffer.
#[cfg(test)]
pub(super) fn iter_words(words: &[u64]) -> Ones<'_> {
    Ones::new(words)
}

/// `into |= from`, saying whether that added any bit.
pub(super) fn union_words(into: &mut [u64], from: &[u64]) -> bool {
    let mut into: BitSet<'_> = Bits::from_words(into);
    into.union_with(&BitSlice::from_words(from))
}

/// What a statement does to a set: adds and removes bits, whatever is in it. Applied to a set
/// as a walk goes, or composed into a block's gen and kill sets
/// ([`crate::dataflow::GenKill::row`]), from the one function either way.
pub(super) trait Transfer {
    fn gen_range(&mut self, range: Range<usize>);
    fn kill_range(&mut self, range: Range<usize>);

    #[inline]
    fn gen_(&mut self, bit: usize) {
        self.gen_range(bit..bit + 1);
    }

    #[inline]
    fn kill(&mut self, bit: usize) {
        self.kill_range(bit..bit + 1);
    }
}

impl<S: WordsMut> Transfer for Bits<S> {
    #[inline]
    fn gen_range(&mut self, range: Range<usize>) {
        self.insert_range(range);
    }

    #[inline]
    fn kill_range(&mut self, range: Range<usize>) {
        self.remove_range(range);
    }
}

#[cfg(test)]
mod tests {
    use super::{Transfer, bits, matrix};

    #[test]
    fn a_set_and_a_matrix_take_no_global_memory() {
        blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let _ = bits(frame, 1);
            let _ = matrix(frame, 1, 1);
            blaze_alloc_count::assert_at_most(0, "a bitset and a matrix in the frame", || {
                let mut set = bits(frame, 1000);
                let mut rows = matrix(frame, 64, 1000);
                set.gen_range(3..900);
                rows.insert(63, 999);
                set.copy_from(&rows.row(63));
                assert!(set.contains(999) && !set.contains(3));
            });
        });
    }
}
