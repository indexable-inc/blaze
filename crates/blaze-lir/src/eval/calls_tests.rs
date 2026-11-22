//! The evaluator's calls ([`super::calls`]): a direct call is run in the caller's run, a run
//! deeper than its bound is refused by name, and `run_in` still follows no call.

use std::alloc::Global;

use super::{Bodies, BodyId, EvalError, Outcome, Val, run_calls_in, run_in};
use crate::{
    View,
    builder::{Builder, Full, Param, Variadic},
    flags::{Flags, IntCond},
    func::{BlockKind, Conv, Func, RefId, RefKind, Value},
    op::Op,
    ty::Ty,
};

/// Far past what the runs below take: none of them may end by fuel.
const FUEL: u64 = 100_000;
const F: BodyId = BodyId([1; 32]);
const G: BodyId = BodyId([2; 32]);
const H: BodyId = BodyId([3; 32]);

/// `f(a, b) = g(a, b) + 1`, its call's instruction, and `g(x, y) = x - y`, all `i32`.
fn f_and_g() -> (Func<Global>, Value, Func<Global>) {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32), Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let g = b.reference(RefKind::Func, 0);
    assert_eq!(g, RefId(0), "the first reference is row 0");
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::I32), b.param(Ty::I32));
    let call = b.call(Flags::NONE, g, sig, &[x, y]);
    let one = b.iconst(Ty::I32, 1);
    let sum = b.binary(Op::Add, Flags::NONE, call, one);
    b.ret(&[sum]);
    let f = b.finish_checked().expect("f builds");

    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32), Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::I32), b.param(Ty::I32));
    let difference = b.binary(Op::Sub, Flags::NONE, x, y);
    b.ret(&[difference]);
    let g = b.finish_checked().expect("g builds");
    (f, call, g)
}

/// `h(n) = if n == 0 { 0 } else { h(n - 1) }`, `i32`, and its call's instruction: a run of
/// `h(n)` holds `n + 1` frames at its deepest.
fn h() -> (Func<Global>, Value) {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let itself = b.reference(RefKind::Func, 0);
    let [entry, done, deeper] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
    b.begin(entry);
    let n = b.param(Ty::I32);
    let zero = b.iconst(Ty::I32, 0);
    let is_zero = b.icmp(IntCond::Eq, n, zero);
    b.br(Flags::NONE, is_zero, (done, &[]), (deeper, &[]));
    b.begin(done);
    b.ret(&[zero]);
    b.begin(deeper);
    let one = b.iconst(Ty::I32, 1);
    let less = b.binary(Op::Sub, Flags::NONE, n, one);
    let call = b.call(Flags::NONE, itself, sig, &[less]);
    b.ret(&[call]);
    (b.finish_checked().expect("h builds"), call)
}

/// `f` calls `g` by its reference 0; nothing else is answered.
struct FG {
    g: Func<Global>,
}

impl Bodies for FG {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        match (from, reference) {
            (F, RefId(0)) => Some((G, self.g.view())),
            _ => None,
        }
    }
}

/// `h` calls itself by its reference 0.
struct SelfCall {
    h: Func<Global>,
}

impl Bodies for SelfCall {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        match (from, reference) {
            (H, RefId(0)) => Some((H, self.h.view())),
            _ => None,
        }
    }
}

/// `f(7, 3) = g(7, 3) + 1 = (7 - 3) + 1 = 5`: the callee's value comes back to the call and the
/// caller goes on after it.
/// Expected: `test eval::calls_tests::a_call_runs_its_callee_and_returns_its_value ... ok`.
#[test]
fn a_call_runs_its_callee_and_returns_its_value() {
    let (f, _, g) = f_and_g();
    let bodies = FG { g };
    let outcome = run_calls_in(f.view(), F, &[Val::Int(7), Val::Int(3)], FUEL, &bodies, Global)
        .expect("the evaluator runs the call");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(5)]), "f(7, 3) = (7 - 3) + 1");
    // The wrapped difference crosses the call too: f(3, 7) = (3 - 7) + 1 = -3 as i32.
    let outcome = run_calls_in(f.view(), F, &[Val::Int(3), Val::Int(7)], FUEL, &bodies, Global)
        .expect("the evaluator runs the call");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(u128::from((-3i32) as u32))]));
}

/// A run holds at most 64 frames: `h(63)` (64 frames) returns 0, `h(64)` (65) is refused at
/// the call that would open the 65th frame.
/// Expected: `test eval::calls_tests::a_call_past_depth_64_is_refused ... ok`.
#[test]
fn a_call_past_depth_64_is_refused() {
    let (h, call) = h();
    let bodies = SelfCall { h };
    let deepest = run_calls_in(bodies.h.view(), H, &[Val::Int(63)], FUEL, &bodies, Global)
        .expect("64 frames are within the bound");
    assert_eq!(deepest, Outcome::<Global>::Returned(vec![Val::Int(0)]));
    let past = run_calls_in(bodies.h.view(), H, &[Val::Int(64)], FUEL, &bodies, Global);
    assert_eq!(past.err(), Some(EvalError::Depth { inst: call.index() }), "the 65th frame is refused");
}

/// `run_in` follows no call: on `f` it stops at the call, unsupported, as before calls ran.
/// Expected: `test eval::calls_tests::run_in_still_refuses_a_call ... ok`.
#[test]
fn run_in_still_refuses_a_call() {
    let (f, call, _) = f_and_g();
    let refused = run_in(f.view(), &[Val::Int(7), Val::Int(3)], FUEL, Global);
    assert!(
        matches!(refused, Err(EvalError::Unsupported { inst, op: Op::Call, .. }) if inst == call.index()),
        "run_in refuses f's call, got {refused:?}",
    );
}
