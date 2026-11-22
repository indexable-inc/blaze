//! An `async fn`'s opaque future, boxed and pinned, coerced to `Pin<Box<dyn Future<Output =
//! u32> + Send>>` (rung s9-app-03's `spawn(Box::pin(worker("a", 3, 2)))`, std census 3 class C):
//! rules/coerce_interior.rs.
//!
//! The rule. The Reference, "Type coercions", "Unsized Coercions": "`T` to `dyn U`, when `T`
//! implements `U + Sized`", and "a type `Foo<T>` can implement `CoerceUnsized<Foo<U>>` when `T`
//! implements `Unsize<U>` or `CoerceUnsized<Foo<U>>`"; core/src/pin.rs `impl<Ptr, U>
//! CoerceUnsized<Pin<U>> for Pin<Ptr> where Ptr: CoerceUnsized<U> + PinSafePointer, U:
//! PinSafePointer`; alloc/src/boxed.rs `impl<T: ?Sized + Unsize<U>, U: ?Sized, A: Allocator>
//! CoerceUnsized<Box<U, A>> for Box<T, A> {}`. "Impl trait", "Auto trait leakage": the opaque is
//! `Send` when its hidden future is; "Auto traits": a future is `Send` when what it holds across
//! an `.await` is; core/src/fmt/mod.rs `impl !Send for Arguments<'_> {}`.
//!
//! The toy world (the allocator parameter left out):
//! ```text
//! #[lang = "sized"] trait Sized {}  #[lang = "unsize"] trait Unsize<U: ?Sized> {}
//! #[lang = "coerce_unsized"] trait CoerceUnsized<U> {}  #[lang = "send"] auto trait Send {}
//! #[lang = "future_trait"] trait Future { type Output; }  unsafe trait PinSafePointer {}
//! struct Args;  impl !Send for Args {}            // stands for `fmt::Arguments`
//! struct Box<T: ?Sized>;  impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Box<U>> for Box<T> {}
//!                         unsafe impl<T: ?Sized> PinSafePointer for Box<T> {}
//! struct Pin<Ptr> { pointer: Ptr }
//! impl<Ptr, U> CoerceUnsized<Pin<U>> for Pin<Ptr> where Ptr: CoerceUnsized<U> + PinSafePointer, U: PinSafePointer {}
//! fn box_pin<T>(x: T) -> Pin<Box<T>>;  fn boxed<T>(x: T) -> Box<T>;
//! async fn wf() -> u32 { .. }   // hidden: its coroutine 0, output u32, interior (u32,) or (Args,)
//! fn user() -> <target> { box_pin(wf()) }     // or `boxed(wf())`
//! ```
//! The interior `(Args,)` is the defining body's SUPERSET (rules/coroutine.rs: every type the
//! body names, here a `format!` temporary of a statement after the `.await`), so the caller's
//! `O: Send` failing on it is the interior gap, not an answer.

use std::vec::Vec;

use super::adjusts;
use crate::columns::Checked;
use crate::diag::DiagCode;
use crate::error::CheckError;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, LangItem, List, Name};
use crate::machine::Plant;
use crate::portable::{PNode, PTy};

/// Which target `user` returns, and through which pointer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    /// `Pin<Box<dyn Future<Output = u32> + Send>>`, made by `box_pin`.
    PinBoxU32,
    /// `Pin<Box<dyn Future<Output = u16> + Send>>`, made by `box_pin`: the wrong output.
    PinBoxU16,
    /// `Box<dyn Future<Output = u32> + Send>`, made by `boxed`.
    BoxU32,
}

/// The world above; `superset_args` picks `wf`'s filed interior: `(Args,)` when true, `(u32,)`
/// when false.
fn world(superset_args: bool, target: Target) -> Fx {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce = fx.tr("CoerceUnsized", 2, Vec::new(), &[]);
    fx.lang(LangItem::CoerceUnsized, coerce);
    let send = fx.tr("Send", 1, Vec::new(), &[]);
    fx.auto(send);
    fx.lang(LangItem::Send, send);
    let future = fx.tr("Future", 1, Vec::new(), &["Output"]);
    fx.lang(LangItem::Future, future);
    let pin_safe = fx.tr("PinSafePointer", 1, Vec::new(), &[]);
    let (p0, p1, u16_, u32_) = (fx.param(0), fx.param(1), fx.u16(), fx.u32());

    let args_k = fx.strukt("Args", 0, &[]);
    let args_ty = fx.adt(args_k, &[]);
    let not_send = fx.imp("impl !Send for Args", 0, args_ty, Some((send, &[])), &[], Vec::new());
    fx.set_polarity(not_send, crate::world::ImplPolarity::Negative);

    let box_k = fx.strukt("Box", 1, &[]);
    let (box_t, box_u) = (fx.adt(box_k, &[p0]), fx.adt(box_k, &[p1]));
    let t_unsize_u = fx.bound(unsize, &[p0, p1]);
    let cu = fx.imp("impl CoerceUnsized<Box<U>> for Box<T>", 2, box_t, Some((coerce, &[box_u])), &[], std::vec![t_unsize_u]);
    fx.unsized_param(cu, 0);
    fx.unsized_param(cu, 1);
    let ps = fx.imp("impl PinSafePointer for Box<T>", 1, box_t, Some((pin_safe, &[])), &[], Vec::new());
    fx.unsized_param(ps, 0);

    let pin_k = fx.strukt("Pin", 1, &[("pointer", p0)]);
    let (pin_ptr, pin_u) = (fx.adt(pin_k, &[p0]), fx.adt(pin_k, &[p1]));
    let ptr_cu_u = fx.bound(coerce, &[p0, p1]);
    let ptr_safe = fx.bound(pin_safe, &[p0]);
    let u_safe = fx.bound(pin_safe, &[p1]);
    fx.imp("impl<Ptr, U> CoerceUnsized<Pin<U>> for Pin<Ptr>", 2, pin_ptr, Some((coerce, &[pin_u])), &[], std::vec![ptr_cu_u, ptr_safe, u_safe]);

    let box_p0 = fx.adt(box_k, &[p0]);
    let pin_box_p0 = fx.adt(pin_k, &[box_p0]);
    fx.func("box_pin", 1, &[p0], pin_box_p0, Vec::new());
    fx.func("boxed", 1, &[p0], box_p0, Vec::new());

    // `async fn wf() -> u32`: opaque 0 with `Future`, `Output = u32`, `Sized`, and the defining
    // body's record (the hidden coroutine by identity, its output and filed interior).
    let o = fx.opaque_key("wf::{opaque#0}");
    let none = fx.list(&[]);
    let opaque = fx.ty(PNode::Opaque(o, none));
    let is_future = fx.bound(future, &[opaque]);
    let out_u32 = fx.proj(future, &[opaque], "Output", u32_);
    let is_sized = fx.bound(sized, &[opaque]);
    fx.set_opaque_bounds(o, std::vec![is_future, out_u32, is_sized]);
    let wf = fx.func("wf", 0, &[], opaque, Vec::new());
    let hidden = fx.ty(PNode::Coroutine { owner: wf, ordinal: 0, args: none });
    let held = if superset_args { args_ty } else { u32_ };
    let interior = fx.tuple(&[held]);
    fx.hidden.push((o, hidden));
    fx.coroutines.push((wf, 0, u32_, interior));
    // The `(Args,)` row is the superset a refused flow leaves; the `(u32,)` row is exact.
    if superset_args {
        fx.provisional_coroutines.push((wf, 0));
    }

    let ex_fut = fx.ty(PNode::ExTrait(future, List::EMPTY));
    let ex_send = fx.ty(PNode::ExAuto(send));
    let out_term: PTy = if target == Target::PinBoxU16 { u16_ } else { u32_ };
    let out = fx.ty(PNode::ExProj { trait_: future, name: Name::of("Output"), args: List::EMPTY, term: out_term });
    let object = fx.t.dynamic(&[ex_send, ex_fut, out]).expect("dyn");
    let box_obj = fx.adt(box_k, &[object]);
    let ret = match target {
        Target::PinBoxU32 | Target::PinBoxU16 => fx.adt(pin_k, &[box_obj]),
        Target::BoxU32 => box_obj,
    };
    fx.func("user", 0, &[], ret, Vec::new());
    fx.finish();
    world
}

/// `fn user() -> <target> { box_pin(wf()) }` (or `boxed(wf())`): the call's id (the coercion
/// site, the body's tail) and the check's result, the gap included.
fn run(fx: &Fx, target: Target) -> (ExprId, Result<Checked<'static>, CheckError>) {
    let mut b = Bb::new();
    let wf = b.path(&["wf"]);
    let fut = b.call(wf, &[]);
    let maker = b.path(&[if target == Target::BoxU32 { "boxed" } else { "box_pin" }]);
    let call = b.call(maker, &[fut]);
    let root = b.block(&[], Some(call));
    let alloc = blaze_own::testing::heap().alloc();
    let mut cx = crate::infer::InferCtx::new_in(alloc);
    let needs = crate::world::Needs::new_in(alloc);
    let got = match crate::check_with(&b.view(fx.key_of("user"), root), fx, &mut cx, &needs, Plant::None, alloc) {
        Ok(crate::Outcome::Done(c)) => Ok(c),
        Ok(crate::Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => Err(e),
    };
    (call, got)
}

fn is_interior_gap<T>(got: &Result<T, CheckError>) -> bool {
    matches!(got, Err(CheckError::FutureWitness { reason, at }) if *reason == crate::INTERIOR_GAP && at.start < at.end)
}

/// (1) The opaque's hidden future holds `(u32,)`: `Pin<Box<O>>` to `Pin<Box<dyn Future<Output =
/// u32> + Send>>` is clean, through pin.rs's impl, boxed.rs's, and `O: Unsize<dyn ..>` from the
/// opaque's bounds (`Future`, `Output = u32`, `Sized`) and its leaked `Send`. One adjustment,
/// the unsize. Passes with or without the fix (it pins that the site handles an opaque source).
/// Should print: "test suite::pin_unsize_interior::a_pinned_boxed_send_opaque_future_coerces ... ok".
#[test]
fn a_pinned_boxed_send_opaque_future_coerces() {
    let fx = world(false, Target::PinBoxU32);
    let (call, got) = run(&fx, Target::PinBoxU32);
    let c = got.expect("checked");
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let adj = adjusts(&fx, &c, call);
    assert!(adj.starts_with("unsize -> Pin<Box<dyn ") && adj.matches("unsize").count() == 1, "adjustments `{adj}`");
}

/// (2) The hidden future's FILED interior is the superset `(Args,)`, `Args` not `Send`: the
/// refusal rests only on the superset, so the check stops at the interior gap, never the E0308
/// "found Pin<Box<Opaque(..)>>, expected Pin<Box<Dynamic(..)>>" of rung s9-app-03. Fails
/// without the fix (Ok with one `Mismatch`).
/// Should print: "test suite::pin_unsize_interior::a_superset_interior_is_the_gap_not_a_mismatch ... ok".
#[test]
fn a_superset_interior_is_the_gap_not_a_mismatch() {
    let fx = world(true, Target::PinBoxU32);
    let (_, got) = run(&fx, Target::PinBoxU32);
    let codes: Option<Vec<DiagCode>> = got.as_ref().ok().map(|c| c.diags.iter().map(|d| d.code).collect());
    assert!(is_interior_gap(&got), "not the interior gap: {codes:?} / {:?}", got.as_ref().err());
}

/// (3) Control: the target's `Output = u16`, the opaque's `u32`. Dropping the auto traits does
/// not make the question hold, so the `No` is the answer: the coercion is refused (E0271) and
/// no unsize is recorded, with the superset interior or without it. Passes with or without the
/// fix: a fix that took every `No` near a future as the gap fails it.
/// Should print: "test suite::pin_unsize_interior::control_a_wrong_output_stays_refused ... ok".
#[test]
fn control_a_wrong_output_stays_refused() {
    for superset in [false, true] {
        let fx = world(superset, Target::PinBoxU16);
        let (call, got) = run(&fx, Target::PinBoxU16);
        let c = got.expect("a finding, not a gap");
        let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
        assert_eq!(codes, std::vec![DiagCode::ProjectionMismatch], "superset {superset}: {:?}", c.diags);
        assert!(!adjusts(&fx, &c, call).contains("unsize"));
    }
}

/// (4) No `Pin`: `Box<O>` to `Box<dyn Future<Output = u32> + Send>` (boxed.rs's impl alone)
/// coerces with `(u32,)`, and is the interior gap with the superset `(Args,)`. The second half
/// fails without the fix.
/// Should print: "test suite::pin_unsize_interior::a_boxed_opaque_future_without_pin ... ok".
#[test]
fn a_boxed_opaque_future_without_pin() {
    let fx = world(false, Target::BoxU32);
    let (call, got) = run(&fx, Target::BoxU32);
    let c = got.expect("checked");
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert!(adjusts(&fx, &c, call).starts_with("unsize -> Box<dyn "));
    let fx = world(true, Target::BoxU32);
    let (_, got) = run(&fx, Target::BoxU32);
    assert!(is_interior_gap(&got), "not the interior gap: {:?}", got.as_ref().err());
}

/// Vary the candidate types independently of the producer's precision. A provisional
/// tuple of sendable values cannot establish Send. An exact tuple containing Args
/// does establish the independent negative, through either pointer coercion.
#[test]
fn precision_controls_whether_the_stored_types_can_decide_send() {
    for target in [Target::PinBoxU32, Target::BoxU32] {
        let mut provisional = world(true, target);
        let u32_ = provisional.u32();
        let candidates = provisional.tuple(&[u32_]);
        provisional.coroutines[0].3 = candidates;
        let (_, got) = run(&provisional, target);
        assert!(is_interior_gap(&got), "a provisional (u32,) decided Send");

        let mut exact = world(false, target);
        let args = exact.adt(exact.key_of("Args"), &[]);
        let stored = exact.tuple(&[args]);
        exact.coroutines[0].3 = stored;
        let (_, got) = run(&exact, target);
        let c = got.expect("an exact non-Send field decides the coercion");
        let codes: Vec<_> = c.diags.iter().map(|d| d.code).collect();
        assert_eq!(codes, [DiagCode::Unsatisfied]);
    }
}
