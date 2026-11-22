//! An undecided goal's finding (E0283, E0282) carries the solver's chain of why it is
//! undecided (`Diag::why`, blaze-solve why_maybe.rs), as an unmet goal's carries why_no's.
//!
//! The rule: the Reference, "Trait and lifetime bounds" / "Implementations": a goal is decided
//! when one impl or bound answers it; a type not known yet, or two impls that may each hold,
//! leave it undecided (rustc's E0282 "type annotations needed", E0283 "cannot satisfy"). The
//! chain's leaf says which: [`WhyLeaf::Unresolved`] (the goal's type is a variable), or
//! [`WhyLeaf::Ambiguous`] (the two impls). Each test has its twin (rule 23): the annotated
//! program is decided, with no finding and no chain row. The other leaf kinds are the solver's
//! (blaze-solve tests/why_maybe.rs), mapped one to one (infer/mod.rs `why_maybe_in`).

use std::vec::Vec;

use super::{case_ambiguous, check_body, world};
use crate::diag::{DiagCode, WhyCand, WhyLeaf, WhyReached};
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// `fn c_ambiguous(a: A) { let x = a.conv(); }` under `impl Conv<u8> for A` and
/// `impl Conv<u16> for A`: E0283 on `A: Conv<?T>`, and its chain is the root alone, whose two
/// impls may each hold.
///
/// Expected line: `test suite::why_maybe::two_impls_leave_the_goal_undecided ... ok`.
#[test]
fn two_impls_leave_the_goal_undecided() {
    let (fx, it) = world();
    let (b, root, owner) = case_ambiguous(&it);
    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.iter().map(|d| d.code).collect::<Vec<_>>(), std::vec![DiagCode::CannotSatisfy], "{:?}", c.diags);
    let why = c.diags[0].why.expect("the undecided finding carries the solver's chain");
    let steps = c.why_of(why).expect("the chain's rows in the record");
    assert_eq!(steps.len(), 1, "the goal is undecided by itself: {steps:?}");
    assert_eq!(steps[0].reached, WhyReached::Root);
    let u8_impl: ItemKey = fx.key_of("impl Conv<u8> for A");
    let u16_impl: ItemKey = fx.key_of("impl Conv<u16> for A");
    match why.leaf {
        WhyLeaf::Ambiguous { first: WhyCand::Impl { impl_: one }, second: WhyCand::Impl { impl_: two }, count: 2 } => {
            let mut got = [one, two];
            got.sort();
            let mut want = [u8_impl, u16_impl];
            want.sort();
            assert_eq!(got, want, "the two impls of `Conv`");
        }
        other => panic!("two impls may hold: {other:?}"),
    }
}

/// TWIN: `let x: u16 = a.conv();` chooses `impl Conv<u16> for A`: no finding, no chain row.
///
/// Expected line: `test suite::why_maybe::an_annotated_conv_is_decided ... ok`.
#[test]
fn an_annotated_conv_is_decided() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let a = b.param("a");
    let e0 = b.local(a);
    let e1 = b.method(e0, "conv", &[]);
    let x = b.bind("x");
    let u16_ = b.ty(PNode::Uint(UintTy::U16));
    let s0 = b.let_(x, Some(u16_), e1);
    let root = b.block(&[s0], None);
    let c = check_body(&fx, &b, it.owners[7], root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert!(c.why.is_empty(), "{:?}", c.why);
}

/// `trait Tr {}  impl Tr for u8 {}  fn need<T: Tr>() -> T;  fn h() { let x = need(); }`:
/// nothing sets `T`, so `?T: Tr` (and `?T: Sized`) stays undecided; the finding's chain is the
/// root, whose `Self` (term 0) is not known.
fn need_world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let tr = fx.tr("Tr", 1, Vec::new(), &[]);
    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    fx.imp("impl Tr for u8", 0, u8_, Some((tr, &[])), &[], Vec::new());
    let t = fx.param(0);
    let bound = fx.bound(tr, &[t]);
    fx.func("need", 1, &[], t, std::vec![bound]);
    let unit = fx.unit();
    let h = fx.func("h", 0, &[], unit, Vec::new());
    fx.finish();
    (fx, h)
}

fn need_body(annotate: bool) -> (Bb, crate::ids::ExprId) {
    let mut b = Bb::new();
    let f = b.path(&["need"]);
    let call = b.call(f, &[]);
    let x = b.bind("x");
    let ty = if annotate { Some(b.ty(PNode::Uint(UintTy::U8))) } else { None };
    let s = b.let_(x, ty, call);
    let root = b.block(&[s], None);
    (b, root)
}

/// Expected line: `test suite::why_maybe::an_unknown_type_leaves_the_goal_undecided ... ok`.
#[test]
fn an_unknown_type_leaves_the_goal_undecided() {
    let (fx, h) = need_world();
    let (b, root) = need_body(false);
    let c = check_body(&fx, &b, h, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 1, "one undecided finding: {:?}", c.diags);
    assert!(matches!(c.diags[0].code, DiagCode::CannotSatisfy | DiagCode::AnnotationsNeeded), "{:?}", c.diags);
    let why = c.diags[0].why.expect("the undecided finding carries the solver's chain");
    let steps = c.why_of(why).expect("the chain's rows in the record");
    assert_eq!(steps.len(), 1, "{steps:?}");
    // The root names `?T`: the record keeps no variable, so its goal is not kept.
    assert_eq!(steps[0].goal, None, "{steps:?}");
    assert_eq!(why.leaf, WhyLeaf::Unresolved { term: 0 }, "`Self` is not known");
}

/// TWIN: `let x: u8 = need();` sets `T = u8`, and `impl Tr for u8` holds: no finding, no row.
///
/// Expected line: `test suite::why_maybe::an_annotated_need_is_decided ... ok`.
#[test]
fn an_annotated_need_is_decided() {
    let (fx, h) = need_world();
    let (b, root) = need_body(true);
    let c = check_body(&fx, &b, h, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert!(c.why.is_empty(), "{:?}", c.why);
}
