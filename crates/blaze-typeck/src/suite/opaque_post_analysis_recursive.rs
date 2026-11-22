//! A self-recursive boxed future revealed after type checking (blaze-solve alias.rs
//! `normalize_opaque_type`, the `TypingMode::PostAnalysis` arm).
//!
//! `async fn f() { Box::pin(f()).await }` is accepted by Rust (the Reference, "Async
//! functions"; recursion through a box since 1.77): `f`'s future holds, across its await, a
//! `Pin<Box<f's future>>`, so the hidden type of `f`'s opaque (its async block's coroutine,
//! whose interior is a field of the solver's term) NAMES THAT OPAQUE again. After type
//! checking an opaque is its hidden type, ONE LAYER per normalization: revealing `O` gives
//! `Coroutine(.., (Pin<Box<O>>,))` with the inner `O` left as it is. Relating the goal's own
//! variable to that value lazily replaced the inner `O` by a variable and a nested
//! `Projection { O, _ }` goal, canonically the goal being answered: a cycle whose provisional
//! answer is overflow, so the instance below was undecided (`NotGround`).
//!
//! The toy world:
//!
//! ```text
//! trait FnOnce<Args> { type Output; }
//! trait Iterator { fn next(&mut self) -> u8; }
//! struct Pn<P>; struct Bx<T>; struct Wrap;
//! struct Flat<U, F>;  impl<U, F: FnOnce<()>> Iterator for Flat<U, F> where <F as FnOnce<()>>::Output == U
//! fn f() -> impl Sized   // hidden: f's async block {coroutine#0}, output (), interior (Pn<Bx<O>>,)
//! ```
//!
//! `<Flat<{coroutine#0 of f}, fn f> as Iterator>::next` is `Flat`'s impl's `next` (the reveal
//! decided, no overflow); the twin `<Flat<Wrap, fn f> as Iterator>::next` is `NoImpl` (the
//! reveal decided the other way: `f`'s future is no `Wrap`), never `NotGround`.
//!
//! Runs of 2026-11-21 (t55..t60) failed the first test with "no impl of trait Iterator holds":
//! instance.rs `try_impl` counted a where-clause still AMBIGUOUS (an overflow) as unmet, so an
//! undecided reveal read as a decided no. It now answers `NotGround` naming the impl, and
//! `each_where_clause_of_the_recursive_instance_holds_alone` asks the impl's clauses one at a
//! time (`fn f: FnOnce<()>`; `<fn f as FnOnce<()>>::Output == f's future` with the term given,
//! then through a variable unified with it as `unify_header` binds `U`, where blaze-ty's
//! generalizer replaces the interior's `O` by a variable and a projection goal), so a failure
//! names the clause and its kind.
//!
//! Runs of 06:45 failed both with `Ambiguous`/`NotGround`: the third route's projection, its
//! term a type holding the generalized variable, was related LAZILY with the revealed hidden
//! type, which deferred the inner `O` into `Projection { O, F[?y] }` and, `?y` bound to the
//! hidden type first, asked the parent goal again (an inductive cycle: overflow). The reveal
//! now relates the two types as their unfoldings (blaze-solve alias.rs `eq_revealing`).
//!
//! Prints `test suite::opaque_post_analysis_recursive::a_recursive_boxed_future_reveals_one_layer_at_an_instance ... ok`,
//! `test suite::opaque_post_analysis_recursive::each_where_clause_of_the_recursive_instance_holds_alone ... ok`,
//! `test suite::opaque_post_analysis_recursive::the_recursive_instance_output_is_no_other_type ... ok`
//! and `test suite::opaque_post_analysis_recursive::a_recursive_future_is_no_other_type_at_an_instance ... ok`.

use std::vec::Vec;

use blaze_own::Alloc;

use crate::fixture::Fx;
use crate::ids::{Abi, FnPtrHeader, ItemKey, LangItem, List, Mutbl, Name, Safety, UintTy};
use crate::instance::ResolveError;
use crate::portable::{PNode, PTy, PortableTys};
use crate::ty::{Cause, Goal, Obligation, Ty, TypeCtx};

struct World {
    fx: Fx,
    fn_once: ItemKey,
    next: ItemKey,
    flat_next: ItemKey,
    flat_k: ItemKey,
    wrap_k: ItemKey,
    pn_k: ItemKey,
    bx_k: ItemKey,
    f: ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let rust_call = FnPtrHeader { abi: Abi::RustCall, safety: Safety::Safe, c_variadic: false };
    let fn_once = fx.tr("FnOnce", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::FnOnce, fn_once);
    let out = fx.alias(fn_once, "Output", &[p0, p1]);
    let call_once = fx.trait_fn(fn_once, "call_once", true, &[p0, p1], out);
    fx.set_header(call_once, rust_call);
    // The rest of the fn family, as the language declares it (`FnMut: FnOnce`, `Fn: FnMut`).
    let sup = fx.bound(fn_once, &[p0, p1]);
    let fn_mut = fx.tr("FnMut", 2, std::vec![sup], &[]);
    fx.lang(LangItem::FnMut, fn_mut);
    let m_self = fx.r(Mutbl::Mut, p0);
    let call_mut = fx.trait_fn(fn_mut, "call_mut", true, &[m_self, p1], out);
    fx.set_header(call_mut, rust_call);
    let sup = fx.bound(fn_mut, &[p0, p1]);
    let fn_ = fx.tr("Fn", 2, std::vec![sup], &[]);
    fx.lang(LangItem::Fn, fn_);
    let r_self = fx.r(Mutbl::Not, p0);
    let call = fx.trait_fn(fn_, "call", true, &[r_self, p1], out);
    fx.set_header(call, rust_call);

    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    let unit = fx.tuple(&[]);
    let iterator = fx.tr("Iterator", 1, Vec::new(), &[]);
    let m_self = fx.r(Mutbl::Mut, p0);
    let next = fx.trait_fn(iterator, "next", true, &[m_self], u8_);

    let pn_k = fx.strukt("Pn", 1, &[("p", p0)]);
    let bx_k = fx.strukt("Bx", 1, &[("t", p0)]);
    let wrap_k = fx.strukt("Wrap", 0, &[]);
    // struct Flat<U, F>;  impl<U, F: FnOnce<()>> Iterator for Flat<U, F>
    //     where <F as FnOnce<()>>::Output == U
    let flat_k = fx.strukt("Flat", 2, &[("u", p0), ("f", p1)]);
    let flat_uf = fx.adt(flat_k, &[p0, p1]);
    let f_once = fx.bound(fn_once, &[p1, unit]);
    let f_out = fx.proj(fn_once, &[p1, unit], "Output", p0);
    let flat_imp = fx.imp("impl Iterator for Flat", 2, flat_uf, Some((iterator, &[])), &[], std::vec![f_once, f_out]);
    let m_flat = fx.r(Mutbl::Mut, flat_uf);
    let flat_next = fx.impl_fn(flat_imp, "next", true, &[m_flat], u8_);

    // fn f() -> impl Sized { async { Box::pin(f()).await } }: the opaque, then (by the
    // fixture's key order, told by a probe) `f`, whose async block the hidden type is.
    let o = fx.opaque_key("f::{opaque#0}");
    let none = fx.list(&[]);
    let opaque = fx.ty(PNode::Opaque(o, none));
    let is_sized = fx.bound(sized, &[opaque]);
    fx.set_opaque_bounds(o, std::vec![is_sized]);
    let probe = fx.opaque_key("probe");
    let f_k = ItemKey(probe.0 + 1);
    let future = fx.ty(PNode::Coroutine { owner: f_k, ordinal: 0, args: List::EMPTY });
    let f = fx.func("f", 0, &[], opaque, Vec::new());
    assert_eq!(f, f_k, "the probe names `f`'s key");
    fx.hidden.push((o, future));
    // The interior across the await: `Pin<Box<f's future>>`, the opaque itself.
    let boxed = fx.adt(bx_k, &[opaque]);
    let pinned = fx.adt(pn_k, &[boxed]);
    let interior = fx.tuple(&[pinned]);
    fx.coroutines.push((f, 0, unit, interior));
    fx.use_trait(iterator);
    fx.finish();
    World { fx, fn_once, next, flat_next, flat_k, wrap_k, pn_k, bx_k, f }
}

/// `Flat<u, fn f>` in `ground`.
fn flat_of(w: &World, ground: &mut PortableTys<'_>, u: PTy) -> List {
    let f_g = ground.intern(PNode::FnDef(w.f, List::EMPTY)).expect("intern");
    let flat_args = ground.intern_list(&[u, f_g]).expect("intern");
    let flat = ground.intern(PNode::Adt(w.flat_k, flat_args)).expect("intern");
    ground.intern_list(&[flat]).expect("intern")
}

/// On failure panics with `resolving next at Flat<f's future, fn f>: ..` (an overflow shows
/// as `NotGround`, an unrevealed opaque as `NoImpl`).
#[test]
fn a_recursive_boxed_future_reveals_one_layer_at_an_instance() {
    let w = world();
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let mut ground = PortableTys::new_in(alloc);
    let future = ground.intern(PNode::Coroutine { owner: w.f, ordinal: 0, args: List::EMPTY }).expect("intern");
    let args = flat_of(&w, &mut ground, future);
    let r = crate::resolve_instance(&w.fx, w.next, args, &mut ground, alloc)
        .unwrap_or_else(|e| panic!("resolving next at Flat<f's future, fn f>: {e}"));
    assert_eq!(r.item, w.flat_next, "`<Flat<f's future, fn f> as Iterator>::next` is `Flat`'s impl's `next`");
}

/// Ground type `t` of `ground` in `cx`, its bodies' types read through `reads`.
fn import_ground(cx: &mut crate::InferCtx<'static>, reads: &mut crate::world::Reads<'_, 'static, Fx>, ground: &PortableTys<'_>, t: PTy) -> Ty {
    let mut src = crate::instance::Asked { reads };
    crate::instance::import(cx, &mut src, ground.view(), t, &[], 0).expect("a ground type imports")
}

/// `Flat`'s impl's where-clauses one at a time, at the instance's arguments, after type
/// checking: `fn f: FnOnce<()>` holds, and `<fn f as FnOnce<()>>::Output` is `f`'s future (one
/// layer of `f`'s opaque revealed: the Reference, "Impl trait"). Panics naming the clause that
/// failed and how (`FulfillErrorKind`): `Ambiguous` is an overflow, `Mismatch`/`Unsatisfied` a
/// relation that refused the revealed type.
#[test]
fn each_where_clause_of_the_recursive_instance_holds_alone() {
    let w = world();
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let mut ground = PortableTys::new_in(alloc);
    let future = ground.intern(PNode::Coroutine { owner: w.f, ordinal: 0, args: List::EMPTY }).expect("intern");
    let f_g = ground.intern(PNode::FnDef(w.f, List::EMPTY)).expect("intern");
    let unit = ground.intern(PNode::Tuple(List::EMPTY)).expect("intern");
    match crate::holds(&w.fx, w.fn_once, &[f_g, unit], &mut ground, alloc) {
        Ok(true) => {}
        other => panic!("`fn f: FnOnce<()>` after type checking: expected Ok(true), got {other:?}"),
    }
    // `<fn f as FnOnce<()>>::Output == f's future`, the term given (as `try_impl` gives it).
    let mut reads = crate::world::Reads::new_in(&w.fx, alloc);
    let mut cx = crate::InferCtx::post_analysis_in(alloc);
    let f_t = import_ground(&mut cx, &mut reads, &ground, f_g);
    let unit_t = import_ground(&mut cx, &mut reads, &ground, unit);
    let future_t = import_ground(&mut cx, &mut reads, &ground, future);
    let args = cx.mk_list(&[f_t, unit_t]).expect("the goal's arguments");
    let goal = Goal::Projection { trait_: w.fn_once, args, name: Name::of("Output"), term: future_t };
    cx.register(Obligation { goal, cause: Cause::Body }).expect("registers");
    cx.select_all(&mut reads).expect("selects");
    if let Some(e) = cx.next_error() {
        panic!("`<fn f as FnOnce<()>>::Output == f's future` (the term given) after type checking: {:?}", e.kind);
    }
    // The same clause with its term a variable first unified with the future, as
    // `unify_header` binds the impl's `U` (blaze-ty generalizes `f`'s opaque inside the
    // interior to a variable and a projection goal).
    let u = cx.fresh(crate::ty::VarKind::Ty).expect("a variable");
    assert!(cx.unify(u, future_t).expect("unifies"), "a variable unifies with f's future");
    let goal = Goal::Projection { trait_: w.fn_once, args, name: Name::of("Output"), term: u };
    cx.register(Obligation { goal, cause: Cause::Body }).expect("registers");
    cx.select_all(&mut reads).expect("selects");
    if let Some(e) = cx.next_error() {
        panic!("`<fn f as FnOnce<()>>::Output == ?U`, `?U` unified with f's future, after type checking: {:?}", e.kind);
    }
}

/// The clause's twins: `<fn f as FnOnce<()>>::Output` is `f`'s future, so `== ?U` with `?U`
/// unified with `u8`, or with `Pn<Bx<f's future>>` (a type that HOLDS the future, its interior's
/// own shape, and is not it), is a decided no (`Mismatch` or `Unsatisfied`): never `Ambiguous`,
/// which an overflow of the reveal would be (blaze-solve alias.rs `eq_revealing`). Panics with
/// `Output == ?U, ?U unified with <which>: expected a decided no, got ..`.
#[test]
fn the_recursive_instance_output_is_no_other_type() {
    let w = world();
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let mut ground = PortableTys::new_in(alloc);
    let future = ground.intern(PNode::Coroutine { owner: w.f, ordinal: 0, args: List::EMPTY }).expect("intern");
    let f_g = ground.intern(PNode::FnDef(w.f, List::EMPTY)).expect("intern");
    let unit = ground.intern(PNode::Tuple(List::EMPTY)).expect("intern");
    let u8_ = ground.intern(PNode::Uint(UintTy::U8)).expect("intern");
    let in_box = ground.intern_list(&[future]).expect("intern");
    let boxed = ground.intern(PNode::Adt(w.bx_k, in_box)).expect("intern");
    let in_pin = ground.intern_list(&[boxed]).expect("intern");
    let pinned = ground.intern(PNode::Adt(w.pn_k, in_pin)).expect("intern");
    for (which, other) in [("u8", u8_), ("Pn<Bx<f's future>>", pinned)] {
        let mut reads = crate::world::Reads::new_in(&w.fx, alloc);
        let mut cx = crate::InferCtx::post_analysis_in(alloc);
        let f_t = import_ground(&mut cx, &mut reads, &ground, f_g);
        let unit_t = import_ground(&mut cx, &mut reads, &ground, unit);
        let other_t = import_ground(&mut cx, &mut reads, &ground, other);
        let args = cx.mk_list(&[f_t, unit_t]).expect("the goal's arguments");
        let u = cx.fresh(crate::ty::VarKind::Ty).expect("a variable");
        assert!(cx.unify(u, other_t).expect("unifies"), "a variable unifies with {which}");
        let goal = Goal::Projection { trait_: w.fn_once, args, name: Name::of("Output"), term: u };
        cx.register(Obligation { goal, cause: Cause::Body }).expect("registers");
        cx.select_all(&mut reads).expect("selects");
        match cx.next_error().map(|e| e.kind) {
            Some(crate::ty::FulfillErrorKind::Mismatch | crate::ty::FulfillErrorKind::Unsatisfied) => {}
            other => panic!("Output == ?U, ?U unified with {which}: expected a decided no, got {other:?}"),
        }
    }
}

/// The twin: `f`'s future revealed is no `Wrap`, a decided `NoImpl` (an overflow would be
/// `NotGround`). Panics with `Flat<Wrap, fn f>: expected NoImpl, got ..`.
#[test]
fn a_recursive_future_is_no_other_type_at_an_instance() {
    let w = world();
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let mut ground = PortableTys::new_in(alloc);
    let wrap = ground.intern(PNode::Adt(w.wrap_k, List::EMPTY)).expect("intern");
    let args = flat_of(&w, &mut ground, wrap);
    match crate::resolve_instance(&w.fx, w.next, args, &mut ground, alloc) {
        Err(ResolveError::NoImpl { .. }) => {}
        other => panic!("Flat<Wrap, fn f>: expected NoImpl, got {other:?}"),
    }
}
