//! `_` in a const-argument position is an INFERRED CONST (the Reference, "Generic arguments":
//! a const argument may be an inferred const `_`; "Array expressions": "the length operand must
//! be either an inferred const or a constant expression of type `usize`"). The front end hands
//! it over as a `PNode::Infer` hole (a repeat's length with no count, a generic argument), and
//! the checker makes it a const variable, fixed by unification. Left open, it is E0282; it is
//! never checked as the expression `_` (E0000 `UnderscoreExpr`). The lowering half, and the
//! case that made this (panic_unwind/src/gcc.rs:67, `[core::ptr::null(); _]`), are in
//! blaze-lower's typeck_tests/infer_const.rs.
//!
//! No test here runs a compiler for its answer: each expectation is the rule above.

use std::vec::Vec;

use super::check_body;
use crate::body::{Expr, IntSuffix};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ItemKey, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// `fn take<const N: usize>(a: [u8; N])` and the owner `fn g()`.
fn world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let (unit, u8_, usize_) = (fx.unit(), fx.u8(), fx.usize());
    let n0 = fx.ty(PNode::ConstParam(0));
    let arr_n = fx.ty(PNode::Array(u8_, n0));
    let take = fx.func("take", 1, &[arr_n], unit, Vec::new());
    fx.const_param(take, 0, usize_);
    let g = fx.func("g", 0, &[], unit, Vec::new());
    fx.finish();
    (fx, g)
}

/// `take::<_>([0u8; 2]);`: the argument `_` for the const parameter `N` is a const variable,
/// fixed to `2` by the argument's type: clean, the callee `fn take<2>`. A hole lowered as a type
/// variable fails with E0747 (`GenericArgKind`); one left open, with E0282.
///
/// Prints: `test suite::infer_const_arg::a_const_generic_argument_underscore_takes_the_arguments_length ... ok`
#[test]
fn a_const_generic_argument_underscore_takes_the_arguments_length() {
    let (fx, g) = world();
    let mut b = Bb::new();
    let hole = b.ty(PNode::Infer);
    let callee = b.path_args("take", &[hole]);
    let zero = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let two = b.t.usize_value(2).expect("intern");
    let rep = b.e(Expr::Repeat { elem: zero, len: two, count: None });
    let call = b.call(callee, &[rep]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, g, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.is_empty(), "unexpected diagnostics {codes:?}");
    let types = c.types.view();
    assert_eq!(show(&fx, types, c.expr_ty[callee.0 as usize]), "fn take<2>");
    assert_eq!(show(&fx, types, c.expr_ty[rep.0 as usize]), "[u8; 2]");
}

/// `let x = [0u8; _];`: a repeat whose length is an inferred const (a hole, no count) that
/// nothing fixes. E0282 and only E0282: never E0000 for `_` as an expression, and never a
/// guessed length.
///
/// Prints: `test suite::infer_const_arg::an_inferred_repeat_length_nothing_fixes_is_e0282 ... ok`
#[test]
fn an_inferred_repeat_length_nothing_fixes_is_e0282() {
    let (fx, g) = world();
    let mut b = Bb::new();
    let x = b.bind("x");
    let zero = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let hole = b.ty(PNode::Infer);
    let rep = b.e(Expr::Repeat { elem: zero, len: hole, count: None });
    let s = b.let_(x, None, rep);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, g, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(!codes.is_empty() && codes.iter().all(|&code| code == DiagCode::AnnotationsNeeded), "want only E0282, got {codes:?}");
}
