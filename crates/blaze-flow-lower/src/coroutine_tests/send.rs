//! The whole path of an auto trait on an async block's future (async plan P13 over P2): the
//! type checker files the goal that failed on its superset interior as a stalled row
//! (`blaze_typeck::check_deferring`), the block is lowered as its own flow ([`crate::lower_async`]), its
//! exact interior computed ([`interior`]), and the row decided on it
//! (`blaze_typeck::decide_stalled`).
//!
//! The rule: the Reference, "Auto traits": an async block's future is `Send` when every value
//! its state holds is; "Async blocks": the state is "roughly equivalent to an enum with one
//! variant per await point, where each variant stores the data needed to resume from its
//! corresponding point". So `async { let r = Rc::new(1); f().await; drop(r) }` is not `Send`
//! (`r` is held across the `.await`), and with `drop(r)` before the `.await` it is.

use std::vec;

use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{
    Capture, Checked, Def, DiagCode, ExprId, IntTy, InferCtx, ItemKey, LangItem, Mutbl, Needs, Outcome, PNode,
    check_deferring, decide_stalled,
};

use super::{AsyncFacts, declare, live_across, moved_before};
use crate::region_fixture::{Decls, WithRegions};
use crate::{Region, interior};

/// A temporary made in a statement after the `.await` (the `format!` class).
mod after_await;

/// The parent's world (`Future`, `IntoFuture`, `Ready`, `Rc<T>`, `make_rc`, `drop_rc`) with
/// `#[lang = "send"] auto trait Send {}`, `Rc<T>` not `Send` (the library's alloc/src/rc.rs:
/// `impl<T: ?Sized, A: Allocator> !Send for Rc<T, A> {}`, written as an impl whose
/// where-clause never holds: the fixture has no negative impls, and an impl that may apply
/// disqualifies the auto impl), and `fn spawn<F: Send>(_: F)`.
fn send_world() -> (Fx, ItemKey, ItemKey, Decls) {
    let mut fx = Fx::new();
    let (p0, unit) = (fx.param(0), fx.unit());
    let i32_ = fx.ty(PNode::Int(IntTy::I32));
    let copy = fx.tr("Copy", 1, vec::Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    let future = fx.tr("Future", 1, vec::Vec::new(), &["Output"]);
    fx.lang(LangItem::Future, future);
    let into_future = fx.tr("IntoFuture", 1, vec::Vec::new(), &["Output", "IntoFuture"]);
    fx.lang(LangItem::IntoFuture, into_future);
    let held = fx.alias(into_future, "IntoFuture", &[p0]);
    let f = fx.trait_fn(into_future, "into_future", true, &[p0], held);
    fx.lang_def(LangItem::IntoFutureIntoFuture, Def::Fn(f));
    let fut_out = fx.alias(future, "Output", &[p0]);
    let is_future = fx.bound(future, &[p0]);
    fx.imp(
        "impl<F: Future> IntoFuture for F",
        1,
        p0,
        Some((into_future, &[])),
        &[("Output", fut_out), ("IntoFuture", p0)],
        vec![is_future],
    );
    let send = fx.tr("Send", 1, vec::Vec::new(), &[]);
    fx.auto(send);
    fx.lang(LangItem::Send, send);
    let never = fx.tr("Never", 1, vec::Vec::new(), &[]);
    let ready = fx.strukt("Ready", 0, &[]);
    let ready_t = fx.adt(ready, &[]);
    fx.imp("impl Future for Ready", 0, ready_t, Some((future, &[])), &[("Output", unit)], vec::Vec::new());
    let rc = fx.strukt("Rc", 1, &[]);
    let rc_p0 = fx.adt(rc, &[p0]);
    let rc_never = fx.bound(never, &[rc_p0]);
    fx.imp("impl<T> Send for Rc<T> where Rc<T>: Never", 1, rc_p0, Some((send, &[])), &[], vec![rc_never]);
    let rc_i32 = fx.adt(rc, &[i32_]);
    let ready_f = fx.func("ready", 0, &[], ready_t, vec::Vec::new());
    let make_rc = fx.func("make_rc", 0, &[], rc_i32, vec::Vec::new());
    let drop_rc = fx.func("drop_rc", 0, &[rc_i32], unit, vec::Vec::new());
    let r_rc = fx.r(Mutbl::Not, rc_i32);
    let take = fx.func("take", 0, &[r_rc, unit], unit, vec::Vec::new());
    let is_send = fx.bound(send, &[p0]);
    let spawn = fx.func("spawn", 1, &[p0], unit, vec![is_send]);
    let owner = fx.func("owner", 0, &[], unit, vec::Vec::new());
    fx.finish();
    // The heads as written above: `take(_: &Rc<i32>, _: ())` one elided input lifetime,
    // `Param(0)`, no output borrowing; the others none (`F: Send` names no lifetime).
    let mut decls = Decls::new();
    declare(&mut decls, ready_f, &[], ready_t, &[]);
    declare(&mut decls, make_rc, &[], rc_i32, &[]);
    declare(&mut decls, drop_rc, &[rc_i32], unit, &[]);
    declare(&mut decls, take, &[r_rc, unit], unit, &[Region::Param(0)]);
    declare(&mut decls, spawn, &[p0], unit, &[]);
    declare(&mut decls, owner, &[], unit, &[]);
    (fx, owner, ready, decls)
}

/// `fn owner() { spawn(async { inner }); }` through the whole path: the stalled rows' count
/// and the findings `decide_stalled` gives on the exact interior.
fn send_findings(inner: fn(&mut Bb) -> ExprId) -> (usize, vec::Vec<DiagCode>) {
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let (fx, owner, _, decls) = send_world();
    let mut b = Bb::new();
    let e0 = b.path(&["spawn"]);
    let body = inner(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let call = b.call(e0, &[block]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let view = b.view(owner, root);
    let mut cx = InferCtx::new_in(alloc);
    let mut c: Checked<'static> = match check_deferring(&view, &fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check_deferring refused: {e:?}"),
    };
    assert!(c.diags.is_empty(), "nothing is decided before the flow: {:?}", c.diags);
    // The heads laid in the record's own table, as the lowering reads them.
    let regions = decls.lay(fx.t.view(), &mut c.types);
    let facts = WithRegions { base: &AsyncFacts, regions: &regions };
    let lowered = match crate::oracle::lower_async_checked(&view, &c, block, &facts, alloc) {
        Ok(l) => l,
        Err(r) => panic!("lower_async refused: {r:?}"),
    };
    let mut out = c.types.fork_in(alloc).expect("fork the record's table");
    let exact = match interior(&lowered, &c, block, &mut out, alloc) {
        Ok(t) => t,
        Err(r) => panic!("interior refused: {r:?}"),
    };
    let got = match decide_stalled(&view, &c, out.view(), &[(block, exact)], &fx, alloc) {
        Ok(d) => d,
        Err(e) => panic!("decide_stalled refused: {e:?}"),
    };
    for d in got.iter() {
        assert!(c.stalled.iter().any(|s| s.diag == *d), "each finding is a stalled row's own: {d:?}");
        assert!(d.at.end > d.at.start, "E0277 has a place: {d:?}");
    }
    (c.stalled.len(), got.iter().map(|d| d.code).collect())
}

/// `async { let r = make_rc(); ready().await; drop_rc(r) }` given to `spawn<F: Send>`: `r` is
/// live across the `.await`, so `Rc<i32>` is in the exact interior and the future is not
/// `Send`: one E0277, placed at the stalled goal's site.
/// Prints "rc held across the await: E0277".
#[test]
fn rc_held_across_await_is_not_send() {
    let (stalled, got) = send_findings(live_across);
    assert_eq!(stalled, 1, "the superset holds `Rc<i32>`: the goal stalls");
    assert_eq!(got, vec![DiagCode::Unsatisfied]);
    std::println!("rc held across the await: E0277");
}

/// `async { let r = make_rc(); drop_rc(r); ready().await }`: `r` is moved out before the
/// `.await`, so the exact interior holds no `Rc` and the future is `Send`: no finding.
/// Without the exact interior the check gave the gap "an auto trait (`Send`, `Sync`) of an async
/// block's future that fails only on a value not known to be live across an `.await`".
/// Prints "rc dropped before the await: Send".
#[test]
fn rc_dropped_before_await_is_send() {
    let (stalled, got) = send_findings(moved_before);
    assert_eq!(stalled, 1, "the superset holds `Rc<i32>`: the goal stalls");
    assert!(got.is_empty(), "no finding on the exact interior: {got:?}");
    std::println!("rc dropped before the await: Send");
}

/// `async { take(&make_rc(), ready().await) }` given to `spawn<F: Send>`: the `Rc<i32>`
/// temporary is made before the `.await` (arguments are evaluated left to right, the Reference,
/// "Call expressions") and lives to the end of the statement ("Temporary scopes"), so the state
/// holds it across the `.await` and the future is not `Send`: one E0277. Main's ruling of
/// 2026-11-16: a pre-await temporary is a flow local; a flow that did not name it would answer
/// `Send` here, wrongly.
/// Prints "a temporary held across the await: E0277".
#[test]
fn a_temporary_held_across_an_await_is_not_send() {
    let (stalled, got) = send_findings(super::state_recipe::temp_across);
    assert_eq!(stalled, 1, "the superset holds `Rc<i32>`: the goal stalls");
    assert_eq!(got, vec![DiagCode::Unsatisfied]);
    std::println!("a temporary held across the await: E0277");
}
