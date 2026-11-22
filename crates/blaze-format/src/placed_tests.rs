//! Tests of blaze-format-core's `Placed`, here because they use blaze-sim, which is on std.

use blaze_sim::Config;

use blaze_own::Alloc;

use crate::{Own, Placed, Record, SchemaHash, Slice, Writer, WriterOpts};

/// A writer on the test's heap (D170 C1: a test names its allocator too).
type Out = Writer<Own<Alloc<'static>>>;

/// The copy `hir-ty`'s `consteval.rs`, `infer/persist.rs`, `lower/persist.rs`,
/// `body_store/infer.rs`, `body_store/mir.rs` and `check_cache/part.rs` each had.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Reference {
    at: u64,
    len: usize,
}

impl Reference {
    fn push<T: Record>(out: &mut Out, values: &[T]) -> Reference {
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

fn writer() -> Out {
    Writer::new(WriterOpts {
        schema: SchemaHash(blaze_ids::ContentId([0; 32])),
        alloc: Own(blaze_own::testing::heap().alloc()),
        capacity: None,
    })
}

#[test]
fn placed_is_the_copies_it_replaced() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let before: Vec<u8> = tape.bytes(0..=8, "before");
        let values: Vec<u32> = tape.vec(0..=63, "values", |tape| tape.int("value"));
        let gap: u64 = tape.int_in(0..=63, "gap");
        let (mut ours, mut theirs) = (writer(), writer());
        ours.push_slice(&before);
        theirs.push_slice(&before);
        let placed = Placed::push(&mut ours, &values);
        let reference = Reference::push(&mut theirs, &values);
        assert_eq!(&ours.bytes, &theirs.bytes);
        assert_eq!((placed.at, placed.len), (reference.at, reference.len));
        let field_at = ours.next::<Slice<u32>>() + gap;
        assert_eq!(placed.slice::<u32>(field_at), reference.slice::<u32>(field_at));
    });
}

#[test]
fn push_each_writes_what_push_writes() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let before: Vec<u8> = tape.bytes(0..=8, "before");
        let values: Vec<u64> = tape.vec(0..=63, "values", |tape| tape.int("value"));
        let (mut each, mut whole) = (writer(), writer());
        each.push_slice(&before);
        whole.push_slice(&before);
        let a = Placed::push_each(&mut each, values.len(), values.iter().copied());
        let b = Placed::push(&mut whole, &values);
        assert_eq!(a, b);
        assert_eq!(&each.bytes, &whole.bytes);
    });
}

#[test]
fn an_empty_column_writes_nothing() {
    let mut out = writer();
    let len = out.bytes.len();
    assert_eq!(Placed::push::<u32, _>(&mut out, &[]), Placed::EMPTY);
    assert_eq!(Placed::push_each::<u32, _>(&mut out, 0, std::iter::empty()), Placed::EMPTY);
    assert_eq!(out.bytes.len(), len);
    assert_eq!(Placed::EMPTY.slice::<u32>(1024), Slice::EMPTY);
}
