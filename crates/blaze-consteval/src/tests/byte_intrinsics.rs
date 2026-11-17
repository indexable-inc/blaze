//! `blaze_int_cast`, `raw_eq` and `compare_bytes` in constants, each the one rule blaze-build's
//! builder is tested against (intrinsics.rs `int_cast_bits` and `byte_order`).
//!
//! blaze's library/core/src/intrinsics/mod.rs on `blaze_int_cast`: "`x as D` between two integer
//! types of any width ... it truncates to `D`'s width, or extends by `S`'s sign". The pinned
//! core on `raw_eq`: "Determines whether the raw bytes of the two values are equal"; on
//! `compare_bytes`: "returning negative if `left` is less, zero if all the bytes match, or
//! positive if `left` is greater".
//!
//! Control: with the arms taken out of machine.rs `intrinsic` (each falls to `other =>
//! unsupported(..)`), every constant here is refused as `Unsupported { what: "<name>" }`.

use blaze_typeck::{Mutbl, PNode, UintTy};

use super::{eval_int, refusal, world};
use crate::Intrinsic;

/// Truncation, sign extension by `S` and zero extension, as `as` does them.
/// Prints `test tests::byte_intrinsics::blaze_int_cast_is_as ... ok`.
#[test]
fn blaze_int_cast_is_as() {
    let mut w = world();
    let (p0, p1) = (w.fx.param(0), w.fx.param(1));
    w.intrinsic("blaze_int_cast", 2, &[p0], p1, Intrinsic::BlazeIntCast);
    w.konst("TRUNC", "u8", "blaze_int_cast::<u32, u8>(0x1234)");
    w.konst("SEXT", "u32", "blaze_int_cast::<i8, u32>(-2)");
    w.konst("ZEXT", "i64", "blaze_int_cast::<u8, i64>(200)");
    w.build();
    assert_eq!(eval_int(&mut w, "TRUNC"), Ok(0x34));
    assert_eq!(eval_int(&mut w, "SEXT"), Ok(0xffff_fffe), "extended by `S`'s sign");
    assert_eq!(eval_int(&mut w, "ZEXT"), Ok(200));
}

/// Equal and unequal arrays.
/// Prints `test tests::byte_intrinsics::raw_eq_compares_the_bytes ... ok`.
#[test]
fn raw_eq_compares_the_bytes() {
    let mut w = world();
    let p0 = w.fx.param(0);
    let r = w.fx.ty(PNode::Ref(Mutbl::Not, p0));
    let b = w.fx.ty(PNode::Bool);
    w.intrinsic("raw_eq", 1, &[r, r], b, Intrinsic::RawEq);
    w.konst("SAME", "bool", "raw_eq::<[u8; 3]>(&[1, 2, 3], &[1, 2, 3])");
    w.konst("DIFFER", "bool", "raw_eq::<[u16; 2]>(&[1, 2], &[1, 3])");
    w.build();
    assert_eq!(eval_int(&mut w, "SAME"), Ok(1));
    assert_eq!(eval_int(&mut w, "DIFFER"), Ok(0));
}

/// The sign of the first differing byte, unsigned, and 0 for equal ranges.
/// Prints `test tests::byte_intrinsics::compare_bytes_orders_unsigned_bytes ... ok`.
#[test]
fn compare_bytes_orders_unsigned_bytes() {
    let mut w = world();
    // The fixture's declaration takes `&[u8; 3]` where core's takes `*const u8`: the evaluator
    // reads both the same (the pointer's place), and the fixture's parser has no pointer casts.
    let r3 = w.ty("&[u8; 3]");
    let usize_ = w.fx.ty(PNode::Uint(UintTy::Usize));
    let i32_ = w.ty("i32");
    w.intrinsic("compare_bytes", 0, &[r3, r3, usize_], i32_, Intrinsic::CompareBytes);
    w.const_fn("cmp", &[("a", "&[u8; 3]"), ("b", "&[u8; 3]"), ("n", "usize")], "i32", "{ compare_bytes(a, b, n) }");
    w.konst("LESS", "i32", "cmp(&[1, 2, 3], &[1, 0xff, 0], 3)");
    w.konst("GREATER", "i32", "cmp(&[0x80, 0, 0], &[0x7f, 9, 9], 3)");
    w.konst("EQUAL", "i32", "cmp(&[5, 6, 7], &[5, 6, 8], 2)");
    w.konst("PAST", "i32", "cmp(&[5, 6, 7], &[5, 6, 7], 4)");
    w.build();
    assert_eq!(eval_int(&mut w, "LESS"), Ok(0xffff_ffff), "negative: 2 < 0xff");
    assert_eq!(eval_int(&mut w, "GREATER"), Ok(1), "unsigned: 0x80 > 0x7f");
    assert_eq!(eval_int(&mut w, "EQUAL"), Ok(0), "the first two bytes only");
    assert!(refusal(&mut w, "PAST").contains("OutOfBounds"), "a range past the arrays is refused: {}", refusal(&mut w, "PAST"));
}
