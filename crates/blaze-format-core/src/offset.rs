//! How wide a reference's offset is: [`Offset`], the type parameter `O` of [`Rel`]`<T, O>`,
//! [`Slice`]`<T, O>` and [`Text`]`<O>`, and [`Unfit`], the refusal of a distance or a length
//! that the width cannot hold.
//!
//! `i32` (with a `u32` length) is the default, and reaches anywhere in a buffer of 2 GiB. A
//! record whose references point a few bytes or kilobytes back (a node's own short column, its
//! children written just before it) takes `i16` (32 KiB back, a `u16` length) or `i8` (128
//! bytes back, a `u8` length): a `Rel` of 2 or 1 bytes, a `Slice` of 4 or 2, against 4 and 8.
//! The writer finds out whether a distance fits when it makes the reference
//! ([`Rel::between_as`]), and is refused by type when it does not; the default `i32` offset's
//! [`Rel::between`] is a plain value, which panics past 2 GiB.
//!
//! [`Text`]: crate::Text
#![blaze::sans_io]

use core::fmt;

use crate::Record;
#[cfg(doc)]
use crate::{Rel, Slice};

mod sealed {
    pub trait Sealed {}
}

/// The integer a reference's offset is held in, and the one its slice's length is: `i32` and
/// `u32`, `i16` and `u16`, or `i8` and `u8`. Implemented only here.
pub trait Offset: Record + Copy + Eq + fmt::Debug + sealed::Sealed {
    /// The length of a [`Slice`] with this offset.
    type Len: Record + Copy + Eq + fmt::Debug;
    /// What the schema hash of a reference of this width adds, and what [`Unfit`] names.
    const NAME: &'static str;
    /// `i32`, whose references' schema hashes are the ones they were before widths existed:
    /// the default adds no part, every other width adds [`Offset::NAME`].
    const WIDE: bool;
    const ZERO: Self;
    const LEN_ZERO: Self::Len;
    /// The offset as a signed distance in bytes.
    fn distance(self) -> i64;
    /// The offset of `distance`, `None` when it does not fit.
    fn of_distance(distance: i64) -> Option<Self>;
    /// The length as a count.
    fn count(len: Self::Len) -> u64;
    /// The length of `count`, `None` when it does not fit.
    fn of_count(count: u64) -> Option<Self::Len>;
}

macro_rules! offset {
    ($($offset:ty, $len:ty, $name:literal, $wide:literal;)*) => {$(
        impl sealed::Sealed for $offset {}

        impl Offset for $offset {
            type Len = $len;
            const NAME: &'static str = $name;
            const WIDE: bool = $wide;
            const ZERO: $offset = 0;
            const LEN_ZERO: $len = 0;
            #[inline]
            fn distance(self) -> i64 {
                i64::from(self)
            }
            #[inline]
            fn of_distance(distance: i64) -> Option<$offset> {
                <$offset>::try_from(distance).ok()
            }
            #[inline]
            fn count(len: $len) -> u64 {
                u64::from(len)
            }
            #[inline]
            fn of_count(count: u64) -> Option<$len> {
                <$len>::try_from(count).ok()
            }
        }
    )*};
}

offset! {
    i8, u8, "i8", false;
    i16, u16, "i16", false;
    i32, u32, "i32", true;
}

/// Why a reference cannot be made between two places of a buffer: what [`Rel::between_as`] and
/// [`Slice::between_as`] refuse. Both places are from the buffer's start.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unfit {
    /// The target is not before the field: a buffer's references point back, at what was
    /// written before the record holding them (a [`Rel`]'s target strictly before, a
    /// [`Slice`]'s at most at the field).
    Forward { field_at: u64, target_at: u64 },
    /// The distance from the field back to the target does not fit an offset of `offset`.
    Far { field_at: u64, target_at: u64, offset: &'static str },
    /// The slice's length does not fit the length of an offset of `offset`.
    Long { field_at: u64, len: u64, offset: &'static str },
}

impl fmt::Display for Unfit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unfit::Forward { field_at, target_at } => write!(
                f,
                "a reference at byte {field_at} to byte {target_at}, which is not written before it"
            ),
            Unfit::Far { field_at, target_at, offset } => write!(
                f,
                "a reference at byte {field_at} to byte {target_at}: {} bytes back is past an offset of {offset}",
                field_at.abs_diff(*target_at)
            ),
            Unfit::Long { field_at, len, offset } => write!(
                f,
                "a slice at byte {field_at} of {len} values: past the length of an offset of {offset}"
            ),
        }
    }
}

impl core::error::Error for Unfit {}

/// The offset from a field at `field_at` back to a target at `target_at`, both from the
/// buffer's start: `strict` when the target must be strictly before the field (a [`Rel`],
/// whose offset 0 means none).
pub(crate) fn offset_between<O: Offset>(
    field_at: u64,
    target_at: u64,
    strict: bool,
) -> Result<O, Unfit> {
    let forward = match strict {
        true => target_at >= field_at,
        false => target_at > field_at,
    };
    if forward {
        return Err(Unfit::Forward { field_at, target_at });
    }
    let far = Unfit::Far { field_at, target_at, offset: O::NAME };
    // Both are below 2^63 in any buffer; one that is not is far past every width.
    let (field, target) = match (i64::try_from(field_at), i64::try_from(target_at)) {
        (Ok(field), Ok(target)) => (field, target),
        _ => return Err(far),
    };
    O::of_distance(target - field).ok_or(far)
}
