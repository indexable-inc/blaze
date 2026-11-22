//! Const generic arguments that are no literal, in a body, as the front end hands them over
//! (blaze-lower `to_typeck`: `Types::name_arg`, `Types::const_term`, `Types::repeat_term`):
//!
//! ```text
//! struct W<const N: usize>;
//! const SIZE: usize = 4;
//! fn w<const N: usize>() -> W<N>;
//! fn f<const N: usize>() -> [u8; N] { [0u8; N] }        // the item's own const parameter
//! fn g() -> [u8; 4] { [0u8; SIZE] }                     // a constant item's value
//! fn h() -> W<4> { w::<SIZE>() }                        // the same, as a generic argument
//! fn k() -> W<4> { w::<{ SIZE + 0 }>() }                // a hole: the front end's gap
//! ```
//!
//! The rules, from the Rust Reference:
//! - "Generic parameters", "Const generics": "Const parameters can be used anywhere a const
//!   item can be used", and a const argument is a value of the parameter's type; two generic
//!   types are the same only when their const arguments are equal.
//! - the same section: "When there is ambiguity if a generic argument could be resolved as
//!   either a type or const argument, it is always resolved as a type." So `w::<SIZE>` with
//!   no type named `SIZE` is the constant: the FRONT END decides that (it resolves names) and
//!   hands the checker a `ConstValue` (the constant's evaluated value) or the owner's
//!   `ConstParam`; the checker never asks the world a constant's value for a type.
//! - "Array expressions": in `[expr; len]` the length is "a constant expression of type
//!   `usize`", and the array's type is `[T; len]`; "Array types": two array types are equal
//!   when their lengths are.
//!
//! A const argument the front end cannot value yet (an expression over a generic parameter,
//! an anonymous constant whose type it does not know) arrives as a hole (`PNode::Infer`) and
//! is listed by the front end as a gap (blaze-lower `TypeckBody::const_gaps`): the checker
//! makes it a const variable and reports NO diagnostic for it.
//!
//! No test here runs a compiler for its answer: each expectation is the rule above.

use std::vec::Vec;

use super::check_body;
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::Def;

/// The world above: `[f, g, h]` (`k` is checked as `h`: the same signature).
fn world() -> (Fx, [ItemKey; 3]) {
    let mut fx = Fx::new();
    let (u8_, usize_) = (fx.u8(), fx.usize());
    let n0 = fx.ty(PNode::ConstParam(0));
    let w = fx.strukt("W", 1, &[]);
    fx.const_param(w, 0, usize_);
    fx.konst("SIZE", usize_);
    let w_n = fx.adt(w, &[n0]);
    let make = fx.func("w", 1, &[], w_n, Vec::new());
    fx.const_param(make, 0, usize_);
    let arr_n = fx.ty(PNode::Array(u8_, n0));
    let f = fx.func("f", 1, &[], arr_n, Vec::new());
    fx.const_param(f, 0, usize_);
    // `N` as a value names `f`'s own const parameter (a generic parameter of the owner).
    fx.use_path("N", Def::TyParam(0));
    let arr4 = fx.array(u8_, 4);
    let g = fx.func("g", 0, &[], arr4, Vec::new());
    let four = fx.ty(PNode::ConstValue { ty: usize_, bits: 4 });
    let w4 = fx.adt(w, &[four]);
    let h = fx.func("h", 0, &[], w4, Vec::new());
    fx.finish();
    (fx, [f, g, h])
}

fn codes(c: &crate::columns::Checked<'_>) -> Vec<DiagCode> {
    c.diags.iter().map(|d| d.code).collect()
}

/// `fn f<const N: usize>() -> [u8; N] { [0u8; N] }`: the repeat's length is the owner's
/// `ConstParam(0)`, so its type is `[u8; N]`, the declared output: no diagnostic, and the
/// repeat's type in the record is an array whose length is `ConstParam(0)`. A length that
/// lost the parameter (a value, a hole left unresolved) fails the last assertion.
///
/// Prints: `test suite::const_args::a_repeat_of_the_owners_const_parameter ... ok`
#[test]
fn a_repeat_of_the_owners_const_parameter() {
    let (fx, [f, _, _]) = world();
    let mut b = Bb::new();
    let zero = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let n = b.path(&["N"]);
    let len = b.ty(PNode::ConstParam(0));
    let rep = b.repeat_counted(zero, len, n);
    let root = b.block(&[], Some(rep));
    let c = check_body(&fx, &b, f, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let types = c.types.view();
    let PNode::Array(elem, got) = types.node(c.expr_ty[rep.0 as usize]).expect("the repeat's type") else {
        panic!("the repeat is no array: {:?}", types.node(c.expr_ty[rep.0 as usize]));
    };
    assert_eq!(types.node(elem).expect("the element"), PNode::Uint(UintTy::U8));
    assert_eq!(types.node(got).expect("the length"), PNode::ConstParam(0));
}

/// `[0u8; SIZE]` with the front end's value of `SIZE` as the length's term.
fn repeat_of_size(value: u64) -> Vec<DiagCode> {
    let (fx, [_, g, _]) = world();
    let mut b = Bb::new();
    let zero = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let size = b.path(&["SIZE"]);
    let len = b.t.usize_value(value).expect("intern");
    let rep = b.repeat_counted(zero, len, size);
    let root = b.block(&[], Some(rep));
    codes(&check_body(&fx, &b, g, root, Plant::None, InferPlant::None))
}

/// `const SIZE: usize = 4; fn g() -> [u8; 4] { [0u8; SIZE] }` checks; with the constant's
/// value 3 (`[0u8; 3]` where `[u8; 4]` is declared) it is one E0308 ("Array types": the
/// lengths must be equal).
///
/// Prints: `test suite::const_args::a_repeat_of_a_constant_items_value ... ok`
#[test]
fn a_repeat_of_a_constant_items_value() {
    assert_eq!(repeat_of_size(4), []);
    assert_eq!(repeat_of_size(3), [DiagCode::Mismatch]);
}

/// `w::<ARG>()` as `h`'s body, `ARG` the term the front end made.
fn call_with(arg: PNode) -> Vec<DiagCode> {
    let (fx, [_, _, h]) = world();
    let mut b = Bb::new();
    let arg = match arg {
        PNode::ConstValue { bits, .. } => {
            let ty = b.ty(PNode::Uint(UintTy::Usize));
            b.ty(PNode::ConstValue { ty, bits })
        }
        n => b.ty(n),
    };
    let callee = b.path_args("w", &[arg]);
    let call = b.call(callee, &[]);
    let root = b.block(&[], Some(call));
    codes(&check_body(&fx, &b, h, root, Plant::None, InferPlant::None))
}

/// `fn h() -> W<4> { w::<SIZE>() }`: `SIZE` names no type, so it is the constant, a `usize`
/// of value 4: `W<4>`. With the value 5 it is `W<5>` where `W<4>` is declared: one E0308
/// ("Const generics": the arguments must be equal).
///
/// Prints: `test suite::const_args::a_constant_item_as_a_generic_argument ... ok`
#[test]
fn a_constant_item_as_a_generic_argument() {
    let usize_ = crate::portable::PTy(0);
    assert_eq!(call_with(PNode::ConstValue { ty: usize_, bits: 4 }), []);
    assert_eq!(call_with(PNode::ConstValue { ty: usize_, bits: 5 }), [DiagCode::Mismatch]);
}

/// `fn k() -> W<4> { w::<{ SIZE + 0 }>() }` as the front end hands it over when it cannot
/// value the argument: a hole. It is a const variable that the declared `W<4>` settles: no
/// diagnostic (the gap is the front end's to report). Without the hole's rule in
/// `generic_arg` (rules/lit_path.rs) the hole is a TYPE variable given for a const parameter:
/// E0747 (`GenericArgKind`), a diagnostic for a correct program.
///
/// Prints: `test suite::const_args::a_const_argument_left_a_hole_is_no_diagnostic ... ok`
#[test]
fn a_const_argument_left_a_hole_is_no_diagnostic() {
    assert_eq!(call_with(PNode::Infer), []);
}
