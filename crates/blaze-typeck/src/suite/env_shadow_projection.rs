//! A projection on the self type of an impl, under a where-clause of the body that proves the
//! trait and names no value for the item, is RIGID (the reference: rustc_next_trait_solver
//! assembly/mod.rs `assemble_and_merge_candidates`, the `ParamEnv | AliasBound` arm, "we want
//! to treat aliases as rigid if there are no applicable projection bounds in the
//! environment"); without that clause it normalizes through the impl. Every side of the body
//! names the projection (the front end lowers `Self::Item` in an impl as `<SelfTy as
//! Trait>::Item`, never as the impl's `type Item` text: blaze-ladder front2/types.rs), so the
//! same answer is on every side.
//!
//! The miniature of core's `<impl Iterator for Iter<'a, T>>::rposition`
//! (core/src/slice/iter/macros.rs): `Self: ExactSizeIterator + DoubleEndedIterator` elaborates
//! to `Iter<'a, T>: Iterator`; the element `x` from `next_back()` and the input of `predicate:
//! P` with `P: FnMut(Self::Item) -> bool` are the same rigid alias. Before, the front end read
//! the bound's `Self::Item` eagerly as `&'a T` while `x` was the rigid alias: a false E0308
//! (the control below plants that read back).
//!
//! ```text
//! trait It { type Item; }
//! trait Back: It {
//!     fn keep<P: Fn(<Self as It>::Item) -> u8>(x: <Self as It>::Item, p: P) -> u8 { p(x) }
//! }
//! struct Iter<T> { v: T }
//! impl<T> It for Iter<T> { type Item = &T; }
//! impl<T> Back for Iter<T> {}
//! impl<T> Iter<T> {
//!     // shadowed: rigid on both sides
//!     fn shadowed<P: Fn(<Iter<T> as It>::Item) -> u8>(x: <Iter<T> as It>::Item, p: P) -> u8
//!         where Iter<T>: Back { p(x) }                      // clean
//!     //  { let y: &T = x; }                                 // E0308, as rustc
//!     fn unshadowed<P: Fn(<Iter<T> as It>::Item) -> u8>(x: <Iter<T> as It>::Item, p: P) -> u8
//!         { p(x) }                                          // clean
//!     //  { let y: &T = x; }                                 // clean: normalized by the impl
//!     // the control: the bound's input read eagerly as `&T` (the old front end)
//!     fn eager<P: Fn(&T) -> u8>(x: <Iter<T> as It>::Item, p: P) -> u8
//!         where Iter<T>: Back { p(x) }                      // E0308, the old false error
//! }
//! ```
//!
//! `Fn` stands for core's `FnMut` (the call's autoref is not what is tested here); the methods
//! sit in an inherent impl of `Iter<T>` (the environment, not the container, is what decides).

use std::format;
use std::string::String;
use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{Abi, ExprId, FnPtrHeader, ItemKey, LangItem, Mutbl, Name, Safety, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

struct Items {
    shadowed: ItemKey,
    unshadowed: ItemKey,
    eager: ItemKey,
    keep: ItemKey,
}

fn world() -> (Fx, Items) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let rust_call = FnPtrHeader { abi: Abi::RustCall, safety: Safety::Safe, c_variadic: false };

    // The `Fn*` traits, as suite/fn_traits.rs declares them.
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

    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    let it = fx.tr("It", 1, Vec::new(), &["Item"]);
    let is_it = fx.bound(it, &[p0]);
    let back = fx.tr("Back", 1, std::vec![is_it], &[]);
    let iter = fx.strukt("Iter", 1, &[("v", p0)]);
    let iter_t = fx.adt(iter, &[p0]);
    let ref_t = fx.r(Mutbl::Not, p0);
    fx.imp("It for Iter", 1, iter_t, Some((it, &[])), &[("Item", ref_t)], Vec::new());
    fx.imp("Back for Iter", 1, iter_t, Some((back, &[])), &[], Vec::new());

    // `P: Fn(input) -> u8` and `P: Sized`, `P` the method's own parameter 1.
    let p_bounds = |fx: &mut Fx, input: crate::PTy| -> Vec<crate::PClause> {
        let args = fx.tuple(&[input]);
        std::vec![fx.bound(sized, &[p1]), fx.bound(fn_, &[p1, args]), fx.proj(fn_once, &[p1, args], "Output", u8_)]
    };

    // The trait's own method: `Self` is a parameter, its item rigid.
    let self_item = fx.alias(it, "Item", &[p0]);
    let own = p_bounds(&mut fx, self_item);
    let keep = fx.trait_fn_own(back, "keep", false, 1, &[self_item, p1], u8_, own);

    let iter_item = fx.alias(it, "Item", &[iter_t]);
    let methods = fx.imp("Iter methods", 1, iter_t, None, &[], Vec::new());
    let mut own = p_bounds(&mut fx, iter_item);
    own.push(fx.bound(back, &[iter_t]));
    let shadowed = fx.impl_fn_own(methods, "shadowed", false, 1, &[iter_item, p1], u8_, own);
    let own = p_bounds(&mut fx, iter_item);
    let unshadowed = fx.impl_fn_own(methods, "unshadowed", false, 1, &[iter_item, p1], u8_, own);
    // The control's world: the bound's `Self::Item` read eagerly as the impl's `&T`.
    let mut own = p_bounds(&mut fx, ref_t);
    own.push(fx.bound(back, &[iter_t]));
    let eager = fx.impl_fn_own(methods, "eager", false, 1, &[iter_item, p1], u8_, own);
    fx.finish();
    (fx, Items { shadowed, unshadowed, eager, keep })
}

/// `{ p(x) }`: expr 0 `p`, 1 `x`, 2 the call, 3 the block.
fn call_body() -> (Bb, ExprId) {
    let mut b = Bb::new();
    let x = b.param("x");
    let p = b.param("p");
    let e0 = b.local(p);
    let e1 = b.local(x);
    let e2 = b.call(e0, &[e1]);
    let root = b.block(&[], Some(e2));
    (b, root)
}

/// `{ let y: &T = x; 0 }`: expr 0 `x`, 1 the literal, 2 the block. The tail is the fn's `u8`
/// (first run, 2026-11-18: with no tail the block was `()` against the declared `u8`, one
/// E0308 of the fixture's own in both `let` tests).
fn let_body() -> (Bb, ExprId) {
    let mut b = Bb::new();
    let x = b.param("x");
    let _p = b.param("p");
    let e0 = b.local(x);
    let t = b.ty(PNode::Param(0));
    let ref_t = b.ty(PNode::Ref(Mutbl::Not, t));
    let y = b.bind("y");
    let s = b.let_(y, Some(ref_t), e0);
    let e1 = b.int(None);
    let root = b.block(&[s], Some(e1));
    (b, root)
}

/// The item as the suite prints it, `<self_ty as It>::Item` (a name is its hash).
fn item_shown(self_ty: &str) -> String {
    format!("<{self_ty} as It>::{:x}", Name::of("Item").0)
}

fn codes(c: &crate::Checked<'_>) -> Vec<DiagCode> {
    c.diags.iter().map(|d| d.code).collect()
}

/// Shadowed: `x` and the bound's input are the same rigid alias, so `p(x)` is clean.
#[test]
fn shadowed_call_is_clean_both_sides_rigid() {
    let (fx, it) = world();
    let (b, root) = call_body();
    let c = check_body(&fx, &b, it.shadowed, root, Plant::None, InferPlant::None);
    assert_eq!(codes(&c), Vec::<DiagCode>::new(), "no finding");
    assert_eq!(show(&fx, c.types.view(), c.expr_ty[1]), item_shown("Iter<P0>"), "`x` is the rigid alias");
    assert_eq!(show(&fx, c.types.view(), c.expr_ty[2]), "u8");
}

/// Shadowed: the alias is rigid, so `let y: &T = x;` is E0308 (rustc rejects it too: the pin
/// of faithfulness), expected `&T`, found the associated type.
#[test]
fn shadowed_let_is_a_mismatch_as_in_rustc() {
    let (fx, it) = world();
    let (b, root) = let_body();
    let c = check_body(&fx, &b, it.shadowed, root, Plant::None, InferPlant::None);
    assert_eq!(codes(&c), std::vec![DiagCode::Mismatch], "one E0308");
    let types = c.types.view();
    let d = &c.diags[0];
    assert_eq!(d.expected.map(|t| show(&fx, types, t)).as_deref(), Some("&P0"));
    assert_eq!(d.found.map(|t| show(&fx, types, t)), Some(item_shown("Iter<P0>")));
}

/// Not shadowed: the impl decides, `x` is `&T`, the call and the `let` are clean.
#[test]
fn unshadowed_normalizes_through_the_impl() {
    let (fx, it) = world();
    let (b, root) = call_body();
    let c = check_body(&fx, &b, it.unshadowed, root, Plant::None, InferPlant::None);
    assert_eq!(codes(&c), Vec::<DiagCode>::new(), "`p(x)`: no finding");
    assert_eq!(show(&fx, c.types.view(), c.expr_ty[1]), "&P0", "`x` normalized");
    let (b, root) = let_body();
    let c = check_body(&fx, &b, it.unshadowed, root, Plant::None, InferPlant::None);
    assert_eq!(codes(&c), Vec::<DiagCode>::new(), "`let y: &T = x;`: no finding");
}

/// The trait's default method: `Self` a parameter, the item rigid on both sides: clean.
#[test]
fn trait_default_method_is_clean_rigid() {
    let (fx, it) = world();
    let (b, root) = call_body();
    let c = check_body(&fx, &b, it.keep, root, Plant::None, InferPlant::None);
    assert_eq!(codes(&c), Vec::<DiagCode>::new(), "no finding");
    assert_eq!(show(&fx, c.types.view(), c.expr_ty[1]), item_shown("P0"));
}

/// The control (rule 23): the cause planted back, the bound's input read eagerly as `&T`
/// beside a rigid `x`, gives the old false E0308 at the call's argument; removing it (the
/// `shadowed` test) takes the finding away.
#[test]
fn control_eager_self_item_is_the_old_mismatch() {
    let (fx, it) = world();
    let (b, root) = call_body();
    let c = check_body(&fx, &b, it.eager, root, Plant::None, InferPlant::None);
    assert_eq!(codes(&c), std::vec![DiagCode::Mismatch], "the old E0308");
    let types = c.types.view();
    let d = &c.diags[0];
    assert_eq!(d.expected.map(|t| show(&fx, types, t)).as_deref(), Some("&P0"));
    assert_eq!(d.found.map(|t| show(&fx, types, t)), Some(item_shown("Iter<P0>")), "found the alias");
}
