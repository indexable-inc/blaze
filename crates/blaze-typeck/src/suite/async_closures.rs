//! Async closures against the `AsyncFn*` traits (docs: port27/tempo-run/ASYNC.md D-A9, prompt
//! P9). The pinned library's declarations (library/core/src/ops/async_function.rs):
//!
//! ```text
//! #[lang = "async_fn"]
//! pub trait AsyncFn<Args: Tuple>: AsyncFnMut<Args> {
//!     extern "rust-call" fn async_call(&self, args: Args) -> Self::CallRefFuture<'_>;
//! }
//! #[lang = "async_fn_mut"]
//! pub trait AsyncFnMut<Args: Tuple>: AsyncFnOnce<Args> {
//!     #[lang = "call_ref_future"]
//!     type CallRefFuture<'a>: Future<Output = Self::Output> where Self: 'a;
//!     extern "rust-call" fn async_call_mut(&mut self, args: Args) -> Self::CallRefFuture<'_>;
//! }
//! #[lang = "async_fn_once"]
//! pub trait AsyncFnOnce<Args: Tuple> {
//!     #[lang = "call_once_future"]
//!     type CallOnceFuture: Future<Output = Self::Output>;
//!     #[lang = "async_fn_once_output"]
//!     type Output;
//!     extern "rust-call" fn async_call_once(self, args: Args) -> Self::CallOnceFuture;
//! }
//! ```
//!
//! The world is rung 7c's ([`crate::fixture::rung_7c`]) plus `Copy` (with `impl Copy for u8`),
//! `Future`, `IntoFuture` with its blanket impl (as suite/coroutine.rs), the three traits above,
//! and:
//!
//! ```text
//! struct Rc {}   fn make_rc() -> Rc;   fn use_rc(r: Rc);
//! fn twice<F: AsyncFnMut()>(f: F);
//! fn once<F: AsyncFnOnce()>(f: F);
//! async fn c_async() -> u8;   // the owner of each case's body
//! ```
//!
//! The rule: the Reference, "Closure types": a closure "implements FnOnce", and `FnMut` when it
//! does not move out of a capture, and `Fn` when it also does not mutate one; "Closure
//! expressions": an async closure's call returns a future of its body. The async traits follow
//! the same kinds (the library's supertraits `AsyncFn: AsyncFnMut: AsyncFnOnce`, as
//! `Fn: FnMut: FnOnce`). A wrong rule that never implements the async traits fails the first
//! test (E0277 for `twice`); one that implements them whatever the kind fails the second (no
//! E0277 for a closure that moves its capture out).

use std::vec::Vec;

use super::check_body;
use crate::body::{Expr, IntSuffix};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, rung_7c};
use crate::ids::{ExprId, ItemKey, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::Def;

struct Items {
    /// `async fn c_async() -> u8`.
    c_async: ItemKey,
}

fn world() -> (Fx, Items) {
    let (mut world, _s) = rung_7c();
    let fx = &mut world;
    let (p0, p1, unit, u8_) = (fx.param(0), fx.param(1), fx.unit(), fx.u8());

    let copy = fx.tr("Copy", 1, Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    fx.imp("impl Copy for u8", 0, u8_, Some((copy, &[])), &[], Vec::new());
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

    // The three traits. Their associated types are answered from the trait and the name
    // (`LangItem::assoc_type`), as `FnOnce::Output` is.
    let async_fn_once = fx.tr("AsyncFnOnce", 2, Vec::new(), &["Output", "CallOnceFuture"]);
    fx.lang(LangItem::AsyncFnOnce, async_fn_once);
    let sup = fx.bound(async_fn_once, &[p0, p1]);
    let async_fn_mut = fx.tr("AsyncFnMut", 2, std::vec![sup], &["CallRefFuture"]);
    fx.lang(LangItem::AsyncFnMut, async_fn_mut);
    let sup = fx.bound(async_fn_mut, &[p0, p1]);
    let async_fn = fx.tr("AsyncFn", 2, std::vec![sup], &[]);
    fx.lang(LangItem::AsyncFn, async_fn);

    let rc = fx.strukt("Rc", 0, &[]);
    let rc_t = fx.adt(rc, &[]);
    fx.func("make_rc", 0, &[], rc_t, Vec::new());
    fx.func("use_rc", 0, &[rc_t], unit, Vec::new());
    let none = fx.tuple(&[]);
    let b = fx.bound(async_fn_mut, &[p0, none]);
    fx.func("twice", 1, &[p0], unit, std::vec![b]);
    let b = fx.bound(async_fn_once, &[p0, none]);
    fx.func("once", 1, &[p0], unit, std::vec![b]);

    let c_async = fx.func("c_async", 0, &[], u8_, Vec::new());
    fx.finish();
    (world, Items { c_async })
}

/// `async fn c_async() -> u8 { let mut n = 0u8; let mut c = async || { n = 1u8; }; <use>; n }`,
/// `<use>` being `c().await; c().await;` (`direct`) or `twice(c);`. The closure assigns its
/// capture `n`, so it is `FnMut` and not `Fn` ("Closure types"), hence `AsyncFnMut`.
/// (`mut` on the bindings is borrowck's, not the checker's: the fixture's binder has none.)
fn mutating_case(direct: bool) -> (Fx, Bb, crate::columns::Checked<'static>, [ExprId; 2]) {
    let (fx, it) = world();
    let mut b = Bb::new();
    let out = b.ty(PNode::Uint(UintTy::U8));
    b.async_fn(out);
    let n = b.bind("n");
    let e0 = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let s0 = b.let_(n, None, e0);
    let e1 = b.local(n);
    let e2 = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let e3 = b.e(Expr::Assign { lhs: e1, rhs: e2 });
    let s1 = b.semi(e3);
    let e4 = b.block(&[s1], None);
    let e5 = b.async_closure(&[], e4);
    let c = b.bind("c");
    let s2 = b.let_(c, None, e5);
    let mut stmts = std::vec![s0, s2];
    let mut awaited = [e5, e5];
    if direct {
        for slot in awaited.iter_mut() {
            let callee = b.local(c);
            let call = b.call(callee, &[]);
            let aw = b.await_(call);
            *slot = aw;
            stmts.push(b.semi(aw));
        }
    } else {
        let callee = b.path(&["twice"]);
        let arg = b.local(c);
        let call = b.call(callee, &[arg]);
        stmts.push(b.semi(call));
    }
    let tail = b.local(n);
    let root = b.block(&stmts, Some(tail));
    let checked = check_body(&fx, &b, it.c_async, root, Plant::None, InferPlant::None);
    (fx, b, checked, awaited)
}

/// The ready case of prompt P9: `let mut n = 0; let mut c = async || { n = 1 }; c().await;
/// c().await;` has no finding and each `.await` is `()` (the body's type, "Closure
/// expressions"); and the same closure passed to `fn twice<F: AsyncFnMut()>` meets the bound:
/// no E0277 naming `AsyncFnMut` (before P9 the solver had no `AsyncFn*` rule, and every such
/// bound was E0277).
/// Expected line: `test suite::async_closures::an_async_closure_called_twice_is_async_fn_mut ... ok`.
#[test]
fn an_async_closure_called_twice_is_async_fn_mut() {
    let (fx, _b, c, awaited) = mutating_case(true);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    for e in awaited {
        assert_eq!(crate::fixture::show(&fx, c.types.view(), c.expr_ty[e.0 as usize]), "()");
    }
    let (_fx, _b, c, _) = mutating_case(false);
    assert!(
        !c.diags.iter().any(|d| d.code == DiagCode::Unsatisfied),
        "an FnMut async closure must meet `AsyncFnMut()`: {:?}",
        c.diags
    );
    assert!(c.diags.is_empty(), "{:?}", c.diags);
}

/// `let s = make_rc(); let c = async || { use_rc(s); }; <call>(c);`: the closure moves its
/// non-`Copy` capture out, so it is only `FnOnce` ("Closure types": "A closure which moves out
/// of a captured variable ... implements only FnOnce"), hence only `AsyncFnOnce`:
/// `twice(c)` is E0277 (`AsyncFnMut` unmet), `once(c)` has no finding.
/// Expected line: `test suite::async_closures::an_async_closure_that_moves_a_capture_is_only_async_fn_once ... ok`.
#[test]
fn an_async_closure_that_moves_a_capture_is_only_async_fn_once() {
    for (callee, want_e0277) in [("twice", true), ("once", false)] {
        let (fx, it) = world();
        let mut b = Bb::new();
        let out = b.ty(PNode::Uint(UintTy::U8));
        b.async_fn(out);
        let s = b.bind("s");
        let e0 = b.path(&["make_rc"]);
        let e1 = b.call(e0, &[]);
        let s0 = b.let_(s, None, e1);
        let e2 = b.path(&["use_rc"]);
        let e3 = b.local(s);
        let e4 = b.call(e2, &[e3]);
        let s1 = b.semi(e4);
        let e5 = b.block(&[s1], None);
        let e6 = b.async_closure(&[], e5);
        let c = b.bind("c");
        let s2 = b.let_(c, None, e6);
        let e7 = b.path(&[callee]);
        let e8 = b.local(c);
        let e9 = b.call(e7, &[e8]);
        let s3 = b.semi(e9);
        let tail = b.int(Some(IntSuffix::Uint(UintTy::U8)));
        let root = b.block(&[s0, s2, s3], Some(tail));
        let checked = check_body(&fx, &b, it.c_async, root, Plant::None, InferPlant::None);
        let has = checked.diags.iter().any(|d| d.code == DiagCode::Unsatisfied);
        assert_eq!(has, want_e0277, "{callee}(c): {:?}", checked.diags);
    }
}

/// RFC 3668 ("Async closures"; rustc `CoroutineClosureSignature::tupled_upvars_by_closure_kind`):
/// an `FnMut` async closure's call futures are two, `async_call_once(self)`'s storing the
/// closure's captures and `async_call_mut(&mut self)`'s storing `&mut P` for each place it
/// uses mutably. `let mut n = 0u8; let c = async || { n = 1u8; }; c().await; n` captures `n`
/// BY REFERENCE (`&mut u8`, no `move`): both futures store `&mut u8`, so the checker's one
/// future term is exact and, the body not suspending, its witness is `Complete`
/// (rules/closure.rs `borrows_alike`; before it, `NeedsCapturePlaces`: a named gap at every
/// auto-trait question on the call future). CONTROL (rule 23), the cause removed: the same
/// closure `move`: `n` is held BY VALUE, the two futures store `u8` and `&mut u8`, one term
/// cannot be both, and the witness stays `NeedsCapturePlaces` (never decided from one layout).
/// Expected line: `test suite::async_closures::a_borrowing_async_closure_call_future_is_decided ... ok`.
#[test]
fn a_borrowing_async_closure_call_future_is_decided() {
    for (is_move, want) in [(false, crate::CoroutineWitness::Complete), (true, crate::CoroutineWitness::NeedsCapturePlaces)] {
        let (fx, it) = world();
        let mut b = Bb::new();
        let out = b.ty(PNode::Uint(UintTy::U8));
        b.async_fn(out);
        let n = b.bind("n");
        let e0 = b.int(Some(IntSuffix::Uint(UintTy::U8)));
        let s0 = b.let_(n, None, e0);
        let e1 = b.local(n);
        let e2 = b.int(Some(IntSuffix::Uint(UintTy::U8)));
        let e3 = b.e(Expr::Assign { lhs: e1, rhs: e2 });
        let s1 = b.semi(e3);
        let e4 = b.block(&[s1], None);
        let closure = b.async_closure(&[], e4);
        if let Some(Expr::Closure { is_move: m, .. }) = b.exprs.get_mut(closure.0 as usize) {
            *m = is_move;
        }
        let c = b.bind("c");
        let s2 = b.let_(c, None, closure);
        let callee = b.local(c);
        let call = b.call(callee, &[]);
        let aw = b.await_(call);
        let s3 = b.semi(aw);
        let tail = b.local(n);
        let root = b.block(&[s0, s2, s3], Some(tail));
        let checked = check_body(&fx, &b, it.c_async, root, Plant::None, InferPlant::None);
        assert!(checked.diags.is_empty(), "move: {is_move}: {:?}", checked.diags);
        assert_eq!(checked.coroutine_witness(closure), Some(want), "move: {is_move}: the call future's witness");
    }
}
