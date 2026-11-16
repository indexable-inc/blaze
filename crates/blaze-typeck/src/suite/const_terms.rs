//! Const terms in the checker's types (typeck.md, "Const generics"): an array length and a const
//! generic argument are `Ty` handles whose view is `ConstValue`, `ConstParam` or `ConstInfer`.
//!
//! The rules, from the Rust Reference:
//! - "Const generics": a const parameter's type is an integer type, `char` or `bool`; a const
//!   argument is a value of that type. Two generic types are the same type only when their
//!   arguments are pairwise equal, a const argument by its value: `Foo<4u8>` and `Foo<5u8>` are
//!   different types, and a value of another type is no argument of the parameter.
//! - "Array types": `[T; N]` and `[T; M]` are the same type exactly when `T` is and `N == M`.
//! - "Type inference" (an inferred const argument, `_` or a length to infer, takes the one value
//!   that makes the program type-check).
//!
//! Planted bug: `InferPlant::AnyLength` (two arrays unify when their elements do, whatever their
//! lengths), caught by `control_any_length`.

use std::vec::Vec;

use blaze_own::Alloc;

use super::{Want, check_body, expect};
use crate::diag::DiagCode;
use crate::error::{CheckError, Malformed};
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, FloatTy, ItemKey, PatId, UintTy};
use crate::infer::{InferCtx, InferPlant};
use crate::machine::Plant;
use crate::portable::PNode;
use crate::ty::{Ty, TyView, TypeCtx, VarKind};

fn value(cx: &mut InferCtx<'_>, ty: TyView, bits: u128) -> Ty {
    let ty = cx.mk(ty).expect("a scalar type interns");
    cx.mk(TyView::ConstValue { ty, bits }).expect("a const value interns")
}

fn usize_(cx: &mut InferCtx<'_>, n: u128) -> Ty {
    value(cx, TyView::Uint(UintTy::Usize), n)
}

/// "Const generics": equal values (equal type, equal bits) are one argument; a different value,
/// or the same bits at another type, is a different one.
#[test]
fn const_values_unify_by_type_and_value() {
    let mut cx = InferCtx::new_in(Alloc::global());
    let (four, four_again, five) = (usize_(&mut cx, 4), usize_(&mut cx, 4), usize_(&mut cx, 5));
    let four_u8 = value(&mut cx, TyView::Uint(UintTy::U8), 4);
    assert_eq!(cx.unify(four, four_again), Ok(true));
    assert_eq!(cx.unify(four, five), Ok(false), "4 and 5 are different arguments");
    assert_eq!(cx.unify(four, four_u8), Ok(false), "4usize and 4u8 are different arguments");
    let t = value(&mut cx, TyView::Bool, 1);
    assert!(matches!(cx.view(t), TyView::ConstValue { bits: 1, .. }));
}

/// "Const generics": a const parameter in the item's own body is rigid: it equals itself and no
/// value and no other parameter.
#[test]
fn a_const_parameter_equals_only_itself() {
    let mut cx = InferCtx::new_in(Alloc::global());
    let n = cx.mk(TyView::ConstParam(0)).expect("interns");
    let n_again = cx.mk(TyView::ConstParam(0)).expect("interns");
    let m = cx.mk(TyView::ConstParam(1)).expect("interns");
    let four = usize_(&mut cx, 4);
    assert_eq!(cx.unify(n, n_again), Ok(true));
    assert_eq!(cx.unify(n, m), Ok(false));
    assert_eq!(cx.unify(n, four), Ok(false));
    assert_eq!(cx.view(n), TyView::ConstParam(0));
}

/// "Type inference": a const variable takes the value it is unified with, once; a rollback
/// unbinds it.
#[test]
fn a_const_variable_takes_one_value() {
    let mut cx = InferCtx::new_in(Alloc::global());
    let v = cx.fresh(VarKind::Const).expect("a const variable");
    assert!(matches!(cx.view(v), TyView::ConstInfer(var) if var.kind == VarKind::Const));
    let (four, five) = (usize_(&mut cx, 4), usize_(&mut cx, 5));
    let s = cx.snapshot();
    assert_eq!(cx.unify(v, five), Ok(true));
    cx.rollback_to(s);
    let r = cx.shallow_resolve(v);
    assert!(matches!(cx.view(r), TyView::ConstInfer(_)), "the rollback unbinds it");
    assert_eq!(cx.unify(v, four), Ok(true));
    assert_eq!(cx.shallow_resolve(v), four);
    assert_eq!(cx.unify(v, five), Ok(false), "bound to 4, it is not 5");
}

/// "Const generics": arguments compare pairwise, a const argument by value: `S<u8, 4>` and
/// `S<u8, 5>` differ; `S<u8, ?N>` against `S<u8, 4>` infers `?N = 4`.
#[test]
fn const_arguments_in_a_list() {
    let mut cx = InferCtx::new_in(Alloc::global());
    let s = ItemKey(0x5);
    let u8_ = cx.mk(TyView::Uint(UintTy::U8)).expect("interns");
    let (four, five) = (usize_(&mut cx, 4), usize_(&mut cx, 5));
    let v = cx.fresh(VarKind::Const).expect("a const variable");
    let l4 = cx.mk_list(&[u8_, four]).expect("a list with a const interns");
    let l5 = cx.mk_list(&[u8_, five]).expect("interns");
    let lv = cx.mk_list(&[u8_, v]).expect("interns");
    let (s4, s5, sv) = (
        cx.mk(TyView::Adt(s, l4)).expect("interns"),
        cx.mk(TyView::Adt(s, l5)).expect("interns"),
        cx.mk(TyView::Adt(s, lv)).expect("interns"),
    );
    assert_eq!(cx.list_get(l4, 1), four, "a list hands back the const argument");
    assert_eq!(cx.unify(s4, s5), Ok(false));
    assert_eq!(cx.unify(sv, s4), Ok(true));
    assert_eq!(cx.resolve(v), Ok(four));
    let r = cx.resolve(sv).expect("resolves");
    assert_eq!(r, s4, "resolving through the argument list");
}

/// "Const generics": a const generic's type is an integer, `char` or `bool`: a value of a float
/// type is refused as malformed input, by the handle of its type.
#[test]
fn a_const_value_of_a_float_type_is_refused() {
    let mut cx = InferCtx::new_in(Alloc::global());
    let f = cx.mk(TyView::Float(FloatTy::F32)).expect("interns");
    assert_eq!(
        cx.mk(TyView::ConstValue { ty: f, bits: 0 }),
        Err(CheckError::Malformed(Malformed::ConstValueType { node: f.0 }))
    );
}

fn world() -> (Fx, ItemKey) {
    let mut world = Fx::new();
    let fx = &mut world;
    let unit = fx.unit();
    let main = fx.func("main", 0, &[], unit, Vec::new());
    fx.finish();
    (world, main)
}

/// `fn main() { let a: [u8; LEN] = [0u8; 4]; }`: with LEN = 4 the binding is `[u8; 4]`; with
/// LEN = 5 E0308 at the repeat ("Array types": the lengths must be equal).
fn array_body(len: u64, ip: InferPlant) -> (Fx, Bb, crate::columns::Checked<'static>) {
    let (fx, main) = world();
    let mut b = Bb::new();
    let a = b.bind("a");
    let u8_ = b.ty(PNode::Uint(UintTy::U8));
    let arr = b.t.array_n(u8_, len).expect("intern");
    let e0 = b.int(Some(crate::body::IntSuffix::Uint(UintTy::U8)));
    let e1 = b.repeat(e0, 4);
    let s0 = b.let_(a, Some(arr), e1);
    let root = b.block(&[s0], None);
    let c = check_body(&fx, &b, main, root, Plant::None, ip);
    (fx, b, c)
}

#[test]
fn an_array_of_the_declared_length() {
    let (fx, b, c) = array_body(4, InferPlant::None);
    let want = Want {
        exprs: &[(ExprId(0), "u8"), (ExprId(1), "[u8; 4]"), (ExprId(2), "()")],
        pats: &[(PatId(0), "[u8; 4]")],
        diags: &[],
    };
    expect(&fx, &b, &c, &want);
}

#[test]
fn an_array_of_another_length_is_e0308() {
    let (_, _, c) = array_body(5, InferPlant::None);
    assert!(c.diags.iter().any(|d| d.code == DiagCode::Mismatch), "{:?}", c.diags);
}

/// The planted bug lets `[u8; 4]` stand for `[u8; 5]`: the wrong program checks clean.
#[test]
fn control_any_length() {
    let (_, _, c) = array_body(5, InferPlant::AnyLength);
    assert!(!c.diags.iter().any(|d| d.code == DiagCode::Mismatch), "the plant was not reached: {:?}", c.diags);
}

/// `fn take(b: [u8; 1024]); fn main() { take([0; 1024]); let a: [u8; 1024] = [0; 1024]; }`,
/// each `0` unsuffixed: bstd's `Held::new` (`bytes: [0; HELD]` into a field `[u8; HELD]`, the
/// field's length the evaluated constant). The Reference, "Array expressions": in `[expr; len]`
/// `len` is a constant expression of type `usize` and the array's type is `[T; len]`; "Array
/// types": two array types are equal when their lengths are. The element is `u8` by the expected
/// type, the repeat `[u8; 1024]`, and no expression's type is left unknown.
/// The length's own expression (`HELD`, `1024`) is in the body as front ends hand it: each is
/// `usize`. Each form of the length: the constant's evaluated value with its path (`HELD`), a
/// literal, and `_` (no expression; inferred from the declared type).
#[test]
fn a_repeat_into_a_declared_array_of_a_constant_length() {
    let mut fx = Fx::new();
    let unit = fx.unit();
    let u8_ = fx.u8();
    let usize_w = fx.usize();
    fx.konst("HELD", usize_w);
    let arr = fx.array(u8_, 1024);
    fx.func("take", 0, &[arr], unit, Vec::new());
    let main = fx.func("main", 0, &[], unit, Vec::new());
    fx.finish();
    let mut b = Bb::new();
    let a = b.bind("a");
    let e0 = b.path(&["take"]);
    let e1 = b.int(None);
    let held = b.path(&["HELD"]);
    let n = b.t.usize_value(1024).expect("intern");
    let e2 = b.repeat_counted(e1, n, held);
    let e3 = b.call(e0, &[e2]);
    let s0 = b.semi(e3);
    let u8b = b.ty(PNode::Uint(UintTy::U8));
    let arr_b = b.t.array_n(u8b, 1024).expect("intern");
    let e4 = b.int(None);
    let lit = b.int(None);
    let e5 = b.repeat_counted(e4, n, lit);
    let s1 = b.let_(a, Some(arr_b), e5);
    let x = b.bind("x");
    let e6 = b.int(None);
    let hole = b.ty(PNode::Infer);
    let e7 = b.e(crate::body::Expr::Repeat { elem: e6, len: hole, count: None });
    let s2 = b.let_(x, Some(arr_b), e7);
    let root = b.block(&[s0, s1, s2], None);
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    let want = Want {
        exprs: &[
            (e0, "fn take"),
            (e1, "u8"),
            (held, "usize"),
            (e2, "[u8; 1024]"),
            (e3, "()"),
            (e4, "u8"),
            (lit, "usize"),
            (e5, "[u8; 1024]"),
            (e6, "u8"),
            (e7, "[u8; 1024]"),
            (root, "()"),
        ],
        pats: &[(a, "[u8; 1024]"), (x, "[u8; 1024]")],
        diags: &[],
    };
    expect(&fx, &b, &c, &want);
}
