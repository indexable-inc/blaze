//! Where a column of records was written, for the [`Slice`] of a record written after it.
#![blaze::sans_io]

use core::mem::{align_of, size_of};

use crate::{Offset, Record, Slice, Unfit, WriteBuffer, Writer, WriterIn};

/// Where a column was written: nowhere when it is empty.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placed {
    pub at: u64,
    pub len: usize,
}

impl Placed {
    pub const EMPTY: Placed = Placed { at: 0, len: 0 };

    /// Writes `values` as a column; an empty one writes nothing.
    pub fn push<T: Record, P: WriterIn>(out: &mut Writer<P>, values: &[T]) -> Placed {
        match values.is_empty() {
            true => Placed::EMPTY,
            false => Placed { at: out.push_slice(values), len: values.len() },
        }
    }

    /// Pushes the `len` values of `values` straight into `out`, one after another: a record's
    /// size is a multiple of its alignment, so none needs padding after the first.
    pub fn push_each<T: Record, P: WriterIn>(
        out: &mut Writer<P>,
        len: usize,
        values: impl Iterator<Item = T>,
    ) -> Placed {
        if len == 0 {
            return Placed::EMPTY;
        }
        out.bytes.reserve(align_of::<T>() + len * size_of::<T>());
        let at = out.next::<T>();
        let mut pushed = 0;
        for value in values {
            out.push(&value);
            pushed += 1;
        }
        debug_assert_eq!(pushed, len);
        debug_assert_eq!(out.bytes.len() as u64, at + (len * size_of::<T>()) as u64);
        Placed { at, len }
    }

    /// The slice of this column for a field that is written at `field_at`, after it: an `i32`
    /// offset, which reaches across any buffer under 2 GiB. Panics past that, or when the field
    /// is not after the column; a narrow slice is made by [`Placed::slice_as`].
    pub fn slice<T>(self, field_at: u64) -> Slice<T> {
        self.slice_as(field_at).unwrap_or_else(|unfit| panic!("{unfit}"))
    }

    /// [`Placed::slice`] with an offset of `O`, refused when the column is too far back or too
    /// long for it.
    pub fn slice_as<T, O: Offset>(self, field_at: u64) -> Result<Slice<T, O>, Unfit> {
        match self.len {
            0 => Ok(Slice::EMPTY),
            len => Slice::between_as(field_at, self.at, len),
        }
    }
}
