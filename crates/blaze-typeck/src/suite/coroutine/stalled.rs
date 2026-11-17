//! Auto-trait goals of an async block's future decided on its exact interior (src/stalled.rs;
//! async plan P13). The world and bodies are the parent module's (`spawn<F: Send>`, `Rc` not
//! `Send`, `Ready` a future). The exact interiors here are written by hand, as blaze-flow-lower's
//! `interior` gives them (its tests prove those: coroutine_tests.rs and coroutine_tests/send.rs,
//! which runs the whole path).
//!
//! The rule: the Reference, "Auto traits": an async block's future is `Send` when every value
//! its state holds is, and the state is "roughly equivalent to an enum with one variant per await
//! point, where each variant stores the data needed to resume from its corresponding point"
//! ("Async blocks"): its captures and the values live across an `.await`.

use std::vec::Vec;

use super::{Items, rc_across_await, rc_before_await, world};
use crate::body::Capture;
use crate::diag::{Diag, DiagCode};
use crate::fixture::{Bb, Fx};
use crate::ids::ExprId;
use crate::infer::InferCtx;
use crate::portable::{PNode, PTy, PortableTys};
use crate::stalled::{StalledError, check_deferring, decide_stalled};
use crate::world::Needs;

/// `fn c_sync() { spawn(async { inner }); }` checked deferring: the fixture, the body, the
/// block and the record.
struct Case {
    fx: Fx,
    b: Bb,
    owner: crate::ids::ItemKey,
    root: ExprId,
    block: ExprId,
    c: crate::columns::Checked<'static>,
}

fn case(inner: fn(&mut Bb) -> ExprId) -> Case {
    let alloc = blaze_own::testing::heap().alloc();
    let (fx, it): (Fx, Items) = world();
    let mut b = Bb::new();
    let e0 = b.path(&["spawn"]);
    let body = inner(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let call = b.call(e0, &[block]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let mut cx = InferCtx::new_in(alloc);
    let needs = Needs::new_in(alloc);
    let c = match check_deferring(&b.view(it.c_sync, root), &fx, &mut cx, &needs, alloc) {
        Ok(crate::Outcome::Done(c)) => c,
        Ok(crate::Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check_deferring refused: {e:?}"),
    };
    Case { fx, b, owner: it.c_sync, root, block, c }
}

/// The exact interior `(T0, T1, ..)` of `k`'s block in a fork of the record's table: `rc` adds
/// `Rc`, and the awaited `Ready` is always held (blaze-flow-lower coroutine_tests.rs
/// `a_local_live_across_the_await_is_in_the_interior`: "the live Rc<i32> and the awaited Ready").
fn exact(k: &Case, rc: bool) -> (PortableTys<'static>, PTy) {
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = k.c.types.fork_in(alloc).expect("fork the record's table");
    let none = out.intern_list(&[]).expect("list");
    let ready = out.intern(PNode::Adt(k.fx.key_of("Ready"), none)).expect("Ready");
    let mut elems: Vec<PTy> = Vec::new();
    if rc {
        elems.push(out.intern(PNode::Adt(k.fx.key_of("Rc"), none)).expect("Rc"));
    }
    elems.push(ready);
    let list = out.intern_list(&elems).expect("list");
    let t = out.intern(PNode::Tuple(list)).expect("tuple");
    (out, t)
}

fn decide(k: &Case, out: &PortableTys<'_>, rows: &[(ExprId, PTy)]) -> Result<Vec<Diag>, StalledError> {
    let alloc = blaze_own::testing::heap().alloc();
    let got = decide_stalled(&k.b.view(k.owner, k.root), &k.c, out.view(), rows, &k.fx, alloc)?;
    Ok(got.iter().copied().collect())
}

/// `async { let r = make_rc(); ready().await; use_rc(r) }` passed to `spawn<F: Send>`: `r` is
/// live across the `.await`, so `Rc` is in the exact interior and the future is not `Send`:
/// E0277, at the place the check gives the unmet goal (the stalled row's own finding).
///
/// Expected line: `test suite::coroutine::stalled::rc_held_across_await_is_not_send ... ok`.
#[test]
fn rc_held_across_await_is_not_send() {
    let k = case(rc_across_await);
    assert!(k.c.diags.is_empty(), "nothing is decided before the flow: {:?}", k.c.diags);
    assert_eq!(k.c.stalled.len(), 1, "one stalled goal: the future's `Send`");
    let (out, t) = exact(&k, true);
    let got = decide(&k, &out, &[(k.block, t)]).expect("decided");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].code, DiagCode::Unsatisfied);
    assert_eq!(got[0], k.c.stalled[0].diag, "the finding is placed where the check placed it");
    assert!(got[0].at.end > got[0].at.start, "E0277 has a place");
}

/// `async { use_rc(make_rc()); ready().await; }`: the `Rc` is gone before the `.await`, so the
/// exact interior holds only the awaited `Ready` and the Reference accepts the `spawn`.
/// Without `decide_stalled` reading the exact interior this was the gap "an auto trait (`Send`,
/// `Sync`) of an async block's future that fails only on a value not known to be live across an
/// `.await`".
///
/// Expected line: `test suite::coroutine::stalled::rc_dropped_before_await_is_send ... ok`.
#[test]
fn rc_dropped_before_await_is_send() {
    let k = case(rc_before_await);
    assert_eq!(k.c.stalled.len(), 1, "the superset holds `Rc`: the goal stalls");
    let (out, t) = exact(&k, false);
    let got = decide(&k, &out, &[(k.block, t)]).expect("decided");
    assert!(got.is_empty(), "no finding on the exact interior: {got:?}");
}

/// Control: deciding on the SUPERSET interior (the row's own future's) instead of the exact one
/// refuses the accepted body: the decision reads the interior it is given. And a future with no
/// exact interior passed is refused by name, never decided.
///
/// Expected line: `test suite::coroutine::stalled::control_superset_refuses_and_missing_is_named ... ok`.
#[test]
fn control_superset_refuses_and_missing_is_named() {
    let k = case(rc_before_await);
    let row = k.c.stalled[0];
    let superset = match k.c.types.view().node(row.ty).expect("node") {
        PNode::Future(e, _, i) => (e, i),
        other => panic!("the stalled type is the future, got {other:?}"),
    };
    assert_eq!(superset.0, k.block);
    let alloc = blaze_own::testing::heap().alloc();
    let out = k.c.types.fork_in(alloc).expect("fork");
    let got = decide(&k, &out, &[superset]).expect("decided");
    assert_eq!(got.len(), 1, "the superset holds `Rc`: {got:?}");
    assert_eq!(decide(&k, &out, &[]), Err(StalledError::NoExactInterior { block: k.block }));
}

/// The entries that do not defer keep the typed gap: no caller that never decides a stalled
/// goal gets a pass (the parent module's `rc_held_across_await_is_a_gap` is the same claim
/// through `check_with`).
///
/// Expected line: `test suite::coroutine::stalled::check_keeps_the_gap ... ok`.
#[test]
fn check_keeps_the_gap() {
    let alloc = blaze_own::testing::heap().alloc();
    let (fx, it) = world();
    let mut b = Bb::new();
    let e0 = b.path(&["spawn"]);
    let body = rc_before_await(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let call = b.call(e0, &[block]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let mut cx = InferCtx::new_in(alloc);
    let got = crate::check(&b.view(it.c_sync, root), &fx, &mut cx, &Needs::new_in(alloc), alloc);
    assert!(
        matches!(got, Err(crate::error::CheckError::Unported(what)) if what == crate::rules::async_fn::INTERIOR_GAP),
        "{:?}",
        got.as_ref().err()
    );
}
