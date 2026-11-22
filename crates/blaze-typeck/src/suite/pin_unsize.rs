//! Unsizing through a pointer nested in a `CoerceUnsized` struct: `Box::pin(async { .. })`
//! where `Pin<Box<dyn Future<Output = u8> + Send>>` is expected (tempo's `BoxFuture` bodies,
//! about 21 of them refused as E0308 / E0277 around boxed futures), `Pin<Rc<T>>`, and a user
//! wrapper with its own impl.
//!
//! The rule. The Reference, "Type coercions", "Unsized Coercions": "Additionally, a type
//! `Foo<T>` can implement `CoerceUnsized<Foo<U>>` when `T` implements `Unsize<U>` or
//! `CoerceUnsized<Foo<U>>`. This allows it to provide an unsized coercion to `Foo<U>`." The
//! pinned library (nightly-2026-10-27's source; the shelf under ~/.blaze/v2/toolchain holds it
//! only as blobs) says which types do:
//! - core/src/pin.rs: `impl<Ptr, U> CoerceUnsized<Pin<U>> for Pin<Ptr> where Ptr:
//!   CoerceUnsized<U> + PinSafePointer, U: PinSafePointer,`
//! - alloc/src/boxed.rs: `impl<T: ?Sized + Unsize<U>, U: ?Sized, A: Allocator>
//!   CoerceUnsized<Box<U, A>> for Box<T, A> {}` and
//!   `unsafe impl<T: ?Sized, A: StaticAllocator> PinSafePointer for Box<T, A> {}`
//! - alloc/src/rcs/rc.rs: `impl<T: ?Sized + Unsize<U>, U: ?Sized, A: Allocator>
//!   CoerceUnsized<Rc<U, A>> for Rc<T, A> {}` and
//!   `unsafe impl<T: ?Sized, A: StaticAllocator> PinSafePointer for Rc<T, A> {}`
//!
//! and "Unsized Coercions": "`T` to `dyn U`, when `T` implements `U + Sized`, and `U` is dyn
//! compatible"; an object's projection (`Output = u8`) and auto traits (`Send`) are bounds the
//! source must meet ("Trait objects"). "Async blocks": the block's type implements
//! `Future<Output = T>`; "Auto traits": it is `Send` when what it holds is.
//!
//! The toy world (the allocator parameter left out):
//! ```text
//! #[lang = "sized"] trait Sized {}  #[lang = "copy"] trait Copy {}
//! #[lang = "unsize"] trait Unsize<U: ?Sized> {}  #[lang = "coerce_unsized"] trait CoerceUnsized<U> {}
//! #[lang = "future_trait"] trait Future { type Output; }  #[lang = "send"] auto trait Send {}
//! trait Never {}  unsafe trait PinSafePointer {}  trait Tr {}
//! struct Foo;  impl Tr for Foo {}
//! struct Local;  impl Send for Local where Local: Never {}   // stands for `impl !Send for Local`
//! struct Box<T: ?Sized>;  impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Box<U>> for Box<T> {}
//!                         unsafe impl<T: ?Sized> PinSafePointer for Box<T> {}
//! struct Rc<T: ?Sized>;   (the same two impls)
//! struct Pin<Ptr> { pointer: Ptr }
//! impl<Ptr, U> CoerceUnsized<Pin<U>> for Pin<Ptr> where Ptr: CoerceUnsized<U> + PinSafePointer, U: PinSafePointer {}
//! struct Wrap<P>(P);  impl<P: CoerceUnsized<U>, U> CoerceUnsized<Wrap<U>> for Wrap<P> {}
//! struct Plain<P>(P);   // no impl
//! fn box_pin<T>(x: T) -> Pin<Box<T>>;   // `Box::pin`
//! ```
//! Without the fix (rules/coerce_nested.rs: the site asked only when an argument of the target
//! IS a `dyn` or slice), tests (a), (b), (d), (e) and the control fail: each coercion is a
//! mismatch. (c) and (f) pass either way: they pin that the wider question refuses what it must.

use std::string::String;
use std::vec::Vec;

use super::{adjusts, check_body};
use crate::body::{Capture, IntSuffix};
use crate::columns::Checked;
use crate::diag::{Because, DiagCode, UnmetGoal};
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ExprId, ItemKey, LangItem, List, Name, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::{PNode, PTy, PTys};

fn world() -> Fx {
    let mut world = Fx::new();
    // Built through `&mut Fx`, as `rung_7c` is: on `Fx` itself `fx.adt(..)` picks `World::adt`.
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let copy = fx.tr("Copy", 1, Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce = fx.tr("CoerceUnsized", 2, Vec::new(), &[]);
    fx.lang(LangItem::CoerceUnsized, coerce);
    let future = fx.tr("Future", 1, Vec::new(), &["Output"]);
    fx.lang(LangItem::Future, future);
    let send = fx.tr("Send", 1, Vec::new(), &[]);
    fx.auto(send);
    fx.lang(LangItem::Send, send);
    let never = fx.tr("Never", 1, Vec::new(), &[]);
    let pin_safe = fx.tr("PinSafePointer", 1, Vec::new(), &[]);
    let tr = fx.tr("Tr", 1, Vec::new(), &[]);
    let (p0, p1, u8_, u16_) = (fx.param(0), fx.param(1), fx.u8(), fx.u16());

    let foo_k = fx.strukt("Foo", 0, &[]);
    let foo = fx.adt(foo_k, &[]);
    fx.imp("impl Tr for Foo", 0, foo, Some((tr, &[])), &[], Vec::new());
    let local_k = fx.strukt("Local", 0, &[]);
    let local = fx.adt(local_k, &[]);
    let local_never = fx.bound(never, &[local]);
    fx.imp("impl Send for Local where Local: Never", 0, local, Some((send, &[])), &[], std::vec![local_never]);

    // `Box` and `Rc`: the pointee unsizes, and the pointer is pin safe.
    let pointer = |fx: &mut Fx, name: &str| -> ItemKey {
        let k = fx.strukt(name, 1, &[]);
        let (t, u) = (fx.adt(k, &[p0]), fx.adt(k, &[p1]));
        let t_unsize_u = fx.bound(unsize, &[p0, p1]);
        let cu = fx.imp(&std::format!("impl CoerceUnsized<{name}<U>> for {name}<T>"), 2, t, Some((coerce, &[u])), &[], std::vec![t_unsize_u]);
        fx.unsized_param(cu, 0);
        fx.unsized_param(cu, 1);
        let ps = fx.imp(&std::format!("impl PinSafePointer for {name}<T>"), 1, t, Some((pin_safe, &[])), &[], Vec::new());
        fx.unsized_param(ps, 0);
        k
    };
    let box_k = pointer(fx, "Box");
    let rc_k = pointer(fx, "Rc");

    let pin_k = fx.strukt("Pin", 1, &[("pointer", p0)]);
    let (pin_ptr, pin_u) = (fx.adt(pin_k, &[p0]), fx.adt(pin_k, &[p1]));
    let ptr_cu_u = fx.bound(coerce, &[p0, p1]);
    let ptr_safe = fx.bound(pin_safe, &[p0]);
    let u_safe = fx.bound(pin_safe, &[p1]);
    fx.imp(
        "impl<Ptr, U> CoerceUnsized<Pin<U>> for Pin<Ptr>",
        2,
        pin_ptr,
        Some((coerce, &[pin_u])),
        &[],
        std::vec![ptr_cu_u, ptr_safe, u_safe],
    );

    let wrap_k = fx.tuple_strukt("Wrap", 1, &[p0]);
    let (wrap_p, wrap_u) = (fx.adt(wrap_k, &[p0]), fx.adt(wrap_k, &[p1]));
    let p_cu_u = fx.bound(coerce, &[p0, p1]);
    fx.imp("impl<P: CoerceUnsized<U>, U> CoerceUnsized<Wrap<U>> for Wrap<P>", 2, wrap_p, Some((coerce, &[wrap_u])), &[], std::vec![p_cu_u]);
    let plain_k = fx.tuple_strukt("Plain", 1, &[p0]);

    let ex_fut = fx.ty(PNode::ExTrait(future, List::EMPTY));
    let ex_send = fx.ty(PNode::ExAuto(send));
    let ex_tr = fx.ty(PNode::ExTrait(tr, List::EMPTY));
    let output = |fx: &mut Fx, term: PTy| fx.ty(PNode::ExProj { trait_: future, name: Name::of("Output"), args: List::EMPTY, term });
    let (out_u8, out_u16) = (output(fx, u8_), output(fx, u16_));
    let dyn_fut_u8 = fx.t.dynamic(&[ex_send, ex_fut, out_u8]).expect("dyn");
    let dyn_fut_u16 = fx.t.dynamic(&[ex_send, ex_fut, out_u16]).expect("dyn");
    let dyn_tr = fx.t.dynamic(&[ex_tr]).expect("dyn");

    // `outer<inner<t>>`.
    let nest = |fx: &mut Fx, outer: ItemKey, inner: ItemKey, t: PTy| {
        let i = fx.adt(inner, &[t]);
        fx.adt(outer, &[i])
    };
    let pin_box_p0 = nest(fx, pin_k, box_k, p0);
    fx.func("box_pin", 1, &[p0], pin_box_p0, Vec::new());
    let ret_u8 = nest(fx, pin_k, box_k, dyn_fut_u8);
    let ret_u16 = nest(fx, pin_k, box_k, dyn_fut_u16);
    fx.func("a_send_future", 0, &[], ret_u8, Vec::new());
    fx.func("b_local_future", 0, &[local], ret_u8, Vec::new());
    fx.func("c_output_u16", 0, &[], ret_u16, Vec::new());
    let (pin_rc_foo, pin_rc_dyn) = (nest(fx, pin_k, rc_k, foo), nest(fx, pin_k, rc_k, dyn_tr));
    fx.func("d_pin_rc", 0, &[pin_rc_foo], pin_rc_dyn, Vec::new());
    let (wrap_foo, wrap_dyn) = (nest(fx, wrap_k, box_k, foo), nest(fx, wrap_k, box_k, dyn_tr));
    fx.func("e_wrap", 0, &[wrap_foo], wrap_dyn, Vec::new());
    let (plain_foo, plain_dyn) = (nest(fx, plain_k, box_k, foo), nest(fx, plain_k, box_k, dyn_tr));
    fx.func("f_plain", 0, &[plain_foo], plain_dyn, Vec::new());
    fx.finish();
    world
}

/// `fn <owner>(..) -> Pin<Box<dyn Future<Output = ..> + Send>> { box_pin(async { 7u8 }) }`, or
/// with `take_local`, `fn <owner>(r: Local) -> .. { box_pin(async move { r; 7u8 }) }`: the
/// call (the coercion site), the async block (the source of the output), and the check.
fn async_case(fx: &Fx, owner: &str, take_local: bool) -> (ExprId, ExprId, Checked<'static>) {
    async_case_with_output(fx, owner, take_local, UintTy::U8)
}

fn async_case_with_output(fx: &Fx, owner: &str, take_local: bool, output: UintTy) -> (ExprId, ExprId, Checked<'static>) {
    let mut b = Bb::new();
    let r = if take_local { Some(b.param("r")) } else { None };
    let callee = b.path(&["box_pin"]);
    let mut stmts = Vec::new();
    if let Some(r) = r {
        let e = b.local(r);
        stmts.push(b.semi(e));
    }
    let seven = b.int(Some(IntSuffix::Uint(output)));
    let body = b.block(&stmts, Some(seven));
    let fut = b.async_block(body, if take_local { Capture::Move } else { Capture::Ref });
    let call = b.call(callee, &[fut]);
    let root = b.block(&[], Some(call));
    let c = check_body(fx, &b, fx.key_of(owner), root, Plant::None, InferPlant::None);
    (call, fut, c)
}

/// `fn <owner>(x: ..) -> .. { x }`: the tail's id and the check.
fn param_case(fx: &Fx, owner: &str, plant: Plant) -> (ExprId, Checked<'static>) {
    let mut b = Bb::new();
    let x = b.param("x");
    let e0 = b.local(x);
    let root = b.block(&[], Some(e0));
    let c = check_body(fx, &b, fx.key_of(owner), root, plant, InferPlant::None);
    (e0, c)
}

fn codes(c: &Checked<'_>) -> Vec<DiagCode> {
    c.diags.iter().map(|d| d.code).collect()
}

/// An unmet goal printed: `Self: Trait<A, ..>`, or `<Self as Trait<..>>::name == Term`.
fn goal_shown(fx: &Fx, types: PTys<'_>, g: UnmetGoal) -> String {
    let (UnmetGoal::Trait { trait_, args } | UnmetGoal::Projection { trait_, args, .. }) = g;
    let all: Vec<String> = types.list(args).expect("in the table").iter().map(|&t| show(fx, types, t)).collect();
    let (self_, rest) = all.split_first().map_or(("?", &[][..]), |(s, r)| (s.as_str(), r));
    let tr = if rest.is_empty() { String::from(fx.name_of(trait_)) } else { std::format!("{}<{}>", fx.name_of(trait_), rest.join(", ")) };
    match g {
        UnmetGoal::Trait { .. } => std::format!("{self_}: {tr}"),
        UnmetGoal::Projection { name, term, .. } => std::format!("<{self_} as {tr}>::{name:?} == {}", show(fx, types, term)),
    }
}

/// (a) `let f: Pin<Box<dyn Future<Output = u8> + Send>> = Box::pin(async { 7u8 });` checks:
/// `Pin<Box<{async}>>: CoerceUnsized<Pin<Box<dyn ..>>>` by pin.rs's impl, its
/// `Box<{async}>: CoerceUnsized<Box<dyn ..>>` by boxed.rs's, and `{async}: Unsize<dyn Future<Output
/// = u8> + Send>` by "Unsized Coercions" (the block is `Future<Output = u8>`, `Send` by what it
/// holds, `Sized`). No finding, and the call's one adjustment is the unsize to the return type.
/// Fails without the fix (a mismatch: the target's argument `Box<dyn ..>` is not itself a `dyn`).
/// Should print: "test suite::pin_unsize::a_boxed_async_block_coerces_to_a_pinned_boxed_object ... ok".
#[test]
fn a_boxed_async_block_coerces_to_a_pinned_boxed_object() {
    let fx = world();
    let (call, _, c) = async_case(&fx, "a_send_future", false);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let got = adjusts(&fx, &c, call);
    assert!(got.starts_with("unsize -> Pin<Box<dyn ") && got.matches("unsize").count() == 1, "adjustments `{got}`");
}

/// (b) The same with an `async move` block that holds a `Local` (not `Send`): "Auto traits":
/// the block is `Send` only when what it holds is, so `{async}: Unsize<dyn Future<Output = u8> +
/// Send>` does not hold and the coercion is refused as an unmet bound (E0277) whose goal names
/// `Send`, never as a mismatch. Fails without the fix (E0308 instead).
/// Should print: "test suite::pin_unsize::a_boxed_async_block_holding_a_non_send_value_is_refused_naming_send ... ok".
#[test]
fn a_boxed_async_block_holding_a_non_send_value_is_refused_naming_send() {
    let fx = world();
    let (_, _, c) = async_case(&fx, "b_local_future", true);
    assert_eq!(codes(&c), std::vec![DiagCode::Unsatisfied], "{:?}", c.diags);
    let d = &c.diags[0];
    let why = match d.because {
        Some(Because::Unmet { goal, leaf }) => {
            let mut s = goal_shown(&fx, c.types.view(), goal);
            if let Some(l) = leaf {
                s.push_str(" because ");
                s.push_str(&goal_shown(&fx, c.types.view(), l));
            }
            s
        }
        other => std::format!("{other:?}"),
    };
    assert!(why.contains("Send"), "the finding does not name `Send`: {why}");
}

/// (c) `Output = u16` expected, a `u8` block: "Unsized Coercions" needs the object's projection
/// `<{async} as Future>::Output == u16`, and it is `u8` ("Async blocks"), so no unsizing; the
/// coercion is refused (E0271) at the async block, and no unsize is
/// recorded. This catches a hint being used as the block's output instead of being checked
/// against it.
/// Should print: "test suite::pin_unsize::a_boxed_async_block_with_another_output_is_refused ... ok".
/// The matching `u16` body is accepted by the same object projection.
#[test]
fn a_boxed_async_block_with_another_output_is_refused() {
    let fx = world();
    let (call, block, c) = async_case(&fx, "c_output_u16", false);
    assert_eq!(codes(&c), std::vec![DiagCode::ProjectionMismatch], "{:?}", c.diags);
    let d = &c.diags[0];
    assert_eq!(d.code.code(), "E0271");
    assert_eq!(d.node, Some(crate::diag::Node::Expr(block)));
    assert_eq!(show(&fx, c.types.view(), d.expected.expect("required Output")), "u16");
    assert_eq!(show(&fx, c.types.view(), d.found.expect("block Output")), "u8");
    assert!(matches!(d.because, Some(Because::Unmet {
        goal: UnmetGoal::Projection { trait_, name, term, .. }, leaf: None,
    }) if trait_ == fx.key_of("Future") && name == Name::of("Output") && Some(term) == d.expected));
    assert!(!c.adjusts.column::<1>().contains(&crate::columns::AdjustKind::Unsize), "{}", adjusts(&fx, &c, call));
}

#[test]
fn a_boxed_async_block_with_matching_output_is_accepted() {
    let fx = world();
    let (call, _, c) = async_case_with_output(&fx, "c_output_u16", false, UintTy::U16);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let got = adjusts(&fx, &c, call);
    assert!(got.starts_with("unsize -> Pin<Box<dyn ") && got.matches("unsize").count() == 1, "adjustments `{got}`");
}

/// (d) `fn d_pin_rc(x: Pin<Rc<Foo>>) -> Pin<Rc<dyn Tr>> { x }`: pin.rs's impl over rc.rs's
/// (`Rc<T>: CoerceUnsized<Rc<U>>`, `Rc<T>: PinSafePointer`) and `Foo: Unsize<dyn Tr>`. Fails
/// without the fix.
/// Should print: "test suite::pin_unsize::a_pinned_rc_coerces_to_a_pinned_rc_object ... ok".
#[test]
fn a_pinned_rc_coerces_to_a_pinned_rc_object() {
    let fx = world();
    let (e, c) = param_case(&fx, "d_pin_rc", Plant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert_eq!(adjusts(&fx, &c, e), "unsize -> Pin<Rc<dyn Tr>>");
}

/// (e) A user wrapper with its own impl, `impl<P: CoerceUnsized<U>, U> CoerceUnsized<Wrap<U>> for
/// Wrap<P>` (the Reference's "`Foo<T>` can implement `CoerceUnsized<Foo<U>>` when `T` implements
/// .. `CoerceUnsized<Foo<U>>`"): `Wrap<Box<Foo>>` to `Wrap<Box<dyn Tr>>` coerces, with no special
/// case for any library type. Fails without the fix.
/// Should print: "test suite::pin_unsize::a_user_wrapper_with_its_impl_coerces ... ok".
#[test]
fn a_user_wrapper_with_its_impl_coerces() {
    let fx = world();
    let (e, c) = param_case(&fx, "e_wrap", Plant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert_eq!(adjusts(&fx, &c, e), "unsize -> Wrap<Box<dyn Tr>>");
}

/// (f) `Plain<P>(P)` has no `CoerceUnsized` impl: "Coercion types" lists no other constructor,
/// so `Plain<Box<Foo>>` to `Plain<Box<dyn Tr>>` is refused (E0308), nothing recorded. Passes
/// with or without the fix: the deeper filter asks, and the solver says no.
/// Should print: "test suite::pin_unsize::a_wrapper_without_the_impl_is_refused ... ok".
#[test]
fn a_wrapper_without_the_impl_is_refused() {
    let fx = world();
    let (e, c) = param_case(&fx, "f_plain", Plant::None);
    assert_eq!(codes(&c), std::vec![DiagCode::Mismatch], "{:?}", c.diags);
    assert_eq!(adjusts(&fx, &c, e), "");
}

/// The control for (f): with the planted bug that takes an unsizing without asking the solver
/// (`Plant::UnsizeUnasked`), `Plain<Box<Foo>>` to `Plain<Box<dyn Tr>>` is accepted: so it is
/// the `CoerceUnsized` question, not the filter, that refuses (f). Fails without the fix (the
/// old filter never reached the question, so the plant changed nothing).
/// Should print: "with the bug: accepted" and "test suite::pin_unsize::control_plain_wrapper_unasked_is_accepted ... ok".
#[test]
fn control_plain_wrapper_unasked_is_accepted() {
    let fx = world();
    let (e, c) = param_case(&fx, "f_plain", Plant::UnsizeUnasked);
    let accepted = c.diags.is_empty() && adjusts(&fx, &c, e) == "unsize -> Plain<Box<dyn Tr>>";
    std::println!("with the bug: {}", if accepted { "accepted" } else { "refused" });
    assert!(accepted, "the unasked unsizing was not taken: {:?}", c.diags);
}
