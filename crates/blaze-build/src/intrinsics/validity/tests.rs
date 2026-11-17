//! The validity assertions' decision (validity.rs), by the layout. The pinned core,
//! library/core/src/intrinsics/mod.rs:753-782: "This will statically either panic, or do
//! nothing"; `mem::uninitialized`'s doc: "filled with 0x01".

use blaze_typeck::{AdtKind, PNode, UintTy};

use super::{Check, permits};
use crate::layout::tests::{G, World};
use crate::layout::{LayoutCx, Layouts, ReprAttrs};

/// Expected line: "validity: 12 decided". Each case is (check, type, permitted):
/// `u8` passes all three; `!` and an enum with no variants fail all three; `&u8` fails the zero
/// fill (a reference is never null) and the 0x01 fill (not dereferenceable), `&()` passes the
/// 0x01 fill (a zero-sized, byte-aligned pointee); `bool` passes both (only 0 and 1 are
/// valid, and a one-byte 0x01 fill is 1);`(u8, &u8)` fails zero by its field;
/// `Option<&u8>` passes zero (None); `[&u8; 0]` passes zero (no element).
#[test]
fn validity_follows_the_layout() {
    let mut w = World::default();
    let empty = w.def(AdtKind::Enum, 0, &[], ReprAttrs::RUST);
    let option = w.option();
    let mut g = G::new();
    let u8_ = g.n(PNode::Uint(UintTy::U8));
    let never = g.n(PNode::Never);
    let unit = g.tuple(&[]);
    let ref_u8 = g.r(u8_);
    let ref_unit = g.r(unit);
    let bool_ = g.n(PNode::Bool);
    let pair = g.tuple(&[u8_, ref_u8]);
    let l = g.t.intern_list(&[ref_u8]).unwrap();
    let opt_ref = g.n(PNode::Adt(option, l));
    let empty_t = g.n(PNode::Adt(empty, blaze_typeck::List::EMPTY));
    let none_of = g.array(ref_u8, 0);
    let cases = [
        (Check::Inhabited, u8_, true),
        (Check::Inhabited, never, false),
        (Check::Inhabited, empty_t, false),
        (Check::Zero, u8_, true),
        (Check::Zero, ref_u8, false),
        (Check::Ones, ref_u8, false),
        (Check::Ones, ref_unit, true),
        (Check::Zero, bool_, true),
        (Check::Ones, bool_, true),
        (Check::Zero, pair, false),
        (Check::Zero, opt_ref, true),
        (Check::Zero, none_of, true),
    ];
    let mut ls = Layouts::new_in(blaze_own::testing::heap().alloc());
    let mut n = 0;
    for (check, ty, want) in cases {
        let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: blaze_own::testing::heap().alloc() };
        assert_eq!(permits(check, ty, &mut ls, &mut cx).unwrap(), want, "{check:?} of type {}", ty.0);
        n += 1;
    }
    // The planted rule: with no fill check (the zero fill read as always valid) `&u8` would
    // pass; it must not.
    let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: blaze_own::testing::heap().alloc() };
    assert!(!permits(Check::Zero, ref_u8, &mut ls, &mut cx).unwrap());
    println!("validity: {n} decided");
}
