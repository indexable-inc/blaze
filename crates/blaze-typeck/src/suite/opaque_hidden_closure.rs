//! An instance resolved at ground arguments through a fn whose output is an opaque type whose
//! hidden type holds a closure BELOW its root (infer/facts.rs `fill_opaque`, `body_terms`).
//!
//! The owner's hello world stopped at `<impl Drop for Kqueue>::drop` (blaze-core kqueue.rs) with
//! "no impl of trait .. holds at these arguments": `LazySlab::iter_mut`'s hidden type is
//! `FlatMap<option::IterMut<Slab<T>>, <Slab::iter_mut's opaque>, fn item Slab::iter_mut>`, and
//! `Slab::iter_mut`'s (blaze-mem handle.rs) is `Map<Filter<Enumerate<..>, {closure#0}>,
//! {closure#1}>`. `FlatMap`'s `Iterator` impl needs `<F as FnOnce<(I::Item,)>>::Output == U`,
//! whose left side is `Slab::iter_mut`'s opaque (from its signature) and whose right side is
//! that opaque already revealed. The rules:
//! - The Reference, "Impl trait", "Abstract return types": the caller sees only the bounds, but
//!   after type checking (rustc's post-analysis typing mode, in which instances are resolved
//!   and constants evaluated) every opaque is its hidden type, so the two sides are equal.
//!   (blaze-ladder's world answers so at ground resolutions, `Reveal::All`; this fixture's
//!   world always answers the hidden type.)
//! - The Reference, "Closure types": a closure's type is its identity, and its kind and
//!   signature come from its owner's check (`Question::Closure`), at any depth of a type.
//!
//! The toy world:
//!
//! ```text
//! trait FnOnce<Args> { type Output; }  trait FnMut<Args>: FnOnce<Args>  trait Fn<Args>: FnMut<Args>
//! trait Iterator { type Item; fn next(&mut self) -> u8; }
//! struct Wrap;                    impl Iterator for Wrap { type Item = u8; }
//! struct Map<I, F>;               impl<I: Iterator, F> Iterator for Map<I, F> { type Item = u8; }
//! struct Flat<U, F>;              impl<U: Iterator, F: FnOnce<()>> Iterator for Flat<U, F>
//!                                     where <F as FnOnce<()>>::Output == U { type Item = u8; }
//! fn mk() -> impl Iterator<Item = u8> { Map(Wrap, |x: u8| x) }   // hidden: Map<Wrap, {closure#0 of mk}>
//! ```
//!
//! `<Flat<Map<Wrap, {closure#0 of mk}>, fn mk> as Iterator>::next` is `Flat`'s impl's `next`.
//! Without the nested-closure conversion the solver stops with
//! `Malformed(BodyTypeInRecord(Opaque(..)))`; with the opaque not revealed it is `NoImpl`.
//!
//! Prints `test suite::opaque_hidden_closure::an_opaque_output_with_a_nested_closure_reveals_at_an_instance ... ok`.

use std::vec::Vec;

use blaze_own::Alloc;

use crate::fixture::Fx;
use crate::ids::{Abi, FnPtrHeader, ItemKey, LangItem, List, Mutbl, Safety, UintTy};
use crate::portable::{PNode, PortableTys};
use crate::ty::ClosureKind;

/// Prints nothing of its own; on failure it panics with
/// `resolving next at Flat<Map<..>, fn mk>: an opaque output did not reveal to its hidden type: ..`.
#[test]
fn an_opaque_output_with_a_nested_closure_reveals_at_an_instance() {
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
    let iterator = fx.tr("Iterator", 1, Vec::new(), &["Item"]);
    let m_self = fx.r(Mutbl::Mut, p0);
    let next = fx.trait_fn(iterator, "next", true, &[m_self], u8_);

    // struct Wrap;  impl Iterator for Wrap
    let wrap_k = fx.strukt("Wrap", 0, &[]);
    let wrap = fx.adt(wrap_k, &[]);
    fx.imp("impl Iterator for Wrap", 0, wrap, Some((iterator, &[])), &[("Item", u8_)], Vec::new());
    // struct Map<I, F>;  impl<I: Iterator, F> Iterator for Map<I, F>
    let map_k = fx.strukt("Map", 2, &[("i", p0), ("f", p1)]);
    let map_ij = fx.adt(map_k, &[p0, p1]);
    let i_iter = fx.bound(iterator, &[p0]);
    fx.imp("impl Iterator for Map", 2, map_ij, Some((iterator, &[])), &[("Item", u8_)], std::vec![i_iter]);
    // struct Flat<U, F>;  impl<U: Iterator, F: FnOnce<()>> Iterator for Flat<U, F>
    //     where <F as FnOnce<()>>::Output == U
    let flat_k = fx.strukt("Flat", 2, &[("u", p0), ("f", p1)]);
    let flat_uf = fx.adt(flat_k, &[p0, p1]);
    let unit = fx.tuple(&[]);
    let u_iter = fx.bound(iterator, &[p0]);
    let f_once = fx.bound(fn_once, &[p1, unit]);
    let f_out = fx.proj(fn_once, &[p1, unit], "Output", p0);
    let flat_imp = fx.imp("impl Iterator for Flat", 2, flat_uf, Some((iterator, &[])), &[("Item", u8_)], std::vec![u_iter, f_once, f_out]);
    let m_flat = fx.r(Mutbl::Mut, flat_uf);
    let flat_next = fx.impl_fn(flat_imp, "next", true, &[m_flat], u8_);

    // fn mk() -> impl Iterator<Item = u8> { Map(Wrap, |x: u8| x) }. The fixture names keys in
    // order: a probe key tells `mk`'s key, so its hidden type can name its own closure.
    let o = fx.opaque_key("mk::{opaque#0}");
    let none = fx.list(&[]);
    let opaque = fx.ty(PNode::Opaque(o, none));
    let is_iter = fx.bound(iterator, &[opaque]);
    let item_u8 = fx.proj(iterator, &[opaque], "Item", u8_);
    let is_sized = fx.bound(sized, &[opaque]);
    fx.set_opaque_bounds(o, std::vec![is_iter, item_u8, is_sized]);
    let probe = fx.opaque_key("probe");
    let mk_k = ItemKey(probe.0 + 1);
    let closure = fx.ty(PNode::Closure { owner: mk_k, ordinal: 0, args: List::EMPTY });
    let mk = fx.func("mk", 0, &[], opaque, Vec::new());
    assert_eq!(mk, mk_k, "the probe names `mk`'s key");
    let hidden = fx.adt(map_k, &[wrap, closure]);
    fx.hidden.push((o, hidden));
    let io = fx.list(&[u8_, u8_]);
    let sig_u8 = fx.ty(PNode::FnPtr(io, FnPtrHeader::RUST));
    fx.closures.push((mk, 0, ClosureKind::FnMut, sig_u8));
    fx.use_trait(iterator);
    fx.finish();

    // The instance's ground arguments: `[Flat<Map<Wrap, {closure#0 of mk}>, fn mk>]`, as
    // blaze-build's `ground` hands them over (every opaque in them already revealed).
    let mut ground = PortableTys::new_in(Alloc::global());
    let wrap_g = ground.intern(PNode::Adt(wrap_k, List::EMPTY)).expect("intern");
    let closure_g = ground.intern(PNode::Closure { owner: mk, ordinal: 0, args: List::EMPTY }).expect("intern");
    let map_args = ground.intern_list(&[wrap_g, closure_g]).expect("intern");
    let map_g = ground.intern(PNode::Adt(map_k, map_args)).expect("intern");
    let mk_g = ground.intern(PNode::FnDef(mk, List::EMPTY)).expect("intern");
    let flat_args = ground.intern_list(&[map_g, mk_g]).expect("intern");
    let flat_g = ground.intern(PNode::Adt(flat_k, flat_args)).expect("intern");
    let args = ground.intern_list(&[flat_g]).expect("intern");
    let r = crate::resolve_instance(&fx, next, args, &mut ground, Alloc::global())
        .unwrap_or_else(|e| panic!("resolving next at Flat<Map<..>, fn mk>: an opaque output did not reveal to its hidden type: {e}"));
    assert_eq!(r.item, flat_next, "`<Flat<Map<Wrap, {{closure#0 of mk}}>, fn mk> as Iterator>::next` is `Flat`'s impl's `next`");
}
