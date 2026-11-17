//! Async blocks lowered as their own flows, and their exact interiors (coroutine.rs; async plan
//! P2, D-A2). Bodies are built with the type checker's fixture world, checked by the real type
//! checker, lowered with [`lower_async`], and checked by blaze-flow. Each expectation is stated
//! from a written rule, quoted per test.

use std::format;
use std::string::String;
use std::vec;

use blaze_flow::record::check;
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{
    Capture, Checked, Def, ExprId, IntSuffix, IntTy, InferCtx, ItemKey, LangItem, Mutbl, Name, Needs, Outcome, PNode, PTy,
    UintTy,
};

use crate::coroutine::{InteriorPlant, interior_planted};
use crate::{AdtFacts, CalleeFacts, Facts, LoweredAsync, lower_async};

/// Every ADT of the fixtures is a plain struct with no `Copy` impl, no lifetimes, needing drop;
/// `Ready` is a `Future` (its impl below), and no other operand of an `.await` is.
struct AsyncFacts {
    ready: Option<PTy>,
}

impl Facts for AsyncFacts {
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: false, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
        None
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
    fn is_future(&self, ty: PTy) -> Option<bool> {
        Some(self.ready == Some(ty))
    }
}

struct Items {
    /// `fn owner()`: the body holding the async block.
    owner: ItemKey,
    ready: ItemKey,
    rc: ItemKey,
}

/// `Future`, `IntoFuture` with the library's blanket impl (core/src/future/into_future.rs:
/// `impl<F: Future> IntoFuture for F { type Output = F::Output; type IntoFuture = F; .. }`),
/// `struct Ready; impl Future for Ready { type Output = (); }`, `struct Rc<T>`,
/// `fn ready() -> Ready`, `fn make_rc() -> Rc<i32>` (the Reference's `Rc::new(1)`),
/// `fn drop_rc(_: Rc<i32>)` (its `drop(r)`), `fn read(_: &u8) -> u8`, `fn spawn<T>(_: T)`,
/// `fn owner()`.
fn world() -> (Fx, Items) {
    let mut fx = Fx::new();
    let (p0, unit, u8_) = (fx.param(0), fx.unit(), fx.u8());
    let i32_ = fx.ty(PNode::Int(IntTy::I32));
    // An async block's capture modes ask `Copy` of what the block takes by value.
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
    let ready = fx.strukt("Ready", 0, &[]);
    let ready_t = fx.adt(ready, &[]);
    fx.imp("impl Future for Ready", 0, ready_t, Some((future, &[])), &[("Output", unit)], vec::Vec::new());
    let rc = fx.strukt("Rc", 1, &[]);
    let rc_i32 = fx.adt(rc, &[i32_]);
    let ru8 = fx.r(Mutbl::Not, u8_);
    fx.func("ready", 0, &[], ready_t, vec::Vec::new());
    fx.func("make_rc", 0, &[], rc_i32, vec::Vec::new());
    fx.func("drop_rc", 0, &[rc_i32], unit, vec::Vec::new());
    fx.func("read", 0, &[ru8], u8_, vec::Vec::new());
    fx.func("spawn", 1, &[p0], unit, vec::Vec::new());
    let owner = fx.func("owner", 0, &[], unit, vec::Vec::new());
    fx.finish();
    (fx, Items { owner, ready, rc })
}

fn checked(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId) -> Checked<'static> {
    let mut cx = InferCtx::new_in(Alloc::global());
    match blaze_typeck::check(&b.view(owner, root), fx, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    }
}

/// The type of the record whose node is `Adt(adt, ..)` (each fixture has one instance).
fn adt_ty(c: &Checked<'_>, adt: ItemKey) -> Option<PTy> {
    let view = c.types.view();
    view.nodes().position(|n| matches!(*n, PNode::Adt(k, _) if k == adt)).map(|i| PTy(i as u32))
}

/// `fn owner() { spawn(async { inner }); }`: the fixture, its record, the block, the facts.
struct Case {
    b: Bb,
    c: Checked<'static>,
    block: ExprId,
    facts: AsyncFacts,
    it: Items,
}

fn case(inner: fn(&mut Bb) -> ExprId) -> Case {
    let (fx, it) = world();
    let mut b = Bb::new();
    let e0 = b.path(&["spawn"]);
    let body = inner(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let call = b.call(e0, &[block]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let c = checked(&fx, &b, it.owner, root);
    let facts = AsyncFacts { ready: adt_ty(&c, it.ready) };
    Case { b, c, block, facts, it }
}

fn lowered(k: &Case) -> LoweredAsync<Alloc<'static>> {
    let root = k.block;
    let view = k.b.view(k.it.owner, root);
    match lower_async(&view, &k.c, k.block, &k.facts, Alloc::global()) {
        Ok(l) => l,
        Err(r) => panic!("refused: {r:?}"),
    }
}

/// The interior's elements, under `plant`.
fn interior_of(k: &Case, plant: InteriorPlant) -> vec::Vec<PTy> {
    let l = lowered(k);
    let mut out = k.c.types.fork_in(Alloc::global()).expect("fork");
    let t = interior_planted(&l, &k.c, k.block, &mut out, plant, Alloc::global()).expect("interior");
    match out.view().node(t) {
        Ok(PNode::Tuple(list)) => out.view().list(list).expect("list").to_vec(),
        other => panic!("the interior is a tuple, got {other:?}"),
    }
}

/// `let x = 1u8; let r = &x; ready().await; read(r)`.
fn borrow_across(b: &mut Bb) -> ExprId {
    let e0 = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let x = b.bind("x");
    let s0 = b.let_(x, None, e0);
    let e1 = b.local(x);
    let e2 = b.borrow(Mutbl::Not, e1);
    let r = b.bind("r");
    let s1 = b.let_(r, None, e2);
    let e3 = b.path(&["ready"]);
    let e4 = b.call(e3, &[]);
    let e5 = b.await_(e4);
    let s2 = b.semi(e5);
    let e6 = b.path(&["read"]);
    let e7 = b.local(r);
    let e8 = b.call(e6, &[e7]);
    b.block(&[s0, s1, s2], Some(e8))
}

/// `let r = make_rc(); drop_rc(r); ready().await`.
fn moved_before(b: &mut Bb) -> ExprId {
    let e0 = b.path(&["make_rc"]);
    let e1 = b.call(e0, &[]);
    let r = b.bind("r");
    let s0 = b.let_(r, None, e1);
    let e2 = b.path(&["drop_rc"]);
    let e3 = b.local(r);
    let e4 = b.call(e2, &[e3]);
    let s1 = b.semi(e4);
    let e5 = b.path(&["ready"]);
    let e6 = b.call(e5, &[]);
    let e7 = b.await_(e6);
    b.block(&[s0, s1], Some(e7))
}

/// `let r = make_rc(); ready().await; drop_rc(r)`.
fn live_across(b: &mut Bb) -> ExprId {
    let e0 = b.path(&["make_rc"]);
    let e1 = b.call(e0, &[]);
    let r = b.bind("r");
    let s0 = b.let_(r, None, e1);
    let e2 = b.path(&["ready"]);
    let e3 = b.call(e2, &[]);
    let e4 = b.await_(e3);
    let s1 = b.semi(e4);
    let e5 = b.path(&["drop_rc"]);
    let e6 = b.local(r);
    let e7 = b.call(e5, &[e6]);
    b.block(&[s0, s1], Some(e7))
}

/// The Reference, "Async blocks": the future is "roughly equivalent to an enum with one variant
/// per await point, where each variant stores the data needed to resume from its corresponding
/// point", and `Future::poll` takes `self: Pin<&mut Self>` (core/src/future/future.rs:113): the
/// state never moves once polled, so a reference to a local of the block held across an
/// `.await` stays valid. `async { let x = 1u8; let r = &x; ready().await; read(r) }` is
/// accepted: one suspension, no move or borrow finding.
/// Prints "a borrow held across an await: 1 suspension, 0 findings".
#[test]
fn a_borrow_held_across_an_await_is_accepted() {
    let k = case(borrow_across);
    let l = lowered(&k);
    let rec = check(&l.flow, Alloc::global()).expect("bounded");
    let mut found: vec::Vec<String> = vec::Vec::new();
    for m in rec.moves.iter() {
        found.push(format!("move {:?} at {:?}", m.kind, m.at));
    }
    for f in rec.borrows.iter() {
        found.push(format!("borrow {f:?}"));
    }
    assert_eq!(l.point_of.len(), 1, "one `.await`, one suspension");
    assert!(found.is_empty(), "no finding: {found:?}");
    std::println!("a borrow held across an await: {} suspension, {} findings", l.point_of.len(), found.len());
}

/// What the interior must not hold: a type of a local moved out before every suspension.
fn rc_not_in(interior: &[PTy], rc: Option<PTy>) -> Result<(), String> {
    match rc {
        Some(rc) if interior.contains(&rc) => Err(String::from("Rc<i32> must not be in the interior: it was moved before the await")),
        _ => Ok(()),
    }
}

/// The Reference's example (quoted at blaze-typeck rules/async_fn.rs:29): "`let r =
/// Rc::new(1); drop(r); f().await`": the interior is "its captures and every value live across
/// an `.await`" (typeck's rules/coroutine.rs doc lines 11-12); `r` is moved out before the
/// `.await`, so no value of type `Rc<i32>` is held across it and the interior holds no `Rc`.
/// Control: every local taken as saved (`InteriorPlant::EveryLocal`) puts `Rc<i32>` in it.
/// Prints "interior without the moved Rc: ok" then, for the control, "Rc<i32> must not be in
/// the interior: it was moved before the await".
#[test]
fn a_moved_local_is_not_in_the_interior() {
    let k = case(moved_before);
    let rc = adt_ty(&k.c, k.it.rc);
    assert!(rc.is_some(), "the record names Rc<i32>");
    let got = interior_of(&k, InteriorPlant::None);
    assert_eq!(rc_not_in(&got, rc), Ok(()));
    assert_eq!(got, vec![k.facts.ready.expect("Ready")], "the interior is the awaited future alone");
    std::println!("interior without the moved Rc: ok");
    let planted = interior_of(&k, InteriorPlant::EveryLocal);
    match rc_not_in(&planted, rc) {
        Err(why) => {
            assert_eq!(why, "Rc<i32> must not be in the interior: it was moved before the await");
            std::println!("{why}");
        }
        Ok(()) => panic!("the control (every local saved) must put Rc<i32> in the interior"),
    }
}

/// The same rule's other side: `let r = make_rc(); ready().await; drop_rc(r)` uses `r` after
/// the `.await`, so `r` is live across it and `Rc<i32>` is in the interior, beside the future
/// the `.await` holds while suspended (`Ready`, the Reference's step 2 `tmp`).
/// Prints "interior with the live Rc: 2 types".
#[test]
fn a_local_live_across_the_await_is_in_the_interior() {
    let k = case(live_across);
    let rc = adt_ty(&k.c, k.it.rc).expect("the record names Rc<i32>");
    let ready = k.facts.ready.expect("Ready");
    let got = interior_of(&k, InteriorPlant::None);
    let mut want = vec![rc, ready];
    want.sort_unstable();
    assert_eq!(got, want, "the interior is the live Rc<i32> and the awaited Ready, sorted");
    std::println!("interior with the live Rc: {} types", got.len());
}
