//! The operator `*` on a type whose `Deref` target is not decided yet (rules/op.rs,
//! machine.rs `overloaded_deref_at` with `DerefAt::Operator`).
//!
//! The site is turso core/skiplist/set_tests.rs:225:
//! `let s = SkipSet::new(); let keys = || s.iter().map(|e| *e).collect::<Vec<_>>(); s.insert(3);`.
//! Here `e: Entry<'_, ?T>` and `impl<T, C, A: SkiplistAllocator> Deref for Entry<'_, T, C, A>
//! { type Target = T; }`. turso c1851p gave "found `Entry<i32>` (E0614 CantDeref)".
//!
//! Cause, from reading:
//! - Normalizing `<Entry<?T> as Deref>::Target` selects the impl.
//! - The impl's implied `?T: Sized` is ambiguous (blaze-solve trait_goals.rs: "a self type
//!   that is still a type variable: forced ambiguity").
//! - The term is then `?T`, and `normalize` answers `None` for a term that is a variable
//!   (infer/mod.rs).
//! - `overloaded_deref` then gave `None`, and the operator reported E0614.
//!
//! The rule: rustc_hir_typeck place_op.rs `lookup_derefing` takes `try_overloaded_deref`, whose
//! `lookup_method_for_operator` checks only `predicate_may_hold` (method/mod.rs). The method's
//! output is normalized, with its obligations registered. The Reference, "The dereference
//! operator": "On non-pointer types `*x` is equivalent to `*std::ops::Deref::deref(&x)`".
//!
//! ```text
//! #[lang = "sized"] trait Sized {}  #[lang = "deref"] trait Deref { type Target; }
//! struct E<T>;  impl<T> Deref for E<T> { type Target = T; }      // implied `T: Sized`
//! struct N<T>;                                                   // no `Deref`
//! fn mk<T>() -> E<T>;  fn mk_n<T>() -> N<T>;
//! fn g() -> u8 { *mk() }          // clean: the target is decided by the return type
//! fn h() -> u8 { *mk_n() }        // the twin: E0614, `N<?T>: Deref` cannot hold
//! fn m() { let _x = *mk(); }      // the twin: `T` is never decided (rustc E0282), never clean
//! fn k() -> u8 { *mk::<u8>() }    // the control: a decided target, clean before and after
//! ```
//!
//! CONTROL (rule 23): with the `None if at == DerefAt::Operator` arm removed, `g` is E0614
//! again and the first test fails. `k` is clean either way.

use std::vec::Vec;

use super::check_body;
use crate::body::{Expr, UnOp};
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

fn world() -> Fx {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    fx.lang(LangItem::Deref, deref);
    let (p0, u8_) = (fx.param(0), fx.u8());
    let unit = fx.unit();

    let e_k = fx.strukt("E", 1, &[]);
    let e_t = fx.adt(e_k, &[p0]);
    fx.imp("impl<T> Deref for E<T>", 1, e_t, Some((deref, &[])), &[("Target", p0)], Vec::new());
    let n_k = fx.strukt("N", 1, &[]);
    let n_t = fx.adt(n_k, &[p0]);

    fx.func("mk", 1, &[], e_t, Vec::new());
    fx.func("mk_n", 1, &[], n_t, Vec::new());
    fx.func("g", 0, &[], u8_, Vec::new());
    fx.func("h", 0, &[], u8_, Vec::new());
    fx.func("m", 0, &[], unit, Vec::new());
    fx.func("k", 0, &[], u8_, Vec::new());
    fx.finish();
    world
}

/// `*<callee>()`, the callee made by `make`.
fn deref_of_call(b: &mut Bb, make: impl FnOnce(&mut Bb) -> ExprId) -> ExprId {
    let callee = make(b);
    let call = b.call(callee, &[]);
    b.e(Expr::Unary { op: UnOp::Deref, operand: call })
}

fn codes(fx: &Fx, owner: &str, b: &Bb, root: ExprId) -> Vec<&'static str> {
    let checked = check_body(fx, b, fx.key_of(owner), root, Plant::None, InferPlant::None);
    checked.diags.iter().map(|d| d.code.code()).collect()
}

/// turso's shape. Prints nothing; fails with "g: [\"E0614\"]" before the fix.
#[test]
fn deref_operator_with_an_undecided_target_is_a_variable() {
    let fx = world();
    let mut b = Bb::new();
    let d = deref_of_call(&mut b, |b| b.path(&["mk"]));
    let root = b.block(&[], Some(d));
    let got = codes(&fx, "g", &b, root);
    assert!(got.is_empty(), "g: {got:?}");
}

/// The twin rustc refuses with E0614: `N<?T>: Deref` cannot hold, so no variable is made.
#[test]
fn deref_operator_on_a_type_with_no_deref_is_e0614() {
    let fx = world();
    let mut b = Bb::new();
    let d = deref_of_call(&mut b, |b| b.path(&["mk_n"]));
    let root = b.block(&[], Some(d));
    let got = codes(&fx, "h", &b, root);
    assert_eq!(got, ["E0614"], "h");
}

/// The twin rustc refuses with E0282: nothing decides `T`. The new variable must not make
/// the body clean.
#[test]
fn deref_operator_target_never_decided_is_not_clean() {
    let fx = world();
    let mut b = Bb::new();
    let d = deref_of_call(&mut b, |b| b.path(&["mk"]));
    let x = b.bind("_x");
    let s = b.let_(x, None, d);
    let root = b.block(&[s], None);
    let got = codes(&fx, "m", &b, root);
    assert!(!got.is_empty(), "m is clean: the open target was never decided");
}

/// The control: a decided target is the target, clean.
#[test]
fn control_deref_operator_with_a_decided_target() {
    let fx = world();
    let mut b = Bb::new();
    let u8_ = b.ty(PNode::Uint(UintTy::U8));
    let d = deref_of_call(&mut b, |b| b.path_args("mk", &[u8_]));
    let root = b.block(&[], Some(d));
    let got = codes(&fx, "k", &b, root);
    assert!(got.is_empty(), "k: {got:?}");
}
