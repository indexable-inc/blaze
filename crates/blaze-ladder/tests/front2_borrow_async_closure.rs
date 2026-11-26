//! An async closure in a future's body, by cause, each with its twin (rule 23). Every ORACLE
//! case is accepted by rustc; every TWIN refused by it.
//!
//! Rust's rule: the Reference, "Closure expressions", "Async closures" (stable since 1.85, RFC
//! 3668): an `async` closure is a closure VALUE, its captures taken as a closure's; calling it
//! returns a future of its body. "Auto traits": a future is `Send` when everything it stores
//! across a suspension is, so an async closure value held across an `.await` is one more stored
//! value, `Send` when its captures are.
//!
//! The cause (blaze-flow-lower lib.rs, the body walk): `Expr::Closure { is_async: true }` was
//! refused `Form::AsyncClosure` in EVERY lowering, the witness lowering of an async block's
//! interior too (front2 exact_interior.rs `lower_async_witness`). The block's interior was then
//! `BlockInterior::Refused`, filed a superset, and a caller's `Send` goal on the future stopped
//! at EXTERNAL_WITNESS_GAP (tempo bin/tempo/src/tempo_cmd.rs `execute`, through `Info::run`'s
//! `self.run_with(async |rpc_url: &str| ..)` and `ClaimFees::run`'s `self.resolve_call(async ||
//! ..)`). The fix: the witness lowering lays an async closure as a closure value
//! (`closure_value`); the borrow lowering still refuses it by name (no lowering of its body
//! gives its requirements yet).
//!
//! 1. Held across an `.await` (the witness):
//!    - `execute`: `require_send(hold())`, the closure captures nothing `!Send`: clean.
//!    - TWIN `execute_rc`: the closure captures an `Rc` and is held across the `.await`: rustc
//!      E0277; blaze TODAY the named gap of the closure's call future (`CapturesUnknown`, the
//!      test's comment at the twins says why), never clean.
//! 2. Passed to an `AsyncFnOnce` callee (tempo's `Info::run` shape):
//!    - `execute_info`: clean.
//!    - TWIN `execute_info_rc`: the closure captures an `Rc`: rustc E0277; blaze today the same
//!      named gap (by reading, not yet run: the closure's kind is `Fn` here too).
//! 3. CONTROL (rule 23): the borrow lowering, where the cause is NOT removed: `hold`'s borrows are
//!    still refused `AsyncClosure` by name, never checked clean as if the body needed nothing.
//!
//! 4. The FUTURE the closure's call returns holds the BODY's locals across its own `.await`s,
//!    which the closure value's captures do not show; the fix lays the VALUE only, and that
//!    future's interior stays undecided until the closure's body is lowered as a coroutine of
//!    its own. By reading:
//!    - blaze-typeck rules/closure.rs (capture settling): an async closure whose body suspends
//!      and owns its environment gets `CoroutineWitness::NeedsSavedLocals` (a borrowing one
//!      stays `NeedsCapturePlaces`); `Complete` only for a body with no suspension.
//!    - In its own body, a `Send` goal on `Future(closure, _, interior)` not `Complete` is
//!      `Witness::LocalFlow` (rules/async_fn.rs): stalled, then decided by front2
//!      stalled_goals.rs, whose `witness_all` lowers ASYNC BLOCKS only. In a fn with no async
//!      block: `FutureWitness` "no lowered async block supplied its captures and saved
//!      locals"; else blaze-typeck stalled.rs `LocalWitnesses::interior` finds no flow tuple for
//!      the closure: `NoExactInterior`, the `INTERIOR_GAP` at the closure's range.
//!    - In another body (through an opaque): opaque_auto.rs `file_row` files the closure's
//!      coroutine (ordinal `CLOSURE_BASE + n`) `Superset` (exact_interior.rs lowers no async
//!      closure body) or not at all (`CapturesUnknown`): a reader's `Witness::ForeignSuperset`
//!      or the named gap, never the superset's `No` or `Yes` taken as exact.
//!    So the twins are a NAMED GAP today (never clean), E0277 once the body is lowered:
//!    - TWIN `returned_rc`: `needs_send(c(1))`, the body holds an `Rc` across `other().await`.
//!    - TWIN `simulate_rc`: the same through reth's `spawn_blocking_io_fut` bounds
//!      (`F: FnOnce(u8) -> Fut + Send + 'static, Fut: Future + Send + 'static`).
//!    - `returned_dropped`: the `Rc` dropped before the `.await` (rustc accepts): a gap is
//!      allowed, a false E0277 is not.
//!
//! NOT YET RUN (written 2026-11-21 by a write-only lane). UNSURE: that blaze's type check takes
//! an `AsyncFnOnce` bound and the call of an async closure value (`c().await`); case 2 depends on
//! it, case 1 only on the call. That the `FnOnce(u8) -> Fut` candidate for an async closure is
//! selected (case 4's `simulate_rc`); refused there, it is not clean either (a refusal).

use blaze_flow_lower::Form;
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowRefused;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::future::Future;
use std::rc::Rc;

fn require_send<T: Send>(_: T) {}

async fn hold() -> u8 {
    let c = async || 1u8;
    async {}.await;
    c().await
}

async fn hold_rc() -> u8 {
    let r = Rc::new(1u8);
    let c = async move || *r;
    async {}.await;
    c().await
}

fn execute() {
    require_send(hold());
}

fn execute_rc() {
    require_send(hold_rc());
}

async fn run_with<F: AsyncFnOnce(&str) -> u8>(f: F) -> u8 {
    f("x").await
}

async fn info() -> u8 {
    run_with(async |s: &str| s.len() as u8).await
}

async fn info_rc() -> u8 {
    let r = Rc::new(1u8);
    run_with(async move |_s: &str| *r).await
}

fn execute_info() {
    require_send(info());
}

fn execute_info_rc() {
    require_send(info_rc());
}

async fn other() {}

fn needs_send<F: Future + Send>(_: F) {}

fn returned_rc() {
    let c = async move |x: u8| {
        let r = Rc::new(x);
        other().await;
        drop(r);
    };
    needs_send(c(1));
}

fn returned_dropped() {
    let c = async move |x: u8| {
        let r = Rc::new(x);
        drop(r);
        other().await;
    };
    needs_send(c(1));
}

fn spawn_io<F: FnOnce(u8) -> Fut + Send + 'static, Fut: Future<Output = ()> + Send + 'static>(_: F) {}

fn simulate_rc() {
    spawn_io(async move |x: u8| {
        let r = Rc::new(x);
        other().await;
        drop(r);
    });
}

// `FnOnce`-only async-move closure whose body only reads its captures by `&self` (mirrors
// `reth_node_metrics::MetricServer::start_endpoint`'s `spawn_with_graceful_shutdown_signal(async
// move |mut signal| loop { ... })`). Because every callable obligation on the closure is
// by-value `FnOnce`, its future owns its environment.
fn spawn_move_capture_send() {
    let cap = 7u8;
    spawn_io(async move |x: u8| {
        other().await;
        let _ = cap.wrapping_add(x);
    });
}

fn spawn_move_capture_rc() {
    let r = Rc::new(7u8);
    spawn_io(async move |x: u8| {
        other().await;
        let _ = (*r).wrapping_add(x);
    });
}

// Foreign-body caller checking `Send` on an `async fn` that awaits an async closure
// (mirrors `reth_e2e_test_utils::E2ETestSetupBuilder::build`'s `(0..n).map(async |idx| ..)`).
async fn build_with_async_closure(n: usize) -> usize {
    let f = async |idx: usize| {
        let tmp = Rc::new(idx);
        drop(tmp);
        other().await;
        idx
    };
    f(n).await
}

fn call_build_send() {
    require_send(build_with_async_closure(1));
}

type BoxFut<T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'static>>;
type Relaunch = std::sync::Arc<dyn Fn() -> BoxFut<u8> + Send + Sync>;

fn launch_node_arc_closure_unsize(flag: bool, val: u8) -> BoxFut<u8> {
    Box::pin(async move {
        let saved = flag.then(|| val);
        async {}.await;
        if let Some(v) = saved {
            let _relaunch: Relaunch = std::sync::Arc::new(move || Box::pin(async move { v }));
        }
        val
    })
}

fn main() {}
"#;

#[test]
fn an_async_closure_is_a_stored_closure_value() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/asyncclosure/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "asyncclosure",
        dir: "/asyncclosure/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"),
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect("fixture body");

    // 1 and 2: the type check's verdict on `Send` of the future (the witness). The oracles:
    // clean, no gap (their closures capture nothing, so the call future's witness is
    // `Complete`: blaze-typeck rules/closure.rs `owns_environment` holds by `rows.is_empty()`).
    for name in ["execute", "execute_info"] {
        let got = world.check(BodyRef::Free(key_of(name)), alloc);
        std::println!("{name}: {got:?}");
        assert!(got.gaps.is_empty() && got.refused.is_none() && got.undecided.is_empty(), "{name}: no witness gap: {got:?}");
        assert!(got.diagnostics.is_empty(), "{name}: clean (rustc accepts it): {got:?}");
    }
    // The TWINS (rustc: E0277, the closure value holding an `Rc` is stored across the
    // `.await`). INVARIANT: never clean. TODAY (wave-1119 t68 for `execute_rc`): the NAMED GAP,
    // not E0277. The cause, by reading: the closure `async move || *r` only reads through its
    // capture, so its kind is `Fn` (blaze-typeck rules/closure.rs:516, the capture analysis's
    // kind), and an async closure
    // with captures whose kind is not `FnOnce` does not own its environment by the checker's
    // test (closure.rs:565 `owns_environment`): its call future's witness stays the initial
    // `NeedsCapturePlaces` (rules/coroutine.rs:151), because its `AsyncFn::async_call(&self)`
    // future stores its captures BEHIND A REFERENCE (RFC 3668; rustc
    // `tupled_upvars_by_closure_kind`: `&T` for `Fn`, `&mut T` for `FnMut`), a stored type the
    // checker does not make. The defining body's row is then filed `CapturesUnknown`
    // (front2 opaque_auto.rs:308) and a reader of the call future's coroutine
    // (`CLOSURE_BASE + 0` of the defining body) is that gap (opaque_auto.rs:218,
    // world_items.rs `coroutine_once`). The gap is reached although another stored value, the
    // closure itself, is surely `!Send` (Rust's answer is E0277 whatever the call future
    // holds): the checker's fetch of the row ends the question before the conjunction is
    // decided. When the checker makes the by-reference call future (or decides a conjunction
    // with a `No` part as `No`), these become E0277: change the assertion to E0277 then.
    use blaze_ladder::front2::opaque_auto::CoroutineAnswer;
    use blaze_ladder::front2::world_items::Gap;
    let captures_unknown = CoroutineAnswer::CapturesUnknown.why();
    // `hold_rc` calls `c()` directly (not exclusively via `FnOnce`/`AsyncFnOnce`), so its call
    // future remains `CapturesUnknown`.
    {
        let got = world.check(BodyRef::Free(key_of("execute_rc")), alloc);
        std::println!("execute_rc: {got:?}");
        let the_row = |g: &Gap| {
            matches!(g, Gap::CoroutineRow { ordinal, path, why, .. }
                if *ordinal >= blaze_typeck::coroutine_id::CLOSURE_BASE && path.contains("hold_rc") && Some(*why) == captures_unknown)
        };
        assert!(
            got.diagnostics.is_empty() && !got.gaps.is_empty() && got.gaps.iter().all(the_row),
            "TWIN `execute_rc`: direct call of `Fn` async-move closure stays `CapturesUnknown`: {got:?}"
        );
    }
    // `info_rc` passes `async move |_s: &str| *r` exclusively to an `AsyncFnOnce` bound, so the
    // future owns its environment and proves `Rc<u8>: !Send` (E0277, zero gaps).
    {
        let got = world.check(BodyRef::Free(key_of("execute_info_rc")), alloc);
        std::println!("execute_info_rc: {got:?}");
        assert!(
            got.diagnostics.iter().any(|d| d.code == "E0277") && got.gaps.is_empty() && got.undecided.is_empty(),
            "TWIN `execute_info_rc`: AsyncFnOnce-only async-move closure holding Rc is E0277 with zero gaps: {got:?}"
        );
    }

    // 4: the FUTURE an async closure's call returns: its exact interior is lowered by
    // `lower_async_witness`, so `Rc<u8>` held across `.await` is E0277 and dropped before `.await`
    // is clean with zero gaps.
    for name in ["returned_rc", "simulate_rc", "spawn_move_capture_rc"] {
        let got = world.check(BodyRef::Free(key_of(name)), alloc);
        std::println!("{name}: {got:?}");
        assert!(
            got.diagnostics.iter().any(|d| d.code == "E0277") && got.gaps.is_empty(),
            "TWIN `{name}`: rustc refuses it (E0277: `Rc<u8>` held across the body's `.await`): {got:?}"
        );
    }
    for name in ["returned_dropped", "spawn_move_capture_send", "call_build_send", "launch_node_arc_closure_unsize"] {
        let got = world.check(BodyRef::Free(key_of(name)), alloc);
        std::println!("{name}: {got:?}");
        assert!(got.is_clean() && got.gaps.is_empty() && got.undecided.is_empty(), "{name}: clean with zero gaps: {got:?}");
    }

    // 3: CONTROL, the borrow lowering: refused by name.
    let (types, borrows) = world.check_borrows(BodyRef::Free(key_of("hold")), alloc);
    std::println!("hold: types {types:?} borrows refused {:?}", borrows.refused.as_ref().map(|r| std::format!("{r}")));
    assert!(types.is_clean(), "hold's types: {types:?}");
    assert!(
        matches!(&borrows.refused, Some(BorrowRefused::Lower(r)) if r.form == Form::AsyncClosure),
        "CONTROL: the borrow lowering still refuses the async closure by name: {:?}",
        borrows.refused
    );
}
