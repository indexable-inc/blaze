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
    for (name, owner) in [("execute_rc", "hold_rc"), ("execute_info_rc", "info_rc")] {
        let got = world.check(BodyRef::Free(key_of(name)), alloc);
        std::println!("{name}: {got:?}");
        let e0277 = got.diagnostics.iter().any(|d| d.code == "E0277");
        let gap = got.refused.is_some() || !got.gaps.is_empty() || !got.undecided.is_empty();
        assert!(e0277 || gap, "TWIN `{name}`: rustc refuses it (E0277); never clean: {got:?}");
        let the_row = |g: &Gap| {
            matches!(g, Gap::CoroutineRow { ordinal, path, why, .. }
                if *ordinal >= blaze_typeck::coroutine_id::CLOSURE_BASE && path.contains(owner) && Some(*why) == captures_unknown)
        };
        assert!(
            got.diagnostics.is_empty() && !got.gaps.is_empty() && got.gaps.iter().all(the_row),
            "TWIN `{name}` TODAY: the named gap, the call future of `{owner}`'s async closure whose captured places \
             were not established (`CapturesUnknown`: its kind is `Fn`, so its call future stores its captures behind \
             a reference, a type the checker does not make), not E0277; if this is E0277 now, the checker's by-reference \
             call future landed: assert E0277: {got:?}"
        );
    }

    // 4: the FUTURE an async closure's call returns, never decided from the closure value's
    // captures (see the module doc, case 4). Not clean: E0277 or a named gap.
    for name in ["returned_rc", "simulate_rc"] {
        let got = world.check(BodyRef::Free(key_of(name)), alloc);
        std::println!("{name}: {got:?}");
        let e0277 = got.diagnostics.iter().any(|d| d.code == "E0277");
        let gap = got.refused.is_some() || !got.gaps.is_empty() || !got.undecided.is_empty();
        assert!(e0277 || gap, "TWIN `{name}`: rustc refuses it (E0277: `Rc<u8>` held across the body's `.await`); never clean: {got:?}");
    }
    // Its oracle: no `Rc` held across the `.await`. Rust accepts it; blaze has no exact interior
    // of the closure's body yet, so a gap is allowed, a false E0277 is not.
    let got = world.check(BodyRef::Free(key_of("returned_dropped")), alloc);
    std::println!("returned_dropped: {got:?}");
    assert!(!got.diagnostics.iter().any(|d| d.code == "E0277"), "returned_dropped: no false E0277: {got:?}");

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
