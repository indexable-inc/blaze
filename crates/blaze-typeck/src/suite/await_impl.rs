//! `.await` on a type with its OWN `IntoFuture` impl (alloy's request builders: tempo's
//! `portal.zoneHeight().call().await`, `pending.get_receipt().await`): the impl is selected
//! by the body's solver and recorded as the callee (rules/await_impl.rs), its `IntoFuture` and
//! `Output` projected from that impl.
//!
//! The Reference, "Await expressions": `operand.await` evaluates
//! `IntoFuture::into_future(operand)` and polls the future it returns; the pinned library's
//! core/src/future/into_future.rs declares `trait IntoFuture { type Output; type IntoFuture:
//! Future<Output = Self::Output>; fn into_future(self) -> Self::IntoFuture; }`.
//!
//! ```text
//! #[lang = "sized"] trait Sized {}
//! trait Send {}
//! trait IntoFuture { type Output; type IntoFuture; fn into_future(self) -> Self::IntoFuture; }
//! struct Ready<T>(T);
//! struct B;
//! impl IntoFuture for B { type Output = u32; type IntoFuture = Ready<u32>; fn into_future(self) -> Ready<u32> }
//! struct Req<T>(T);
//! impl<T: Send> IntoFuture for Req<T> { type Output = T; type IntoFuture = Ready<T>; fn into_future(self) -> Ready<T> }
//! // the ambiguous world only:
//! impl<T> IntoFuture for T { type Output = u32; type IntoFuture = Ready<u32>; fn into_future(self) -> Ready<u32> }
//! ```

use std::vec::Vec;

use super::check_body;
use crate::Checked;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, Name, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::Def;

struct Items {
    trait_fn: ItemKey,
    ready: ItemKey,
    /// `B`'s impl's `into_future`.
    b_fn: ItemKey,
    /// `Req<T>`'s impl's `into_future`.
    req_fn: ItemKey,
    /// `async fn wait(b: B) -> u32`.
    wait: ItemKey,
    /// `async fn go<U: Send>(r: Req<U>) -> U`.
    go: ItemKey,
    /// `async fn by_bound<T: IntoFuture<Output = u32>>(t: T) -> u32`.
    by_bound: ItemKey,
}

fn world(ambiguous: bool) -> (Fx, Items) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let send = fx.tr("Send", 1, Vec::new(), &[]);
    let u32_ = fx.ty(PNode::Uint(UintTy::U32));
    let into_future = fx.tr("IntoFuture", 1, Vec::new(), &["Output", "IntoFuture"]);
    fx.lang(LangItem::IntoFuture, into_future);
    let p0 = fx.param(0);
    let held_p0 = fx.alias(into_future, "IntoFuture", &[p0]);
    let trait_fn = fx.trait_fn(into_future, "into_future", true, &[p0], held_p0);
    fx.lang_def(LangItem::IntoFutureIntoFuture, Def::Fn(trait_fn));
    let ready = fx.tuple_strukt("Ready", 1, &[p0]);
    let ready_u32 = fx.adt(ready, &[u32_]);
    let ready_p0 = fx.adt(ready, &[p0]);

    let b = fx.strukt("B", 0, &[]);
    let b_t = fx.adt(b, &[]);
    let b_imp = fx.imp("impl IntoFuture for B", 0, b_t, Some((into_future, &[])), &[("Output", u32_), ("IntoFuture", ready_u32)], Vec::new());
    let b_fn = fx.impl_fn(b_imp, "into_future", true, &[b_t], ready_u32);

    let req = fx.tuple_strukt("Req", 1, &[p0]);
    let req_p0 = fx.adt(req, &[p0]);
    let send_p0 = fx.bound(send, &[p0]);
    let req_imp = fx.imp(
        "impl<T: Send> IntoFuture for Req<T>",
        1,
        req_p0,
        Some((into_future, &[])),
        &[("Output", p0), ("IntoFuture", ready_p0)],
        std::vec![send_p0],
    );
    let req_fn = fx.impl_fn(req_imp, "into_future", true, &[req_p0], ready_p0);

    if ambiguous {
        let all = fx.imp("impl<T> IntoFuture for T", 1, p0, Some((into_future, &[])), &[("Output", u32_), ("IntoFuture", ready_u32)], Vec::new());
        fx.impl_fn(all, "into_future", true, &[p0], ready_u32);
    }

    let wait = fx.func("wait", 0, &[b_t], u32_, Vec::new());
    let send_u = fx.bound(send, &[p0]);
    let go = fx.func("go", 1, &[req_p0], p0, std::vec![send_u]);
    let bound = fx.bound(into_future, &[p0]);
    let output = fx.proj(into_future, &[p0], "Output", u32_);
    let by_bound = fx.func("by_bound", 1, &[p0], u32_, std::vec![bound, output]);
    fx.finish();
    (world, Items { trait_fn, ready, b_fn, req_fn, wait, go, by_bound })
}

/// `async fn <owner>(x: ..) -> <out> { x.await }`: pattern 0 is `x`, expression 0 the local,
/// expression 1 the `.await`, expression 2 the block.
fn await_param(fx: &Fx, owner: ItemKey, out: PNode) -> Checked<'static> {
    let mut b = Bb::new();
    let x = b.param("x");
    let out = b.ty(out);
    b.async_fn(out);
    let e0 = b.local(x);
    let e1 = b.await_(e0);
    let root = b.block(&[], Some(e1));
    check_body(fx, &b, owner, root, Plant::None, InferPlant::None)
}

/// The callee the record names at `e`, and its argument count.
fn callee_at(c: &Checked<'_>, e: ExprId) -> Option<(ItemKey, usize)> {
    let i = c.callees.column::<0>().iter().position(|&x| x == e)?;
    let args = c.types.view().list(c.callees.column::<2>()[i]).expect("a callee's arguments in the body's table");
    Some((c.callees.column::<1>()[i], args.len()))
}

/// The ADT the `.await` at `e` holds (`Checked::awaited_of`), if it is one.
fn held_adt(c: &Checked<'_>, e: ExprId) -> Option<ItemKey> {
    let t = c.awaited_of(e)?;
    match c.types.view().node(t).expect("a held type in the body's table") {
        PNode::Adt(k, _) => Some(k),
        _ => None,
    }
}

fn codes(c: &Checked<'_>) -> Vec<(DiagCode, u32, Option<Name>)> {
    c.diags.iter().map(|d| (d.code, d.at.start, d.name)).collect()
}

/// `async fn wait(x: B) -> u32 { x.await }` with `impl IntoFuture for B { type Output = u32;
/// type IntoFuture = Ready<u32>; .. }`: `x.await` is `u32` (`<B as IntoFuture>::Output`, from
/// B's impl), the held future is `Ready<u32>` (`::IntoFuture`, from B's impl), and the callee is
/// B's impl's `into_future` with no arguments (the impl has no generics).
/// Prints "B.await: u32, holds Ready, calls B's into_future".
#[test]
fn a_non_blanket_impl_is_selected_and_projected() {
    let (fx, it) = world(false);
    let c = await_param(&fx, it.wait, PNode::Uint(UintTy::U32));
    assert_eq!(codes(&c), Vec::new(), "no finding");
    let t = c.expr_ty.get(1).copied().expect("the .await's type");
    assert!(matches!(c.types.view().node(t), Ok(PNode::Uint(UintTy::U32))), "B.await is u32");
    assert_eq!(held_adt(&c, ExprId(1)), Some(it.ready), "the held future is Ready<u32>, B's impl's IntoFuture");
    assert_eq!(callee_at(&c, ExprId(1)), Some((it.b_fn, 0)), "the callee is B's impl's into_future, not the trait's");
    std::println!("B.await: u32, holds Ready, calls B's into_future");
}

/// `async fn go<U: Send>(x: Req<U>) -> U { x.await }` with `impl<T: Send> IntoFuture for
/// Req<T>`: the impl's where-clause `U: Send` holds by the body's own where-clause (the
/// Reference, "Trait and lifetime bounds"), so the impl is selected at `T = U`; the callee is
/// Req's impl's `into_future` at one argument, the held future `Ready<U>`.
/// Prints "Req<U>.await: holds Ready, calls Req's into_future<U>".
#[test]
fn a_generic_impl_is_selected_under_the_bodys_bounds() {
    let (fx, it) = world(false);
    let c = await_param(&fx, it.go, PNode::Param(0));
    assert_eq!(codes(&c), Vec::new(), "no finding");
    assert_eq!(held_adt(&c, ExprId(1)), Some(it.ready), "the held future is Ready<U>");
    assert_eq!(callee_at(&c, ExprId(1)), Some((it.req_fn, 1)), "the callee is Req's impl's into_future at [U]");
    std::println!("Req<U>.await: holds Ready, calls Req's into_future<U>");
}

/// The ambiguous world adds `impl<T> IntoFuture for T`: at `B` two impls hold, which coherence
/// refuses in a real crate (E0119). The `.await` is refused by name: E0283 at the `.await`
/// naming `into_future` (rules/await_impl.rs), never one of the two picked; the callee stays
/// the trait's.
/// Prints "B.await with two impls: E0283 naming into_future".
#[test]
fn two_holding_impls_are_refused_by_name() {
    let (fx, it) = world(true);
    let c = await_param(&fx, it.wait, PNode::Uint(UintTy::U32));
    let got = codes(&c);
    assert!(
        got.contains(&(DiagCode::CannotSatisfy, 1, Some(Name::of("into_future")))),
        "E0283 at the .await naming into_future: {got:?}"
    );
    assert_eq!(callee_at(&c, ExprId(1)).map(|r| r.0), Some(it.trait_fn), "no impl is picked");
    std::println!("B.await with two impls: E0283 naming into_future");
}

/// reth's `DebugNodeLauncherFuture` (tempo crates/zones/node/tests/it/utils.rs:2658, false
/// E0282 "undecided: generic argument #3 of <impl<L, Target, N, AddOns, B> IntoFuture for
/// DebugNodeLauncherFuture<L, Target, N, B> ..>", "no goal is pending on it"), reduced:
///
/// ```text
/// trait Launch<T> { type Node; }
/// struct Handle<N, A>(N, A);  struct Fut<L, T, N>(L, T, N);  struct Eng;
/// impl<L, T, N, A> IntoFuture for Fut<L, T, N> where L: Launch<T, Node = Handle<N, A>> {
///     type Output = Handle<N, A>; type IntoFuture = Ready<Handle<N, A>>; fn into_future(self) -> Ready<Handle<N, A>> }
/// impl Launch<u8> for Eng { type Node = Handle<u16, u32>; }   // the twin: Handle<u64, u32>
/// async fn user(x: Fut<Eng, u8, u16>) { let h = x.await; }
/// ```
///
/// `A` is in neither the self type nor the trait: only the projection binding decides it.
/// Returns the world and `[user, the impl's into_future]`.
fn launch_world(node_n: PNode) -> (Fx, [ItemKey; 2]) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let into_future = fx.tr("IntoFuture", 1, Vec::new(), &["Output", "IntoFuture"]);
    fx.lang(LangItem::IntoFuture, into_future);
    let [p0, p1, p2, p3] = [fx.param(0), fx.param(1), fx.param(2), fx.param(3)];
    let held_p0 = fx.alias(into_future, "IntoFuture", &[p0]);
    let trait_fn = fx.trait_fn(into_future, "into_future", true, &[p0], held_p0);
    fx.lang_def(LangItem::IntoFutureIntoFuture, Def::Fn(trait_fn));
    let ready = fx.tuple_strukt("Ready", 1, &[p0]);

    let launch = fx.tr("Launch", 2, Vec::new(), &["Node"]);
    let handle = fx.tuple_strukt("Handle", 2, &[p0, p1]);
    let fut = fx.tuple_strukt("Fut", 3, &[p0, p1, p2]);
    let eng = fx.strukt("Eng", 0, &[]);

    // impl<L, T, N, A> IntoFuture for Fut<L, T, N> where L: Launch<T, Node = Handle<N, A>>
    let fut_ltn = fx.adt(fut, &[p0, p1, p2]);
    let handle_na = fx.adt(handle, &[p2, p3]);
    let ready_handle = fx.adt(ready, &[handle_na]);
    let l_launch = fx.bound(launch, &[p0, p1]);
    let l_node = fx.proj(launch, &[p0, p1], "Node", handle_na);
    let imp = fx.imp(
        "impl<L, T, N, A> IntoFuture for Fut<L, T, N> where L: Launch<T, Node = Handle<N, A>>",
        4,
        fut_ltn,
        Some((into_future, &[])),
        &[("Output", handle_na), ("IntoFuture", ready_handle)],
        std::vec![l_launch, l_node],
    );
    let fut_fn = fx.impl_fn(imp, "into_future", true, &[fut_ltn], ready_handle);

    // impl Launch<u8> for Eng { type Node = Handle<node_n, u32>; }
    let u8_ = fx.u8();
    let u16_ = fx.u16();
    let u32_ = fx.ty(PNode::Uint(UintTy::U32));
    let n = fx.ty(node_n);
    let eng_t = fx.adt(eng, &[]);
    let node = fx.adt(handle, &[n, u32_]);
    fx.imp("impl Launch<u8> for Eng", 0, eng_t, Some((launch, &[u8_])), &[("Node", node)], Vec::new());

    let fut_t = fx.adt(fut, &[eng_t, u8_, u16_]);
    let unit = fx.unit();
    let user = fx.func("user", 0, &[fut_t], unit, Vec::new());
    fx.finish();
    (world, [user, fut_fn])
}

/// `async fn user(x: ..) { let h = x.await; }`: expression 1 is the `.await`.
fn await_into_let(fx: &Fx, owner: ItemKey) -> Checked<'static> {
    let mut b = Bb::new();
    let x = b.param("x");
    let out = b.ty(PNode::Tuple(crate::ids::List::EMPTY));
    b.async_fn(out);
    let e0 = b.local(x);
    let e1 = b.await_(e0);
    let h = b.bind("h");
    let s = b.let_(h, None, e1);
    let root = b.block(&[s], None);
    check_body(fx, &b, owner, root, Plant::None, InferPlant::None)
}

/// The reduced reth site: the selected impl's projection binding decides `A = u32`, so the
/// `.await` is clean and the callee is the impl's `into_future` at `[Eng, u8, u16, u32]`.
/// Before the fix the selected impl's where-clauses were only asked in the probe and rolled
/// back, so the callee row kept `?A` and writeback refused it (E0282). Twin:
/// `Eng::Node = Handle<u64, u32>` cannot be `Handle<u16, A>`: no impl holds, the `.await` is
/// refused (the `IntoFuture` goal unmet), never E0282 and never a selected impl.
/// Prints "Fut.await: A = u32 by the projection binding".
#[test]
fn an_impl_parameter_only_a_projection_binding_names_is_decided_at_await() {
    let (fx, [user, fut_fn]) = launch_world(PNode::Uint(UintTy::U16));
    let c = await_into_let(&fx, user);
    assert_eq!(codes(&c), Vec::new(), "ORACLE: `A` is decided by `L: Launch<T, Node = Handle<N, A>>`: {:?}", c.diags);
    assert_eq!(callee_at(&c, ExprId(1)), Some((fut_fn, 4)), "the callee is Fut's impl's into_future at four arguments");
    let i = c.callees.column::<0>().iter().position(|&x| x == ExprId(1)).expect("the .await's row");
    let args = c.types.view().list(c.callees.column::<2>()[i]).expect("the callee's arguments");
    assert!(matches!(c.types.view().node(args[3]), Ok(PNode::Uint(UintTy::U32))), "A = u32: {:?}", c.types.view().node(args[3]));

    let (fx, [user, fut_fn]) = launch_world(PNode::Uint(UintTy::U64));
    let c = await_into_let(&fx, user);
    let got = codes(&c);
    assert!(!got.is_empty(), "TWIN: `Handle<u64, u32>` is not `Handle<u16, A>`, yet the .await is clean");
    assert!(got.iter().all(|&(code, _, _)| code != DiagCode::AnnotationsNeeded), "TWIN: a mismatch, not an undecided type: {got:?}");
    assert_ne!(callee_at(&c, ExprId(1)).map(|r| r.0), Some(fut_fn), "TWIN: no impl is selected");
    std::println!("Fut.await: A = u32 by the projection binding");
}

/// The control: `async fn by_bound<T: IntoFuture<Output = u32>>(x: T) -> u32 { x.await }`.
/// `T` has no impl bucket: its `IntoFuture` is the where-clause's, whose `into_future` no impl
/// defines, so the callee stays the trait's at `[T]` (the flow lowering's relation for an
/// unknown body, as before this piece). Prints "T.await: calls the trait's into_future<T>".
#[test]
fn a_bound_keeps_the_traits_callee() {
    let (fx, it) = world(false);
    let c = await_param(&fx, it.by_bound, PNode::Uint(UintTy::U32));
    assert_eq!(codes(&c), Vec::new(), "no finding");
    assert_eq!(callee_at(&c, ExprId(1)), Some((it.trait_fn, 1)), "the trait's into_future at [T]");
    std::println!("T.await: calls the trait's into_future<T>");
}
