//! A qualified value path whose trait writes no arguments takes the trait's defaults
//! (rules/qpath_defaults.rs): `<i32 as Cmp>::eq` with `trait Cmp<Rhs = Self>` is
//! `Cmp<i32>::eq`, as the pinned library's std/src/path.rs `impl_cmp_os_str!` needs
//! (`<Path as PartialEq>::eq(self.as_ref(), other)` with `other: &&Path`).
//!
//! The Reference, "Generic parameters": a type parameter "may have a default"; a type path
//! leaving it out takes it. "Paths", "Qualified paths": the trait in `<S as T1>` is such a
//! type path.
//!
//! Expected: `a_qualified_path_takes_the_trait_default` passes (no diagnostic, the call is
//! `bool`); without the fix (`Rhs` a fresh variable) `Rhs` becomes `&i32` from `b: &&i32`,
//! `i32: Cmp<&i32>` has no impl, and the test fails on an E0277. The control,
//! `with_no_default_rhs_is_inferred_and_refused`, is that old form: the same body over a
//! trait with no default, where inferring is the rule, gives E0277 (`Unsatisfied`).

use std::vec::Vec;

use super::check_body;
use crate::Checked;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ExprId, IntTy, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// ```text
/// #[lang = "sized"] trait Sized {}
/// trait Cmp<Rhs = Self> { fn eq(&self, other: &Rhs) -> bool; }   // `= Self` when `default_self`
/// impl Cmp for i32 {}                                             // Cmp<i32>
/// fn f(a: &i32, b: &&i32) -> bool { <i32 as Cmp>::eq(a, b) }
/// ```
fn check(default_self: bool) -> (Fx, Checked<'static>, ExprId) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let i32_ = fx.ty(PNode::Int(IntTy::I32));
    let r_i32 = fx.r(Mutbl::Not, i32_);
    let rr_i32 = fx.r(Mutbl::Not, r_i32);
    let bool_ = fx.ty(PNode::Bool);
    let (rp0, rp1) = (fx.r(Mutbl::Not, p0), fx.r(Mutbl::Not, p1));
    let cmp = fx.tr("Cmp", 2, Vec::new(), &[]);
    if default_self {
        fx.trait_defaults(cmp, &[Some(p0)]);
    }
    fx.trait_fn(cmp, "eq", true, &[rp0, rp1], bool_);
    fx.imp("cmp for i32", 0, i32_, Some((cmp, &[i32_])), &[], Vec::new());
    // A second impl, so `i32: Cmp<?Rhs>` has two candidates and selection before the
    // argument's coercion (machine.rs `resolve_with_obligations`, rustc
    // `resolve_vars_with_obligations`) leaves `Rhs` open: with one impl it would rightly fix
    // `Rhs = i32` and the control would no longer tell the default from inference.
    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    fx.imp("cmp u8 for i32", 0, i32_, Some((cmp, &[u8_])), &[], Vec::new());
    let owner = fx.func("f", 0, &[r_i32, rr_i32], bool_, Vec::new());
    fx.finish();
    let mut bb = Bb::new();
    let (pa, pb) = (bb.param("a"), bb.param("b"));
    let self_ty = bb.ty(PNode::Int(IntTy::I32));
    let callee = bb.qpath_as(self_ty, "Cmp", &["eq"]);
    let (a, b) = (bb.local(pa), bb.local(pb));
    let call = bb.call(callee, &[a, b]);
    let root = bb.block(&[], Some(call));
    let c = check_body(&fx, &bb, owner, root, Plant::None, InferPlant::None);
    (fx, c, call)
}

/// `<i32 as Cmp>::eq(a, b)` with `Rhs = Self`: `Cmp<i32>`, `b: &&i32` coerced to `&i32`.
#[test]
fn a_qualified_path_takes_the_trait_default() {
    let (fx, c, call) = check(true);
    assert!(c.diags.is_empty(), "diagnostics: {:?}", c.diags);
    assert_eq!(show(&fx, c.types.view(), c.expr_ty[call.ix()]), "bool");
}

/// Control: `Rhs` with no default is inferred (`&i32`, from `b`), and `i32: Cmp<&i32>` is
/// refused: the answer the fixed form must not give when a default is declared.
#[test]
fn with_no_default_rhs_is_inferred_and_refused() {
    let (_, c, _) = check(false);
    assert!(c.diags.iter().any(|d| d.code == DiagCode::Unsatisfied), "diagnostics: {:?}", c.diags);
}
