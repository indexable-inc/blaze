//! An `.await` on a future of ANOTHER item (a library's struct implementing `Future`, an
//! `async fn`'s opaque future) lowered from the type checker's record: the future the `.await`
//! holds is `Checked::awaited`'s row (coroutine.rs `await_done`), never solved in the lowering;
//! it is a local of the block's flow, so the exact interior holds it, and an auto trait of the
//! block's future is decided on it (`blaze_typeck::decide_stalled`).
//!
//! The rules. The Reference, "Await expressions": "1. Create a future by calling
//! `IntoFuture::into_future` on the future operand. 2. Evaluate the future to a future `tmp`;
//! 3. Pin `tmp` using `Pin::new_unchecked`; 4. This pinned future is then polled .. 5. If the
//! call to `poll` returns `Poll::Pending`, then the future returns `Poll::Pending`, suspending
//! its state": `tmp` is held across the suspension. Its type is `<E as IntoFuture>::IntoFuture`;
//! for a type implementing `Future` that is the type itself (core/src/future/into_future.rs:138:
//! `impl<F: Future> IntoFuture for F { type Output = F::Output; type IntoFuture = F; .. }`).
//! "Async blocks": the state "stores the data needed to resume" from each await point; "Auto
//! traits": `Send` holds for it when it holds for all it stores.
//!
//! The world: `Sleep` (no fields: `Send`) and `Local` (not `Send`, as a struct holding an `Rc`
//! is: alloc/src/rc.rs `impl<T: ?Sized, A: Allocator> !Send for Rc<T, A> {}`, written as an
//! impl whose where-clause never holds, as send.rs does), each with `impl Future`, made by
//! `fn sleep() -> Sleep` and `fn local() -> Local`; `async fn fetch(x: &u8) -> u8` as the front
//! end files an async fn of another module (its output opaque, bounds `Future<Output = u8>` and
//! `Sized`, its hidden type the defining body's coroutine whose interior is `(&u8,)`); and
//! `fn spawn<F: Send>(_: F)`. The facts given to the lowering answer nothing about futures.
//!
//! Control (rule 23): with `await_done`'s row read replaced by `None` (or before this piece,
//! when the lowering asked `Facts::is_future` and these facts know no future), the first three
//! tests fail at "lower_async refused: AwaitOperand"; the fourth shows that refusal by name.

use std::vec;

use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{
    Capture, Checked, Def, DiagCode, ExprId, InferCtx, ItemKey, LangItem, Mutbl, Name, Needs, Outcome, PNode, PTy, check_deferring,
    decide_stalled,
};

use super::{adt_ty, declare};
use crate::region_fixture::{Decls, WithRegions};
use crate::{AdtFacts, CalleeFacts, Facts, Form, Region, interior};

/// Plain structs needing drop; nothing said of any future.
struct LibFacts;

impl Facts for LibFacts {
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

struct Lib {
    owner: ItemKey,
    sleep: ItemKey,
    local: ItemKey,
    /// `fetch`'s opaque output.
    fetch_opaque: ItemKey,
}

/// The module doc's world.
fn lib_world() -> (Fx, Lib, Decls) {
    let mut fx = Fx::new();
    let (p0, unit, u8_) = (fx.param(0), fx.unit(), fx.u8());
    let sized = fx.tr("Sized", 1, vec::Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
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
    // The library-shaped futures.
    let sleep = fx.strukt("Sleep", 0, &[]);
    let sleep_t = fx.adt(sleep, &[]);
    fx.imp("impl Future for Sleep", 0, sleep_t, Some((future, &[])), &[("Output", unit)], vec::Vec::new());
    let local = fx.strukt("Local", 0, &[]);
    let local_t = fx.adt(local, &[]);
    fx.imp("impl Future for Local", 0, local_t, Some((future, &[])), &[("Output", unit)], vec::Vec::new());
    let local_never = fx.bound(never, &[local_t]);
    fx.imp("impl Send for Local where Local: Never", 0, local_t, Some((send, &[])), &[], vec![local_never]);
    let sleep_f = fx.func("sleep", 0, &[], sleep_t, vec::Vec::new());
    let local_f = fx.func("local", 0, &[], local_t, vec::Vec::new());
    // `async fn fetch(x: &u8) -> u8` of another module, as the front end files it. The input
    // is a reference: the lowering gives an opaque one origin position (it may capture a
    // lifetime), and a call's elided output takes its one input lifetime (the Reference,
    // "Lifetime elision"); with no input lifetime the call is refused `ElisionAmbiguous`.
    let fetch_opaque = fx.opaque_key("fetch::{opaque#0}");
    let none = fx.list(&[]);
    let opaque = fx.ty(PNode::Opaque(fetch_opaque, none));
    let opaque_future = fx.bound(future, &[opaque]);
    let out_u8 = fx.proj(future, &[opaque], "Output", u8_);
    let opaque_sized = fx.bound(sized, &[opaque]);
    fx.set_opaque_bounds(fetch_opaque, vec![opaque_future, out_u8, opaque_sized]);
    let ref_u8 = fx.ty(PNode::Ref(Mutbl::Not, u8_));
    let fetch = fx.func("fetch", 0, &[ref_u8], opaque, vec::Vec::new());
    let hidden = fx.ty(PNode::Coroutine { owner: fetch, ordinal: 0, args: none });
    let fetch_interior = fx.tuple(&[ref_u8]);
    fx.hidden.push((fetch_opaque, hidden));
    fx.coroutines.push((fetch, 0, u8_, fetch_interior));
    let is_send = fx.bound(send, &[p0]);
    let spawn = fx.func("spawn", 1, &[p0], unit, vec![is_send]);
    let owner = fx.func("owner", 0, &[], unit, vec::Vec::new());
    fx.finish();
    // The heads as written above. `async fn fetch(x: &u8) -> u8` is `fn fetch<'a>(x: &'a u8)
    // -> impl Future<Output = u8>` whose opaque captures the input's one elided lifetime (the
    // Reference, "Async functions": the returned future captures every input lifetime): one
    // lifetime, `Param(0)`, for the input and the opaque's one capture. The others name none.
    let mut decls = Decls::new();
    decls.opaque(fetch_opaque, 1);
    declare(&mut decls, sleep_f, &[], sleep_t, &[]);
    declare(&mut decls, local_f, &[], local_t, &[]);
    declare(&mut decls, fetch, &[ref_u8], opaque, &[Region::Param(0), Region::Param(0)]);
    declare(&mut decls, spawn, &[p0], unit, &[]);
    declare(&mut decls, owner, &[], unit, &[]);
    (fx, Lib { owner, sleep, local, fetch_opaque }, decls)
}

/// `sleep().await`.
fn sleep_awaited(b: &mut Bb) -> ExprId {
    let e0 = b.path(&["sleep"]);
    let e1 = b.call(e0, &[]);
    b.await_(e1)
}

/// `let x = 2u8; local().await;`. Not the bare `local().await`: a block that is only one
/// forwarding await has its exact state at the check (captures plus the awaited future, the
/// Reference's "async blocks": a future stores what is held across its awaits), so the check
/// itself decides `Local: !Send` there (rules/coroutine.rs `forwarded_await`). A block with a
/// further statement needs the flow's saved-local analysis: the stalled path this test covers.
fn local_awaited(b: &mut Bb) -> ExprId {
    let e0 = b.int(Some(blaze_typeck::IntSuffix::Uint(blaze_typeck::UintTy::U8)));
    let x = b.bind("x");
    let s0 = b.let_(x, None, e0);
    let e1 = b.path(&["local"]);
    let e2 = b.call(e1, &[]);
    let e3 = b.await_(e2);
    let s1 = b.semi(e3);
    b.block(&[s0, s1], None)
}

/// `let x = 2u8; fetch(&x).await;` (the block's value is `()`, as `spawn`'s other blocks).
fn fetch_awaited(b: &mut Bb) -> ExprId {
    let e0 = b.int(Some(blaze_typeck::IntSuffix::Uint(blaze_typeck::UintTy::U8)));
    let x = b.bind("x");
    let s0 = b.let_(x, None, e0);
    let e1 = b.path(&["fetch"]);
    let e2 = b.local(x);
    let e3 = b.borrow(Mutbl::Not, e2);
    let e4 = b.call(e1, &[e3]);
    let e5 = b.await_(e4);
    let s1 = b.semi(e5);
    b.block(&[s0, s1], None)
}

/// What the whole path gave for `fn owner() { spawn(async { inner }); }`.
struct Run {
    c: Checked<'static>,
    /// The exact interior's elements.
    interior: vec::Vec<PTy>,
    /// `decide_stalled`'s findings.
    found: vec::Vec<DiagCode>,
}

/// The whole path; `drop_rows` empties the record's `awaited` column first (the test of a
/// missing row). `Err` is the lowering's refusal.
fn run(lib_body: fn(&mut Bb) -> ExprId, drop_rows: bool) -> (Lib, Result<Run, Form>) {
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let (fx, lib, decls) = lib_world();
    let mut b = Bb::new();
    let e0 = b.path(&["spawn"]);
    let body = lib_body(&mut b);
    let block = b.async_block(body, Capture::Ref);
    let call = b.call(e0, &[block]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let view = b.view(lib.owner, root);
    let mut cx = InferCtx::new_in(alloc);
    let mut c: Checked<'static> = match check_deferring(&view, &fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check_deferring refused: {e:?}"),
    };
    assert!(c.diags.is_empty(), "the body type-checks; auto traits wait for the flow: {:?}", c.diags);
    assert_eq!(c.awaited.len(), 1, "the checker records the one `.await`'s held future: {:?}", c.awaited);
    if drop_rows {
        c.awaited.clear();
    }
    // The heads laid in the record's own table, as the lowering reads them.
    let regions = decls.lay(fx.t.view(), &mut c.types);
    let facts = WithRegions { base: &LibFacts, regions: &regions };
    let lowered = match crate::oracle::lower_async_checked(&view, &c, block, &facts, alloc) {
        Ok(l) => l,
        Err(r) => return (lib, Err(r.form)),
    };
    let mut out = c.types.fork_in(alloc).expect("fork the record's table");
    let exact = match interior(&lowered, &c, block, &mut out, alloc) {
        Ok(t) => t,
        Err(r) => panic!("interior refused: {r:?}"),
    };
    let elems = match out.view().node(exact) {
        Ok(PNode::Tuple(list)) => out.view().list(list).expect("the interior's list").to_vec(),
        other => panic!("the interior is a tuple, got {other:?}"),
    };
    let got = match decide_stalled(&view, &c, out.view(), &[(block, exact)], &fx, alloc) {
        Ok(d) => d,
        Err(e) => panic!("decide_stalled refused: {e:?}"),
    };
    let found = got.iter().map(|d| d.code).collect();
    (lib, Ok(Run { c, interior: elems, found }))
}

fn lowered(r: Result<Run, Form>) -> Run {
    match r {
        Ok(run) => run,
        Err(form) => panic!("lower_async refused: {form:?}"),
    }
}

/// `spawn(async { sleep().await })`: the held future is the record's row, `Sleep` itself (the
/// blanket impl), a local of the flow, in the interior; `Sleep` is `Send`: no finding.
/// Prints "library future awaited: Sleep held, Send".
#[test]
fn a_library_future_struct_is_held_and_send() {
    let (lib, r) = run(sleep_awaited, false);
    let k = lowered(r);
    let sleep = adt_ty(&k.c, lib.sleep).expect("the record names Sleep");
    assert_eq!(k.c.awaited[0].1, sleep, "the held future is the operand's own type: {:?}", k.c.awaited);
    assert!(k.interior.contains(&sleep), "the interior holds the awaited Sleep: {:?}", k.interior);
    assert!(k.found.is_empty(), "Sleep is Send: {:?}", k.found);
    std::println!("library future awaited: Sleep held, Send");
}

/// `spawn(async { let x = 2u8; local().await; })`: `Local` is held across its own `.await` and is not
/// `Send`: the stalled goal fails on the exact interior, one E0277.
/// Prints "non-Send library future across the await: E0277".
#[test]
fn a_non_send_library_future_across_its_await_is_e0277() {
    let (lib, r) = run(local_awaited, false);
    let k = lowered(r);
    let local = adt_ty(&k.c, lib.local).expect("the record names Local");
    assert!(k.interior.contains(&local), "the interior holds the awaited Local: {:?}", k.interior);
    assert_eq!(k.c.stalled.len(), 1, "the superset holds `Local`: the goal stalls");
    assert_eq!(k.found, vec![DiagCode::Unsatisfied], "Local is not Send");
    std::println!("non-Send library future across the await: E0277");
}

/// `spawn(async { let x = 2u8; fetch(&x).await; })`: the held future is `fetch`'s opaque
/// output (an `async fn` of another module), in the interior; its `Send` leaks from the hidden
/// coroutine, whose interior is `(&u8,)`: no finding.
/// Prints "async fn future awaited: held, Send".
#[test]
fn an_async_fn_future_of_another_module_is_held_and_send() {
    let (lib, r) = run(fetch_awaited, false);
    let k = lowered(r);
    let view = k.c.types.view();
    let opaque = view
        .nodes()
        .position(|n| matches!(*n, PNode::Opaque(o, _) if o == lib.fetch_opaque))
        .map(|i| PTy(i as u32))
        .expect("the record names fetch's opaque");
    assert_eq!(k.c.awaited[0].1, opaque, "the held future is the opaque itself: {:?}", k.c.awaited);
    assert!(k.interior.contains(&opaque), "the interior holds fetch's future: {:?}", k.interior);
    assert!(k.found.is_empty(), "fetch's future is Send: {:?}", k.found);
    std::println!("async fn future awaited: held, Send");
}

/// The same `sleep().await` with the record's `awaited` row taken away: the lowering does not
/// solve `<Sleep as IntoFuture>::IntoFuture` itself (rule 77); the `.await` is refused by name.
/// Prints "no awaited row: AwaitOperand".
#[test]
fn an_await_without_its_row_is_refused_by_name() {
    let (_, r) = run(sleep_awaited, true);
    match r {
        Err(form) => assert_eq!(form, Form::AwaitOperand),
        Ok(k) => panic!("lowered without the record's row: interior {:?}", k.interior),
    }
    std::println!("no awaited row: AwaitOperand");
}
