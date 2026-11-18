//! Unary `-` by the Reference (rules/op.rs `expr_unary`, `is_neg_builtin`): builtin on signed
//! integers and floats, the `Neg` trait's method on anything else, and so REFUSED on an
//! unsigned integer, for which core has no `Neg` impl.
//!
//! The Reference, "Negation operators", [expr.negate.results]: "This table summarizes the
//! behavior of them on primitive types and which traits are used to overload these operators
//! for other types." `-`: Integer "Negation*", Floating Point "Negation", Overloading Trait
//! `std::ops::Neg`; "* Only for signed integer types." The pinned core (library/core/src/ops/
//! arith.rs) implements `Neg` by `neg_impl! { isize i8 i16 i32 i64 i128 f16 f32 f64 f128 }`:
//! no unsigned type.
//!
//! The world:
//! ```text
//! #[lang = "sized"] trait Sized {}
//! #[lang = "neg"] trait Neg { type Output; fn neg(self) -> Self::Output; }
//! impl Neg for i32 { type Output = i32; }
//! ```

use std::vec::Vec;

use super::check_body;
use crate::body::{Expr, UnOp};
use crate::diag::{Because, DiagCode};
use crate::fixture::{Bb, Fx};
use crate::ids::{FloatTy, IntTy, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::{PNode, PTy};

/// `fn f(a: <param>) -> <out> { -a }`, checked in the module doc's world (`bounded`: `f` is
/// generic over `T: Neg<Output = T>` and `param` is `T`).
fn negate(param: impl Fn(&mut Fx) -> PTy, bounded: bool) -> crate::Checked<'static> {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let p0 = fx.param(0);
    let i32_ = fx.ty(PNode::Int(IntTy::I32));
    let neg = fx.tr("Neg", 1, Vec::new(), &["Output"]);
    fx.lang(LangItem::Neg, neg);
    let out = fx.alias(neg, "Output", &[p0]);
    fx.trait_fn(neg, "neg", true, &[p0], out);
    fx.imp("neg for i32", 0, i32_, Some((neg, &[])), &[("Output", i32_)], Vec::new());
    let t = param(&mut fx);
    let (generics, clauses) = if bounded {
        let bound = fx.bound(neg, &[p0]);
        let output = fx.proj(neg, &[p0], "Output", p0);
        (1, std::vec![bound, output])
    } else {
        (0, Vec::new())
    };
    let owner = fx.func("f", generics, &[t], t, clauses);
    fx.finish();
    let mut bb = Bb::new();
    let a = bb.param("a");
    let operand = bb.local(a);
    let neg_e = bb.e(Expr::Unary { op: UnOp::Neg, operand });
    let root = bb.block(&[], Some(neg_e));
    check_body(&fx, &bb, owner, root, Plant::None, InferPlant::None)
}

/// CLEAN: `-a` on `i32`, `i64` and `f64` is builtin (no finding, no callee row); on a type
/// parameter `T: Neg<Output = T>` it is `Neg::neg` at `[T]`.
#[test]
fn negation_of_signed_integers_floats_and_a_neg_parameter_is_clean() {
    for node in [PNode::Int(IntTy::I32), PNode::Int(IntTy::I64), PNode::Float(FloatTy::F64)] {
        let c = negate(|fx| fx.ty(node), false);
        assert!(c.diags.is_empty(), "-a on {node:?}: {:?}", c.diags);
        assert_eq!(c.callees.len(), 0, "-a on {node:?} left a callee row");
    }
    let c = negate(|fx| fx.param(0), true);
    assert!(c.diags.is_empty(), "-a on T: Neg: {:?}", c.diags);
    assert_eq!(c.callees.len(), 1, "-a on T: Neg is the trait's method");
}

/// REFUSED: `-a` on `u8`, `u32` and `usize` is E0600, the finding carrying the Reference's
/// rule ([`Because::Reference`], `expr.negate.results`). Before the fix the checker took it as
/// the builtin negation (`is_integral`) and found nothing.
#[test]
fn negation_of_an_unsigned_integer_is_refused_by_the_reference() {
    for node in [PNode::Uint(UintTy::U8), PNode::Uint(UintTy::U32), PNode::Uint(UintTy::Usize)] {
        let c = negate(|fx| fx.ty(node), false);
        let found: Vec<_> = c.diags.iter().map(|d| (d.code, d.because)).collect();
        assert!(
            matches!(found.as_slice(), [(DiagCode::UnOp, Some(Because::Reference { rule: "expr.negate.results", .. }))]),
            "-a on {node:?}: {found:?}"
        );
    }
}

/// `fn g() { let x = 5; let y = -x; let z: <late> = x; }` in the module doc's world: `-x` is
/// checked while `x` is still `{integer}`, and `x` is resolved by the LATER `let`.
fn negate_then_resolve(late: PNode) -> crate::Checked<'static> {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let p0 = fx.param(0);
    let i32_ = fx.ty(PNode::Int(IntTy::I32));
    let neg = fx.tr("Neg", 1, Vec::new(), &["Output"]);
    fx.lang(LangItem::Neg, neg);
    let out = fx.alias(neg, "Output", &[p0]);
    fx.trait_fn(neg, "neg", true, &[p0], out);
    fx.imp("neg for i32", 0, i32_, Some((neg, &[])), &[("Output", i32_)], Vec::new());
    // A second signed impl, as core has one per signed integer and float: with `Neg for i32`
    // alone, selecting `{integer}: Neg` before the later `let` (rustc
    // `resolve_vars_with_obligations` at each coercion, machine.rs `resolve_with_obligations`)
    // has ONE candidate and rightly fixes `x` to `i32`, so the later `u32` is a mismatch, not
    // the negation's refusal. With core's impls it stays ambiguous, as here.
    let i64_ = fx.ty(PNode::Int(IntTy::I64));
    fx.imp("neg for i64", 0, i64_, Some((neg, &[])), &[("Output", i64_)], Vec::new());
    let unit = fx.unit();
    let owner = fx.func("g", 0, &[], unit, Vec::new());
    fx.finish();
    let mut bb = Bb::new();
    let x = bb.bind("x");
    let lit = bb.int(None);
    let s0 = bb.let_(x, None, lit);
    let y = bb.bind("y");
    let xe = bb.local(x);
    let neg_e = bb.e(Expr::Unary { op: UnOp::Neg, operand: xe });
    let s1 = bb.let_(y, None, neg_e);
    let z = bb.bind("z");
    let zt = bb.ty(late);
    let xe2 = bb.local(x);
    let s2 = bb.let_(z, Some(zt), xe2);
    let root = bb.block(&[s0, s1, s2], None);
    check_body(&fx, &bb, owner, root, Plant::None, InferPlant::None)
}

/// REFUSED: `-x` where `x` is `{integer}` at the negation and `u32` after the next `let` is
/// refused (the obligation `u32: Neg`, which no impl answers: E0277 or E0600, at the
/// negation). CLEAN twin: the same with `i32`, which core's `Neg` impl answers.
/// Before the fix (Edit 1 of wiring wrong-r7-neg-var-1.md) the `u32` case found nothing.
/// Prints `test suite::neg_unsigned::negation_of_a_variable_resolved_unsigned_later_is_refused ... ok`.
#[test]
fn negation_of_a_variable_resolved_unsigned_later_is_refused() {
    let clean = negate_then_resolve(PNode::Int(IntTy::I32));
    assert!(clean.diags.is_empty(), "-x with x: i32 later: {:?}", clean.diags);
    let c = negate_then_resolve(PNode::Uint(UintTy::U32));
    let found: Vec<_> = c.diags.iter().map(|d| d.code).collect();
    assert!(
        found.iter().any(|code| matches!(code, DiagCode::UnOp | DiagCode::Unsatisfied)),
        "-x with x: u32 later is not refused: {found:?}"
    );
}
