//! A builtin operator through one shared reference (rules/op.rs `peel_shared_ref`).
//!
//! The site is turso core/json/jsonb.rs:1041: `Some(header_byte) => { let header_size =
//! header_byte >> 4; match header_size { size if size <= 11 => .., 12 => .., .. } }`, with
//! `header_byte: &u8` from `slice.get(cursor)`. turso c1851p gave "`<&u8 as Shr<i32>>::Output
//! == i32` (E0271)".
//!
//! Cause, from reading: the operator was not builtin on `&u8`, so its type was the trait's
//! output `?O` with `&u8: Shr<{integer}>` open. The match's literal patterns made `?O` an
//! integer variable. Fallback then made both `i32`, but `<&u8 as Shr<i32>>::Output` is `u8`.
//!
//! The rule: rustc_hir_typeck op.rs `is_builtin_binop` and `enforce_builtin_binop_types` first
//! do `let (lhs, rhs) = (deref_ty_if_possible(lhs), deref_ty_if_possible(rhs));` ("Special-case
//! a single layer of referencing, so that things like `5.0 + &6.0f32` work"). For a shift,
//! "result type is same as LHS always". So the expression is `u8`, and `demand_eqtype` makes
//! the trait's output `u8` too.
//!
//! ```text
//! #[lang = "sized"] trait Sized {}
//! #[lang = "shr"] trait Shr<Rhs> { type Output; fn shr(self, rhs: Rhs) -> Self::Output; }
//! impl Shr<i32> for &u8 { type Output = u8; }  impl Shr<i64> for &u8 { type Output = u8; }
//! fn same<T>(a: T, b: T);
//! fn f(h: &u8) { same(h >> 4, 12) }      // clean: `h >> 4` is `u8`, so `12` is `u8`
//! fn t(h: &u8) { same(h >> 4, 1u16) }    // the twin: rustc E0308 (`u8` against `u16`)
//! fn g(h: &u8) -> u8 { h >> 4 }          // the control: clean, the row `shr<&u8, i32>`
//! ```
//!
//! CONTROL (rule 23): with `peel_shared_ref` returning `t` unchanged, `f` is E0271 again and
//! the first test fails. `g` is clean either way.

use std::vec::Vec;

use super::{callee, check_body};
use crate::body::{BinOp, Expr, IntSuffix};
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, IntTy, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

fn world() -> Fx {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    let r_u8 = fx.r(Mutbl::Not, u8_);
    let (i32_, i64_) = (fx.ty(PNode::Int(IntTy::I32)), fx.ty(PNode::Int(IntTy::I64)));
    let unit = fx.unit();
    let shr = fx.tr("Shr", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::Shr, shr);
    let out = fx.alias(shr, "Output", &[p0, p1]);
    fx.trait_fn(shr, "shr", true, &[p0, p1], out);
    fx.imp("shr i32 for &u8", 0, r_u8, Some((shr, &[i32_])), &[("Output", u8_)], Vec::new());
    fx.imp("shr i64 for &u8", 0, r_u8, Some((shr, &[i64_])), &[("Output", u8_)], Vec::new());
    fx.func("same", 1, &[p0, p0], unit, Vec::new());
    fx.func("f", 0, &[r_u8], unit, Vec::new());
    fx.func("t", 0, &[r_u8], unit, Vec::new());
    fx.func("g", 0, &[r_u8], u8_, Vec::new());
    fx.finish();
    fx
}

/// `h >> 4` in a fresh body with the parameter `h`.
fn shift(b: &mut Bb) -> ExprId {
    let h = b.param("h");
    let lhs = b.local(h);
    let rhs = b.int(None);
    b.e(Expr::Binary { op: BinOp::Shr, lhs, rhs })
}

/// The codes of `fn <owner>(h: &u8) { same(h >> 4, <lit>) }`.
fn same_codes(owner: &str, lit: Option<IntSuffix>) -> Vec<&'static str> {
    let fx = world();
    let mut b = Bb::new();
    let s = shift(&mut b);
    let callee = b.path(&["same"]);
    let second = b.int(lit);
    let call = b.call(callee, &[s, second]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, fx.key_of(owner), root, Plant::None, InferPlant::None);
    c.diags.iter().map(|d| d.code.code()).collect()
}

/// turso's shape. Prints nothing; fails with "f: [\"E0271\"]" before the fix.
#[test]
fn a_shift_of_a_reference_to_an_integer_is_the_integer_type() {
    let got = same_codes("f", None);
    assert!(got.is_empty(), "f: {got:?}");
}

/// The twin rustc refuses: `h >> 4` is `u8`, so `1u16` is a mismatch (E0308).
#[test]
fn a_shift_of_a_reference_is_not_another_integer_type() {
    let got = same_codes("t", Some(IntSuffix::Uint(UintTy::U16)));
    assert!(got.contains(&"E0308"), "t: {got:?}");
}

/// The control: `h >> 4` returned as `u8` is clean. The operator stays the trait's method on
/// the reference: the build calls it (core's `forward_ref_binop`), never a builtin shift of a
/// pointer.
#[test]
fn control_a_shift_of_a_reference_keeps_its_callee_row() {
    let fx = world();
    let mut b = Bb::new();
    let s = shift(&mut b);
    let root = b.block(&[], Some(s));
    let c = check_body(&fx, &b, fx.key_of("g"), root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "g: {:?}", c.diags);
    assert_eq!(callee(&fx, &c, s), "shr<&u8, i32>");
}
