//! A closure's signature from an expected trait object. tempo's zone_payload
//! `PrewarmingExecutionContext::lookahead` (crates/zones/payload/src/prewarming.rs:113):
//!
//! ```text
//! PoolLookahead::new(inner, Box::new(move |tx| context.spawn(.., Simulation::Pool(tx.clone()))))
//! // where  fn new(inner: I, simulate: Box<dyn FnMut(&BestTransaction) + Send>) -> Self
//! ```
//!
//! was a false E0282 at `tx`: nothing typed the closure's parameter before `tx.clone()`.
//! Reduced:
//!
//! ```text
//! trait Wid { fn wid(self) -> u16; }   impl Wid for u8 { .. }
//! struct Box<T: ?Sized>;   impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Box<U>> for Box<T> {}
//! fn box_new<T>(t: T) -> Box<T>;
//! fn g() -> Box<dyn Fn(u8) -> u16> { box_new(|x| x.wid()) }      // clean
//! fn m() { let f: &dyn Fn(u8) -> u16 = &|x| x.wid(); }            // clean
//! fn n() { let f = box_new(|x| x.wid()); }                         // E0282 at `x`
//! ```
//!
//! The rules:
//! - The Reference, "Closure expressions": a closure's parameter types may be left out and are
//!   inferred from context; the context of `|x| x.wid()` in `g` is the formal input of
//!   `box_new`, which the call's expected type `Box<dyn Fn(u8) -> u16>` makes
//!   `dyn Fn(u8) -> u16` (rules/expect_inputs.rs). An unsized type is only a hint for its
//!   argument, which is still coerced to the formal `T` (rustc's `Expectation::rvalue_hint`:
//!   `ty::Slice | ty::Str | ty::Dynamic` give `ExpectRvalueLikeUnsized`, and
//!   `check_argument_types` coerces to `formal_input_ty` when the expectation has no
//!   `only_has_type`). The closure deduces its signature from that object as rustc's
//!   `deduce_closure_signature` does for `ty::Dynamic`: the inputs and output of its
//!   `FnOnce<Args>::Output` projection (library/core/src/ops/function.rs: `trait FnOnce<Args:
//!   Tuple> { type Output; .. }`; `Fn(u8) -> u16` is `Fn<(u8,)>` with `Output = u16`). So
//!   `x: u8`, `x.wid()` is `<u8 as Wid>::wid`, and `Box<closure>` unsizes to the object
//!   (the Reference, "Type coercions": `T` to `dyn U` when `T: U`, through `CoerceUnsized`).
//! - In `m` the expected type of `&|x| ..` is `&dyn Fn(u8) -> u16`, so the closure's is the
//!   object: the same deduction.
//! - In `n` nothing gives the closure a signature: `x` has no type when `x.wid()` is looked up
//!   (the Reference, "Method call expressions": the receiver's type is needed first), E0282.
//!
//! No test here runs a compiler for its answer: each expectation is the rule above.
//!
//! Prints: `test suite::dyn_closure_sig::<name> ... ok` for each test.

use std::vec::Vec;

use super::check_body;
use crate::columns::Checked;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{Abi, ExprId, FnPtrHeader, ItemKey, LangItem, Mutbl, Name, Safety, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::{PNode, PTy};

/// `[g, m, n]`, then `[Fn, FnOnce]`.
fn world() -> (Fx, [ItemKey; 3], [ItemKey; 2]) {
    let mut world = Fx::new();
    // Built through `&mut Fx`, as suite/pin_unsize.rs is: on `Fx` itself `fx.adt(..)` picks
    // `World::adt`.
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce = fx.tr("CoerceUnsized", 2, Vec::new(), &[]);
    fx.lang(LangItem::CoerceUnsized, coerce);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let rust_call = FnPtrHeader { abi: Abi::RustCall, safety: Safety::Safe, c_variadic: false };

    // The `Fn*` traits as suite/fn_traits.rs declares them.
    let fn_once = fx.tr("FnOnce", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::FnOnce, fn_once);
    let out = fx.alias(fn_once, "Output", &[p0, p1]);
    let call_once = fx.trait_fn(fn_once, "call_once", true, &[p0, p1], out);
    fx.set_header(call_once, rust_call);
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

    let (u8_, u16_, unit) = (fx.ty(PNode::Uint(UintTy::U8)), fx.ty(PNode::Uint(UintTy::U16)), fx.unit());

    // `trait Wid { fn wid(self) -> u16; }  impl Wid for u8`.
    let wid = fx.tr("Wid", 1, Vec::new(), &[]);
    fx.trait_fn(wid, "wid", true, &[p0], u16_);
    fx.use_trait(wid);
    let for_u8 = fx.imp("impl Wid for u8", 0, u8_, Some((wid, &[])), &[], Vec::new());
    fx.impl_fn(for_u8, "wid", true, &[u8_], u16_);

    // `struct Box<T: ?Sized>;  impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Box<U>> for Box<T>`.
    let box_k = fx.strukt("Box", 1, &[]);
    let (box_t, box_u) = (fx.adt(box_k, &[p0]), fx.adt(box_k, &[p1]));
    let t_unsize_u = fx.bound(unsize, &[p0, p1]);
    let cu = fx.imp("impl CoerceUnsized<Box<U>> for Box<T>", 2, box_t, Some((coerce, &[box_u])), &[], std::vec![t_unsize_u]);
    fx.unsized_param(cu, 0);
    fx.unsized_param(cu, 1);
    fx.func("box_new", 1, &[p0], box_t, Vec::new());

    // `Box<dyn Fn(u8) -> u16>`.
    let object = fn_object(fx, fn_, fn_once, u8_, u16_);
    let box_object = fx.adt(box_k, &[object]);
    let g = fx.func("g", 0, &[], box_object, Vec::new());
    let m = fx.func("m", 0, &[], unit, Vec::new());
    let n = fx.func("n", 0, &[], unit, Vec::new());
    fx.finish();
    (world, [g, m, n], [fn_, fn_once])
}

/// `dyn Fn(arg) -> ret`: the principal `Fn<(arg,)>` and `FnOnce<(arg,)>::Output = ret`.
fn fn_object(fx: &mut Fx, fn_: ItemKey, fn_once: ItemKey, arg: PTy, ret: PTy) -> PTy {
    let args = fx.tuple(&[arg]);
    let args_l = fx.t.intern_list(&[args]).expect("list");
    let principal = fx.ty(PNode::ExTrait(fn_, args_l));
    let output = fx.ty(PNode::ExProj { trait_: fn_once, name: Name::of("Output"), args: args_l, term: ret });
    fx.t.dynamic(&[principal, output]).expect("dyn")
}

/// `|x| x.wid()`: the closure's id.
fn wid_closure(b: &mut Bb) -> ExprId {
    let x = b.bind("x");
    let lx = b.local(x);
    let body = b.method(lx, "wid", &[]);
    b.closure(&[x], body)
}

/// The codes of `c`'s diagnostics.
fn codes(c: &Checked<'_>) -> Vec<DiagCode> {
    c.diags.iter().map(|d| d.code).collect()
}

/// `fn g() -> Box<dyn Fn(u8) -> u16> { box_new(|x| x.wid()) }` is clean: the call's expected
/// type makes `box_new`'s input `dyn Fn(u8) -> u16`, a hint the closure takes its signature
/// from (`x: u8`). Without the fix (an unsized input says nothing to its argument, or a
/// trait object gives a closure no signature) this fails with
/// "g: unexpected diagnostics: [AnnotationsNeeded]".
#[test]
fn a_closure_takes_its_signature_from_a_boxed_objects_expected_type() {
    let (fx, [g, ..], _) = world();
    let mut b = Bb::new();
    let callee = b.path(&["box_new"]);
    let closure = wid_closure(&mut b);
    let made = b.call(callee, &[closure]);
    let root = b.block(&[], Some(made));
    let c = check_body(&fx, &b, g, root, Plant::None, InferPlant::None);
    let got = codes(&c);
    assert!(got.is_empty(), "g: unexpected diagnostics: {got:?}");
}

/// `fn m() { let f: &dyn Fn(u8) -> u16 = &|x| x.wid(); }` is clean: the closure's expected
/// type is the object. Without the `Dynamic` arm of `deduce_sig` this fails with
/// "m: unexpected diagnostics: [AnnotationsNeeded]".
#[test]
fn a_closure_takes_its_signature_from_a_referenced_object() {
    let (fx, [_, m, _], [fn_, fn_once]) = world();
    let mut b = Bb::new();
    let closure = wid_closure(&mut b);
    let borrowed = b.borrow(Mutbl::Not, closure);
    let u8_ = b.ty(PNode::Uint(UintTy::U8));
    let u16_ = b.ty(PNode::Uint(UintTy::U16));
    let args = {
        let fields = b.t.intern_list(&[u8_]).expect("list");
        b.ty(PNode::Tuple(fields))
    };
    let args_l = b.t.intern_list(&[args]).expect("list");
    let principal = b.ty(PNode::ExTrait(fn_, args_l));
    let output = b.ty(PNode::ExProj { trait_: fn_once, name: Name::of("Output"), args: args_l, term: u16_ });
    let object = b.t.dynamic(&[principal, output]).expect("dyn");
    let r_object = b.ty(PNode::Ref(Mutbl::Not, object));
    let f = b.bind("f");
    let s = b.let_(f, Some(r_object), borrowed);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, m, root, Plant::None, InferPlant::None);
    let got = codes(&c);
    assert!(got.is_empty(), "m: unexpected diagnostics: {got:?}");
}

/// The control: `fn n() { let f = box_new(|x| x.wid()); }` has no expected type, nothing
/// types `x`, and the method call on it stays E0282. A fix that guessed a closure's parameter
/// types fails here with "n: want E0282 (AnnotationsNeeded), got []".
#[test]
fn a_closure_with_no_expected_signature_stays_e0282() {
    let (fx, [_, _, n], _) = world();
    let mut b = Bb::new();
    let callee = b.path(&["box_new"]);
    let closure = wid_closure(&mut b);
    let made = b.call(callee, &[closure]);
    let f = b.bind("f");
    let s = b.let_(f, None, made);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, n, root, Plant::None, InferPlant::None);
    let got = codes(&c);
    assert!(got.contains(&DiagCode::AnnotationsNeeded), "n: want E0282 (AnnotationsNeeded), got {got:?}");
}
