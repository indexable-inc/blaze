//! A closure handed to `std::panic::set_hook` takes its signature from the hook's trait object:
//! the reduction of tempo census 14's E0282 in `reth_state_root_antithesis::main`
//! (contrib/antithesis/reth-state-root/src/main.rs:653):
//!
//! ```text
//! std::panic::set_hook(Box::new(move |information| {
//!     assert_unreachable!("..", &json!({"panic": information.to_string()}));
//!     previous_hook(information);
//! }));
//! // library/std/src/panicking.rs:140:
//! // pub fn set_hook(hook: Box<dyn Fn(&PanicHookInfo<'_>) + 'static + Sync + Send>)
//! ```
//!
//! The census reported it "at `assert_unreachable!(..)`: found None, expected None": a
//! diagnostic inside an expansion is placed at its call. It is not the expansion's: the same
//! `assert_helper!` + `json!` expansion (antithesis_sdk-0.3.0 src/assert/macros.rs:35-86, a
//! `let details = &json!(..)` passed to `assert_impl<S: Serialize>(.., details: &S, ..)`) is
//! checked clean in the same body by `assert_always_or_unreachable!(result.is_ok(), ..,
//! &json!({..}))` at main.rs:669, the body's one diagnostic being the closure's. What differs
//! is `information`: a closure parameter with no written type, used as a method receiver
//! (`information.to_string()` inside `json!`), typed only by the object `set_hook`'s formal
//! makes `Box::new`'s input. That is the class e0282-1 (wiring/blaze-typeck/e0282-1.md:
//! rules/expect_inputs.rs `unsized_arg_hints`, rules/closure.rs `object_closure_sig`) closes;
//! this file proves it for the panic hook's shape, which e0282-1's own tests do not have:
//! - the expected `Box<dyn ..>` comes from a CALL's formal input (`set_hook(Box::new(..))`),
//!   not from the body's return type;
//! - the object has auto-trait rows (`+ Sync + Send`) after its principal and its
//!   `FnOnce::Output = ()` row (the Reference, "Trait objects": "Trait objects are written as
//!   ... followed by zero or more auto traits");
//! - the closure's input is a REFERENCE (`&PanicHookInfo<'_>`, here `&u8`; the fixture has no
//!   regions, so the higher-ranked `for<'a>` is not modelled) and the method is found through
//!   autoderef (the Reference, "Method call expressions": "the receiver expression is
//!   repeatedly dereferenced"), as `to_string` is on `&PanicHookInfo`.
//!
//! Reduced:
//!
//! ```text
//! trait Wid { fn wid(self) -> u16; }   impl Wid for u8 { .. }
//! fn box_new<T>(t: T) -> Box<T>;
//! fn set_hook(hook: Box<dyn Fn(&u8) + Sync + Send>);
//! fn quiet(hook: Box<dyn Sync + Send>);
//! fn h() { set_hook(box_new(move |information| { information.wid(); })) }   // clean
//! fn q() { quiet(box_new(move |information| { information.wid(); })) }      // E0282
//! ```
//!
//! The rules: the Reference, "Closure expressions": a closure's parameter types may be left out
//! and are inferred from context; `set_hook`'s formal makes `box_new`'s input
//! `dyn Fn(&u8) + Sync + Send`, whose `Fn<(&u8,)>` principal and `FnOnce<(&u8,)>::Output = ()`
//! row give the closure `(&u8) -> ()` (library/core/src/ops/function.rs: `trait FnOnce<Args:
//! Tuple> { type Output; .. }`); `information: &u8`, and `information.wid()` is `<u8 as
//! Wid>::wid` after one deref. In `q` the object names no `Fn*` trait: nothing gives the closure
//! a signature, `information` has no type when its method is looked up: E0282.
//!
//! No test here runs a compiler for its answer: each expectation is the rule above.
//!
//! Prints: `test suite::panic_hook_closure::<name> ... ok` for each of the two tests.

use std::vec::Vec;

use super::check_body;
use crate::columns::Checked;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{Abi, ExprId, FnPtrHeader, ItemKey, LangItem, Mutbl, Name, Safety, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// `[h, q]`: the two bodies' owners.
fn world() -> (Fx, [ItemKey; 2]) {
    let mut world = Fx::new();
    // Built through `&mut Fx`, as suite/dyn_closure_sig.rs is: on `Fx` itself `fx.adt(..)`
    // picks `World::adt`.
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce = fx.tr("CoerceUnsized", 2, Vec::new(), &[]);
    fx.lang(LangItem::CoerceUnsized, coerce);
    let send = fx.tr("Send", 1, Vec::new(), &[]);
    fx.auto(send);
    let sync = fx.tr("Sync", 1, Vec::new(), &[]);
    fx.auto(sync);
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

    // `Box<dyn Fn(&u8) + Sync + Send>`: the principal `Fn<(&u8,)>`, `FnOnce<(&u8,)>::Output =
    // ()`, then the two auto rows (`dynamic` sorts them after the principal and the projection).
    let r_u8 = fx.r(Mutbl::Not, u8_);
    let args = fx.tuple(&[r_u8]);
    let args_l = fx.t.intern_list(&[args]).expect("list");
    let principal = fx.ty(PNode::ExTrait(fn_, args_l));
    let output = fx.ty(PNode::ExProj { trait_: fn_once, name: Name::of("Output"), args: args_l, term: unit });
    let (ex_send, ex_sync) = (fx.ty(PNode::ExAuto(send)), fx.ty(PNode::ExAuto(sync)));
    let hook = fx.t.dynamic(&[ex_send, principal, ex_sync, output]).expect("dyn");
    let box_hook = fx.adt(box_k, &[hook]);
    fx.func("set_hook", 0, &[box_hook], unit, Vec::new());

    // `Box<dyn Sync + Send>`: an object with no `Fn*` row.
    let quiet = fx.t.dynamic(&[ex_send, ex_sync]).expect("dyn");
    let box_quiet = fx.adt(box_k, &[quiet]);
    fx.func("quiet", 0, &[box_quiet], unit, Vec::new());

    let h = fx.func("h", 0, &[], unit, Vec::new());
    let q = fx.func("q", 0, &[], unit, Vec::new());
    fx.finish();
    (world, [h, q])
}

/// `callee(box_new(move |information| { information.wid(); }))` as the body's tail: its root.
fn hook_call(b: &mut Bb, callee: &str) -> ExprId {
    let information = b.bind("information");
    let local = b.local(information);
    let used = b.method(local, "wid", &[]);
    let stmt = b.semi(used);
    let body = b.block(&[stmt], None);
    let closure = b.move_closure(&[information], body);
    let box_new = b.path(&["box_new"]);
    let boxed = b.call(box_new, &[closure]);
    let callee = b.path(&[callee]);
    let made = b.call(callee, &[boxed]);
    b.block(&[], Some(made))
}

/// The codes of `c`'s diagnostics.
fn codes(c: &Checked<'_>) -> Vec<DiagCode> {
    c.diags.iter().map(|d| d.code).collect()
}

/// `fn h() { set_hook(box_new(move |information| { information.wid(); })) }` is clean: the
/// closure takes `(&u8) -> ()` from `dyn Fn(&u8) + Sync + Send`, the object `set_hook`'s formal
/// makes `box_new`'s input. Without e0282-1 (an unsized input says nothing to its argument, or
/// an object gives a closure no signature) this fails with
/// "h: unexpected diagnostics: [AnnotationsNeeded]".
#[test]
fn a_panic_hooks_closure_takes_its_signature_from_the_formals_object() {
    let (fx, [h, _]) = world();
    let mut b = Bb::new();
    let root = hook_call(&mut b, "set_hook");
    let c = check_body(&fx, &b, h, root, Plant::None, InferPlant::None);
    let got = codes(&c);
    assert!(got.is_empty(), "h: unexpected diagnostics: {got:?}");
}

/// The control: `fn q() { quiet(box_new(move |information| { information.wid(); })) }`: the
/// object `dyn Sync + Send` names no `Fn*` trait, so nothing types `information` and the method
/// call on it stays E0282. A fix that took a signature from any object (or from its auto rows)
/// fails here with "q: want E0282 (AnnotationsNeeded), got [..]".
#[test]
fn a_closure_under_an_object_with_no_fn_row_stays_e0282() {
    let (fx, [_, q]) = world();
    let mut b = Bb::new();
    let root = hook_call(&mut b, "quiet");
    let c = check_body(&fx, &b, q, root, Plant::None, InferPlant::None);
    let got = codes(&c);
    assert!(got.contains(&DiagCode::AnnotationsNeeded), "q: want E0282 (AnnotationsNeeded), got {got:?}");
}
