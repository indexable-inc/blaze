//! Bodies in the forms blaze-lower makes (typeck.md, "Owed to blaze-lower"), checked by
//! `InferCtx`, each with the expected type of every expression and binding and the rule it
//! follows. The world is the ladder's rung 7c ([`crate::fixture::rung_7c`]).
//!
//! `?` reaches the body as blaze-lower's `try_op` builds it, its paths lang items
//! (`BodyPath::lang`), resolved by `World::lang_path`.

use std::format;
use std::string::String;
use std::vec::Vec;

use super::{Want, callee, check_body, expect};
use crate::diag::DiagCode;
use crate::fixture::{Bb, rung_7c};
use crate::ids::{ExprId, PatId};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::world::{CtorKind, Def};

/// The checker's invariant on itself: a body with no diagnostic and nothing left to ask names
/// every type it records, so the passes after it never meet "unknown". The world here answers a
/// signature whose output is the error type (input no correct world gives): the call checks
/// with no diagnostic, and the record would carry the error type as the call's type; the
/// writeback refuses it, naming the expression (`CheckError::UnknownInCleanBody`). Without the
/// scan this body passed as clean (the ladder's walk met the same silence on `?`, 2026-11-16).
#[test]
fn a_clean_body_never_records_an_unknown_type() {
    use crate::error::{CheckError, Recorded};
    use crate::fixture::Fx;
    use crate::{Outcome, check_with};
    use crate::portable::PNode;
    use crate::world::Needs;
    use crate::InferCtx;
    use blaze_own::Alloc;
    let mut fx = Fx::new();
    let unit = fx.unit();
    let err = fx.ty(PNode::Error);
    let mystery = fx.func("mystery", 0, &[], err, Vec::new());
    let owner = fx.func("f", 0, &[], unit, Vec::new());
    fx.use_path("mystery", Def::Fn(mystery));
    fx.finish();
    let mut b = Bb::new();
    let callee_e = b.path(&["mystery"]);
    let call = b.call(callee_e, &[]);
    let s0 = b.semi(call);
    let root = b.block(&[s0], None);
    let mut cx = InferCtx::new_in(Alloc::global());
    let got = check_with(&b.view(owner, root), &fx, &mut cx, &Needs::new_in(Alloc::global()), Plant::None, Alloc::global());
    match got {
        Err(CheckError::UnknownInCleanBody { at }) => assert_eq!(at, Recorded::ExprTy(call)),
        Ok(Outcome::Done(c)) => panic!("a clean record with an unknown type passed: {} diagnostics", c.diags.len()),
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    }
}

/// An expression no list or parent holds is the lowering's mistake, refused by name: blaze-lower
/// once lowered a `format_args!` template as a string literal, read its text, and left the node
/// in the body (core's `panic_bounds_check`, the first library body every program reaches,
/// 2026-11-16). Without the `reached` scan this was `UnknownInCleanBody { at: ExprTy(..) }`,
/// which reads as a type the checker could not name.
#[test]
fn an_expression_no_list_holds_is_refused_by_name() {
    use crate::error::{CheckError, Malformed};
    use crate::fixture::Fx;
    use crate::world::Needs;
    use crate::{InferCtx, check_with};
    use blaze_own::Alloc;
    let mut fx = Fx::new();
    let unit = fx.unit();
    let owner = fx.func("f", 0, &[], unit, Vec::new());
    fx.finish();
    let mut b = Bb::new();
    let orphan = b.int(None);
    let root = b.block(&[], None);
    let mut cx = InferCtx::new_in(Alloc::global());
    match check_with(&b.view(owner, root), &fx, &mut cx, &Needs::new_in(Alloc::global()), Plant::None, Alloc::global()) {
        Err(CheckError::Malformed(Malformed::Unreached(x))) => assert_eq!(x, orphan),
        other => panic!("an unplaced literal must be refused as unreached: {:?}", other.map(|_| ())),
    }
}

/// The Reference, "Block expressions": a block with no final operand has type `()` unless it
/// diverges, so `fn f() -> u8 { 1; }` is E0308 (rustc's `coerce_forced_unit`). The mismatch is
/// the block's: it carries the block's own range. It once carried the body's empty place
/// (`At::Body`) and every such finding stopped the check as `DiagWithoutPlace` (13 bstd bodies,
/// a `panic!` as a match arm, 2026-11-16).
#[test]
fn a_forced_unit_mismatch_carries_the_blocks_range() {
    use crate::fixture::Fx;
    let mut fx = Fx::new();
    let u8_ = fx.u8();
    let owner = fx.func("f", 0, &[], u8_, Vec::new());
    fx.finish();
    let mut b = Bb::new();
    let one = b.int(None);
    let s = b.semi(one);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert_eq!(codes, [DiagCode::Mismatch], "one E0308");
    assert!(c.diags[0].at.end > c.diags[0].at.start, "the mismatch has the block's place: {:?}", c.diags[0].at);
}

/// Negative impls. The Reference, "Special types and traits", auto traits: "if `T` implements
/// an auto trait then ... `*const T`, `*mut T` ... implement it" unless "a negative
/// implementation" is written, and the pinned core writes `impl<T: PointeeSized> !Send for
/// *const T` (library/core/src/marker.rs). So `*const u8: Send` does NOT hold. A world that
/// answered every impl as positive (blaze-typeck's facts bridge did, 2026-11-16) made that
/// impl a positive `Send` impl and the call below checked clean; a world with no impl at all
/// proves it structurally from `u8: Send`, the wrong answer front2 gives today while its heads
/// refuse `impl !Trait`.
#[test]
fn a_negative_impl_keeps_an_auto_trait_from_holding() {
    use crate::ids::Mutbl;
    use crate::portable::PNode;
    use crate::world::ImplPolarity;
    use crate::fixture::Fx;
    let mut fx = Fx::new();
    let send = fx.tr("Send", 1, Vec::new(), &[]);
    fx.auto(send);
    let unit = fx.unit();
    let t = fx.param(0);
    let ptr_t = fx.ty(PNode::Ptr(Mutbl::Not, t));
    let neg = fx.imp("impl<T> !Send for *const T", 1, ptr_t, Some((send, &[])), &[], Vec::new());
    fx.set_polarity(neg, ImplPolarity::Negative);
    let u8_ = fx.u8();
    let ptr_u8 = fx.ty(PNode::Ptr(Mutbl::Not, u8_));
    let bound = fx.bound(send, &[t]);
    let need = fx.func("need_send", 1, &[t], unit, std::vec![bound]);
    let mk = fx.func("mk", 0, &[], ptr_u8, Vec::new());
    let owner = fx.func("f", 0, &[], unit, Vec::new());
    fx.use_path("need_send", Def::Fn(need));
    fx.use_path("mk", Def::Fn(mk));
    fx.finish();
    let mut b = Bb::new();
    let mk_e = b.path(&["mk"]);
    let made = b.call(mk_e, &[]);
    let need_e = b.path(&["need_send"]);
    let call = b.call(need_e, &[made]);
    let s0 = b.semi(call);
    let root = b.block(&[s0], None);
    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(!codes.is_empty(), "`*const u8: Send` must not hold under `impl<T> !Send for *const T`: the call checked clean");
}

/// The front end once answered an impl header's trait arguments WITH `Self` first, where
/// `ImplHeader::trait_args` means those after `Self` (`ArgsAfterSelf`); every trait goal then
/// silently matched no impl. The Reference, "Implementations": "trait implementations ... the
/// trait being implemented" with its generic arguments; `Self` is the implementing type, not one
/// of them. Here `impl From<Missing> for Error` answers `[Error, Missing]` for `From`'s one
/// parameter besides `Self`: the checker refuses it where the answer enters, naming the impl.
#[test]
fn an_impl_header_with_self_first_is_refused_by_name() {
    use crate::error::CheckError;
    use crate::{InferCtx, Outcome, check_with};
    use crate::world::Needs;
    use blaze_own::Alloc;
    let (mut fx, s) = rung_7c();
    let bad = fx.key_of("impl From<Missing> for Error");
    let from = fx.key_of("From");
    fx.answer_self_first(bad);
    let mut b = Bb::new();
    let _cx = b.param("cx");
    let io = b.param("io");
    let e0 = b.local(io);
    let e1 = b.method(e0, "need_out", &[]);
    let e9 = b.try_op(e1);
    let s0 = b.semi(e9);
    let ok = b.path(&["Ok"]);
    let unit = b.tuple(&[]);
    let call = b.call(ok, &[unit]);
    let root = b.block(&[s0], Some(call));
    let mut cx = InferCtx::new_in(Alloc::global());
    match check_with(&b.view(s.main, root), &fx, &mut cx, &Needs::new_in(Alloc::global()), Plant::None, Alloc::global()) {
        Err(CheckError::ImplTraitArity { impl_, trait_, expected, got }) => {
            assert_eq!((impl_, trait_, expected, got), (bad, from, 1, 2));
        }
        Ok(Outcome::Done(c)) => panic!("a self-first impl header passed with {} diagnostics", c.diags.len()),
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    }
}

/// ```text
/// fn main(cx: &Cx, io: Io) -> Result<(), Error> {
///     io.need_out()?.line(cx, "Hello, world!")?;
///     Ok(())
/// }
/// ```
/// Each `?` is `match Try::branch(e) { ControlFlow::Break(residual) => return
/// FromResidual::from_residual(residual), ControlFlow::Continue(val) => val }` (rustc_ast_lowering
/// `lower_expr_try`). `Try::branch` is a trait fn by path: `Self` a fresh variable, unified
/// with the argument, `Self: Try` selecting the `Result` impl (`instantiate_value_path`, the
/// fn's predicates); its output `ControlFlow<Self::Residual, Self::Output>` normalizes through
/// that impl to `ControlFlow<Result<Infallible, Missing>, Out>`. `from_residual` takes `Self`
/// from the `return`'s coercion to the fn's output (`check_expr_return`) and `R` from its
/// argument; `Result<(), Error>: FromResidual<Result<Infallible, Missing>>` holds by the impl
/// and its where-clause `Error: From<Missing>`. The `Break` arm diverges (`!`, coerced in the
/// match: `check_match`'s `CoerceMany`), so the match is the `Continue` payload's type.
#[test]
fn question_mark_through_try_and_from_residual() {
    let (fx, s) = rung_7c();
    let mut b = Bb::new();
    let cx = b.param("cx");
    let io = b.param("io");
    let e0 = b.local(io);
    let e1 = b.method(e0, "need_out", &[]);
    let e9 = b.try_op(e1);
    let e10 = b.local(cx);
    let e11 = b.str_();
    let e12 = b.method(e9, "line", &[e10, e11]);
    let e20 = b.try_op(e12);
    let s0 = b.semi(e20);
    let e21 = b.path(&["Ok"]);
    let e22 = b.tuple(&[]);
    let e23 = b.call(e21, &[e22]);
    let root = b.block(&[s0], Some(e23));
    assert_eq!(root, ExprId(24));
    let c = check_body(&fx, &b, s.main, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "Io"),
                (ExprId(1), "Result<Out, Missing>"),
                (ExprId(2), "fn branch<Result<Out, Missing>>"),
                (ExprId(3), "ControlFlow<Result<Infallible, Missing>, Out>"),
                (ExprId(4), "fn from_residual<Result<(), Error>, Result<Infallible, Missing>>"),
                (ExprId(5), "Result<Infallible, Missing>"),
                (ExprId(6), "Result<(), Error>"),
                (ExprId(7), "!"),
                (ExprId(8), "Out"),
                (ExprId(9), "Out"),
                (ExprId(10), "&Cx"),
                (ExprId(11), "&str"),
                (ExprId(12), "Result<(), OutError>"),
                (ExprId(13), "fn branch<Result<(), OutError>>"),
                (ExprId(14), "ControlFlow<Result<Infallible, OutError>, ()>"),
                (ExprId(15), "fn from_residual<Result<(), Error>, Result<Infallible, OutError>>"),
                (ExprId(16), "Result<Infallible, OutError>"),
                (ExprId(17), "Result<(), Error>"),
                (ExprId(18), "!"),
                (ExprId(19), "()"),
                (ExprId(20), "()"),
                (ExprId(21), "fn Result::Ok<(), Error>"),
                (ExprId(22), "()"),
                (ExprId(23), "Result<(), Error>"),
                (ExprId(24), "Result<(), Error>"),
            ],
            pats: &[
                (PatId(0), "&Cx"),
                (PatId(1), "Io"),
                (PatId(2), "Result<Infallible, Missing>"),
                (PatId(4), "Out"),
                (PatId(6), "Result<Infallible, OutError>"),
                (PatId(8), "()"),
            ],
            diags: &[],
        },
    );
    assert_eq!(callee(&fx, &c, ExprId(1)), "need_out<>");
    assert_eq!(callee(&fx, &c, ExprId(3)), "branch<Result<Out, Missing>>");
    assert_eq!(callee(&fx, &c, ExprId(6)), "from_residual<Result<(), Error>, Result<Infallible, Missing>>");
    assert_eq!(callee(&fx, &c, ExprId(12)), "line<>");
    // The resolutions the backend switches on: each `?`'s arms name `ControlFlow`'s variants.
    let defs: Vec<(PatId, Def)> =
        c.pat_defs.column::<0>().iter().copied().zip(c.pat_defs.column::<1>().iter().copied()).collect();
    let brk = Def::Ctor { adt: s.control_flow, variant: 1, kind: CtorKind::Tuple };
    let cont = Def::Ctor { adt: s.control_flow, variant: 0, kind: CtorKind::Tuple };
    assert_eq!(defs, std::vec![(PatId(3), brk), (PatId(5), cont), (PatId(7), brk), (PatId(9), cont)]);
    let ok = Def::Ctor { adt: s.result, variant: 0, kind: CtorKind::Tuple };
    let edefs: Vec<(ExprId, Def)> =
        c.expr_defs.column::<0>().iter().copied().zip(c.expr_defs.column::<1>().iter().copied()).collect();
    assert!(edefs.contains(&(ExprId(21), ok)), "{edefs:?}");
}

/// ```text
/// fn bad(io: Io) -> u32 { io.need_out()?; 0 }
/// ```
/// `u32: FromResidual<Result<Infallible, Missing>>` has no impl: E0277 at the `from_residual`
/// path whose predicate it is (rustc reports it at the `?`; the range is the desugaring's).
#[test]
fn question_mark_in_a_fn_returning_no_try() {
    let (fx, s) = rung_7c();
    let mut b = Bb::new();
    let io = b.param("io");
    let e0 = b.local(io);
    let e1 = b.method(e0, "need_out", &[]);
    let e9 = b.try_op(e1);
    let s0 = b.semi(e9);
    let e10 = b.int(None);
    let root = b.block(&[s0], Some(e10));
    let c = check_body(&fx, &b, s.bad, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "Io"),
                (ExprId(1), "Result<Out, Missing>"),
                (ExprId(2), "fn branch<Result<Out, Missing>>"),
                (ExprId(3), "ControlFlow<Result<Infallible, Missing>, Out>"),
                (ExprId(4), "fn from_residual<u32, Result<Infallible, Missing>>"),
                (ExprId(5), "Result<Infallible, Missing>"),
                (ExprId(6), "u32"),
                (ExprId(7), "!"),
                (ExprId(8), "Out"),
                (ExprId(9), "Out"),
                (ExprId(10), "u32"),
                (ExprId(11), "u32"),
            ],
            pats: &[(PatId(0), "Io")],
            diags: &[(DiagCode::Unsatisfied, ExprId(4))],
        },
    );
    // The finding names the goal entire: `u32: FromResidual<Result<Infallible, Missing>>` (no
    // impl of `FromResidual` for `u32`, so no leaf under it).
    let d = c.diags[0];
    let want = "u32: FromResidual<Result<Infallible, Missing>>";
    assert_eq!(d.because.map(|b| bound_name(&fx, c.types.view(), b)), Some(String::from(want)), "{d:?}");
    let shown = |t: Option<crate::portable::PTy>| t.map(|t| super::show(&fx, c.types.view(), t));
    assert_eq!(shown(d.found), Some("u32".into()));
}

/// An unmet goal printed: `Self: Trait<A, ..>`, or `<Self as Trait<..>>::name == Term`.
fn goal_shown(fx: &crate::fixture::Fx, types: crate::portable::PTys<'_>, g: crate::diag::UnmetGoal) -> String {
    use crate::diag::UnmetGoal;
    let (UnmetGoal::Trait { trait_, args } | UnmetGoal::Projection { trait_, args, .. }) = g;
    let all: Vec<String> = types.list(args).expect("in the table").iter().map(|&t| super::show(fx, types, t)).collect();
    let (self_, rest) = all.split_first().map_or(("?", &[][..]), |(s, r)| (s.as_str(), r));
    let tr = if rest.is_empty() { String::from(fx.name_of(trait_)) } else { format!("{}<{}>", fx.name_of(trait_), rest.join(", ")) };
    match g {
        UnmetGoal::Trait { .. } => format!("{self_}: {tr}"),
        UnmetGoal::Projection { name, term, .. } => format!("<{self_} as {tr}>::{name:?} == {}", super::show(fx, types, term)),
    }
}

/// A finding's reason printed: the unmet goal, and the leaf under it after "because".
fn bound_name(fx: &crate::fixture::Fx, types: crate::portable::PTys<'_>, b: crate::diag::Because) -> String {
    match b {
        crate::diag::Because::Unmet { goal, leaf: None } => goal_shown(fx, types, goal),
        crate::diag::Because::Unmet { goal, leaf: Some(l) } => format!("{} because {}", goal_shown(fx, types, goal), goal_shown(fx, types, l)),
        other => format!("{other:?}"),
    }
}

/// `fn <owner>(io: Io) -> R { io.<first>()?; io.<tail>() }` on rung 7c: ids as in the cases
/// above (the `?` is expressions 2 to 9, its residual binding pattern 1, its value pattern 3).
fn one_try(first: &str, tail: &str) -> (Bb, ExprId) {
    let mut b = Bb::new();
    let io = b.param("io");
    let e0 = b.local(io);
    let e1 = b.method(e0, first, &[]);
    let e9 = b.try_op(e1);
    let s0 = b.semi(e9);
    let e10 = b.local(io);
    let e11 = b.method(e10, tail, &[]);
    let root = b.block(&[s0], Some(e11));
    assert_eq!(root, ExprId(12));
    (b, root)
}

/// The Reference, "The question mark operator" (expr.try): "If applied to `Err(e)` it will
/// return `Err(From::from(e))` from the enclosing function or closure", through core's
/// `impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F>`
/// (library/core/src/result.rs) with `F = E` met by `impl<T> From<T> for T`. The recorded
/// `from_residual` has every argument known, and those arguments select that impl.
/// ```text
/// fn same(io: Io) -> Result<u8, Error> { io.same()?; io.same() }
/// ```
#[test]
fn question_mark_on_result_with_the_same_error_type() {
    let (fx, s) = rung_7c();
    let (b, root) = one_try("same", "same");
    let c = check_body(&fx, &b, s.same, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "Io"),
                (ExprId(1), "Result<u8, Error>"),
                (ExprId(2), "fn branch<Result<u8, Error>>"),
                (ExprId(3), "ControlFlow<Result<Infallible, Error>, u8>"),
                (ExprId(4), "fn from_residual<Result<u8, Error>, Result<Infallible, Error>>"),
                (ExprId(5), "Result<Infallible, Error>"),
                (ExprId(6), "Result<u8, Error>"),
                (ExprId(7), "!"),
                (ExprId(8), "u8"),
                (ExprId(9), "u8"),
                (ExprId(10), "Io"),
                (ExprId(11), "Result<u8, Error>"),
                (ExprId(12), "Result<u8, Error>"),
            ],
            pats: &[(PatId(0), "Io"), (PatId(1), "Result<Infallible, Error>"), (PatId(3), "u8")],
            diags: &[],
        },
    );
    assert_eq!(callee(&fx, &c, ExprId(6)), "from_residual<Result<u8, Error>, Result<Infallible, Error>>");
    assert_eq!(selected_impl(&fx, s.from_residual_tr, c), Some(s.result_from_residual));
}

/// The impl of `tr` the recorded `from_residual` call (expression 6) selects, at its ground
/// arguments (`Self`, `R`).
fn selected_impl(fx: &crate::fixture::Fx, tr: crate::ids::ItemKey, mut c: crate::Checked<'static>) -> Option<crate::ids::ItemKey> {
    let i = c.callees.column::<0>().iter().position(|&x| x == ExprId(6)).expect("a callee at 6");
    let args: Vec<crate::portable::PTy> = c.types.view().list(c.callees.column::<2>()[i]).expect("the callee's arguments in the body's table").to_vec();
    crate::instance::select_impl(fx, tr, &args, &mut c.types, blaze_own::Alloc::global()).expect("selects").map(|(imp, _)| imp)
}

/// The Reference, expr.try: "If applied to `None`, it returns `None` from the enclosing
/// function or closure", through core's `impl<T> FromResidual<Option<Infallible>> for
/// Option<T>` (library/core/src/option.rs).
/// ```text
/// fn opt(io: Io) -> Option<u8> { io.maybe()?; io.maybe() }
/// ```
#[test]
fn question_mark_on_option() {
    let (fx, s) = rung_7c();
    let (b, root) = one_try("maybe", "maybe");
    let c = check_body(&fx, &b, s.opt, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "Io"),
                (ExprId(1), "Option<u8>"),
                (ExprId(2), "fn branch<Option<u8>>"),
                (ExprId(3), "ControlFlow<Option<Infallible>, u8>"),
                (ExprId(4), "fn from_residual<Option<u8>, Option<Infallible>>"),
                (ExprId(5), "Option<Infallible>"),
                (ExprId(6), "Option<u8>"),
                (ExprId(7), "!"),
                (ExprId(8), "u8"),
                (ExprId(9), "u8"),
                (ExprId(10), "Io"),
                (ExprId(11), "Option<u8>"),
                (ExprId(12), "Option<u8>"),
            ],
            pats: &[(PatId(0), "Io"), (PatId(1), "Option<Infallible>"), (PatId(3), "u8")],
            diags: &[],
        },
    );
    assert_eq!(selected_impl(&fx, s.from_residual_tr, c), Some(s.option_from_residual));
}

/// The Reference, expr.try: `Err(e)` returns `Err(From::from(e))`, so the error types must
/// convert; with no `impl From<Other> for Error` the bound is unmet: E0277 naming the trait and
/// both types (rustc: "`?` couldn't convert the error to `Error`"). Before `Because::Bound`
/// this E0277 carried no trait and no type.
/// ```text
/// fn same(io: Io) -> Result<u8, Error> { io.other()?; io.same() }
/// ```
#[test]
fn question_mark_with_no_from_impl_is_e0277() {
    let (fx, s) = rung_7c();
    let (b, root) = one_try("other", "same");
    let c = check_body(&fx, &b, s.same, root, Plant::None, InferPlant::None);
    let got: Vec<String> = c
        .diags
        .iter()
        .map(|d| {
            let shown = |t: Option<crate::portable::PTy>| t.map(|t| super::show(&fx, c.types.view(), t));
            format!("{} {:?} {:?}", d.code.code(), d.because.map(|b| bound_name(&fx, c.types.view(), b)), shown(d.found))
        })
        .collect();
    // The goal entire, and the leaf a person can fix: the impl's where-clause `Error:
    // From<Other>` (the Reference, "Implementations": an impl applies only where its
    // where-clauses hold; blaze names the leaf, typeck.md "The whole failed goal").
    let want = "E0277 Some(\"Result<u8, Error>: FromResidual<Result<Infallible, Other>> because Error: From<Other>\") Some(\"Result<u8, Error>\")";
    assert_eq!(got, [want], "{:?}", c.diags);
}

/// The Reference, "Trait and lifetime bounds" and "Associated types": the bound `T: Tr<Out =
/// u8>` holds only where `T`'s implementation of `Tr` defines `Out` as `u8`; `B`'s defines it
/// as `u16`, so the call's where-clause `<B as Tr>::Out == u8` cannot hold. The finding names
/// that projection entire (self type, trait, name, term) at the path whose where-clause it is
/// (as `missing_impl_reported`), and no trait goal: `B: Tr` holds. The impl decides `Out`, so it
/// is E0271 ("type mismatch resolving `<B as Tr>::Out == u8`", rustc's code), naming `u16` found
/// and `u8` expected (machine.rs `projection_found`); E0277 is left for a projection whose trait
/// goal fails.
/// ```text
/// trait Tr { type Out; }   struct B;   impl Tr for B { type Out = u16; }
/// fn wants<T: Tr<Out = u8>>(t: T);
/// fn main(b: B) { wants(b); }
/// ```
#[test]
fn a_projection_that_cannot_hold_names_the_projection_and_its_term() {
    use crate::diag::{Because, UnmetGoal};
    use crate::ids::{LangItem, Name};
    let mut fx = crate::fixture::Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let tr = fx.tr("Tr", 1, Vec::new(), &["Out"]);
    let b_k = fx.strukt("B", 0, &[]);
    let (b_ty, u8_, u16_, unit, p0) = (fx.adt(b_k, &[]), fx.u8(), fx.u16(), fx.unit(), fx.param(0));
    fx.imp("tr_b", 0, b_ty, Some((tr, &[])), &[("Out", u16_)], Vec::new());
    let (bound, proj) = (fx.bound(tr, &[p0]), fx.proj(tr, &[p0], "Out", u8_));
    fx.func("wants", 1, &[p0], unit, std::vec![bound, proj]);
    let main = fx.func("main", 0, &[b_ty], unit, Vec::new());
    fx.finish();
    let mut b = Bb::new();
    let p = b.param("b");
    let e0 = b.path(&["wants"]);
    let e1 = b.local(p);
    let e2 = b.call(e0, &[e1]);
    let s = b.semi(e2);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    let types = c.types.view();
    let got: Vec<String> = c
        .diags
        .iter()
        .map(|d| {
            let goal = match d.because {
                Some(Because::Unmet { goal: g @ UnmetGoal::Projection { trait_, name, .. }, leaf: None }) if trait_ == tr && name == Name::of("Out") => {
                    goal_shown(&fx, types, g)
                }
                other => format!("not the projection: {other:?}"),
            };
            format!("{} at {} {goal}", d.code.code(), d.at.start)
        })
        .collect();
    // At `b` of `wants(b)`, the argument for the input that names the clause's `T`.
    let want = format!("E0271 at {} <B as Tr>::{:?} == u8", e1.0, Name::of("Out"));
    assert_eq!(got, [want], "{:?}", c.diags);
    let shown = |t: Option<crate::portable::PTy>| t.map(|t| super::show(&fx, types, t));
    assert_eq!((shown(c.diags[0].expected), shown(c.diags[0].found)), (Some("u8".into()), Some("u16".into())), "{:?}", c.diags);
}

/// A lang item the world cannot answer is the one finding naming it (typeck.md, "`?` on the
/// ladder"): with `ControlFlow::Continue` unanswered, `x?` reports `MissingLangItem` naming
/// `Continue`, never an E0425 without a name.
#[test]
fn question_mark_without_continue_names_the_lang_item() {
    use crate::diag::Because;
    use crate::ids::LangItem;
    let (fx, s) = crate::fixture::rung_7c_without(Some(LangItem::ControlFlowContinue));
    let (b, root) = one_try("same", "same");
    let c = check_body(&fx, &b, s.same, root, Plant::None, InferPlant::None);
    let got: Vec<(DiagCode, Option<Because>)> = c.diags.iter().map(|d| (d.code, d.because)).collect();
    assert_eq!(got, [(DiagCode::MissingLangItem, Some(Because::NoLangItem(LangItem::ControlFlowContinue)))]);
}

/// bstd's `#[derive(Clone)] enum Source { Process(&'static [&'static str]), Given(Arc<[String]>) }`
/// (blaze-std args.rs): the derived `clone` clones a `&[&str]` and an `Arc<[String]>`, whose
/// impls core and alloc write over `T: PointeeSized` (the pinned core, clone.rs: `impl<T:
/// PointeeSized> Clone for &T`). The Reference, "Clone": "`Clone` ... implemented for shared
/// references `&T`" whatever `T` (`[T]: Clone` is not required for `&[T]: Clone`); RFC 3729
/// (the sized hierarchy): `PointeeSized` is the bound every type meets, the absence of any
/// sizedness bound. Here: `impl<T: PointeeSized> Clone for &T`, `impl<T: PointeeSized> Clone
/// for Shared<T>`; `x.clone()` with `x: &&[u8]` and `s.clone()` with `s: &Shared<[u8]>` are
/// `&[u8]` and `Shared<[u8]>`, no diagnostic.
#[test]
fn a_pointee_sized_bound_on_an_unsized_argument_holds() {
    use crate::fixture::Fx;
    use crate::ids::{LangItem, Mutbl};
    use crate::portable::PNode;
    let mut fx = Fx::new();
    let pointee = fx.tr("PointeeSized", 1, Vec::new(), &[]);
    fx.lang(LangItem::PointeeSized, pointee);
    let clone = fx.tr("Clone", 1, Vec::new(), &[]);
    fx.lang(LangItem::Clone, clone);
    fx.use_trait(clone);
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    fx.trait_fn(clone, "clone", true, &[r_self], p0);
    let shared = fx.strukt("Shared", 1, &[]);
    let r_t = fx.r(Mutbl::Not, p0);
    let pb = fx.bound(pointee, &[p0]);
    fx.imp("clone_ref", 1, r_t, Some((clone, &[])), &[], std::vec![pb]);
    let shared_t = fx.adt(shared, &[p0]);
    let pb = fx.bound(pointee, &[p0]);
    fx.imp("clone_shared", 1, shared_t, Some((clone, &[])), &[], std::vec![pb]);
    let u8_ = fx.u8();
    let slice = fx.ty(PNode::Slice(u8_));
    let r_slice = fx.r(Mutbl::Not, slice);
    let rr_slice = fx.r(Mutbl::Not, r_slice);
    let shared_slice = fx.adt(shared, &[slice]);
    let r_shared = fx.r(Mutbl::Not, shared_slice);
    let unit = fx.unit();
    let main = fx.func("main", 0, &[rr_slice, r_shared], unit, Vec::new());
    fx.finish();
    let mut b = Bb::new();
    let (x, s) = (b.param("x"), b.param("s"));
    let e0 = b.local(x);
    let e1 = b.method(e0, "clone", &[]);
    let s0 = b.semi(e1);
    let e2 = b.local(s);
    let e3 = b.method(e2, "clone", &[]);
    let s1 = b.semi(e3);
    let root = b.block(&[s0, s1], None);
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    let want = Want {
        exprs: &[(e0, "&&[u8]"), (e1, "&[u8]"), (e2, "&Shared<[u8]>"), (e3, "Shared<[u8]>"), (root, "()")],
        pats: &[(x, "&&[u8]"), (s, "&Shared<[u8]>")],
        diags: &[],
    };
    expect(&fx, &b, &c, &want);
}
