//! Bit sets: a bit per small integer, bit `i` at bit `i % 64` of word `i / 64`.
//!
//! One set type, [`Bits`], over any storage of `u64` words, so every set shares one copy of
//! the operations:
//!
//! - [`BitSet`]: its words in a `Vec`, sized at creation or grown by `*_growing` inserts;
//! - [`BitArray`]: `W` words inline, no heap; its operations that need no index type are
//!   `const` ([`Bits::with_bit`], [`Bits::has_bit`], [`Bits::or`]) for sets made at compile
//!   time;
//! - [`BitSlice`] and [`BitSliceMut`]: a view of words held elsewhere (a matrix row, a column
//!   of mapped bytes, a `SmallVec` or `ThinVec` set of a smaller field, which are also storage
//!   of their own).
//!
//! Beside it: [`BitMatrix`], rows of one width in one buffer (a dataflow's entry sets, one per
//! block); [`Ranked`], a fixed set that counts the members before each word, so a member's
//! rank is one popcount; [`AtomicBits`], bits set from many threads.
//!
//! An operation on two sets is one loop over whole words that LLVM vectorizes, saying whether
//! it changed anything (what a dataflow iterates to a fixed point on); members are visited a
//! `trailing_zeros` each. Equality and hashing are of the words: two sets with the same
//! members compare equal when they have as many words, which every caller keeps by sizing
//! the sets of one domain alike.

use std::{alloc::Allocator, fmt, hash, iter::FusedIterator, marker::PhantomData, ops::Range};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use smallvec::SmallVec;
use thin_vec::ThinVec;

use crate::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
mod tests;

/// The words a set of `bits` bits takes.
#[inline]
pub const fn words_for(bits: usize) -> usize {
    bits.div_ceil(64)
}

/// The word bit `bit` is in, and its mask in that word.
#[inline]
pub const fn locate(bit: usize) -> (usize, u64) {
    (bit / 64, 1 << (bit % 64))
}

/// The bits of `bit`'s word below it.
#[inline]
pub const fn below(bit: usize) -> u64 {
    (1 << (bit % 64)) - 1
}

/// Calls `f(word, mask)` for each word `range` touches, in order, `mask` the bits of the word
/// in the range: a range is set or cleared a word at a time.
#[inline]
pub fn range_masks(range: Range<usize>, mut f: impl FnMut(usize, u64)) {
    if range.start >= range.end {
        return;
    }
    let (first, last) = (range.start / 64, (range.end - 1) / 64);
    let low = !0u64 << (range.start % 64);
    let high = !0u64 >> (63 - (range.end - 1) % 64);
    if first == last {
        f(first, low & high);
        return;
    }
    f(first, low);
    for word in first + 1..last {
        f(word, !0);
    }
    f(last, high);
}

/// What a set holds: a type that is a small integer, its bit.
pub trait BitIndex: Copy {
    fn to_bit(self) -> usize;
    fn from_bit(bit: usize) -> Self;
}

impl BitIndex for usize {
    #[inline]
    fn to_bit(self) -> usize {
        self
    }

    #[inline]
    fn from_bit(bit: usize) -> usize {
        bit
    }
}

macro_rules! narrow_index {
    ($($int:ty),*) => {$(
        impl BitIndex for $int {
            #[inline]
            fn to_bit(self) -> usize {
                self as usize
            }

            #[inline]
            fn from_bit(bit: usize) -> $int {
                match <$int>::try_from(bit) {
                    Ok(it) => it,
                    Err(_) => panic!("bit {bit} of a set of {}", stringify!($int)),
                }
            }
        }
    )*};
}

narrow_index!(u8, u16, u32);

/// Storage a set reads its words from.
pub trait Words {
    fn words(&self) -> &[u64];
}

/// Storage a set writes its words in.
pub trait WordsMut: Words {
    fn words_mut(&mut self) -> &mut [u64];
}

/// Storage whose word count changes.
pub trait GrowWords: WordsMut {
    /// Makes it `len` words, the new ones zero.
    fn resize_words(&mut self, len: usize);

    /// Gives back the room past its words.
    fn shrink_words(&mut self);
}

impl<A: Allocator> Words for Vec<u64, A> {
    #[inline]
    fn words(&self) -> &[u64] {
        self
    }
}

impl<A: Allocator> WordsMut for Vec<u64, A> {
    #[inline]
    fn words_mut(&mut self) -> &mut [u64] {
        self
    }
}

impl<A: Allocator> GrowWords for Vec<u64, A> {
    fn resize_words(&mut self, len: usize) {
        self.resize(len, 0); // alloc: a growable set's words, grown to its domain once
    }

    fn shrink_words(&mut self) {
        self.shrink_to_fit();
    }
}

impl Words for Box<[u64]> {
    #[inline]
    fn words(&self) -> &[u64] {
        self
    }
}

impl WordsMut for Box<[u64]> {
    #[inline]
    fn words_mut(&mut self) -> &mut [u64] {
        self
    }
}

impl<const W: usize> Words for [u64; W] {
    #[inline]
    fn words(&self) -> &[u64] {
        self
    }
}

impl<const W: usize> WordsMut for [u64; W] {
    #[inline]
    fn words_mut(&mut self) -> &mut [u64] {
        self
    }
}

impl Words for &[u64] {
    #[inline]
    fn words(&self) -> &[u64] {
        self
    }
}

impl Words for &mut [u64] {
    #[inline]
    fn words(&self) -> &[u64] {
        self
    }
}

impl WordsMut for &mut [u64] {
    #[inline]
    fn words_mut(&mut self) -> &mut [u64] {
        self
    }
}

impl<const N: usize> Words for SmallVec<[u64; N]>
where
    [u64; N]: smallvec::Array<Item = u64>,
{
    #[inline]
    fn words(&self) -> &[u64] {
        self
    }
}

impl<const N: usize> WordsMut for SmallVec<[u64; N]>
where
    [u64; N]: smallvec::Array<Item = u64>,
{
    #[inline]
    fn words_mut(&mut self) -> &mut [u64] {
        self
    }
}

impl<const N: usize> GrowWords for SmallVec<[u64; N]>
where
    [u64; N]: smallvec::Array<Item = u64>,
{
    fn resize_words(&mut self, len: usize) {
        self.resize(len, 0); // alloc: inline up to N words
    }

    fn shrink_words(&mut self) {
        self.shrink_to_fit();
    }
}

impl Words for ThinVec<u64> {
    #[inline]
    fn words(&self) -> &[u64] {
        self
    }
}

impl WordsMut for ThinVec<u64> {
    #[inline]
    fn words_mut(&mut self) -> &mut [u64] {
        self
    }
}

impl GrowWords for ThinVec<u64> {
    fn resize_words(&mut self, len: usize) {
        self.resize(len, 0); // alloc: a growable set's words, grown to its domain once
    }

    fn shrink_words(&mut self) {
        self.shrink_to_fit();
    }
}

/// A set of `I`s, a bit each, in the words `S` holds.
pub struct Bits<S = Vec<u64>, I = usize> {
    words: S,
    index: PhantomData<fn(I) -> I>,
}

/// A set in a `Vec` of words.
pub type BitSet<I = usize> = Bits<Vec<u64>, I>;
/// A set of `W * 64` bits inline.
pub type BitArray<const W: usize, I = usize> = Bits<[u64; W], I>;
/// A set read in words held elsewhere.
pub type BitSlice<'a, I = usize> = Bits<&'a [u64], I>;
/// A set written in words held elsewhere.
pub type BitSliceMut<'a, I = usize> = Bits<&'a mut [u64], I>;

impl<S, I> Bits<S, I> {
    /// The set whose words are `words`.
    #[inline]
    pub const fn from_words(words: S) -> Self {
        Bits { words, index: PhantomData }
    }

    pub fn into_words(self) -> S {
        self.words
    }
}

impl<S: Words, I: BitIndex> Bits<S, I> {
    #[inline]
    pub fn words(&self) -> &[u64] {
        self.words.words()
    }

    #[inline]
    pub fn as_slice(&self) -> BitSlice<'_, I> {
        Bits::from_words(self.words.words())
    }

    /// The bits its words hold: one past the largest member it can have.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.words().len() * 64
    }

    /// Whether `at` is a member; past the capacity it is not.
    #[inline]
    pub fn contains(&self, at: I) -> bool {
        let (word, mask) = locate(at.to_bit());
        self.words().get(word).is_some_and(|it| it & mask != 0)
    }

    pub fn is_empty(&self) -> bool {
        is_zero(self.words())
    }

    /// How many members it has.
    pub fn count(&self) -> usize {
        count_words(self.words())
    }

    /// How many members are below `at`.
    pub fn count_below(&self, at: I) -> usize {
        let bit = at.to_bit();
        let words = self.words();
        let word = bit / 64;
        let partial = words.get(word).map_or(0, |it| (it & below(bit)).count_ones() as usize);
        count_words(&words[..word.min(words.len())]) + partial
    }

    /// The members, ascending (or descending, from the back).
    #[inline]
    pub fn iter(&self) -> Ones<'_, I> {
        Ones::new(self.words())
    }

    pub fn first(&self) -> Option<I> {
        self.iter().next()
    }

    pub fn last(&self) -> Option<I> {
        self.iter().next_back()
    }

    /// The smallest member in `range`.
    pub fn first_in(&self, range: Range<I>) -> Option<I> {
        let words = self.words();
        let start = range.start.to_bit();
        let end = range.end.to_bit().min(words.len() * 64);
        if start >= end {
            return None;
        }
        let last = (end - 1) / 64;
        let mut at = start / 64;
        let mut word = words[at] & (!0u64 << (start % 64));
        loop {
            if at == last {
                word &= !0u64 >> (63 - (end - 1) % 64);
            }
            if word != 0 {
                return Some(I::from_bit(at * 64 + word.trailing_zeros() as usize));
            }
            if at == last {
                return None;
            }
            at += 1;
            word = words[at];
        }
    }

    /// The largest member in `range`.
    pub fn last_in(&self, range: Range<I>) -> Option<I> {
        let words = self.words();
        let start = range.start.to_bit();
        let end = range.end.to_bit().min(words.len() * 64);
        if start >= end {
            return None;
        }
        let first = start / 64;
        let mut at = (end - 1) / 64;
        let mut word = words[at] & (!0u64 >> (63 - (end - 1) % 64));
        loop {
            if at == first {
                word &= !0u64 << (start % 64);
            }
            if word != 0 {
                return Some(I::from_bit(at * 64 + 63 - word.leading_zeros() as usize));
            }
            if at == first {
                return None;
            }
            at -= 1;
            word = words[at];
        }
    }

    /// The smallest bit below the capacity that is not a member: a free slot of a table.
    pub fn first_absent(&self) -> Option<I> {
        self.words()
            .iter()
            .position(|&word| word != !0)
            .map(|at| I::from_bit(at * 64 + self.words()[at].trailing_ones() as usize))
    }

    /// Whether every member is one of `other`'s.
    pub fn is_subset<T: Words>(&self, other: &Bits<T, I>) -> bool {
        let (ours, theirs) = (self.words(), other.words());
        let shared = ours.len().min(theirs.len());
        let outside = ours[..shared].iter().zip(theirs).fold(0, |acc, (a, b)| acc | (a & !b));
        outside == 0 && is_zero(&ours[shared..])
    }

    /// Whether no member is one of `other`'s.
    pub fn is_disjoint<T: Words>(&self, other: &Bits<T, I>) -> bool {
        self.words().iter().zip(other.words()).fold(0, |acc, (a, b)| acc | (a & b)) == 0
    }
}

impl<S: WordsMut, I: BitIndex> Bits<S, I> {
    #[inline]
    pub fn words_mut(&mut self) -> &mut [u64] {
        self.words.words_mut()
    }

    #[inline]
    pub fn as_slice_mut(&mut self) -> BitSliceMut<'_, I> {
        Bits::from_words(self.words.words_mut())
    }

    /// Adds `at`; whether it was not a member. Panics past the capacity.
    #[inline]
    pub fn insert(&mut self, at: I) -> bool {
        let (word, mask) = locate(at.to_bit());
        let word = &mut self.words_mut()[word];
        let added = *word & mask == 0;
        *word |= mask;
        added
    }

    /// Takes `at` out; whether it was a member.
    #[inline]
    pub fn remove(&mut self, at: I) -> bool {
        let (word, mask) = locate(at.to_bit());
        match self.words_mut().get_mut(word) {
            Some(word) => {
                let had = *word & mask != 0;
                *word &= !mask;
                had
            }
            None => false,
        }
    }

    /// Makes `at` a member or not. Panics past the capacity.
    #[inline]
    pub fn set(&mut self, at: I, on: bool) {
        let bit = at.to_bit();
        let word = &mut self.words_mut()[bit / 64];
        *word = (*word & !(1 << (bit % 64))) | (u64::from(on) << (bit % 64));
    }

    /// Adds every member of `range`, a word at a time. Panics past the capacity.
    pub fn insert_range(&mut self, range: Range<I>) {
        let words = self.words_mut();
        range_masks(range.start.to_bit()..range.end.to_bit(), |word, mask| words[word] |= mask);
    }

    /// Takes out every member of `range`, a word at a time. Panics past the capacity.
    pub fn remove_range(&mut self, range: Range<I>) {
        let words = self.words_mut();
        range_masks(range.start.to_bit()..range.end.to_bit(), |word, mask| words[word] &= !mask);
    }

    /// Takes out every member, keeping the words.
    pub fn clear(&mut self) {
        self.words_mut().fill(0);
    }

    /// Adds `other`'s members; whether that added any. Panics when `other` has a member past
    /// this one's capacity.
    #[inline]
    pub fn union_with<T: Words>(&mut self, other: &Bits<T, I>) -> bool {
        union_words(self.words_mut(), other.words())
    }

    /// Keeps only `other`'s members; whether that took any out.
    #[inline]
    pub fn intersect_with<T: Words>(&mut self, other: &Bits<T, I>) -> bool {
        intersect_words(self.words_mut(), other.words())
    }

    /// Takes out `other`'s members; whether that took any out.
    #[inline]
    pub fn subtract<T: Words>(&mut self, other: &Bits<T, I>) -> bool {
        subtract_words(self.words_mut(), other.words())
    }

    /// ORs bits `range` of `from` into bits `at..at + range.len()` of this set, up to 64 at a
    /// time: a shifted load of `from`'s word and one or two ORs into this one's per step. A
    /// splice of bitmaps (a tree's node bits built from ranges of old ones). Panics past this
    /// set's capacity or `from`'s words.
    pub fn or_range_at<T: Words>(&mut self, at: usize, from: &Bits<T, I>, range: Range<usize>) {
        let src = from.words();
        assert!(
            range.start <= range.end && range.end <= src.len() * 64,
            "bits {range:?} of a set of {} bits",
            src.len() * 64
        );
        let dst = self.words_mut();
        assert!(
            at + range.len() <= dst.len() * 64,
            "{} bits at {at} of a set of {} bits",
            range.len(),
            dst.len() * 64
        );
        let (mut bit, mut to) = (range.start, at);
        while bit < range.end {
            let n = (64 - bit % 64).min(range.end - bit);
            let bits = (src[bit / 64] >> (bit % 64)) & (u64::MAX >> (64 - n));
            let shift = to % 64;
            dst[to / 64] |= bits << shift;
            if shift + n > 64 {
                dst[to / 64 + 1] |= bits >> (64 - shift);
            }
            bit += n;
            to += n;
        }
    }

    /// Makes its members `other`'s. Panics when `other` has a member past this one's capacity.
    pub fn copy_from<T: Words>(&mut self, other: &Bits<T, I>) {
        let (ours, theirs) = (self.words_mut(), other.words());
        let shared = ours.len().min(theirs.len());
        assert!(is_zero(&theirs[shared..]), "a copy of a set with a member past {} bits", ours.len() * 64);
        ours[..shared].copy_from_slice(&theirs[..shared]);
        ours[shared..].fill(0);
    }
}

/// Sets whose storage starts empty on its own; one in an allocator's `Vec` starts from
/// [`Bits::from_words`] of `Vec::new_in`.
impl<S: GrowWords + Default, I: BitIndex> Bits<S, I> {
    /// The empty set with room for the bits below `len`.
    pub fn new(len: usize) -> Self {
        let mut words = S::default();
        words.resize_words(words_for(len));
        Bits::from_words(words)
    }

    /// The set of every bit below `len`.
    pub fn filled(len: usize) -> Self {
        let mut out = Self::new(len);
        let words = out.words_mut();
        range_masks(0..len, |word, mask| words[word] |= mask);
        out
    }
}

impl<S: GrowWords, I: BitIndex> Bits<S, I> {
    /// Room for the bits below `len`; it never shrinks.
    #[inline]
    pub fn grow(&mut self, len: usize) {
        let words = words_for(len);
        if words > self.words().len() {
            self.words.resize_words(words);
        }
    }

    /// [`Bits::insert`], growing it to hold `at`.
    #[inline]
    pub fn insert_growing(&mut self, at: I) -> bool {
        self.grow(at.to_bit() + 1);
        self.insert(at)
    }

    /// [`Bits::set`], growing it to hold `at` either way: a set kept beside a list, a bit per
    /// position, has the list's words whatever its last bit is.
    #[inline]
    pub fn set_growing(&mut self, at: I, on: bool) {
        self.grow(at.to_bit() + 1);
        self.set(at, on);
    }

    /// [`Bits::union_with`], growing it to hold `other`'s words.
    pub fn union_growing<T: Words>(&mut self, other: &Bits<T, I>) -> bool {
        self.grow(other.words().len() * 64);
        self.union_with(other)
    }

    /// Empties it and sizes it for the bits below `len`, keeping its buffer.
    pub fn reset(&mut self, len: usize) {
        self.words.resize_words(0);
        self.words.resize_words(words_for(len));
    }

    /// Makes its words `words`, keeping its buffer.
    pub fn assign_words(&mut self, words: &[u64]) {
        self.words.resize_words(words.len());
        self.words_mut().copy_from_slice(words);
    }

    /// Keeps the members below `len` and the words they take.
    pub fn truncate(&mut self, len: usize) {
        let words = words_for(len);
        if self.words().len() > words {
            self.words.resize_words(words);
        }
        if len % 64 != 0
            && let Some(last) = self.words_mut().get_mut(words - 1)
        {
            *last &= below(len);
        }
    }

    /// Gives back the room past its words.
    pub fn shrink_to_fit(&mut self) {
        self.words.shrink_words();
    }
}

/// The operations on an inline set that need no index type, `const` for sets made at compile
/// time (a target's features, a group of lints): bits are `usize`s here.
impl<const W: usize, I> Bits<[u64; W], I> {
    pub const EMPTY: Self = Bits::from_words([0; W]);

    /// The set of every bit below `len`, at most `W * 64`.
    pub const fn all_below(len: usize) -> Self {
        assert!(len <= W * 64, "a set of more bits than its words hold");
        let mut words = [0; W];
        let mut at = 0;
        while at < len / 64 {
            words[at] = !0;
            at += 1;
        }
        if len % 64 != 0 {
            words[len / 64] = below(len);
        }
        Bits::from_words(words)
    }

    /// The set with bit `bit` too.
    #[inline]
    pub const fn with_bit(mut self, bit: usize) -> Self {
        self.words[bit / 64] |= 1 << (bit % 64);
        self
    }

    #[inline]
    pub const fn has_bit(&self, bit: usize) -> bool {
        bit / 64 < W && self.words[bit / 64] >> (bit % 64) & 1 == 1
    }

    /// The union of the two.
    pub const fn or(mut self, other: &Self) -> Self {
        let mut at = 0;
        while at < W {
            self.words[at] |= other.words[at];
            at += 1;
        }
        self
    }

    /// The intersection of the two.
    pub const fn and(mut self, other: &Self) -> Self {
        let mut at = 0;
        while at < W {
            self.words[at] &= other.words[at];
            at += 1;
        }
        self
    }

    /// This set without `other`'s members.
    pub const fn and_not(mut self, other: &Self) -> Self {
        let mut at = 0;
        while at < W {
            self.words[at] &= !other.words[at];
            at += 1;
        }
        self
    }

    /// The inline words.
    pub const fn array(&self) -> &[u64; W] {
        &self.words
    }
}

impl<S: Clone, I> Clone for Bits<S, I> {
    #[inline]
    fn clone(&self) -> Self {
        Bits::from_words(self.words.clone()) // clone: a set's words, where its owner copies the set
    }

    fn clone_from(&mut self, source: &Self) {
        self.words.clone_from(&source.words);
    }
}

impl<S: Copy, I> Copy for Bits<S, I> {}

impl<S: Words, I> PartialEq for Bits<S, I> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.words.words() == other.words.words()
    }
}

impl<S: Words, I> Eq for Bits<S, I> {}

impl<S: Words, I> hash::Hash for Bits<S, I> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.words.words().hash(state);
    }
}

impl<S: Default, I> Default for Bits<S, I> {
    fn default() -> Self {
        Bits::from_words(S::default())
    }
}

impl<S: Words, I> fmt::Debug for Bits<S, I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(Ones::<usize>::new(self.words.words())).finish()
    }
}

/// Its words, as `S` writes them: a `Vec` set persists as the `Vec`.
impl<S: Serialize, I> Serialize for Bits<S, I> {
    fn serialize<Z: Serializer>(&self, serializer: Z) -> Result<Z::Ok, Z::Error> {
        self.words.serialize(serializer)
    }
}

impl<'de, S: Deserialize<'de>, I> Deserialize<'de> for Bits<S, I> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        S::deserialize(deserializer).map(Bits::from_words)
    }
}

impl<S: GrowWords + Default, I: BitIndex> FromIterator<I> for Bits<S, I> {
    fn from_iter<T: IntoIterator<Item = I>>(members: T) -> Self {
        let mut set: Self = Bits::from_words(S::default());
        set.extend(members);
        set
    }
}

impl<S: GrowWords, I: BitIndex> Extend<I> for Bits<S, I> {
    fn extend<T: IntoIterator<Item = I>>(&mut self, members: T) {
        for at in members {
            self.insert_growing(at);
        }
    }
}

impl<'a, S: Words, I: BitIndex> IntoIterator for &'a Bits<S, I> {
    type Item = I;
    type IntoIter = Ones<'a, I>;

    fn into_iter(self) -> Ones<'a, I> {
        self.iter()
    }
}

/// The set bits of some words, from either end: one `trailing_zeros` (or `leading_zeros`)
/// a member, and a load a word.
#[derive(Clone)]
pub struct Ones<'a, I = usize> {
    front: u64,
    /// The bit of `front`'s bit 0.
    front_at: usize,
    back: u64,
    back_at: usize,
    /// The words between `front`'s and `back`'s.
    middle: std::slice::Iter<'a, u64>,
    index: PhantomData<fn() -> I>,
}

impl<'a, I> Ones<'a, I> {
    pub fn new(words: &'a [u64]) -> Self {
        let (front, back, back_at, middle) = match words {
            [] => (0, 0, 0, [].iter()),
            [only] => (*only, 0, 0, [].iter()),
            [first, middle @ .., last] => (*first, *last, (words.len() - 1) * 64, middle.iter()),
        };
        Ones { front, front_at: 0, back, back_at, middle, index: PhantomData }
    }
}

impl<I: BitIndex> Iterator for Ones<'_, I> {
    type Item = I;

    #[inline]
    fn next(&mut self) -> Option<I> {
        loop {
            if self.front != 0 {
                let bit = self.front.trailing_zeros() as usize;
                self.front &= self.front - 1;
                return Some(I::from_bit(self.front_at + bit));
            }
            match self.middle.next() {
                Some(&word) => {
                    self.front = word;
                    self.front_at += 64;
                }
                None => {
                    if self.back == 0 {
                        return None;
                    }
                    let bit = self.back.trailing_zeros() as usize;
                    self.back &= self.back - 1;
                    return Some(I::from_bit(self.back_at + bit));
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let ends = (self.front.count_ones() + self.back.count_ones()) as usize;
        (ends, Some(ends + self.middle.len() * 64))
    }

    fn count(self) -> usize {
        (self.front.count_ones() + self.back.count_ones()) as usize + count_words(self.middle.as_slice())
    }
}

impl<I: BitIndex> DoubleEndedIterator for Ones<'_, I> {
    #[inline]
    fn next_back(&mut self) -> Option<I> {
        loop {
            if self.back != 0 {
                let bit = 63 - self.back.leading_zeros() as usize;
                self.back &= !(1 << bit);
                return Some(I::from_bit(self.back_at + bit));
            }
            match self.middle.next_back() {
                Some(&word) => {
                    self.back = word;
                    self.back_at -= 64;
                }
                None => {
                    if self.front == 0 {
                        return None;
                    }
                    let bit = 63 - self.front.leading_zeros() as usize;
                    self.front &= !(1 << bit);
                    return Some(I::from_bit(self.front_at + bit));
                }
            }
        }
    }
}

impl<I: BitIndex> FusedIterator for Ones<'_, I> {}

#[inline]
fn is_zero(words: &[u64]) -> bool {
    words.iter().fold(0, |acc, word| acc | word) == 0
}

#[inline]
fn count_words(words: &[u64]) -> usize {
    words.iter().map(|word| word.count_ones() as usize).sum()
}

/// `into |= from`; whether that set any bit.
#[inline]
fn union_words(into: &mut [u64], from: &[u64]) -> bool {
    let shared = into.len().min(from.len());
    assert!(is_zero(&from[shared..]), "a union with a member past {} bits", into.len() * 64);
    let mut changed = 0;
    for (word, &other) in into.iter_mut().zip(&from[..shared]) {
        changed |= other & !*word;
        *word |= other;
    }
    changed != 0
}

/// `into &= from`, the words past `from`'s cleared; whether that cleared any bit.
#[inline]
fn intersect_words(into: &mut [u64], from: &[u64]) -> bool {
    let (head, tail) = into.split_at_mut(into.len().min(from.len()));
    let mut changed = 0;
    for (word, &other) in head.iter_mut().zip(from) {
        changed |= *word & !other;
        *word &= other;
    }
    for word in tail {
        changed |= *word;
        *word = 0;
    }
    changed != 0
}

/// `into &= !from`; whether that cleared any bit.
#[inline]
fn subtract_words(into: &mut [u64], from: &[u64]) -> bool {
    let mut changed = 0;
    for (word, &other) in into.iter_mut().zip(from) {
        changed |= *word & other;
        *word &= !other;
    }
    changed != 0
}

/// A set of `columns` bits per row, every row in one buffer `S`, a row found by one multiply:
/// the entry sets of every block of a body. The index type comes first, unlike [`Bits`],
/// because an owned matrix (`BitMatrix<Local>`) is what nearly every caller names; a matrix
/// over a frame's words is `BitMatrix<I, &'f mut [u64]>`.
pub struct BitMatrix<I = usize, S = Vec<u64>> {
    rows: usize,
    row_words: usize,
    data: S,
    index: PhantomData<fn(I) -> I>,
}

impl<I: BitIndex> BitMatrix<I> {
    pub fn new(rows: usize, columns: usize) -> Self {
        let row_words = words_for(columns);
        // alloc: one buffer for every row, made once per analysis
        BitMatrix { rows, row_words, data: vec![0; rows * row_words], index: PhantomData }
    }

    /// Empties it and sizes it for `rows` rows of `columns` bits, keeping its buffer.
    pub fn reset(&mut self, rows: usize, columns: usize) {
        self.rows = rows;
        self.row_words = words_for(columns);
        self.data.clear();
        self.data.resize(rows * self.row_words, 0); // alloc: grown to the largest analysis, then reused
    }
}

impl<I: BitIndex, S: Words> BitMatrix<I, S> {
    /// The matrix of `rows` rows of `columns` bits in `data`, which holds exactly their words.
    pub fn from_words(rows: usize, columns: usize, data: S) -> Self {
        let row_words = words_for(columns);
        assert_eq!(
            data.words().len(),
            rows * row_words,
            "{rows} rows of {columns} bits in {} words",
            data.words().len()
        );
        BitMatrix { rows, row_words, data, index: PhantomData }
    }

    pub fn into_words(self) -> S {
        self.data
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The words of a row.
    pub fn row_words(&self) -> usize {
        self.row_words
    }

    #[inline]
    pub fn row(&self, row: usize) -> BitSlice<'_, I> {
        Bits::from_words(&self.data.words()[row * self.row_words..(row + 1) * self.row_words])
    }

    #[inline]
    pub fn contains(&self, row: usize, at: I) -> bool {
        self.row(row).contains(at)
    }

    /// The members of `row`, ascending.
    pub fn iter_row(&self, row: usize) -> Ones<'_, I> {
        let words = self.row_words;
        Ones::new(&self.data.words()[row * words..(row + 1) * words])
    }

    /// Every row's words, row after row.
    pub fn words(&self) -> &[u64] {
        self.data.words()
    }
}

impl<I: BitIndex, S: WordsMut> BitMatrix<I, S> {
    #[inline]
    pub fn row_mut(&mut self, row: usize) -> BitSliceMut<'_, I> {
        let words = self.row_words;
        Bits::from_words(&mut self.data.words_mut()[row * words..(row + 1) * words])
    }

    /// Adds `at` to `row`; whether it was not there.
    #[inline]
    pub fn insert(&mut self, row: usize, at: I) -> bool {
        self.row_mut(row).insert(at)
    }

    #[inline]
    pub fn remove(&mut self, row: usize, at: I) -> bool {
        self.row_mut(row).remove(at)
    }

    /// Row `into` gains the members of row `from`; whether it gained any.
    pub fn union_rows(&mut self, into: usize, from: usize) -> bool {
        assert!(into < self.rows && from < self.rows, "rows {into} and {from} of a matrix of {}", self.rows);
        if into == from {
            return false;
        }
        let words = self.row_words;
        let data = self.data.words_mut();
        let (into, from) = if into < from {
            let (low, high) = data.split_at_mut(from * words);
            (&mut low[into * words..(into + 1) * words], &high[..words])
        } else {
            let (low, high) = data.split_at_mut(into * words);
            (&mut high[..words], &low[from * words..(from + 1) * words])
        };
        union_words(into, from)
    }

    /// Row `row` gains `other`'s members; whether it gained any.
    pub fn union_row_with<T: Words>(&mut self, row: usize, other: &Bits<T, I>) -> bool {
        self.row_mut(row).union_with(other)
    }

    /// Takes out every member of every row.
    pub fn clear(&mut self) {
        self.data.words_mut().fill(0);
    }

    pub fn words_mut(&mut self) -> &mut [u64] {
        self.data.words_mut()
    }
}

impl<I, S: Clone> Clone for BitMatrix<I, S> {
    fn clone(&self) -> Self {
        // clone: a matrix's buffer, where its owner copies the analysis
        BitMatrix { rows: self.rows, row_words: self.row_words, data: self.data.clone(), index: PhantomData }
    }
}

impl<I, S: Words> PartialEq for BitMatrix<I, S> {
    fn eq(&self, other: &Self) -> bool {
        (self.rows, self.row_words) == (other.rows, other.row_words) && self.data.words() == other.data.words()
    }
}

impl<I, S: Words> Eq for BitMatrix<I, S> {}

impl<I, S: Default> Default for BitMatrix<I, S> {
    fn default() -> Self {
        BitMatrix { rows: 0, row_words: 0, data: S::default(), index: PhantomData } // alloc: none, empty
    }
}

impl<I, S: Words> fmt::Debug for BitMatrix<I, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let words = self.row_words;
        let data = self.data.words();
        let rows = (0..self.rows).map(|row| DebugOnes(Ones::new(&data[row * words..(row + 1) * words])));
        f.debug_list().entries(rows).finish()
    }
}

struct DebugOnes<'a>(Ones<'a, usize>);

impl fmt::Debug for DebugOnes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.0.clone()).finish() // clone: an iterator's cursor, for `Debug`
    }
}

/// Per word of `words`, how many bits are set in the words before it; `None` past `u32::MAX`
/// of them.
pub fn ranks_of(words: &[u64]) -> Option<Box<[u32]>> {
    // An `Option` collect knows no length, so it would grow a buffer and shrink it to fit.
    // alloc: a ranked set's counts, made once with it at their length
    let mut ranks = Vec::with_capacity(words.len());
    let mut before = 0u32;
    for word in words {
        ranks.push(before);
        before = before.checked_add(word.count_ones())?;
    }
    Some(ranks.into_boxed_slice())
}

/// How many bits of `words` are set below bit `at`, `ranks` being [`ranks_of`] `words`: one
/// popcount, for columns held elsewhere (a mapped tree's).
#[inline]
pub fn rank_in(words: &[u64], ranks: &[u32], at: usize) -> usize {
    let word = at / 64;
    ranks[word] as usize + (words[word] & below(at)).count_ones() as usize
}

/// A fixed set of `len` bits that keeps, per word, how many members come before it, so a
/// member's rank (where its value sits in a column of the members' values alone) is one
/// popcount, as in Vigna's rank9 with a `u32` count a word: 1.5 bits a bit.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)] // clone: held by blaze-items' `HashColumns` and hir-ty's columns, which derive it
pub struct Ranked {
    words: Box<[u64]>,
    ranks: Box<[u32]>,
    len: usize,
}

impl Ranked {
    /// The set of the positions of `bits` that are `true`, of as many bits as it yields.
    /// Panics past `u32::MAX` members.
    pub fn from_bools(bits: impl IntoIterator<Item = bool>) -> Ranked {
        let bits = bits.into_iter();
        // alloc: a ranked set's words, made once at its length when `bits` knows it
        let mut words: Vec<u64> = Vec::with_capacity(words_for(bits.size_hint().0));
        let mut len = 0usize;
        for bit in bits {
            if len % 64 == 0 {
                words.push(0);
            }
            words[len / 64] |= u64::from(bit) << (len % 64);
            len += 1;
        }
        Ranked::from_words(words, len).expect("a ranked set of more than u32::MAX members")
    }

    /// The set of `len` bits whose words are `words`; `None` past `u32::MAX` members. Panics
    /// when `words` is not `words_for(len)` words or has a bit set at or past `len`.
    pub fn from_words(words: Vec<u64>, len: usize) -> Option<Ranked> {
        assert_eq!(words.len(), words_for(len), "the words of a ranked set of {len} bits");
        assert!(
            len % 64 == 0 || words.last().is_none_or(|last| last & !below(len) == 0),
            "a ranked set of {len} bits with a bit past its end"
        );
        let ranks = ranks_of(&words)?;
        Some(Ranked { words: words.into_boxed_slice(), ranks, len })
    }

    /// Its bits, members or not.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it has no bits (not: no members).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn words(&self) -> &[u64] {
        &self.words
    }

    pub fn ranks(&self) -> &[u32] {
        &self.ranks
    }

    pub fn bits(&self) -> BitSlice<'_> {
        Bits::from_words(&self.words[..])
    }

    /// Whether `at` is a member. Panics at or past its length.
    #[track_caller]
    #[inline]
    pub fn contains(&self, at: usize) -> bool {
        assert!(at < self.len, "bit {at} of a ranked set of {}", self.len);
        self.words[at / 64] >> (at % 64) & 1 == 1
    }

    /// How many members are below `at`. Panics at or past its length.
    #[track_caller]
    #[inline]
    pub fn rank(&self, at: usize) -> usize {
        assert!(at < self.len, "bit {at} of a ranked set of {}", self.len);
        rank_in(&self.words, &self.ranks, at)
    }

    /// `at`'s rank when it is a member; `None` when it is not, or is past its length.
    #[inline]
    pub fn place(&self, at: usize) -> Option<usize> {
        if at >= self.len {
            return None;
        }
        let (word, mask) = locate(at);
        (self.words[word] & mask != 0).then(|| rank_in(&self.words, &self.ranks, at))
    }

    /// How many members it has.
    pub fn count(&self) -> usize {
        match (self.ranks.last(), self.words.last()) {
            (Some(&rank), Some(word)) => rank as usize + word.count_ones() as usize,
            _ => 0,
        }
    }

    /// The members, ascending.
    pub fn iter(&self) -> Ones<'_> {
        Ones::new(&self.words)
    }

    /// Each member with its rank, ascending.
    pub fn iter_ranked(&self) -> impl DoubleEndedIterator<Item = (usize, usize)> + '_ {
        self.iter().map(|at| (at, rank_in(&self.words, &self.ranks, at)))
    }
}

/// Bits set and read from many threads, each a load or a read-modify-write of its word at the
/// ordering the caller names: the columns of flags a table keeps beside its slots. Its
/// operations are [`AtomicBitSlice`]'s, for words held elsewhere (a reserved region).
#[derive(Debug, Default)]
pub struct AtomicBits {
    words: Box<[AtomicU64]>,
}

impl AtomicBits {
    /// Room for the bits below `len`, none set.
    pub fn new(len: usize) -> AtomicBits {
        // alloc: a table's flags, made once with it
        AtomicBits { words: (0..words_for(len)).map(|_| AtomicU64::new(0)).collect() }
    }

    #[inline]
    pub fn as_slice(&self) -> AtomicBitSlice<'_> {
        AtomicBitSlice::from_words(&self.words)
    }

    /// The bits its words hold.
    pub fn capacity(&self) -> usize {
        self.as_slice().capacity()
    }

    /// [`AtomicBitSlice::contains`].
    #[inline]
    pub fn contains(&self, at: usize, order: Ordering) -> bool {
        self.as_slice().contains(at, order)
    }

    /// [`AtomicBitSlice::insert`].
    #[inline]
    pub fn insert(&self, at: usize, order: Ordering) -> bool {
        self.as_slice().insert(at, order)
    }

    /// [`AtomicBitSlice::insert_if_clear`].
    #[inline]
    pub fn insert_if_clear(&self, at: usize, order: Ordering) -> bool {
        self.as_slice().insert_if_clear(at, order)
    }

    /// [`AtomicBitSlice::remove`].
    #[inline]
    pub fn remove(&self, at: usize, order: Ordering) -> bool {
        self.as_slice().remove(at, order)
    }

    /// The words, for a caller that moves whole words (a rotation of periods).
    pub fn words(&self) -> &[AtomicU64] {
        &self.words
    }

    /// Clears every bit, with no other thread holding it.
    pub fn clear(&mut self) {
        for word in self.words.iter_mut() {
            *word.get_mut() = 0;
        }
    }
}

/// [`AtomicBits`]' operations on atomic words held elsewhere.
#[derive(Clone, Copy, Debug)]
pub struct AtomicBitSlice<'a> {
    words: &'a [AtomicU64],
}

impl<'a> AtomicBitSlice<'a> {
    #[inline]
    pub fn from_words(words: &'a [AtomicU64]) -> Self {
        AtomicBitSlice { words }
    }

    /// The bits its words hold.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.words.len() * 64
    }

    pub fn words(&self) -> &'a [AtomicU64] {
        self.words
    }

    /// Whether bit `at` is set; past the capacity it is not.
    #[inline]
    pub fn contains(&self, at: usize, order: Ordering) -> bool {
        let (word, mask) = locate(at);
        self.words.get(word).is_some_and(|it| it.load(order) & mask != 0)
    }

    /// Sets bit `at`; whether this call set it. Panics past the capacity.
    #[inline]
    pub fn insert(&self, at: usize, order: Ordering) -> bool {
        let (word, mask) = locate(at);
        self.words[word].fetch_or(mask, order) & mask == 0
    }

    /// [`AtomicBitSlice::insert`] after a relaxed load finds the bit clear: a bit set already
    /// costs a read, and its line stays shared between the cores reading it. Panics past the
    /// capacity.
    #[inline]
    pub fn insert_if_clear(&self, at: usize, order: Ordering) -> bool {
        let (word, mask) = locate(at);
        let word = &self.words[word];
        word.load(Ordering::Relaxed) & mask == 0 && word.fetch_or(mask, order) & mask == 0
    }

    /// Clears bit `at`; whether this call cleared it. Panics past the capacity.
    #[inline]
    pub fn remove(&self, at: usize, order: Ordering) -> bool {
        let (word, mask) = locate(at);
        self.words[word].fetch_and(!mask, order) & mask != 0
    }

    /// Moves every bit into `to`, a word at a time, leaving these clear: a period of flags
    /// becoming the one before. A bit set meanwhile lands in one or the other. `order` is the
    /// swap's and the store's, so `Relaxed`, `Release` or `SeqCst`. Panics when `to` is
    /// shorter.
    pub fn move_into(&self, to: AtomicBitSlice<'_>, order: Ordering) {
        assert!(to.words.len() >= self.words.len(), "a move of {} words into {}", self.words.len(), to.words.len());
        for (from, to) in self.words.iter().zip(to.words) {
            to.store(from.swap(0, order), order);
        }
    }
}
