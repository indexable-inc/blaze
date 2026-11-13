//! Sizes and offsets as types (docs/v2/plan/design/fallible-bstd.md, decision D29): a count
//! of bytes is a [`ByteLen`] and a position in a file or a buffer an [`Offset`]. Their
//! arithmetic is checked and returns [`Overflow`], naming the operation and its operands;
//! there is no `+` on either, so no sum wraps or panics, and the units do not mix: an offset
//! moves by a length ([`Offset::after`]), two offsets differ by a length ([`Offset::since`]),
//! and adding two offsets, or an offset to a length, does not compile.
//!
//! Both are a `u64` underneath, whatever the target's pointer width: a file is larger than a
//! 32-bit address space. [`ByteLen::in_memory`] is the one way to a `usize`, and it is
//! checked.

use std::fmt;

/// An operation on a [`ByteLen`] or an [`Offset`] whose result does not fit: past `u64::MAX`,
/// below zero, or (for [`ByteLen::in_memory`]) past the target's `usize`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Overflow {
    /// The operation, as its method is named: `"ByteLen::plus"`, `"Offset::since"`.
    pub what: &'static str,
    pub left: u64,
    pub right: u64,
}

impl fmt::Display for Overflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} of {} and {} does not fit its type (rule: sizes and offsets are checked)",
            self.what, self.left, self.right
        )
    }
}

impl std::error::Error for Overflow {}

/// A count of bytes: a length, a limit, a quota, what a budget has left.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct ByteLen(u64);

impl ByteLen {
    pub const ZERO: ByteLen = ByteLen(0);

    #[inline]
    pub const fn new(bytes: u64) -> ByteLen {
        ByteLen(bytes)
    }

    /// The length of something in memory. A `usize` is at most 64 bits on every target blaze
    /// builds for, which the `const` below holds, so nothing is lost.
    #[inline]
    pub const fn of_memory(bytes: usize) -> ByteLen {
        const _: () = assert!(usize::BITS <= u64::BITS);
        ByteLen(bytes as u64)
    }

    /// The length of `bytes`.
    #[inline]
    pub const fn of(bytes: &[u8]) -> ByteLen {
        ByteLen::of_memory(bytes.len())
    }

    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// The count as a length in memory, when this target's `usize` holds it.
    #[inline]
    pub fn in_memory(self) -> Result<usize, Overflow> {
        match usize::try_from(self.0) {
            Ok(bytes) => Ok(bytes),
            Err(_) => {
                Err(Overflow { what: "ByteLen::in_memory", left: self.0, right: usize::MAX as u64 })
            }
        }
    }

    /// `self + more`.
    #[inline]
    pub const fn plus(self, more: ByteLen) -> Result<ByteLen, Overflow> {
        match self.0.checked_add(more.0) {
            Some(sum) => Ok(ByteLen(sum)),
            None => Err(Overflow { what: "ByteLen::plus", left: self.0, right: more.0 }),
        }
    }

    /// `self - less`: what is left of a budget once `less` is taken.
    #[inline]
    pub const fn minus(self, less: ByteLen) -> Result<ByteLen, Overflow> {
        match self.0.checked_sub(less.0) {
            Some(rest) => Ok(ByteLen(rest)),
            None => Err(Overflow { what: "ByteLen::minus", left: self.0, right: less.0 }),
        }
    }

    /// `self * count`: the bytes of `count` things of this size.
    #[inline]
    pub const fn times(self, count: u64) -> Result<ByteLen, Overflow> {
        match self.0.checked_mul(count) {
            Some(product) => Ok(ByteLen(product)),
            None => Err(Overflow { what: "ByteLen::times", left: self.0, right: count }),
        }
    }

    /// The smaller of the two.
    #[inline]
    pub const fn at_most(self, limit: ByteLen) -> ByteLen {
        if self.0 <= limit.0 { self } else { limit }
    }
}

impl fmt::Display for ByteLen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// A position in a file or a buffer, in bytes from its start.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Offset(u64);

impl Offset {
    pub const START: Offset = Offset(0);

    #[inline]
    pub const fn new(bytes_from_start: u64) -> Offset {
        Offset(bytes_from_start)
    }

    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The offset `len` further on: where a read or write of `len` at this offset ends.
    #[inline]
    pub const fn after(self, len: ByteLen) -> Result<Offset, Overflow> {
        match self.0.checked_add(len.0) {
            Some(end) => Ok(Offset(end)),
            None => Err(Overflow { what: "Offset::after", left: self.0, right: len.0 }),
        }
    }

    /// The bytes from `earlier` to this offset; an `earlier` that is the later one is refused.
    #[inline]
    pub const fn since(self, earlier: Offset) -> Result<ByteLen, Overflow> {
        match self.0.checked_sub(earlier.0) {
            Some(len) => Ok(ByteLen(len)),
            None => Err(Overflow { what: "Offset::since", left: self.0, right: earlier.0 }),
        }
    }

    /// The length of a file or buffer that ends here: an offset is as many bytes from the
    /// start.
    #[inline]
    pub const fn from_start(self) -> ByteLen {
        ByteLen(self.0)
    }
}

impl fmt::Display for Offset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "byte {}", self.0)
    }
}
