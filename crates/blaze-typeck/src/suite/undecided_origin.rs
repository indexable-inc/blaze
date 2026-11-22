//! An E0282 names what is undecided (rule 19; `Diag::undecided`).
//!
//! tempo's crates/zones/node/tests/it/utils.rs:2658 was refused with an E0282 at
//! `.extend_rpc_modules(` that named only the place: not which variable was undecided, nor
//! which goal was still waiting on it. A finding now keeps the variable's origin (a generic
//! argument of an item, a closure's parameter or return) and the goals pending on it.
//!
//! ```text
//! trait Tr {}  impl Tr for u8 {}  impl Tr for u16 {}
//! fn g<T: Tr>() -> T;   fn g2<T>() -> T;
//! fn h() { let y = g(); y.f; }
//! ```
//!
//! `y.f` resolves `y`'s type structurally (rules/field.rs), and it is `g`'s `T`, which nothing
//! decides: two impls of `Tr` leave `?T: Tr` ambiguous, so the goal is still pending.

use super::check_body;
use crate::body::Expr;
use crate::diag::{DiagCode, UndecidedVar, UnmetGoal, VarOrigin};
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, Name, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// The world: `Sized`, `Tr` with impls for `u8` and `u16`, `g<T: Tr>() -> T`,
/// `g2<T>() -> T`, and `h()`. Returns `[g, g2, h, tr]`.
fn world() -> (Fx, [ItemKey; 4]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Default::default(), &[]);
    fx.lang(LangItem::Sized, sized);
    let tr = fx.tr("Tr", 1, Default::default(), &[]);
    let u8_ = fx.u8();
    let u16_ = fx.ty(PNode::Uint(UintTy::U16));
    fx.imp("impl Tr for u8", 0, u8_, Some((tr, &[])), &[], Default::default());
    fx.imp("impl Tr for u16", 0, u16_, Some((tr, &[])), &[], Default::default());
    let p0 = fx.param(0);
    let c_tr = fx.bound(tr, &[p0]);
    let g = fx.func("g", 1, &[], p0, std::vec![c_tr]);
    let g2 = fx.func("g2", 1, &[], p0, Default::default());
    let unit = fx.unit();
    let h = fx.func("h", 0, &[], unit, Default::default());
    fx.finish();
    (fx, [g, g2, h, tr])
}

/// The one E0282's undecided variable, checking `root` as `h`'s body.
fn undecided(fx: &Fx, b: &Bb, h: ItemKey, root: ExprId) -> UndecidedVar {
    let checked = check_body(fx, b, h, root, Plant::None, InferPlant::None);
    let d = checked.diags.iter().find(|d| d.code == DiagCode::AnnotationsNeeded);
    let Some(d) = d else { panic!("want an E0282, got {:?}", checked.diags) };
    let Some(u) = d.undecided else { panic!("the E0282 names no undecided variable: {d:?}") };
    u
}

/// `fn h() { let y = <callee>(); y.f; }`.
fn call_then_field(b: &mut Bb, callee: &str) -> ExprId {
    let callee = b.path(&[callee]);
    let call = b.call(callee, &[]);
    let y = b.bind("y");
    let s = b.let_(y, None, call);
    let base = b.local(y);
    let field = b.e(Expr::Field { base, name: Name::of("f") });
    let s2 = b.semi(field);
    b.block(&[s, s2], None)
}

fn pends_on(u: &UndecidedVar, tr: ItemKey) -> bool {
    u.pending.iter().flatten().any(|g| matches!(g, UnmetGoal::Trait { trait_, .. } if *trait_ == tr))
}

/// The E0282 at `y.f` names `g`'s parameter #0 and the goal `?T: Tr` pending on it. Twin:
/// `g2<T>` has no `Tr` bound, so the same E0282 names `g2`'s parameter and no `Tr` goal, and
/// one goal fewer pends: the goal named is the bound's, not something the walk made up.
/// Without `Diag::undecided` (export setting `None`) this fails with "names no undecided
/// variable".
///
/// Expected line: `test suite::undecided_origin::an_e0282_names_its_variable_and_the_goals_on_it ... ok`.
#[test]
fn an_e0282_names_its_variable_and_the_goals_on_it() {
    let (fx, [g, g2, h, tr]) = world();

    let mut b = Bb::new();
    let root = call_then_field(&mut b, "g");
    let bounded = undecided(&fx, &b, h, root);
    assert_eq!(bounded.origin, VarOrigin::GenericArg { item: g, index: 0 }, "{bounded:?}");
    assert!(pends_on(&bounded, tr), "ORACLE: `?T: Tr` from `g`'s bound pends on the variable: {bounded:?}");

    let mut b = Bb::new();
    let root = call_then_field(&mut b, "g2");
    let free = undecided(&fx, &b, h, root);
    assert_eq!(free.origin, VarOrigin::GenericArg { item: g2, index: 0 }, "{free:?}");
    assert!(!pends_on(&free, tr), "TWIN: `g2` has no `Tr` bound, yet a `Tr` goal was named: {free:?}");
    assert_eq!(bounded.pending_count, free.pending_count + 1, "the bound is the one goal more: {bounded:?} / {free:?}");
}

/// `|x| x.f`: the E0282 at `x.f` names the closure's parameter #0 (nothing written, nothing
/// expected). Twin: `let y: u8 = g2(); y.f`, the variable decided by the annotation: the
/// refusal is then a field one, no E0282 and nothing undecided.
///
/// Expected line: `test suite::undecided_origin::an_unannotated_closure_parameter_is_named ... ok`.
#[test]
fn an_unannotated_closure_parameter_is_named() {
    let (fx, [_, _, h, _]) = world();

    let mut b = Bb::new();
    let x = b.bind("x");
    let base = b.local(x);
    let field = b.e(Expr::Field { base, name: Name::of("f") });
    let closure = b.closure(&[x], field);
    let c = b.bind("c");
    let s = b.let_(c, None, closure);
    let root = b.block(&[s], None);
    let u = undecided(&fx, &b, h, root);
    assert_eq!(u.origin, VarOrigin::ClosureParam { closure, index: 0 }, "{u:?}");

    // Twin: `let y: u8 = g2(); y.f` decides the variable, so no E0282 and nothing undecided.
    let mut b = Bb::new();
    let u8_ = b.ty(PNode::Uint(UintTy::U8));
    let callee = b.path(&["g2"]);
    let call = b.call(callee, &[]);
    let y = b.bind("y");
    let s = b.let_(y, Some(u8_), call);
    let base = b.local(y);
    let field = b.e(Expr::Field { base, name: Name::of("f") });
    let s2 = b.semi(field);
    let root = b.block(&[s, s2], None);
    let checked = check_body(&fx, &b, h, root, Plant::None, InferPlant::None);
    assert!(
        checked.diags.iter().all(|d| d.code != DiagCode::AnnotationsNeeded && d.undecided.is_none()),
        "TWIN: an annotated type leaves nothing undecided: {:?}",
        checked.diags
    );
}
