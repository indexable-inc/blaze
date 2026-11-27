//! Bit-packed boolean ([`BitCol`]) and small-tag ([`SmallTagCol`]) columns over an explicit
//! [`Allocator`].
//!
//! - [`BitCol<A>`] packs 1 bit per row into a `Vec<u64, A>` (8x denser than `Vec<bool>`), keeping
//!   unused high bits in the final partial word zeroed so word-parallel operations (`count_ones`,
//!   `iter_ones`, `and_with`, `or_with`) process 64 rows per instruction without tail masking.
//! - [`SmallTagCol<BITS, A>`] packs 1-, 2-, or 4-bit tags (64, 32, or 16 tags per `u64` word) for
//!   compact enum discriminants and pattern-type columns.
//!
//! Zero `unsafe` code; all allocation lives inside [`Vec<u64, A>`](crate::Vec).

use core::{alloc::Allocator, fmt, iter::FusedIterator};

use crate::{
    error::{AllocFailure, fail},
    make::CloneIn,
    vec::Vec,
};

const BIT_COL_WHAT: &str = "BitCol";
const SMALL_TAG_COL_WHAT: &str = "SmallTagCol";

/// Returns the mask of the low `rem` bits (`1..=63`).
#[inline]
const fn low_bits_mask(rem: usize) -> u64 {
    debug_assert!(rem > 0 && rem < 64);
    (1u64 << rem) - 1
}

/// A bit-packed `bool` column backed by `Vec<u64, A>` (1 bit per row, 8x smaller than `Vec<bool>`).
///
/// # Invariants
///
/// - `words.len() == len.div_ceil(64)`.
/// - Whenever `len % 64 != 0`, every bit at index `>= len % 64` in the final word is `0`.
pub struct BitCol<A: Allocator> {
    words: Vec<u64, A>,
    len: usize,
}

impl<A: Allocator> BitCol<A> {
    /// Creates an empty [`BitCol`] in `alloc`; allocates nothing until a bit is pushed.
    #[inline]
    pub const fn new_in(alloc: A) -> BitCol<A> {
        BitCol { words: Vec::new_in(alloc), len: 0 }
    }

    /// Creates an empty [`BitCol`] with room for at least `bits` rows.
    #[track_caller]
    #[inline]
    pub fn with_capacity_in(bits: usize, alloc: A) -> BitCol<A> {
        match BitCol::try_with_capacity_in(bits, alloc) {
            Ok(col) => col,
            Err(failure) => fail(failure),
        }
    }

    /// Fallible form of [`BitCol::with_capacity_in`].
    #[inline]
    pub fn try_with_capacity_in(bits: usize, alloc: A) -> Result<BitCol<A>, AllocFailure> {
        let words = Vec::try_with_capacity_in(bits.div_ceil(64), alloc)?;
        Ok(BitCol { words, len: 0 })
    }

    /// Creates a [`BitCol`] of `len` rows all initialized to `val` in `O(len / 64)` time.
    ///
    /// Unused high bits in the final partial word are always zeroed.
    #[track_caller]
    #[inline]
    pub fn from_elem_in(val: bool, len: usize, alloc: A) -> BitCol<A> {
        match BitCol::try_from_elem_in(val, len, alloc) {
            Ok(col) => col,
            Err(failure) => fail(failure),
        }
    }

    /// Fallible form of [`BitCol::from_elem_in`].
    pub fn try_from_elem_in(val: bool, len: usize, alloc: A) -> Result<BitCol<A>, AllocFailure> {
        let word_count = len.div_ceil(64);
        let fill = if val { !0u64 } else { 0u64 };
        let mut words = Vec::try_with_capacity_in(word_count, alloc)?;
        words.resize(word_count, fill);
        let rem = len & 63;
        if val && rem != 0 {
            if let Some(last) = words.last_mut() {
                *last = low_bits_mask(rem);
            }
        }
        Ok(BitCol { words, len })
    }

    /// Returns the number of bits in the column.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the column holds zero bits.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the number of bits that can be pushed without reallocating.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.words.capacity().saturating_mul(64)
    }

    /// Returns a reference to the backing allocator.
    #[inline]
    pub fn allocator(&self) -> &A {
        self.words.allocator()
    }

    /// Returns the packed `u64` words.
    ///
    /// Unused high bits in the final partial word (when `len % 64 != 0`) are guaranteed to be `0`.
    #[inline]
    pub fn words(&self) -> &[u64] {
        self.words.as_slice()
    }

    /// Appends a single bit to the end of the column.
    #[blaze::cost(amortized O(1))]
    #[track_caller]
    #[inline]
    pub fn push(&mut self, val: bool) {
        if let Err(failure) = self.try_push(val) {
            fail(failure);
        }
    }

    /// Fallible form of [`BitCol::push`].
    #[blaze::cost(amortized O(1))]
    #[inline]
    pub fn try_push(&mut self, val: bool) -> Result<(), AllocFailure> {
        let Some(next_len) = self.len.checked_add(1) else {
            return Err(AllocFailure::CapacityOverflow {
                what: BIT_COL_WHAT,
                len: self.len,
                additional: 1,
            });
        };
        let bit = self.len & 63;
        if bit == 0 {
            self.words.try_push(u64::from(val)).map_err(|refused| refused.why)?;
        } else if val {
            self.words[self.len >> 6] |= 1u64 << bit;
        }
        self.len = next_len;
        Ok(())
    }

    /// Removes and returns the last bit, or `None` if empty.
    #[blaze::cost(O(1))]
    #[inline]
    pub fn pop(&mut self) -> Option<bool> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let word_idx = self.len >> 6;
        let bit = self.len & 63;
        let mask = 1u64 << bit;
        let val = (self.words[word_idx] & mask) != 0;
        if bit == 0 {
            self.words.pop();
        } else {
            self.words[word_idx] &= !mask;
        }
        Some(val)
    }

    /// Returns the bit at index `i`, or `None` if `i >= self.len()`.
    #[blaze::cost(O(1))]
    #[inline]
    pub fn get(&self, i: usize) -> Option<bool> {
        if i >= self.len {
            return None;
        }
        Some(((self.words[i >> 6] >> (i & 63)) & 1) != 0)
    }

    /// Sets the bit at index `i` to `val`.
    ///
    /// # Panics
    ///
    /// Panics if `i >= self.len()`.
    #[blaze::cost(O(1))]
    #[track_caller]
    #[inline]
    pub fn set(&mut self, i: usize, val: bool) {
        assert!(i < self.len, "BitCol::set: index {i} out of bounds for len {}", self.len);
        let word = &mut self.words[i >> 6];
        let mask = 1u64 << (i & 63);
        if val {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }

    /// Removes all bits while keeping the allocated buffer.
    #[inline]
    pub fn clear(&mut self) {
        self.words.clear();
        self.len = 0;
    }

    /// Shortens the column to `new_len` bits (no-op if `new_len >= self.len()`).
    ///
    /// Clears any newly unused high bits in the final partial word.
    #[inline]
    pub fn truncate(&mut self, new_len: usize) {
        if new_len >= self.len {
            return;
        }
        let new_words = new_len.div_ceil(64);
        self.words.truncate(new_words);
        let rem = new_len & 63;
        if rem != 0 {
            self.words[new_words - 1] &= low_bits_mask(rem);
        }
        self.len = new_len;
    }

    /// Counts set (`true`) bits at 64 rows per instruction via `u64::count_ones`.
    #[blaze::cost(O(n), space = O(1))]
    #[inline]
    pub fn count_ones(&self) -> usize {
        let mut total = 0usize;
        for &w in self.words.as_slice() {
            total += w.count_ones() as usize;
        }
        total
    }

    /// Counts clear (`false`) bits at 64 rows per instruction.
    #[blaze::cost(O(n), space = O(1))]
    #[inline]
    pub fn count_zeros(&self) -> usize {
        self.len - self.count_ones()
    }

    /// Iterates over the indices of all set (`true`) bits in ascending order, jumping directly
    /// from set bit to set bit via `trailing_zeros` and `word &= word - 1`.
    #[inline]
    pub fn iter_ones(&self) -> OnesIter<'_> {
        OnesIter { words: self.words.as_slice(), next_word: 0, current: 0 }
    }

    /// Bitwise AND in place with `other` (`self[i] &= other[i]`), 64 rows per instruction.
    ///
    /// # Panics
    ///
    /// Panics if `self.len() != other.len()`.
    #[blaze::cost(O(n), space = O(1))]
    #[track_caller]
    #[inline]
    pub fn and_with<B: Allocator>(&mut self, other: &BitCol<B>) {
        assert_eq!(
            self.len, other.len,
            "BitCol::and_with: length mismatch ({} vs {})",
            self.len, other.len
        );
        for (a, &b) in self.words.as_mut_slice().iter_mut().zip(other.words.as_slice()) {
            *a &= b;
        }
    }

    /// Bitwise OR in place with `other` (`self[i] |= other[i]`), 64 rows per instruction.
    ///
    /// # Panics
    ///
    /// Panics if `self.len() != other.len()`.
    #[blaze::cost(O(n), space = O(1))]
    #[track_caller]
    #[inline]
    pub fn or_with<B: Allocator>(&mut self, other: &BitCol<B>) {
        assert_eq!(
            self.len, other.len,
            "BitCol::or_with: length mismatch ({} vs {})",
            self.len, other.len
        );
        for (a, &b) in self.words.as_mut_slice().iter_mut().zip(other.words.as_slice()) {
            *a |= b;
        }
    }

    /// Copies this column into `alloc`, or returns [`AllocFailure`].
    pub fn try_clone_in<B: Allocator>(&self, alloc: B) -> Result<BitCol<B>, AllocFailure> {
        let mut words = Vec::try_with_capacity_in(self.words.len(), alloc)?;
        words.try_extend_from_slice(self.words.as_slice())?;
        Ok(BitCol { words, len: self.len })
    }

    /// Copies this column into `alloc`, panicking on allocation failure.
    #[track_caller]
    pub fn clone_in<B: Allocator>(&self, alloc: B) -> BitCol<B> {
        match self.try_clone_in(alloc) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }
}

impl<A: Allocator, B: Allocator> CloneIn<B> for BitCol<A> {
    type In = BitCol<B>;

    fn try_clone_in(&self, alloc: B) -> Result<BitCol<B>, AllocFailure> {
        BitCol::try_clone_in(self, alloc)
    }
}

impl<A: Allocator, B: Allocator> PartialEq<BitCol<B>> for BitCol<A> {
    #[inline]
    fn eq(&self, other: &BitCol<B>) -> bool {
        self.len == other.len && self.words.as_slice() == other.words.as_slice()
    }
}

impl<A: Allocator> Eq for BitCol<A> {}

impl<A: Allocator> fmt::Debug for BitCol<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BitCol")
            .field("len", &self.len)
            .field("ones", &self.count_ones())
            .finish()
    }
}

/// Iterator over the ascending indices of set bits in a [`BitCol`].
///
/// Advances directly from one set bit to the next using `trailing_zeros` and `word &= word - 1`,
/// skipping empty 64-bit words in a single branch.
#[derive(Clone, Debug)]
pub struct OnesIter<'a> {
    words: &'a [u64],
    next_word: usize,
    current: u64,
}

impl Iterator for OnesIter<'_> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        while self.current == 0 {
            if self.next_word >= self.words.len() {
                return None;
            }
            self.current = self.words[self.next_word];
            self.next_word += 1;
        }
        let tz = self.current.trailing_zeros() as usize;
        self.current &= self.current - 1;
        Some(((self.next_word - 1) << 6) | tz)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let lower = self.current.count_ones() as usize;
        let rem_words = self.words.len() - self.next_word;
        let upper = lower.saturating_add(rem_words.saturating_mul(64));
        (lower, Some(upper))
    }
}

impl FusedIterator for OnesIter<'_> {}

/// A bit-packed small-tag column storing `BITS`-wide unsigned tags (`BITS` in `{1, 2, 4}`) inside
/// `Vec<u64, A>` — 64, 32, or 16 tags per `u64` word with zero cross-word straddling.
///
/// Attempting to instantiate or operate on a [`SmallTagCol`] with `BITS` outside `{1, 2, 4}` is a
/// compile-time error:
///
/// ```compile_fail
/// use blaze_own::bit_col::SmallTagCol;
/// use blaze_own::Inline;
/// let arena = Inline::<64>::new();
/// let _bad = SmallTagCol::<3, _>::new_in(&arena);
/// ```
///
/// # Invariants
///
/// - `BITS` is `1`, `2`, or `4`.
/// - `words.len() == len.div_ceil(Self::TAGS_PER_WORD)`.
/// - Unused high bits in the final partial `u64` word are always `0`.
pub struct SmallTagCol<const BITS: u32, A: Allocator> {
    words: Vec<u64, A>,
    len: usize,
}

impl<const BITS: u32, A: Allocator> SmallTagCol<BITS, A> {
    /// Compile-time assertion enforcing `BITS` in `{1, 2, 4}`.
    const VALID_BITS: () = assert!(
        BITS == 1 || BITS == 2 || BITS == 4,
        "SmallTagCol: BITS must be 1, 2, or 4"
    );

    /// Number of tags packed into each `u64` word (`64`, `32`, or `16`).
    pub const TAGS_PER_WORD: usize = {
        let () = Self::VALID_BITS;
        (64 / BITS) as usize
    };

    /// Largest representable tag value (`(1 << BITS) - 1`: `1`, `3`, or `15`).
    pub const MAX_TAG: u8 = {
        let () = Self::VALID_BITS;
        (1u8 << BITS) - 1
    };

    const TAG_MASK: u64 = {
        let () = Self::VALID_BITS;
        (1u64 << BITS) - 1
    };

    const SHIFT_PER_WORD: u32 = Self::TAGS_PER_WORD.trailing_zeros();
    const INDEX_MASK: usize = Self::TAGS_PER_WORD - 1;

    /// Creates an empty [`SmallTagCol`] in `alloc`; allocates nothing until a tag is pushed.
    #[inline]
    pub const fn new_in(alloc: A) -> SmallTagCol<BITS, A> {
        let () = Self::VALID_BITS;
        SmallTagCol { words: Vec::new_in(alloc), len: 0 }
    }

    /// Creates an empty [`SmallTagCol`] with room for at least `tags` entries.
    #[track_caller]
    #[inline]
    pub fn with_capacity_in(tags: usize, alloc: A) -> SmallTagCol<BITS, A> {
        match SmallTagCol::try_with_capacity_in(tags, alloc) {
            Ok(col) => col,
            Err(failure) => fail(failure),
        }
    }

    /// Fallible form of [`SmallTagCol::with_capacity_in`].
    #[inline]
    pub fn try_with_capacity_in(tags: usize, alloc: A) -> Result<SmallTagCol<BITS, A>, AllocFailure> {
        let () = Self::VALID_BITS;
        let words = Vec::try_with_capacity_in(tags.div_ceil(Self::TAGS_PER_WORD), alloc)?;
        Ok(SmallTagCol { words, len: 0 })
    }

    /// Returns the number of tags in the column.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the column holds zero tags.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the number of tags that can be pushed without reallocating.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.words.capacity().saturating_mul(Self::TAGS_PER_WORD)
    }

    /// Returns a reference to the backing allocator.
    #[inline]
    pub fn allocator(&self) -> &A {
        self.words.allocator()
    }

    /// Returns the packed `u64` words.
    #[inline]
    pub fn words(&self) -> &[u64] {
        self.words.as_slice()
    }

    /// Appends `tag` to the end of the column.
    ///
    /// # Panics
    ///
    /// Panics if `tag >= (1 << BITS)` or if allocation fails.
    #[blaze::cost(amortized O(1))]
    #[track_caller]
    #[inline]
    pub fn push(&mut self, tag: u8) {
        if let Err(failure) = self.try_push(tag) {
            fail(failure);
        }
    }

    /// Fallible form of [`SmallTagCol::push`].
    ///
    /// # Panics
    ///
    /// Panics if `tag >= (1 << BITS)`.
    #[blaze::cost(amortized O(1))]
    #[track_caller]
    #[inline]
    pub fn try_push(&mut self, tag: u8) -> Result<(), AllocFailure> {
        let () = Self::VALID_BITS;
        assert!(
            tag <= Self::MAX_TAG,
            "SmallTagCol<{BITS}>::push: tag {tag} out of range 0..={}",
            Self::MAX_TAG
        );
        let Some(next_len) = self.len.checked_add(1) else {
            return Err(AllocFailure::CapacityOverflow {
                what: SMALL_TAG_COL_WHAT,
                len: self.len,
                additional: 1,
            });
        };
        let lane = self.len & Self::INDEX_MASK;
        if lane == 0 {
            self.words.try_push(u64::from(tag)).map_err(|refused| refused.why)?;
        } else {
            let word_idx = self.len >> Self::SHIFT_PER_WORD;
            let shift = (lane as u32) * BITS;
            self.words[word_idx] |= u64::from(tag) << shift;
        }
        self.len = next_len;
        Ok(())
    }

    /// Removes and returns the last tag, or `None` if empty.
    #[blaze::cost(O(1))]
    #[inline]
    pub fn pop(&mut self) -> Option<u8> {
        let () = Self::VALID_BITS;
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let word_idx = self.len >> Self::SHIFT_PER_WORD;
        let lane = self.len & Self::INDEX_MASK;
        let shift = (lane as u32) * BITS;
        let val = ((self.words[word_idx] >> shift) & Self::TAG_MASK) as u8;
        if lane == 0 {
            self.words.pop();
        } else {
            self.words[word_idx] &= !(Self::TAG_MASK << shift);
        }
        Some(val)
    }

    /// Returns the tag at index `i`, or `None` if `i >= self.len()`.
    #[blaze::cost(O(1))]
    #[inline]
    pub fn get(&self, i: usize) -> Option<u8> {
        let () = Self::VALID_BITS;
        if i >= self.len {
            return None;
        }
        let word_idx = i >> Self::SHIFT_PER_WORD;
        let shift = ((i & Self::INDEX_MASK) as u32) * BITS;
        Some(((self.words[word_idx] >> shift) & Self::TAG_MASK) as u8)
    }

    /// Sets the tag at index `i` to `tag`.
    ///
    /// # Panics
    ///
    /// Panics if `i >= self.len()` or `tag >= (1 << BITS)`.
    #[blaze::cost(O(1))]
    #[track_caller]
    #[inline]
    pub fn set(&mut self, i: usize, tag: u8) {
        let () = Self::VALID_BITS;
        assert!(
            i < self.len,
            "SmallTagCol<{BITS}>::set: index {i} out of bounds for len {}",
            self.len
        );
        assert!(
            tag <= Self::MAX_TAG,
            "SmallTagCol<{BITS}>::set: tag {tag} out of range 0..={}",
            Self::MAX_TAG
        );
        let word_idx = i >> Self::SHIFT_PER_WORD;
        let shift = ((i & Self::INDEX_MASK) as u32) * BITS;
        let word = &mut self.words[word_idx];
        *word = (*word & !(Self::TAG_MASK << shift)) | (u64::from(tag) << shift);
    }

    /// Removes all tags while keeping the allocated buffer.
    #[inline]
    pub fn clear(&mut self) {
        self.words.clear();
        self.len = 0;
    }

    /// Shortens the column to `new_len` tags (no-op if `new_len >= self.len()`).
    #[inline]
    pub fn truncate(&mut self, new_len: usize) {
        let () = Self::VALID_BITS;
        if new_len >= self.len {
            return;
        }
        let new_words = new_len.div_ceil(Self::TAGS_PER_WORD);
        self.words.truncate(new_words);
        let rem_lanes = new_len & Self::INDEX_MASK;
        if rem_lanes != 0 {
            let rem_bits = rem_lanes * (BITS as usize);
            self.words[new_words - 1] &= low_bits_mask(rem_bits);
        }
        self.len = new_len;
    }

    /// Copies this column into `alloc`, or returns [`AllocFailure`].
    pub fn try_clone_in<B: Allocator>(&self, alloc: B) -> Result<SmallTagCol<BITS, B>, AllocFailure> {
        let () = Self::VALID_BITS;
        let mut words = Vec::try_with_capacity_in(self.words.len(), alloc)?;
        words.try_extend_from_slice(self.words.as_slice())?;
        Ok(SmallTagCol { words, len: self.len })
    }

    /// Copies this column into `alloc`, panicking on allocation failure.
    #[track_caller]
    pub fn clone_in<B: Allocator>(&self, alloc: B) -> SmallTagCol<BITS, B> {
        match self.try_clone_in(alloc) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }
}

impl<const BITS: u32, A: Allocator, B: Allocator> CloneIn<B> for SmallTagCol<BITS, A> {
    type In = SmallTagCol<BITS, B>;

    fn try_clone_in(&self, alloc: B) -> Result<SmallTagCol<BITS, B>, AllocFailure> {
        SmallTagCol::try_clone_in(self, alloc)
    }
}

impl<const BITS: u32, A: Allocator, B: Allocator> PartialEq<SmallTagCol<BITS, B>>
    for SmallTagCol<BITS, A>
{
    #[inline]
    fn eq(&self, other: &SmallTagCol<BITS, B>) -> bool {
        self.len == other.len && self.words.as_slice() == other.words.as_slice()
    }
}

impl<const BITS: u32, A: Allocator> Eq for SmallTagCol<BITS, A> {}

impl<const BITS: u32, A: Allocator> fmt::Debug for SmallTagCol<BITS, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmallTagCol")
            .field("bits", &BITS)
            .field("len", &self.len)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Vec, testing};

    #[test]
    fn bit_col_word_boundaries_push_get_set_pop_truncate_clear() {
        let alloc = testing::heap().alloc();
        for &n in &[0usize, 1, 63, 64, 65, 127, 128, 129, 1000] {
            let mut col = BitCol::with_capacity_in(n, alloc);
            assert!(col.is_empty());
            assert_eq!(col.len(), 0);
            assert_eq!(col.get(0), None);
            assert_eq!(col.pop(), None);

            let mut expected = Vec::with_capacity_in(n, alloc);
            for i in 0..n {
                let v = (i % 3 == 0) || (i % 7 == 1);
                col.push(v);
                expected.push(v);
            }
            assert_eq!(col.len(), n);
            assert_eq!(col.is_empty(), n == 0);
            assert_eq!(col.words().len(), n.div_ceil(64));

            for i in 0..n {
                assert_eq!(col.get(i), Some(expected[i]), "n={n}, i={i}");
            }
            assert_eq!(col.get(n), None);

            // Flip every 5th bit via `set`.
            for i in (0..n).step_by(5) {
                expected[i] = !expected[i];
                col.set(i, expected[i]);
            }
            for i in 0..n {
                assert_eq!(col.get(i), Some(expected[i]), "after set: n={n}, i={i}");
            }

            // Check count_ones / count_zeros / iter_ones against oracle.
            let mut oracle_ones = Vec::new_in(alloc);
            for (i, &b) in expected.iter().enumerate() {
                if b {
                    oracle_ones.push(i);
                }
            }
            assert_eq!(col.count_ones(), oracle_ones.len(), "n={n}");
            assert_eq!(col.count_zeros(), n - oracle_ones.len(), "n={n}");
            let mut got_ones = Vec::new_in(alloc);
            for idx in col.iter_ones() {
                got_ones.push(idx);
            }
            assert_eq!(got_ones.as_slice(), oracle_ones.as_slice(), "iter_ones n={n}");

            // Truncate to a partial word, then verify tail invariant holds.
            let cut = n / 2;
            col.truncate(cut);
            expected.truncate(cut);
            assert_eq!(col.len(), cut);
            let expected_ones = expected.iter().filter(|&&b| b).count();
            assert_eq!(col.count_ones(), expected_ones, "after truncate n={n} -> {cut}");
            assert_eq!(col.count_zeros(), cut - expected_ones);
            for idx in col.iter_ones() {
                assert!(idx < cut, "leaked high bit {idx} >= {cut}");
                assert!(expected[idx]);
            }

            // Pop all remaining bits and verify LIFO values + tail cleanliness.
            while let Some(v) = col.pop() {
                let exp = expected.pop().unwrap();
                assert_eq!(v, exp);
                let rem_ones = expected.iter().filter(|&&b| b).count();
                assert_eq!(col.count_ones(), rem_ones);
            }
            assert!(col.is_empty());
            assert_eq!(col.words().len(), 0);
            assert_eq!(col.count_ones(), 0);

            // Re-push after clear and verify clean state.
            col.push(true);
            col.push(false);
            col.clear();
            assert!(col.is_empty());
            assert_eq!(col.words().len(), 0);
            assert_eq!(col.count_ones(), 0);
        }
    }

    #[test]
    fn bit_col_from_elem_in_tail_mask_invariant() {
        let alloc = testing::heap().alloc();
        for &n in &[0usize, 1, 63, 64, 65, 127, 128, 129, 1000] {
            let all_true = BitCol::from_elem_in(true, n, alloc);
            assert_eq!(all_true.len(), n);
            assert_eq!(all_true.count_ones(), n, "from_elem_in(true, {n}) leaked high bits");
            assert_eq!(all_true.count_zeros(), 0);
            let mut count = 0usize;
            for (expected_idx, actual_idx) in all_true.iter_ones().enumerate() {
                assert_eq!(actual_idx, expected_idx);
                count += 1;
            }
            assert_eq!(count, n);

            let all_false = BitCol::from_elem_in(false, n, alloc);
            assert_eq!(all_false.len(), n);
            assert_eq!(all_false.count_ones(), 0);
            assert_eq!(all_false.count_zeros(), n);
            assert_eq!(all_false.iter_ones().next(), None);
        }

        // Explicit 65-bit check: 2 words, second word must be 0b1 (count_ones == 65, not 128).
        let c65 = BitCol::from_elem_in(true, 65, alloc);
        assert_eq!(c65.words(), &[!0u64, 1u64]);
        assert_eq!(c65.count_ones(), 65);
    }

    #[test]
    fn bit_col_and_or_with() {
        let alloc = testing::heap().alloc();
        let n = 195usize; // 3 full words + 3 bits
        let mut a = BitCol::with_capacity_in(n, alloc);
        let mut b = BitCol::with_capacity_in(n, alloc);
        for i in 0..n {
            a.push(i % 2 == 0);
            b.push(i % 3 == 0);
        }

        let mut anded = a.clone_in(alloc);
        anded.and_with(&b);
        for i in 0..n {
            assert_eq!(anded.get(i), Some(i % 6 == 0), "and at {i}");
        }
        assert_eq!(anded.count_ones(), (0..n).filter(|i| i % 6 == 0).count());

        let mut ored = a.clone_in(alloc);
        ored.or_with(&b);
        for i in 0..n {
            assert_eq!(ored.get(i), Some(i % 2 == 0 || i % 3 == 0), "or at {i}");
        }
        assert_eq!(ored.count_ones(), (0..n).filter(|i| i % 2 == 0 || i % 3 == 0).count());
    }

    #[test]
    #[should_panic(expected = "length mismatch")]
    fn bit_col_and_with_panics_on_length_mismatch() {
        let alloc = testing::heap().alloc();
        let mut a = BitCol::from_elem_in(true, 64, alloc);
        let b = BitCol::from_elem_in(true, 65, alloc);
        a.and_with(&b);
    }

    #[test]
    #[should_panic(expected = "length mismatch")]
    fn bit_col_or_with_panics_on_length_mismatch() {
        let alloc = testing::heap().alloc();
        let mut a = BitCol::from_elem_in(false, 64, alloc);
        let b = BitCol::from_elem_in(false, 65, alloc);
        a.or_with(&b);
    }

    #[test]
    fn bit_col_iter_ones_sparse_across_zero_words() {
        let alloc = testing::heap().alloc();
        let mut col = BitCol::from_elem_in(false, 1000, alloc);
        let targets = [0usize, 1, 63, 64, 65, 255, 256, 513, 999];
        for &idx in &targets {
            col.set(idx, true);
        }
        let mut got = Vec::new_in(alloc);
        for idx in col.iter_ones() {
            got.push(idx);
        }
        assert_eq!(got.as_slice(), &targets);
        assert_eq!(col.count_ones(), targets.len());
        assert_eq!(col.count_zeros(), 1000 - targets.len());
    }

    #[test]
    fn small_tag_col_1_2_4_round_trips_across_word_boundaries() {
        let alloc = testing::heap().alloc();

        // 1-bit tags (64/word)
        let mut c1 = SmallTagCol::<1, _>::new_in(alloc);
        for i in 0..200usize {
            c1.push((i & 1) as u8);
        }
        assert_eq!(c1.words().len(), 200usize.div_ceil(64));
        for i in 0..200usize {
            assert_eq!(c1.get(i), Some((i & 1) as u8));
        }

        // 2-bit tags (32/word): boundaries 0, 1, 31, 32, 33, 63, 64, 65, 300
        for &n in &[0usize, 1, 31, 32, 33, 63, 64, 65, 300] {
            let mut c2 = SmallTagCol::<2, _>::with_capacity_in(n, alloc);
            for i in 0..n {
                c2.push((i % 4) as u8);
            }
            assert_eq!(c2.len(), n);
            assert_eq!(c2.words().len(), n.div_ceil(32));
            for i in 0..n {
                assert_eq!(c2.get(i), Some((i % 4) as u8), "2-bit n={n} i={i}");
            }
            assert_eq!(c2.get(n), None);

            // Overwrite every lane with inverted tag and verify no neighbor corruption.
            for i in 0..n {
                c2.set(i, 3 - (i % 4) as u8);
            }
            for i in 0..n {
                assert_eq!(c2.get(i), Some(3 - (i % 4) as u8), "2-bit set n={n} i={i}");
            }

            // Truncate across a word boundary, then push fresh tags to verify zeroed tail.
            let cut = n / 2;
            c2.truncate(cut);
            assert_eq!(c2.len(), cut);
            c2.push(0);
            c2.push(2);
            assert_eq!(c2.get(cut), Some(0));
            assert_eq!(c2.get(cut + 1), Some(2));
            assert_eq!(c2.pop(), Some(2));
            assert_eq!(c2.pop(), Some(0));

            c2.clear();
            assert!(c2.is_empty());
        }

        // 4-bit tags (16/word): boundaries 0, 1, 15, 16, 17, 31, 32, 33, 300
        for &n in &[0usize, 1, 15, 16, 17, 31, 32, 33, 300] {
            let mut c4 = SmallTagCol::<4, _>::with_capacity_in(n, alloc);
            for i in 0..n {
                c4.push((i % 16) as u8);
            }
            assert_eq!(c4.len(), n);
            assert_eq!(c4.words().len(), n.div_ceil(16));
            for i in 0..n {
                assert_eq!(c4.get(i), Some((i % 16) as u8), "4-bit n={n} i={i}");
            }
            assert_eq!(c4.get(n), None);

            for i in 0..n {
                c4.set(i, (15 - (i % 16)) as u8);
            }
            for i in 0..n {
                assert_eq!(c4.get(i), Some((15 - (i % 16)) as u8), "4-bit set n={n} i={i}");
            }

            // Pop across boundary and re-push 0 to confirm cleared bits.
            if n > 0 {
                let popped = c4.pop();
                assert_eq!(popped, Some((15 - ((n - 1) % 16)) as u8));
                c4.push(0);
                assert_eq!(c4.get(n - 1), Some(0));
            }
        }
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn small_tag_col_panics_on_tag_overflow() {
        let alloc = testing::heap().alloc();
        let mut c2 = SmallTagCol::<2, _>::new_in(alloc);
        c2.push(4);
    }

    #[test]
    fn bit_col_and_small_tag_col_handle_allocation_failures() {
        let base = testing::heap().alloc();
        let events = testing::every_failure(&base, |alloc| {
            let mut b = BitCol::try_from_elem_in(true, 130, alloc)?;
            b.try_push(false)?;
            b.try_push(true)?;
            let _copy = b.try_clone_in(alloc)?;

            let mut t = SmallTagCol::<4, _>::try_with_capacity_in(33, alloc)?;
            for i in 0..40usize {
                t.try_push((i % 16) as u8)?;
            }
            let _tcopy = t.try_clone_in(alloc)?;
            Ok::<_, AllocFailure>(())
        })
        .expect("every allocation failure handled cleanly with zero leaks");
        assert!(events > 0);
    }
}
