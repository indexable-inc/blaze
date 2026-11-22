//! Async blocks lowered as their own flows, and their exact interiors (coroutine.rs; async plan
//! P2, D-A2). Bodies are built with the type checker's fixture world, checked by the real type
//! checker, lowered with [`crate::lower_async`] (through the call-rule oracle,
//! `crate::oracle::lower_async_checked`), and checked by blaze-flow. Each expectation is stated
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
use crate::region_fixture::{Decls, FnHead, Regions, WithRegions};
use crate::{AdtFacts, CalleeFacts, Facts, LoweredAsync, Region};

/// Fn `item`'s region head as its world declares it: `slots.len()` lifetimes, each elided
/// (so late-bound, none in a bound), numbered in order of appearance; its column over
/// `fn(inputs) -> output` `slots`. Laid into the checked record's own table
/// ([`Decls::lay`]), never the world's.
pub(super) fn declare(decls: &mut Decls, item: ItemKey, inputs: &[PTy], output: PTy, slots: &[Region]) {
    let lifetimes = slots.iter().filter_map(|r| if let Region::Param(i) = *r { Some(i + 1) } else { None }).max().unwrap_or(0);
    let late: vec::Vec<u32> = (0..lifetimes).collect();
    decls.func(item, FnHead { lifetimes, late: &late, outlives: &[], inputs, output, slots });
}

/// Every ADT of the fixtures is a plain struct with no `Copy` impl, no lifetimes, needing drop.
/// Nothing is said of futures: an `.await`'s future is the record's (`Checked::awaited`).
struct AsyncFacts;

impl Facts for AsyncFacts {
    // Every other fact not known here, written out: `Facts` has no default answer.
    fn regions(&self) -> &dyn crate::RegionFacts {
        &crate::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[crate::Variance]> {
        None
    }
    fn implied_bounds(&self) -> Option<(&[(u32, crate::Region)], &[(crate::Region, crate::Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        false
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[crate::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: blaze_typeck::ItemKey) -> Option<crate::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<crate::CallBounds<'_>> {
        None
    }
    fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
        None
    }
    fn ties(&self, _: blaze_typeck::ItemKey) -> Option<crate::Ties<'_>> {
        None
    }
    fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> {
        None
    }
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
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
/// `fn take(_: &Rc<i32>, _: ())` (a call whose first argument is a temporary made before an
/// `.await` in its second), `fn owner()`. With `read_laid` false, `read`'s head is not laid:
/// a call of it with a reference is the borrow check's `RegionsUnread { why: NoHead }`, the
/// region refusal the witness lowering must not wait on (typeck.md "Witness records").
fn world(read_laid: bool) -> (Fx, Items, Decls) {
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
    let ready_f = fx.func("ready", 0, &[], ready_t, vec::Vec::new());
    let make_rc = fx.func("make_rc", 0, &[], rc_i32, vec::Vec::new());
    let drop_rc = fx.func("drop_rc", 0, &[rc_i32], unit, vec::Vec::new());
    let read = fx.func("read", 0, &[ru8], u8_, vec::Vec::new());
    let spawn = fx.func("spawn", 1, &[p0], unit, vec::Vec::new());
    let r_rc = fx.r(Mutbl::Not, rc_i32);
    let take = fx.func("take", 0, &[r_rc, unit], unit, vec::Vec::new());
    let owner = fx.func("owner", 0, &[], unit, vec::Vec::new());
    fx.finish();
    // The heads as written above: `read(_: &u8)` and `take(_: &Rc<i32>, _: ())` each one
    // elided input lifetime, `Param(0)`, no output borrowing; the others none (`Rc` declares
    // no lifetime).
    let mut decls = Decls::new();
    declare(&mut decls, ready_f, &[], ready_t, &[]);
    declare(&mut decls, make_rc, &[], rc_i32, &[]);
    declare(&mut decls, drop_rc, &[rc_i32], unit, &[]);
    if read_laid {
        declare(&mut decls, read, &[ru8], u8_, &[Region::Param(0)]);
    }
    declare(&mut decls, spawn, &[p0], unit, &[]);
    declare(&mut decls, take, &[r_rc, unit], unit, &[Region::Param(0)]);
    declare(&mut decls, owner, &[], unit, &[]);
    (fx, Items { owner, ready, rc }, decls)
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
    /// The world's heads, laid in `c`'s own table.
    regions: Regions,
    it: Items,
}

fn case(inner: fn(&mut Bb) -> ExprId) -> Case {
    case_with(inner, true)
}

/// [`case`], `read`'s head laid or not ([`world`]).
fn case_with(inner: fn(&mut Bb) -> ExprId, read_laid: bool) -> Case {
    let (fx, it, decls) = world(read_laid);
    let mut b = Bb::new();
    let e0 = b.path(&["spawn"]);
    let body = inner(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let call = b.call(e0, &[block]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let mut c = checked(&fx, &b, it.owner, root);
    let regions = decls.lay(fx.t.view(), &mut c.types);
    let facts = AsyncFacts;
    Case { b, c, block, facts, regions, it }
}

fn lowered(k: &Case) -> LoweredAsync<Alloc<'static>> {
    let root = k.block;
    let view = k.b.view(k.it.owner, root);
    let with = WithRegions { base: &k.facts, regions: &k.regions };
    match crate::oracle::lower_async_checked(&view, &k.c, k.block, &with, crate::region_fixture::alloc()) {
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
    assert_eq!(got, vec![adt_ty(&k.c, k.it.ready).expect("Ready")], "the interior is the awaited future alone");
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
    let ready = adt_ty(&k.c, k.it.ready).expect("Ready");
    let got = interior_of(&k, InteriorPlant::None);
    let mut want = vec![rc, ready];
    want.sort_unstable();
    assert_eq!(got, want, "the interior is the live Rc<i32> and the awaited Ready, sorted");
    std::println!("interior with the live Rc: {} types", got.len());
}

/// W1 (typeck.md "Witness records"): an async block's witness is region-erased (the Reference,
/// "Async blocks": the state "stores the data needed to resume", and "Auto traits" read only
/// the types it holds; rustc computes it with regions erased), so a REGION refusal of the
/// borrow check's lowering must not take it away. `async { let x = 1u8; let r = &x;
/// ready().await; read(r) }` with `read`'s head not laid: the borrow check's lowering is refused
/// `RegionsUnread { item: read, why: NoHead }` (it relates `r`'s region into a head it cannot
/// read); the witness lowering (one_call.rs `Erased`) lowers it, and its statements and saved
/// locals equal production's lowering of the same block with `read` laid (oracle.rs
/// `witness_differs`). Control: production's lowering with one `StorageDead` turned into a
/// `StorageLive` is told apart.
/// Prints "witness despite RegionsUnread: equal to production's".
#[test]
fn a_region_refusal_leaves_the_witness() {
    let unlaid = case_with(borrow_across, false);
    let view = unlaid.b.view(unlaid.it.owner, unlaid.block);
    let with = WithRegions { base: &unlaid.facts, regions: &unlaid.regions };
    let refused = crate::lower_async(&view, &unlaid.c, unlaid.block, &with, crate::region_fixture::alloc());
    match refused {
        Err(r) => assert!(matches!(r.form, crate::Form::RegionsUnread { .. }), "the borrow check's lowering refuses by region: {r:?}"),
        Ok(_) => panic!("the borrow check's lowering must refuse a call of an unlaid head with a reference"),
    }
    let w = match crate::lower_async_witness(&view, &unlaid.c, unlaid.block, &with, crate::region_fixture::alloc()) {
        Ok(w) => w,
        Err(r) => panic!("the witness lowering must not refuse for a region: {r:?}"),
    };
    let laid = case(borrow_across);
    let production = lowered(&laid);
    assert_eq!(crate::oracle::witness_differs(&production, w.lowered_for_oracle(), crate::region_fixture::alloc()), None);
    assert_eq!(w.point_of().len(), 1, "one `.await`, one suspension");
    std::println!("witness despite RegionsUnread: equal to production's");

    // The control: one `StorageDead` changed.
    let mut planted = lowered(&laid);
    let at = planted.flow.stmts.iter().position(|s| matches!(s.kind, blaze_flow::StmtKind::StorageDead(_)));
    let Some(at) = at else { panic!("the block's lowering ends some local's storage") };
    if let blaze_flow::StmtKind::StorageDead(l) = planted.flow.stmts[at].kind {
        planted.flow.stmts[at].kind = blaze_flow::StmtKind::StorageLive(l);
    }
    assert!(
        crate::oracle::witness_differs(&planted, w.lowered_for_oracle(), crate::region_fixture::alloc()).is_some(),
        "CONTROL: a lowering with one StorageDead changed must differ from the witness"
    );
}

/// The whole path of an auto trait on an async block's future (async plan P13).
mod send;

/// An `.await` on a library future, held by the record's type (coroutine.rs `await_done`).
mod await_operand;

/// What a block's state saves, as types (state_recipe.rs; ASYNC.md P8).
mod state_recipe;

/// The value of an `.await` of an `async fn`'s call, by its written return type (await_call.rs
/// `awaited_shadow`).
mod await_value;
