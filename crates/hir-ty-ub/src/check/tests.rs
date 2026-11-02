use super::{
    OffsetFromError::{self, *},
    offset, valid,
};
use crate::{
    ids::{BorTag, Prov, Span},
    memory::{Kind, Memory},
};

// rustc_const_eval validity.rs, `fmt_range`: every arm.
#[test]
fn fmt_range_words() {
    assert_eq!(valid::fmt_range(0, 1, u8::MAX.into()), "less or equal to 1");
    assert_eq!(valid::fmt_range(1, u8::MAX.into(), u8::MAX.into()), "greater or equal to 1");
    assert_eq!(valid::fmt_range(2, 2, u8::MAX.into()), "equal to 2");
    assert_eq!(valid::fmt_range(1, 10, u8::MAX.into()), "in the range 1..=10");
    assert_eq!(valid::fmt_range(10, 1, u8::MAX.into()), "less or equal to 1, or greater or equal to 10");
}

#[test]
fn hex_is_two_digits_a_byte() {
    assert_eq!(valid::hex(2, 1), "0x02");
    assert_eq!(valid::hex(0xd800, 4), "0x0000d800");
    assert_eq!(valid::hex(0, 8), "0x0000000000000000");
}

#[test]
fn merge_joins_touching_and_overlapping() {
    let mut cells = vec![Span::new(8, 4), Span::new(0, 4), Span::new(4, 2), Span::new(10, 6), Span::new(20, 0)];
    valid::merge(&mut cells);
    assert_eq!(cells, vec![Span::new(0, 6), Span::new(8, 8)]);
}

#[test]
fn value_error_display() {
    let whole = valid::ValueError { ty: "bool".into(), path: String::new(), msg: "encountered 0x02, but expected a boolean".into() };
    assert_eq!(whole.to_string(), "constructing invalid value of type bool: encountered 0x02, but expected a boolean");
    let field = valid::ValueError {
        ty: "Option<bool>".into(),
        path: ".<enum-variant(Some)>.0".into(),
        msg: "encountered 0x02, but expected a boolean".into(),
    };
    assert_eq!(
        field.to_string(),
        "constructing invalid value of type Option<bool>: at .<enum-variant(Some)>.0, encountered 0x02, but expected a boolean"
    );
}

/// Two live 16-byte allocations, at `0x1000` and `0x2000`, and a pointer into each.
fn two_allocations() -> (Memory, Option<Prov>, Option<Prov>) {
    let mut mem = Memory::with_pointer_size(8);
    let x = mem.alloc(0x1000, 16, 8, Kind::Rust, false).expect("x");
    let y = mem.alloc(0x2000, 16, 8, Kind::Rust, false).expect("y");
    let tag = BorTag::new(1);
    (mem, Some(Prov::Concrete { alloc: x, tag }), Some(Prov::Concrete { alloc: y, tag }))
}

// rustc interpret/intrinsics.rs, `ptr_offset_from`: each way it fails, and the cases it allows.
#[test]
fn offset_from_follows_rustc() {
    let (mem, x, y) = two_allocations();
    let from = |a: (u64, Option<Prov>), b, unsigned| offset::check(&mem, a, b, unsigned);
    assert_eq!(from((0x1010, x), (0x1000, x), false), Ok(()));
    assert_eq!(from((0x1000, x), (0x1010, x), false), Ok(()));
    assert_eq!(from((0x1010, x), (0x1000, x), true), Ok(()));
    assert_eq!(from((0x1008, x), (0x1008, x), true), Ok(()));
    assert_eq!(from((0x1000, x), (0x1010, x), true), Err(Negative { a: 0x1000, b: 0x1010 }));
    assert_eq!(from((0x1020, x), (0x1000, x), false), Err(OutOfBounds { unsigned: false }));
    assert_eq!(from((0x2000, y), (0x1000, x), false), Err(DifferentAllocations { unsigned: false }));
    assert_eq!(from((0x2000, y), (0x1010, x), true), Err(DifferentAllocations { unsigned: true }));
    assert_eq!(from((u64::MAX, None), (0, None), false), Err(TooFarAhead { unsigned: false }));
    assert_eq!(from((0, None), (u64::MAX, None), false), Err(TooFarBefore));
    // Zero bytes apart: no allocation needed.
    assert_eq!(from((0x10, None), (0x10, None), false), Ok(()));
}

#[test]
fn offset_from_words() {
    let words = |e: OffsetFromError| e.to_string();
    assert_eq!(
        words(DifferentAllocations { unsigned: false }),
        "`ptr_offset_from` called on two different pointers that are not both derived from the same allocation"
    );
    assert_eq!(
        words(OutOfBounds { unsigned: true }),
        "`ptr_offset_from_unsigned` called on two different pointers where the memory range between them is not in-bounds of an allocation"
    );
    assert_eq!(words(TooFarAhead { unsigned: false }), "`ptr_offset_from` called when first pointer is too far ahead of second");
    assert_eq!(words(TooFarBefore), "`ptr_offset_from` called when first pointer is too far before second");
    assert_eq!(
        words(Negative { a: 4096, b: 4112 }),
        "`ptr_offset_from_unsigned` called when first pointer has smaller address than second: 4096 < 4112"
    );
}
