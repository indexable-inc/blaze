//! Async blocks, async closures and `.await` (typeck.md, "Async: decided by lane 2"), each case
//! with the rule of the Reference it checks ("Async blocks", "Await expressions", "Closure
//! expressions", "Auto traits"). The world is rung 7c's ([`crate::fixture::rung_7c`]) plus:
//!
//! ```text
//! #[lang = "future_trait"] trait Future { #[lang = "future_output"] type Output; }
//! #[lang = "into_future"] trait IntoFuture {
//!     type Output; type IntoFuture;
//!     #[lang = "into_future"] fn into_future(self) -> Self::IntoFuture;
//! }
//! impl<F: Future> IntoFuture for F { type Output = <F as Future>::Output; type IntoFuture = F; }
//! #[lang = "send"] auto trait Send {}
//! trait Never {}
//! struct Rc {}  impl Send for Rc where Rc: Never {}  // stands for `impl !Send for Rc {}`
//! struct Ready {}  impl Future for Ready { type Output = u8; }
//! fn ready() -> Ready;  fn make_rc() -> Rc;  fn use_rc(r: Rc);
//! fn spawn<F: Send>(f: F);
//! fn run<F: Future<Output = Result<(), Error>>>(f: F) -> Result<(), Error>;
//! ```
//!
//! The toy world has no negative impls, so `Rc`'s `!Send` is an impl whose where-clause never
//! holds: an impl that may apply disqualifies the auto impl (`assemble_auto`), and that impl
//! fails, which is what `impl !Send` gives. `ready()` stands for an `async fn ready() -> u8`:
//! its future is named here, as the signature's opaque `impl Future<Output = u8>` waits on
//! return-position `impl Trait` in the front.
//!
//! Planted bug, caught by `control_output_from_first_statement`: `Plant::AsyncOutputFromFirst`
//! (the future's output taken from the block's first statement instead of its tail).

use std::vec::Vec;

use super::{Want, check_body, expect};
use crate::body::{Capture, IntSuffix};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, rung_7c};
use crate::ids::{ExprId, ItemKey, LangItem, PatId, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::Def;

struct Items {
    /// `fn main(cx: &Cx, io: Io) -> Result<(), Error>` of rung 7c.
    main: ItemKey,
    /// `fn c_async() -> u8`: the owner of the bodies made `async fn` (`Bb::async_fn`).
    c_async: ItemKey,
    /// `fn c_sync()`: an owner that is not async.
    c_sync: ItemKey,
    /// `fn c_gen<T: Send>(t: T)`.
    c_gen: ItemKey,
    /// `fn c_gen_unbound<T>(t: T)`.
    c_gen_unbound: ItemKey,
}

fn world() -> (Fx, Items) {
    let (mut world, s) = rung_7c();
    let fx = &mut world;
    let error = fx.key_of("Error");
    let (p0, unit, u8_) = (fx.param(0), fx.unit(), fx.u8());

    // An async block's capture modes ask `Copy` of what the block takes by value, as a
    // closure's do (rules/async_fn.rs, rules/closure.rs `moved_out`).
    let copy = fx.tr("Copy", 1, Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    fx.imp("Copy for u8", 0, u8_, Some((copy, &[])), &[], Vec::new());
    let future = fx.tr("Future", 1, Vec::new(), &["Output"]);
    fx.lang(LangItem::Future, future);
    let into_future = fx.tr("IntoFuture", 1, Vec::new(), &["Output", "IntoFuture"]);
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
        std::vec![is_future],
    );

    let send = fx.tr("Send", 1, Vec::new(), &[]);
    fx.auto(send);
    fx.lang(LangItem::Send, send);
    let never = fx.tr("Never", 1, Vec::new(), &[]);
    let rc = fx.strukt("Rc", 0, &[]);
    let rc_t = fx.adt(rc, &[]);
    let rc_never = fx.bound(never, &[rc_t]);
    fx.imp("impl Send for Rc where Rc: Never", 0, rc_t, Some((send, &[])), &[], std::vec![rc_never]);
    let ready = fx.strukt("Ready", 0, &[]);
    let ready_t = fx.adt(ready, &[]);
    fx.imp("impl Future for Ready", 0, ready_t, Some((future, &[])), &[("Output", u8_)], Vec::new());

    fx.func("ready", 0, &[], ready_t, Vec::new());
    fx.func("make_rc", 0, &[], rc_t, Vec::new());
    fx.func("use_rc", 0, &[rc_t], unit, Vec::new());
    let is_send = fx.bound(send, &[p0]);
    fx.func("spawn", 1, &[p0], unit, std::vec![is_send]);
    // `trait Job {}  impl<T: Send> Job for T {}  fn job<F: Job>(f: F);`: a goal that is no
    // auto trait and holds exactly when its type is `Send` (axum's `Handler` for an async fn).
    let job = fx.tr("Job", 1, Vec::new(), &[]);
    fx.imp("impl<T: Send> Job for T", 1, p0, Some((job, &[])), &[], std::vec![is_send]);
    let is_job = fx.bound(job, &[p0]);
    fx.func("job", 1, &[p0], unit, std::vec![is_job]);
    let error_t = fx.adt(error, &[]);
    let res_unit_error = fx.adt(s.result, &[unit, error_t]);
    let output_is = fx.proj(future, &[p0], "Output", res_unit_error);
    fx.func("run", 1, &[p0], res_unit_error, std::vec![is_future, output_is]);

    let c_async = fx.func("c_async", 0, &[], u8_, Vec::new());
    let c_sync = fx.func("c_sync", 0, &[], unit, Vec::new());
    let c_gen = fx.func("c_gen", 1, &[p0], unit, std::vec![is_send]);
    let c_gen_unbound = fx.func("c_gen_unbound", 1, &[p0], unit, Vec::new());
    fx.finish();
    (world, Items { main: s.main, c_async, c_sync, c_gen, c_gen_unbound })
}

/// `async fn c_async() -> u32 { let x: u32 = async { 0; 1u32 }.await; x }`, its `0` a
/// `u8` by its suffix. "Async blocks": the block's type implements `Future<Output = T>` with T
/// the type of its body, the tail; "Await expressions": `e.await` is
/// `<E as IntoFuture>::Output`, here through the blanket impl `<F as Future>::Output`, which
/// normalizes to the block's output. So `x` is `u32` with no finding.
fn case_flow(plant: Plant) -> (Fx, Bb, crate::columns::Checked<'static>) {
    let (fx, it) = world();
    let mut b = Bb::new();
    let out = b.ty(PNode::Uint(UintTy::U32));
    b.async_fn(out);
    let e0 = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let s0 = b.semi(e0);
    let e1 = b.int(Some(IntSuffix::Uint(UintTy::U32)));
    let e2 = b.block(&[s0], Some(e1));
    let e3 = b.async_block(e2, Capture::Ref);
    let e4 = b.await_(e3);
    let x = b.bind("x");
    let s1 = b.let_(x, Some(out), e4);
    let e5 = b.local(x);
    let root = b.block(&[s1], Some(e5));
    let c = check_body(&fx, &b, it.c_async, root, plant, InferPlant::None);
    (fx, b, c)
}

#[test]
fn output_type_flows_through_await() {
    let (fx, b, c) = case_flow(Plant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "u8"),
                (ExprId(1), "u32"),
                (ExprId(2), "u32"),
                (ExprId(3), "future#3<u32; ()>"),
                (ExprId(4), "u32"),
                (ExprId(5), "u32"),
                (ExprId(6), "u32"),
            ],
            pats: &[(PatId(0), "u32")],
            diags: &[],
        },
    );
}

/// The control: with the output taken from the first statement (`u8`) instead of the tail, the
/// `.await` is `u8` and the `let x: u32` is a mismatch the correct rule never reports.
#[test]
fn control_output_from_first_statement() {
    let (_, _, c) = case_flow(Plant::AsyncOutputFromFirst);
    assert!(!c.diags.is_empty(), "the planted bug went unseen");
}

/// `async fn c_async() -> u8 { 1u8.await }`: "Await expressions": the operand must implement
/// `IntoFuture`; `u8` does not (the blanket impl needs `u8: Future`), so E0277 on the `.await`.
#[test]
fn awaiting_a_non_future_is_a_finding() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let out = b.ty(PNode::Uint(UintTy::U8));
    b.async_fn(out);
    let e0 = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let e1 = b.await_(e0);
    let root = b.block(&[], Some(e1));
    let c = check_body(&fx, &b, it.c_async, root, Plant::None, InferPlant::None);
    assert!(c.diags.iter().any(|d| d.code == DiagCode::Unsatisfied), "no E0277: {:?}", c.diags);
}

/// `fn c_sync() { ready().await; }`: "Await expressions": "can only be used inside an async
/// context" (E0728), on the `.await`.
#[test]
fn await_outside_async_is_a_finding() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let e0 = b.path(&["ready"]);
    let e1 = b.call(e0, &[]);
    let e2 = b.await_(e1);
    let s0 = b.semi(e2);
    let root = b.block(&[s0], None);
    let c = check_body(&fx, &b, it.c_sync, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "fn ready"), (ExprId(1), "Ready"), (ExprId(2), "{error}"), (ExprId(3), "()")],
            pats: &[],
            diags: &[(DiagCode::AwaitOutsideAsync, ExprId(2))],
        },
    );
}

/// `async fn c_async() -> u8 { let v = ready().await; async { v }.await }`: an async fn
/// awaiting another's future (`ready()`, `async fn ready() -> u8` by its named future) and then
/// an async block that captures `v` by reference: both `.await`s are `u8`.
#[test]
fn async_fn_awaits_another() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let out = b.ty(PNode::Uint(UintTy::U8));
    b.async_fn(out);
    let e0 = b.path(&["ready"]);
    let e1 = b.call(e0, &[]);
    let e2 = b.await_(e1);
    let v = b.bind("v");
    let s0 = b.let_(v, None, e2);
    let e3 = b.local(v);
    let e4 = b.block(&[], Some(e3));
    let e5 = b.async_block(e4, Capture::Ref);
    let e6 = b.await_(e5);
    let root = b.block(&[s0], Some(e6));
    let c = check_body(&fx, &b, it.c_async, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "fn ready"),
                (ExprId(1), "Ready"),
                (ExprId(2), "u8"),
                (ExprId(3), "u8"),
                (ExprId(4), "u8"),
                // Polling copies the u8 out of the shared reference held by the future.
                (ExprId(5), "future#5<u8; (&u8,)>"),
                (ExprId(6), "u8"),
                (ExprId(7), "u8"),
            ],
            pats: &[(PatId(0), "u8")],
            diags: &[],
        },
    );
}

/// `fn main(cx: &Cx, io: Io) -> Result<(), Error> { run(async { io.need_out()?; Ok(()) }) }`:
/// "Async blocks": "`?` and `return` ... affect the output of the future, not the enclosing
/// function". The `?`'s `return` coerces to the block's output `Result<(), ?E>`; `run`'s
/// `F: Future<Output = Result<(), Error>>` fixes `?E = Error`, and then `Error: From<Missing>`
/// holds. The fn's own output (also `Result<(), Error>`) is reached only through `run`.
#[test]
fn question_mark_targets_the_async_block() {
    let (fx, it) = world();
    let mut b = Bb::new();
    b.param("cx");
    let io = b.param("io");
    let e0 = b.path(&["run"]);
    let e1 = b.local(io);
    let e2 = b.method(e1, "need_out", &[]);
    let e3 = b.try_op(e2);
    let s0 = b.semi(e3);
    let e4 = b.path(&["Ok"]);
    let e5 = b.tuple(&[]);
    let e6 = b.call(e4, &[e5]);
    let body = b.block(&[s0], Some(e6));
    let fut = b.async_block(body, Capture::Ref);
    let e7 = b.call(e0, &[fut]);
    let root = b.block(&[], Some(e7));
    let c = check_body(&fx, &b, it.main, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let shown = crate::fixture::show(&fx, c.types.view(), c.expr_ty[fut.0 as usize]);
    assert!(shown.starts_with("future#") && shown.contains("<Result<(), Error>;"), "{shown}");
}

/// `async fn c_async() -> u8 { let f = async |r| r.await; f(ready()).await }`: "Closure
/// expressions": an async closure's call returns a future of its body; `r` is `Ready` from the
/// call, `r.await` is `u8`, and so is the call's future's `.await`.
#[test]
fn async_closure_called_and_awaited() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let out = b.ty(PNode::Uint(UintTy::U8));
    b.async_fn(out);
    let r = b.bind("r");
    let e0 = b.local(r);
    let e1 = b.await_(e0);
    let e2 = b.async_closure(&[r], e1);
    let f = b.bind("f");
    let s0 = b.let_(f, None, e2);
    let e3 = b.local(f);
    let e4 = b.path(&["ready"]);
    let e5 = b.call(e4, &[]);
    let e6 = b.call(e3, &[e5]);
    let e7 = b.await_(e6);
    let root = b.block(&[s0], Some(e7));
    let c = check_body(&fx, &b, it.c_async, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let ty = |e: ExprId| crate::fixture::show(&fx, c.types.view(), c.expr_ty[e.0 as usize]);
    assert_eq!(ty(e0), "Ready");
    assert_eq!(ty(e1), "u8");
    assert_eq!(ty(e7), "u8");
    assert_eq!(ty(root), "u8");
}

/// One `spawn(async { .. })` in `c_sync`, the async block's body built by `inner`.
fn spawn_case(owner: fn(&Items) -> ItemKey, inner: fn(&mut Bb) -> ExprId, capture: Capture) -> Vec<DiagCode> {
    let (fx, it) = world();
    let mut b = Bb::new();
    if owner(&it) != it.c_sync {
        b.param("t");
    }
    let e0 = b.path(&["spawn"]);
    let body = inner(&mut b);
    let fut = b.async_block(body, capture);
    let call = b.call(e0, &[fut]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, owner(&it), root, Plant::None, InferPlant::None);
    c.diags.iter().map(|d| d.code).collect()
}

/// `let r = make_rc(); ready().await; use_rc(r)`.
fn rc_across_await(b: &mut Bb) -> ExprId {
    let e0 = b.path(&["make_rc"]);
    let e1 = b.call(e0, &[]);
    let r = b.bind("r");
    let s0 = b.let_(r, None, e1);
    let e2 = b.path(&["ready"]);
    let e3 = b.call(e2, &[]);
    let e4 = b.await_(e3);
    let s1 = b.semi(e4);
    let e5 = b.path(&["use_rc"]);
    let e6 = b.local(r);
    let e7 = b.call(e5, &[e6]);
    b.block(&[s0, s1], Some(e7))
}

/// `use_rc(make_rc()); ready().await;`: the `Rc` is gone before the `.await`.
fn rc_before_await(b: &mut Bb) -> ExprId {
    let e0 = b.path(&["use_rc"]);
    let e1 = b.path(&["make_rc"]);
    let e2 = b.call(e1, &[]);
    let e3 = b.call(e0, &[e2]);
    let s0 = b.semi(e3);
    let e4 = b.path(&["ready"]);
    let e5 = b.call(e4, &[]);
    let e6 = b.await_(e5);
    let s1 = b.semi(e6);
    b.block(&[s0, s1], None)
}

/// `ready().await;`: no `Rc` at all.
fn no_rc(b: &mut Bb) -> ExprId {
    let e0 = b.path(&["ready"]);
    let e1 = b.call(e0, &[]);
    let e2 = b.await_(e1);
    let s0 = b.semi(e2);
    b.block(&[s0], None)
}

/// As [`spawn_case`], with the check's own refusal kept: a typed gap is the `Err`.
fn spawn_outcome(owner: fn(&Items) -> ItemKey, inner: fn(&mut Bb) -> ExprId, capture: Capture) -> Result<Vec<DiagCode>, crate::error::CheckError> {
    call_outcome("spawn", owner, inner, capture)
}

/// `callee(async { .. })` in `owner`, the async block's body built by `inner`, with the
/// check's own refusal kept.
fn call_outcome(callee: &str, owner: fn(&Items) -> ItemKey, inner: fn(&mut Bb) -> ExprId, capture: Capture) -> Result<Vec<DiagCode>, crate::error::CheckError> {
    let (fx, it) = world();
    let mut b = Bb::new();
    if owner(&it) != it.c_sync {
        b.param("t");
    }
    let e0 = b.path(&[callee]);
    let body = inner(&mut b);
    let fut = b.async_block(body, capture);
    let call = b.call(e0, &[fut]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let mut cx = crate::infer::InferCtx::new_in(blaze_own::Alloc::global());
    let needs = crate::world::Needs::new_in(blaze_own::Alloc::global());
    match crate::check_with(&b.view(owner(&it), root), &fx, &mut cx, &needs, Plant::None, blaze_own::Alloc::global()) {
        Ok(crate::Outcome::Done(c)) => Ok(c.diags.iter().map(|d| d.code).collect()),
        Ok(crate::Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => Err(e),
    }
}

/// Whether `got` is the gap of an auto trait that failed only on the superset interior.
fn is_interior_gap(got: &Result<Vec<DiagCode>, crate::error::CheckError>) -> bool {
    matches!(got, Err(crate::error::CheckError::FutureWitness { reason, at }) if *reason == crate::INTERIOR_GAP && at.start < at.end)
}

/// "Auto traits": an async block's future is `Send` when every value its state holds is, and
/// an `Rc` live across an `.await` is one: the Reference refuses this `spawn`. The checker
/// cannot tell it from the next case until liveness across `.await` is computed, so it gives
/// neither answer: the typed gap (rules/async_fn.rs, 2). When liveness lands this expectation
/// becomes `Ok([DiagCode::Unsatisfied])`.
///
/// Expected line: `test suite::coroutine::rc_held_across_await_is_a_gap ... ok`.
#[test]
fn rc_held_across_await_is_a_gap() {
    let got = spawn_outcome(|it| it.c_sync, rc_across_await, Capture::Ref);
    assert!(is_interior_gap(&got), "{got:?}");
}

/// The Reference ACCEPTS this one (the `Rc` is dropped before the `.await`, so it is not in
/// the future's state). The superset interior holds every type the body names, so the goal
/// fails on it; that failure is not an answer: the typed gap, never E0277 (this test was
/// `rc_dropped_before_await_is_conservatively_refused` and pinned a false E0277). When
/// liveness lands this expectation becomes `Ok([])`.
///
/// Expected line: `test suite::coroutine::rc_dropped_before_await_is_a_gap_not_e0277 ... ok`.
#[test]
fn rc_dropped_before_await_is_a_gap_not_e0277() {
    let got = spawn_outcome(|it| it.c_sync, rc_before_await, Capture::Ref);
    assert!(is_interior_gap(&got), "{got:?}");
}

/// tempo crates/zones/rpc/src/server.rs:63, reduced: `post(handle_rpc)` asks axum's
/// `Handler`, no auto trait, whose impl for an async fn holds exactly when the fn's future is
/// `Send`. Here `job(async { use_rc(make_rc()); ready().await; })` asks `Job`, whose one impl
/// `impl<T: Send> Job for T` fails only by `Send` of the future, which failed on the superset
/// interior alone: the same gap as `spawn`'s, never E0277 (the checker gave E0277: the
/// classification looked at the goal `Job` only, never at the clause under it).
///
/// Expected line: `test suite::coroutine::send_under_a_blanket_impl_is_a_gap_not_e0277 ... ok`.
#[test]
fn send_under_a_blanket_impl_is_a_gap_not_e0277() {
    let got = call_outcome("job", |it| it.c_sync, rc_before_await, Capture::Ref);
    assert!(is_interior_gap(&got), "{got:?}");
}

/// The negative twin: `fn c_gen_unbound<T>(t: T) { job(async move { t }); }`: the clause
/// under `Job` is `Send` of a future whose initial capture `T` already fails it (no
/// `T: Send`), so the goal's failure is decided: E0277, as `spawn`'s is.
#[test]
fn send_under_a_blanket_impl_failing_by_capture_is_refused() {
    assert_eq!(call_outcome("job", |it| it.c_gen_unbound, move_t, Capture::Move), Ok(std::vec![DiagCode::Unsatisfied]));
}

/// The control: with `T: Send` the clause holds and `job` accepts the future.
#[test]
fn send_under_a_blanket_impl_holds_by_the_bound() {
    assert_eq!(call_outcome("job", |it| it.c_gen, move_t, Capture::Move), Ok(std::vec![]));
}

/// The control for both: with no `Rc`, the interior is `Ready`, `()` and `u8`, all `Send` by
/// the auto impl, so `spawn` accepts the future.
#[test]
fn future_without_rc_is_send() {
    assert_eq!(spawn_case(|it| it.c_sync, no_rc, Capture::Ref), std::vec![]);
}

/// `async move { t }` captures `t: T`.
fn move_t(b: &mut Bb) -> ExprId {
    let e = b.local(PatId(0));
    b.block(&[], Some(e))
}

/// `fn c_gen<T: Send>(t: T) { spawn(async move { t }); }`: the interior is `T`, `Send` by the
/// where-clause in the param env, so the future is `Send`.
#[test]
fn generic_async_block_is_send_by_the_bound() {
    assert_eq!(spawn_case(|it| it.c_gen, move_t, Capture::Move), std::vec![]);
}

/// The control: `fn c_gen_unbound<T>(t: T)`: nothing says `T: Send` (a parameter has no auto
/// impl), so the future is not, E0277.
#[test]
fn generic_async_block_without_the_bound_is_refused() {
    assert_eq!(spawn_case(|it| it.c_gen_unbound, move_t, Capture::Move), std::vec![DiagCode::Unsatisfied]);
}

/// The capture rows of `fn <owner>(t: T) { <block>; }`, and the block's expression (no
/// `spawn`: the rows are asked, not the future's `Send`).
fn capture_rows(owner: fn(&Items) -> ItemKey, inner: fn(&mut Bb) -> ExprId, capture: Capture) -> (ExprId, Vec<(ExprId, PatId, crate::columns::CaptureKind)>) {
    let (fx, it) = world();
    let mut b = Bb::new();
    b.param("t");
    let body = inner(&mut b);
    let fut = b.async_block(body, capture);
    let s = b.semi(fut);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, owner(&it), root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let rows = (0..c.captures.len()).map(|i| (c.captures.column::<0>()[i], c.captures.column::<1>()[i], c.captures.column::<2>()[i])).collect();
    (fut, rows)
}

/// `fn c_gen<T: Send>(t: T) { async move { t }; }`. The Reference, "Async blocks",
/// "Capture modes": "Async blocks capture variables from their environment using the same
/// capture modes as closures." "`async move { .. }` blocks however will move all referenced
/// variables into the resulting future." So the record holds one capture row: the block, `t`,
/// by value: what the code builder makes the future's first field from. Without
/// `analyze_coroutine_captures` there is no row (the closure walk took only closures).
///
/// Expected line: `test suite::coroutine::async_move_block_captures_by_value ... ok`.
#[test]
fn async_move_block_captures_by_value() {
    let (fut, rows) = capture_rows(|it| it.c_gen, move_t, Capture::Move);
    assert_eq!(rows, std::vec![(fut, PatId(0), crate::columns::CaptureKind::ByValue)]);
}

/// `{ async move { t }; }`: `t` named only by an inner `async move` block.
fn inner_move_t(b: &mut Bb) -> ExprId {
    let e0 = b.local(PatId(0));
    let e1 = b.block(&[], Some(e0));
    let e2 = b.async_block(e1, Capture::Move);
    let s0 = b.semi(e2);
    b.block(&[s0], None)
}

/// `fn c_gen<T: Send>(t: T) { async { async move { t }; }; }`: "Capture modes": "when
/// written `async { .. }` the capture mode for each variable will be inferred from the content
/// of the block", and the content uses `t` by value (the inner `async move` moves it in), so
/// the OUTER block captures `t` by value too: `T` is not `Copy`. Two rows: the inner block's
/// and the outer's, both by value. With the closure walk alone the outer row is `ByRef` (the
/// walk reads a nested body's names as borrowed), and the built future would move `t` out of
/// a reference.
///
/// Expected line: `test suite::coroutine::outer_block_holds_by_value_what_an_inner_move_takes ... ok`.
#[test]
fn outer_block_holds_by_value_what_an_inner_move_takes() {
    let (outer, rows) = capture_rows(|it| it.c_gen, inner_move_t, Capture::Ref);
    let by_value = crate::columns::CaptureKind::ByValue;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|r| r.1 == PatId(0) && r.2 == by_value), "{rows:?}");
    assert!(rows.iter().any(|r| r.0 == outer), "{rows:?}");
}

/// `async fn c_async() -> u8 { let x: u16 = ready().await; 0u8 }`, `ready()`'s future having
/// `Output = u8`. The Reference, "Await expressions": "the value contained in the
/// `Poll::Ready` variant is used as the result of the await expression itself", so the
/// `.await` is a `u8` and a `u16` binding of it is refused. The one finding is E0271 on the
/// goal `<Ready as IntoFuture>::Output == u16`, naming `u16` expected and `u8` found (the
/// annotation reaches the `.await` as its expected type, and the output is a projection the
/// blanket impl decides: machine.rs `projection_found`); a body whose `.await` output is taken
/// as written (no goal on `Output`) has no finding and fails this. rustc words it E0308 (its
/// desugaring normalizes the output first); the types named are the same.
///
/// Expected line: `test suite::coroutine::await_output_mismatch_is_refused ... ok`.
#[test]
fn await_output_mismatch_is_refused() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let out = b.ty(PNode::Uint(UintTy::U8));
    b.async_fn(out);
    let e0 = b.path(&["ready"]);
    let e1 = b.call(e0, &[]);
    let e2 = b.await_(e1);
    let x = b.bind("x");
    let u16_ = b.ty(PNode::Uint(UintTy::U16));
    let s0 = b.let_(x, Some(u16_), e2);
    let e3 = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let root = b.block(&[s0], Some(e3));
    let c = check_body(&fx, &b, it.c_async, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert_eq!(codes, std::vec![DiagCode::ProjectionMismatch], "{:?}", c.diags);
    let d = &c.diags[0];
    assert_eq!(d.code.code(), "E0271");
    assert_eq!(crate::fixture::show(&fx, c.types.view(), d.expected.expect("the annotation")), "u16");
    assert_eq!(crate::fixture::show(&fx, c.types.view(), d.found.expect("the future's output")), "u8");
}

/// Auto-trait goals decided on the exact interior (src/stalled.rs).
mod stalled;
/// The solver's why-chain on an unmet future's `Send` finding (`Diag::why`).
mod why;
