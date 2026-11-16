//! Offsets and ranges in UTF-8 text: [`TextSize`] is a byte count in 32 bits, and [`TextRange`]
//! a half-open `start..end` of two of them. It replaces the text-size crate with the same
//! surface the tree uses, so the syntax trees (`syntax-tree`), spans (`span`), token trees (`tt`)
//! and test fixtures (`test-utils`) share one type from a crate with no dependencies.
//!
//! The `Debug` forms (`5`, `2..7`) are the ones snapshot tests print, kept as text-size had them.

#![no_std]

extern crate alloc;

use core::{
    cmp::Ordering,
    fmt, iter,
    num::TryFromIntError,
    ops::{Add, AddAssign, Bound, Index, IndexMut, Range, RangeBounds, Sub, SubAssign},
};

use alloc::string::String;

/// A byte offset or length in UTF-8 text.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextSize {
    raw: u32,
}

impl fmt::Debug for TextSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.raw)
    }
}

impl TextSize {
    #[inline]
    pub const fn new(raw: u32) -> TextSize {
        TextSize { raw }
    }

    /// The length of `text` in bytes. Panics past `u32::MAX`, as no source file blaze reads is
    /// that long.
    #[inline]
    pub fn of<T: TextLen>(text: T) -> TextSize {
        text.text_len()
    }

    #[inline]
    pub const fn checked_add(self, rhs: TextSize) -> Option<TextSize> {
        match self.raw.checked_add(rhs.raw) {
            Some(raw) => Some(TextSize { raw }),
            None => None,
        }
    }

    #[inline]
    pub const fn checked_sub(self, rhs: TextSize) -> Option<TextSize> {
        match self.raw.checked_sub(rhs.raw) {
            Some(raw) => Some(TextSize { raw }),
            None => None,
        }
    }
}

impl From<u32> for TextSize {
    #[inline]
    fn from(raw: u32) -> Self {
        TextSize { raw }
    }
}

impl From<TextSize> for u32 {
    #[inline]
    fn from(value: TextSize) -> Self {
        value.raw
    }
}

impl TryFrom<usize> for TextSize {
    type Error = TryFromIntError;
    #[inline]
    fn try_from(value: usize) -> Result<Self, TryFromIntError> {
        Ok(u32::try_from(value)?.into())
    }
}

impl From<TextSize> for usize {
    #[inline]
    fn from(value: TextSize) -> Self {
        // A `u32` fits every `usize` blaze runs on (64-bit hosts only).
        value.raw as usize
    }
}

macro_rules! size_ops {
    ($Op:ident, $f:ident, $op:tt) => {
        impl $Op<TextSize> for TextSize {
            type Output = TextSize;
            #[inline]
            fn $f(self, other: TextSize) -> TextSize {
                TextSize { raw: self.raw $op other.raw }
            }
        }
        impl $Op<&TextSize> for TextSize {
            type Output = TextSize;
            #[inline]
            fn $f(self, other: &TextSize) -> TextSize {
                self $op *other
            }
        }
        impl<T> $Op<T> for &TextSize
        where
            TextSize: $Op<T, Output = TextSize>,
        {
            type Output = TextSize;
            #[inline]
            fn $f(self, other: T) -> TextSize {
                *self $op other
            }
        }
    };
}

size_ops!(Add, add, +);
size_ops!(Sub, sub, -);

impl<A> AddAssign<A> for TextSize
where
    TextSize: Add<A, Output = TextSize>,
{
    #[inline]
    fn add_assign(&mut self, rhs: A) {
        *self = *self + rhs;
    }
}

impl<S> SubAssign<S> for TextSize
where
    TextSize: Sub<S, Output = TextSize>,
{
    #[inline]
    fn sub_assign(&mut self, rhs: S) {
        *self = *self - rhs;
    }
}

impl<A> iter::Sum<A> for TextSize
where
    TextSize: Add<A, Output = TextSize>,
{
    #[inline]
    fn sum<I: Iterator<Item = A>>(iter: I) -> TextSize {
        iter.fold(TextSize::new(0), Add::add)
    }
}

mod sealed {
    pub trait Sealed {}
}

/// What has a length in bytes of text, for [`TextSize::of`].
pub trait TextLen: Copy + sealed::Sealed {
    fn text_len(self) -> TextSize;
}

impl sealed::Sealed for &'_ str {}
impl TextLen for &'_ str {
    #[inline]
    fn text_len(self) -> TextSize {
        match TextSize::try_from(self.len()) {
            Ok(size) => size,
            Err(_) => panic!("a text of {} bytes is longer than a `TextSize` holds", self.len()),
        }
    }
}

impl sealed::Sealed for &'_ String {}
impl TextLen for &'_ String {
    #[inline]
    fn text_len(self) -> TextSize {
        self.as_str().text_len()
    }
}

impl sealed::Sealed for char {}
impl TextLen for char {
    #[inline]
    fn text_len(self) -> TextSize {
        // `len_utf8` is at most 4.
        TextSize::new(self.len_utf8() as u32)
    }
}

/// The half-open range `start..end` of text, with `start <= end`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct TextRange {
    start: TextSize,
    end: TextSize,
}

impl fmt::Debug for TextRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start.raw, self.end.raw)
    }
}

impl TextRange {
    /// `start..end`; panics when `end < start`.
    #[inline]
    pub const fn new(start: TextSize, end: TextSize) -> TextRange {
        assert!(start.raw <= end.raw, "a `TextRange` ends before it starts");
        TextRange { start, end }
    }

    /// `offset..offset + len`.
    #[inline]
    pub const fn at(offset: TextSize, len: TextSize) -> TextRange {
        TextRange::new(offset, TextSize::new(offset.raw + len.raw))
    }

    #[inline]
    pub const fn empty(offset: TextSize) -> TextRange {
        TextRange { start: offset, end: offset }
    }

    /// `0..end`.
    #[inline]
    pub const fn up_to(end: TextSize) -> TextRange {
        TextRange { start: TextSize::new(0), end }
    }

    #[inline]
    pub const fn start(self) -> TextSize {
        self.start
    }

    #[inline]
    pub const fn end(self) -> TextSize {
        self.end
    }

    #[inline]
    pub const fn len(self) -> TextSize {
        TextSize { raw: self.end.raw - self.start.raw }
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.start.raw == self.end.raw
    }

    /// `start <= offset < end`.
    #[inline]
    pub fn contains(self, offset: TextSize) -> bool {
        self.start <= offset && offset < self.end
    }

    /// `start <= offset <= end`.
    #[inline]
    pub fn contains_inclusive(self, offset: TextSize) -> bool {
        self.start <= offset && offset <= self.end
    }

    /// Whether `other` lies inside `self`, its ends included.
    #[inline]
    pub fn contains_range(self, other: TextRange) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    /// The common part, empty when the two only touch, `None` when they are apart.
    #[inline]
    pub fn intersect(self, other: TextRange) -> Option<TextRange> {
        let start = Ord::max(self.start, other.start);
        let end = Ord::min(self.end, other.end);
        if end < start {
            return None;
        }
        Some(TextRange::new(start, end))
    }

    /// The smallest range holding both.
    #[inline]
    pub fn cover(self, other: TextRange) -> TextRange {
        let start = Ord::min(self.start, other.start);
        let end = Ord::max(self.end, other.end);
        TextRange::new(start, end)
    }

    /// The smallest range holding `self` and `offset`.
    #[inline]
    pub fn cover_offset(self, offset: TextSize) -> TextRange {
        self.cover(TextRange::empty(offset))
    }

    #[inline]
    pub fn checked_add(self, offset: TextSize) -> Option<TextRange> {
        Some(TextRange { start: self.start.checked_add(offset)?, end: self.end.checked_add(offset)? })
    }

    #[inline]
    pub fn checked_sub(self, offset: TextSize) -> Option<TextRange> {
        Some(TextRange { start: self.start.checked_sub(offset)?, end: self.end.checked_sub(offset)? })
    }

    /// `Less` when `self` ends at or before `other` starts, `Greater` the other way round, and
    /// `Equal` when they overlap: the order a binary search over disjoint ranges needs.
    #[inline]
    pub fn ordering(self, other: TextRange) -> Ordering {
        if self.end <= other.start {
            Ordering::Less
        } else if other.end <= self.start {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }
}

impl Index<TextRange> for str {
    type Output = str;
    #[inline]
    fn index(&self, index: TextRange) -> &str {
        &self[Range::<usize>::from(index)]
    }
}

impl Index<TextRange> for String {
    type Output = str;
    #[inline]
    fn index(&self, index: TextRange) -> &str {
        &self[Range::<usize>::from(index)]
    }
}

impl IndexMut<TextRange> for str {
    #[inline]
    fn index_mut(&mut self, index: TextRange) -> &mut str {
        &mut self[Range::<usize>::from(index)]
    }
}

impl IndexMut<TextRange> for String {
    #[inline]
    fn index_mut(&mut self, index: TextRange) -> &mut str {
        &mut self[Range::<usize>::from(index)]
    }
}

impl RangeBounds<TextSize> for TextRange {
    fn start_bound(&self) -> Bound<&TextSize> {
        Bound::Included(&self.start)
    }

    fn end_bound(&self) -> Bound<&TextSize> {
        Bound::Excluded(&self.end)
    }
}

impl<T: From<TextSize>> From<TextRange> for Range<T> {
    #[inline]
    fn from(r: TextRange) -> Self {
        r.start.into()..r.end.into()
    }
}

impl Add<TextSize> for TextRange {
    type Output = TextRange;
    #[inline]
    fn add(self, offset: TextSize) -> TextRange {
        match self.checked_add(offset) {
            Some(range) => range,
            None => panic!("{self:?} + {offset:?} overflows a `TextSize`"),
        }
    }
}

impl Sub<TextSize> for TextRange {
    type Output = TextRange;
    #[inline]
    fn sub(self, offset: TextSize) -> TextRange {
        match self.checked_sub(offset) {
            Some(range) => range,
            None => panic!("{self:?} - {offset:?} goes below zero"),
        }
    }
}

macro_rules! range_ops {
    ($Op:ident, $f:ident, $op:tt) => {
        impl $Op<&TextSize> for TextRange {
            type Output = TextRange;
            #[inline]
            fn $f(self, other: &TextSize) -> TextRange {
                self $op *other
            }
        }
        impl<T> $Op<T> for &TextRange
        where
            TextRange: $Op<T, Output = TextRange>,
        {
            type Output = TextRange;
            #[inline]
            fn $f(self, other: T) -> TextRange {
                *self $op other
            }
        }
    };
}

range_ops!(Add, add, +);
range_ops!(Sub, sub, -);

impl<A> AddAssign<A> for TextRange
where
    TextRange: Add<A, Output = TextRange>,
{
    #[inline]
    fn add_assign(&mut self, rhs: A) {
        *self = *self + rhs;
    }
}

impl<S> SubAssign<S> for TextRange
where
    TextRange: Sub<S, Output = TextRange>,
{
    #[inline]
    fn sub_assign(&mut self, rhs: S) {
        *self = *self - rhs;
    }
}
