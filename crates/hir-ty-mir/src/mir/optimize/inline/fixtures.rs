//! The inliner over fixtures, through the optimized-body queries a build asks: what it
//! inlines at each level, what it refuses, how it ends recursion, where unwinding out of an
//! inlined body goes, where `#[track_caller]` reports, and the list of inlined callees. An
//! integration test (`tests/mir/main.rs` names this file), so the test database's queries are
//! of the one copy of hir-ty-mir the tests call.

use hir_def::FunctionId;
use hir_ty::{InferBodyId, db::HirDatabase};
use hir_ty_mir::mir::{
    CallerLocation, InlinedBody, MirBody, MirOptLevel, MirOptions, MirSpan, TerminatorKind,
    caller_location,
};
use hir_expand::fixture;

use crate::{db::TestDB, function};

fn options(level: MirOptLevel) -> MirOptions {
    MirOptions {
        level,
        ub_checks: false,
        lifetime_markers: false,
        inline: level == MirOptLevel::Release,
    }
}

/// `check` of the optimized body of the function `name` at `level`.
#[track_caller]
fn optimized<T>(
    fixture: &str,
    name: &str,
    level: MirOptLevel,
    check: impl FnOnce(&TestDB, &MirBody<'_>) -> T,
) -> T {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let func = function(&db, name);
        let body = db
            .optimized_mir_body(func.into(), options(level))
            .unwrap_or_else(|error| panic!("the MIR of `{name}`: {error:?}"));
        check(&db, body)
    })
}

fn calls(body: &MirBody<'_>) -> usize {
    body.basic_blocks
        .iter()
        .filter(|(_, it)| {
            matches!(it.terminator.as_ref().map(|it| &it.kind), Some(TerminatorKind::Call { .. }))
        })
        .count()
}

fn inlined(body: &MirBody<'_>) -> Vec<FunctionId> {
    body.inlined
        .callees
        .iter()
        .map(|it| match it.body {
            InlinedBody::Fn(func) => func,
            InlinedBody::Closure(_) => panic!("a closure is inlined: {:?}", body.inlined.callees),
        })
        .collect()
}

/// `inlined` names each of `expected`, which are distinct, once and nothing else.
#[track_caller]
fn assert_once(inlined: &[FunctionId], expected: &[FunctionId]) {
    let once = expected.iter().all(|it| inlined.iter().filter(|&listed| listed == it).count() == 1);
    assert!(once && inlined.len() == expected.len(), "{inlined:?}, not {expected:?}");
}

const SMALL: &str = r#"
//- minicore: sized, copy
#[inline]
fn add(a: u32, b: u32) -> u32 { a ^ b }
pub fn caller(x: u32) -> u32 { add(x, 1) }
"#;

#[test]
fn a_small_inline_callee_is_inlined_at_level_2_and_not_1() {
    optimized(SMALL, "caller", MirOptLevel::Release, |db, body| {
        assert_eq!(calls(body), 0, "{body:?}");
        assert_eq!(inlined(body), [function(db, "add")]);
    });
    optimized(SMALL, "caller", MirOptLevel::Debug, |_, body| {
        assert_eq!(calls(body), 1, "{body:?}");
        assert!(body.inlined.callees.is_empty());
    });
}

#[test]
fn inline_never_is_not_inlined() {
    let fixture = SMALL.replace("#[inline]", "#[inline(never)]");
    optimized(&fixture, "caller", MirOptLevel::Release, |_, body| {
        assert_eq!(calls(body), 1, "{body:?}");
        assert!(body.inlined.callees.is_empty());
    });
}

/// `<W<Base> as Grow>::Next` normalizes to a type with a bigger alias in it, without end: only
/// once `deep` is instantiated, as its own body leaves `<T as Grow>::Next` rigid.
const GROWING: &str = r#"
//- minicore: sized, copy, option
pub struct W<T>(T);
pub struct Base;
pub trait Grow { type Next; }
impl Grow for Base { type Next = (); }
impl<T: Grow> Grow for W<T> { type Next = W<<W<W<T>> as Grow>::Next>; }
#[inline]
fn deep<T: Grow>() -> u32 { let _p: Option<<T as Grow>::Next> = None; 1 }
pub fn caller() -> u32 { deep::<W<Base>>() }
"#;

#[test]
fn a_callee_whose_alias_grows_past_the_recursion_limit_is_not_inlined() {
    optimized(GROWING, "caller", MirOptLevel::Release, |_, body| {
        assert_eq!(calls(body), 1, "{body:?}");
        assert!(body.inlined.callees.is_empty());
    });
}

/// libm's `sqrt_round<F>` calling `goldschmidt::<F::ISet1>`: the callee's argument names the
/// caller's own parameter, both index 0, which instantiating is not to take for the callee's.
const CALLER_PARAMS: &str = r#"
//- minicore: sized, copy, option
pub trait Base { type D; }
pub trait Helper { type Set: Base; }
#[inline]
fn wide<T: Base>(x: T) -> T { let _d: Option<<T as Base>::D> = None; x }
pub fn caller<F: Helper>(x: <F as Helper>::Set) -> <F as Helper>::Set {
    wide::<<F as Helper>::Set>(x)
}
"#;

#[test]
fn a_callee_whose_arguments_name_the_caller_s_parameters_is_inlined() {
    optimized(CALLER_PARAMS, "caller", MirOptLevel::Release, |db, body| {
        assert_eq!(calls(body), 0, "{body:?}");
        assert_eq!(inlined(body), [function(db, "wide")]);
    });
}

const PAIR: &str = r#"
//- minicore: sized, copy
#[inline]
fn even(n: u32) -> bool { if n == 0 { true } else { odd(n ^ 1) } }
#[inline]
fn odd(n: u32) -> bool { if n == 0 { false } else { even(n ^ 1) } }
pub fn caller(n: u32) -> bool { even(n) }
"#;

#[test]
fn a_recursive_pair_stops() {
    optimized(PAIR, "even", MirOptLevel::Release, |db, body| {
        assert_eq!(calls(body), 1, "`odd` may call `even` back: {body:?}");
        assert!(inlined(body).is_empty());
        let odd = db.optimized_mir_body(function(db, "odd").into(), options(MirOptLevel::Release));
        assert!(odd.is_ok_and(|it| calls(it) == 1));
    });
    optimized(PAIR, "caller", MirOptLevel::Release, |db, body| {
        assert_eq!(calls(body), 1, "{body:?}");
        let pair = [function(db, "even"), function(db, "odd")];
        assert_once(&inlined(body), &pair);
    });
}

const UNWINDING: &str = r#"
//- minicore: sized, copy, drop
struct Guard;
impl Drop for Guard { fn drop(&mut self) {} }
#[inline(never)]
fn may_panic() {}
#[inline]
fn callee(x: u32) -> u32 { may_panic(); x }
pub fn caller(x: u32) -> u32 { let _guard = Guard; callee(x) }
"#;

#[test]
fn unwinding_out_of_an_inlined_callee_runs_the_caller_s_cleanup() {
    optimized(UNWINDING, "caller", MirOptLevel::Release, |db, body| {
        assert_eq!(inlined(body), [function(db, "callee")]);
        let cleanups: Vec<_> = body
            .basic_blocks
            .iter()
            .filter_map(|(_, it)| match it.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Call { cleanup, .. }) => Some(*cleanup),
                _ => None,
            })
            .collect();
        let [Some(mut at)] = cleanups[..] else {
            panic!("one call of `may_panic`, unwinding into a cleanup block: {body:?}")
        };
        loop {
            let block = &body.basic_blocks[at];
            assert!(block.is_cleanup, "{body:?}");
            match block.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Goto { target }) => at = *target,
                Some(TerminatorKind::Drop { .. }) => break,
                other => panic!("the guard is not dropped when `may_panic` unwinds: {other:?}"),
            }
        }
    });
}

const TRACKED: &str = r#"
//- minicore: sized, copy
#[inline]
#[track_caller]
fn tracked(x: u32) -> u32 { x ^ 1 }
#[inline]
fn untracked(x: u32) -> u32 { x ^ 2 }
pub fn caller(x: u32) -> u32 { tracked(x) ^ untracked(x) }
"#;

#[test]
fn track_caller_keeps_the_location_of_the_call() {
    optimized(TRACKED, "caller", MirOptLevel::Release, |db, body| {
        let (caller, tracked, untracked) =
            (function(db, "caller"), function(db, "tracked"), function(db, "untracked"));
        let (mut seen_tracked, mut seen_untracked) = (false, false);
        for (at, span) in body.inlined.spans.iter().enumerate() {
            let scope = body.inlined.scopes[span.scope as usize];
            let InlinedBody::Fn(callee) = body.inlined.callees[scope.callee as usize].body else {
                panic!("a closure is inlined");
            };
            let location = caller_location(db, body, MirSpan::Inlined(at as u32), false);
            if callee == tracked {
                seen_tracked = true;
                let call = CallerLocation::At(InferBodyId::from(caller), scope.call_site);
                assert_eq!(location, call);
            } else {
                assert_eq!(callee, untracked);
                seen_untracked = true;
                assert_eq!(location, CallerLocation::At(InferBodyId::from(untracked), span.span));
            }
        }
        assert!(seen_tracked && seen_untracked, "{:?}", body.inlined);
    });
}

const NESTED: &str = r#"
//- minicore: sized, copy
#[inline]
fn add(a: u32, b: u32) -> u32 { a ^ b }
#[inline]
fn outer(x: u32) -> u32 { add(x, 1) }
pub fn caller(x: u32) -> u32 { add(outer(x), add(x, 2)) }
"#;

#[test]
fn the_inlined_list_names_each_callee_once() {
    optimized(NESTED, "caller", MirOptLevel::Release, |db, body| {
        assert_eq!(calls(body), 0, "{body:?}");
        assert_once(&inlined(body), &[function(db, "add"), function(db, "outer")]);
    });
}
