//! A promoted borrow is a constant row, not a frame temporary (walk/promote.rs; blaze-typeck
//! rules/promote.rs lists it).
//!
//! The Reference, "Destructors", "Constant promotion": "Promotion of a value expression to a
//! `'static` slot occurs when the expression could be written in a constant and borrowed, and
//! that borrow could be dereferenced where the expression was originally written, without
//! changing the runtime behavior. That is, the promoted expression can be evaluated at
//! compile-time and the resulting value does not contain interior mutability or destructors".

use super::*;

/// `fn main() -> u32 { *&7 }`: the checker lists `&7` (a literal of `u32`, `Freeze` with no
/// drop glue); the evaluator's record of it is the four bytes `07 00 00 00`. The built function
/// takes the borrow as the address of a data row holding those bytes (`data_addr`), which
/// outlives the frame, and reads 7 through it. Before walk/promote.rs the borrow was the address
/// of a stack temporary the literal was stored into: no data row of those bytes, so the first
/// assertion fails with "a promoted borrow must not point into the frame". With no record for
/// the borrow the build is refused by name, never built as a temporary. (The prompt's name: the
/// same path carries `&RawWakerVTable::new(..)`'s four words; its fn relocations need the
/// front's `promoted_fn_key`, which the test world does not answer.)
#[test]
fn a_promoted_vtable_outlives_its_fn() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    let mut b = BodyBuf::new();
    let seven = b.int(7, None);
    let r = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: seven });
    let root = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: r });
    w.promoted.push((r, Some(record(&[7, 0, 0, 0], 2))));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("bytes \"07000000\"") && text.contains("data_addr"), "a promoted borrow must not point into the frame:\n{text}");
    assert_eq!(one(&built, &[]), 7);

    w.promoted[0].1 = None;
    let refused = build(&w, &b, root).err().unwrap_or_default();
    assert!(refused.contains("has no promoted value"), "{refused}");
}
