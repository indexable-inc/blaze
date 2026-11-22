//! Holes (`_`) in a type written in a body: each written `_` is its own inference variable.
//!
//! The Reference, "Inferred type": "The inferred type asks the compiler to infer the type if
//! possible based on the surrounding information available." Each `_` is one such request:
//! nothing in the rule ties two of them together, so `let x: (_, _) = (1u8, 2u16);` is
//! accepted with `x: (u8, u16)`.
//!
//! The portable table interns its nodes, so the two holes of `(_, _)` are ONE `Infer` node
//! (and the two `(_,)` of `((_,), (_,))` one `Tuple` node): `Machine::lower_at` memoized by
//! node, which made them one variable (`u8` unified with `u16`: E0308 on accepted code). It
//! now lowers a node that holds a hole once per written occurrence (`Machine::lower_holes`).
//!
//! ```text
//! fn h() {
//!     let x: (_, _) = (1u8, 2u16);
//!     let y: ((_,), (_,)) = ((1u8,), (2u16,));
//! }
//! ```

use super::{Want, check_body, differences};
use crate::body::IntSuffix;
use crate::fixture::{Bb, Fx};
use crate::ids::{LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// Two written holes are two variables, directly in one tuple and through a shared node that
/// holds one. With the memo by node (the bug): `diagnostics: want [], got [(E0308, ..)]` and
/// `binding 0: want (u8, u16), got (u8, u8)`.
///
/// Expected line: `test suite::holes::two_written_holes_are_two_variables ... ok`.
#[test]
fn two_written_holes_are_two_variables() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Default::default(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unit = fx.unit();
    let h = fx.func("h", 0, &[], unit, Default::default());
    fx.finish();

    let mut b = Bb::new();
    let hole = b.ty(PNode::Infer);
    let pair = b.t.intern_list(&[hole, hole]).expect("intern");
    let pair = b.ty(PNode::Tuple(pair));
    let one = b.t.intern_list(&[hole]).expect("intern");
    let one = b.ty(PNode::Tuple(one));
    let nested = b.t.intern_list(&[one, one]).expect("intern");
    let nested = b.ty(PNode::Tuple(nested));

    let x = b.bind("x");
    let a = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let c = b.int(Some(IntSuffix::Uint(UintTy::U16)));
    let init = b.tuple(&[a, c]);
    let let_x = b.let_(x, Some(pair), init);

    let y = b.bind("y");
    let a = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let a = b.tuple(&[a]);
    let c = b.int(Some(IntSuffix::Uint(UintTy::U16)));
    let c = b.tuple(&[c]);
    let init = b.tuple(&[a, c]);
    let let_y = b.let_(y, Some(nested), init);

    let root = b.block(&[let_x, let_y], None);
    let checked = check_body(&fx, &b, h, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[], pats: &[(x, "(u8, u16)"), (y, "((u8,), (u16,))")], diags: &[] };
    let mut d = differences(&fx, &b, &checked, &want);
    d.retain(|l| !l.contains("no expectation written"));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}
