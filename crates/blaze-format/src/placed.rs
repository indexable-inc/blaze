//! Where a column of records was written, for the [`Slice`] of a record written after it.

use crate::{Record, Slice, Writer};

/// Where a column was written: nowhere when it is empty.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placed {
    pub at: u64,
    pub len: usize,
}

impl Placed {
    pub const EMPTY: Placed = Placed { at: 0, len: 0 };

    /// Writes `values` as a column; an empty one writes nothing.
    pub fn push<T: Record>(out: &mut Writer, values: &[T]) -> Placed {
        match values.is_empty() {
            true => Placed::EMPTY,
            false => Placed { at: out.push_slice(values), len: values.len() },
        }
    }

    /// Pushes the `len` values of `values` straight into `out`, one after another: a record's
    /// size is a multiple of its alignment, so none needs padding after the first.
    pub fn push_each<T: Record>(
        out: &mut Writer,
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

#[cfg(test)]
mod tests {
    use hegel::TestCase;
    use hegel::generators as gs;

    use super::Placed;
    use crate::{SchemaHash, Slice, Writer};

    /// The copy `hir-ty`'s `consteval.rs`, `infer/persist.rs`, `lower/persist.rs`,
    /// `body_store/infer.rs`, `body_store/mir.rs` and `check_cache/part.rs` each had.
    #[derive(Clone, Copy, PartialEq, Debug)]
    struct Reference {
        at: u64,
        len: usize,
    }

    impl Reference {
        fn push<T: crate::Record>(out: &mut Writer, values: &[T]) -> Reference {
            match values.is_empty() {
                true => Reference { at: 0, len: 0 },
                false => Reference { at: out.push_slice(values), len: values.len() },
            }
        }

        fn slice<T>(self, field_at: u64) -> Slice<T> {
            match self.len {
                0 => Slice::EMPTY,
                len => Slice::between(field_at, self.at, len),
            }
        }
    }

    fn writer() -> Writer {
        Writer::new(SchemaHash(blaze_ids::ContentId([0; 32])))
    }

    #[hegel::test]
    fn placed_is_the_copies_it_replaced(tc: TestCase) {
        let before: Vec<u8> = tc.draw(gs::binary().max_size(8));
        let values: Vec<u32> = tc.draw(gs::vecs(gs::integers()).max_size(63));
        let gap: u64 = tc.draw(gs::integers().max_value(63));
        let (mut ours, mut theirs) = (writer(), writer());
        ours.push_slice(&before);
        theirs.push_slice(&before);
        let placed = Placed::push(&mut ours, &values);
        let reference = Reference::push(&mut theirs, &values);
        assert_eq!(&ours.bytes, &theirs.bytes);
        assert_eq!((placed.at, placed.len), (reference.at, reference.len));
        let field_at = ours.next::<Slice<u32>>() + gap;
        assert_eq!(placed.slice::<u32>(field_at), reference.slice::<u32>(field_at));
    }

    #[hegel::test]
    fn push_each_writes_what_push_writes(tc: TestCase) {
        let before: Vec<u8> = tc.draw(gs::binary().max_size(8));
        let values: Vec<u64> = tc.draw(gs::vecs(gs::integers()).max_size(63));
        let (mut each, mut whole) = (writer(), writer());
        each.push_slice(&before);
        whole.push_slice(&before);
        let a = Placed::push_each(&mut each, values.len(), values.iter().copied());
        let b = Placed::push(&mut whole, &values);
        assert_eq!(a, b);
        assert_eq!(&each.bytes, &whole.bytes);
    }

    #[test]
    fn an_empty_column_writes_nothing() {
        let mut out = writer();
        let len = out.bytes.len();
        assert_eq!(Placed::push::<u32>(&mut out, &[]), Placed::EMPTY);
        assert_eq!(Placed::push_each::<u32>(&mut out, 0, std::iter::empty()), Placed::EMPTY);
        assert_eq!(out.bytes.len(), len);
        assert_eq!(Placed::EMPTY.slice::<u32>(1024), Slice::EMPTY);
    }
}
