//! Where a column of records was written, for the [`Slice`] of a record written after it.

use core::mem::{align_of, size_of};

use crate::{Record, Slice, WriteBuffer, Writer, WriterIn};

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

    /// The slice of this column for a field that is written at `field_at`.
    pub fn slice<T>(self, field_at: u64) -> Slice<T> {
        match self.len {
            0 => Slice::EMPTY,
            len => Slice::between(field_at, self.at, len),
        }
    }
}
