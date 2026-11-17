//! Unsizing a struct behind a reference through its tail field: core's
//! `array::IntoIter::unsize_mut` returns `self.inner.deref_mut()`, a
//! `&mut PolymorphicIter<[MaybeUninit<T>; N]>`, as `&mut PolymorphicIter<[MaybeUninit<T>]>`
//! (library/core/src/array/iter.rs; `struct PolymorphicIter<DATA: ?Sized> { alive: IndexRange,
//! data: DATA }`), refused before as E0308.
//!
//! The rule. The Reference, "Type coercions", "Unsized Coercions": "`Foo<..., T, ...>` to
//! `Foo<..., U, ...>`, when: `Foo` is a struct. `T` implements `Unsize<U>`. The last field of
//! `Foo` has a type involving `T`. If that field has type `Bar<T>`, then `Bar<T>` implements
//! `Unsize<Bar<U>>`. `T` is not part of the type of any other fields."; and "Coercion types":
//! `&mut T` to `&mut U` when `U` is obtained from `T` by unsized coercion.
//!
//! The toy world:
//! ```text
//! #[lang = "sized"] trait Sized {}  #[lang = "copy"] trait Copy {}
//! #[lang = "unsize"] trait Unsize<U: ?Sized> {}
//! struct Poly<D: ?Sized> { alive: usize, data: D }
//! struct Twice<D: ?Sized> { first: D, data: D }   // `D` also in a field before the tail
//! fn a_tail(x: &mut Poly<[u8; 3]>) -> &mut Poly<[u8]> { x }
//! fn b_other_elem(x: &mut Poly<[u8; 3]>) -> &mut Poly<[u16]> { x }
//! fn c_twice(x: &Twice<[u8; 3]>) -> &Twice<[u8]> { x }
//! ```
//! Without the struct arm in `coerce_unsized` (rules/coerce.rs), (a) is E0308. (b) and (c) pass
//! either way: they pin that the solver's rule, not the site, refuses what it must.

use std::vec::Vec;

use super::{adjusts, check_body};
use crate::columns::Checked;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, LangItem, Mutbl};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

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
    let (p0, u8_, u16_, usize_) = (fx.param(0), fx.u8(), fx.u16(), fx.usize());

    let poly_k = fx.strukt("Poly", 1, &[("alive", usize_), ("data", p0)]);
    let twice_k = fx.strukt("Twice", 1, &[("first", p0), ("data", p0)]);
    let arr = fx.array(u8_, 3);
    let slice_u8 = fx.ty(PNode::Slice(u8_));
    let slice_u16 = fx.ty(PNode::Slice(u16_));

    let poly_arr = fx.adt(poly_k, &[arr]);
    let poly_slice = fx.adt(poly_k, &[slice_u8]);
    let poly_slice16 = fx.adt(poly_k, &[slice_u16]);
    let (from, to, to16) = (fx.r(Mutbl::Mut, poly_arr), fx.r(Mutbl::Mut, poly_slice), fx.r(Mutbl::Mut, poly_slice16));
    fx.func("a_tail", 0, &[from], to, Vec::new());
    fx.func("b_other_elem", 0, &[from], to16, Vec::new());

    let twice_arr = fx.adt(twice_k, &[arr]);
    let twice_slice = fx.adt(twice_k, &[slice_u8]);
    let (tfrom, tto) = (fx.r(Mutbl::Not, twice_arr), fx.r(Mutbl::Not, twice_slice));
    fx.func("c_twice", 0, &[tfrom], tto, Vec::new());
    fx.finish();
    world
}

/// `fn <owner>(x: ..) -> .. { x }`: the tail's id and the check.
fn param_case(fx: &Fx, owner: &str) -> (ExprId, Checked<'static>) {
    let mut b = Bb::new();
    let x = b.param("x");
    let e0 = b.local(x);
    let root = b.block(&[], Some(e0));
    let c = check_body(fx, &b, fx.key_of(owner), root, Plant::None, InferPlant::None);
    (e0, c)
}

fn codes(c: &Checked<'_>) -> Vec<DiagCode> {
    c.diags.iter().map(|d| d.code).collect()
}

/// (a) `&mut Poly<[u8; 3]>` to `&mut Poly<[u8]>`: `Poly`'s parameter is only in its tail
/// field, and `[u8; 3]: Unsize<[u8]>`, so the coercion is taken with its unsize last. Fails
/// without the fix (E0308, no adjustment).
/// Should print: "test suite::struct_tail_unsize::a_struct_with_an_array_tail_unsizes_behind_mut ... ok".
#[test]
fn a_struct_with_an_array_tail_unsizes_behind_mut() {
    let fx = world();
    let (e, c) = param_case(&fx, "a_tail");
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let got = adjusts(&fx, &c, e);
    assert!(got.ends_with("unsize -> &mut Poly<[u8]>") && got.matches("unsize").count() == 1, "adjustments `{got}`");
}

/// (b) `&mut Poly<[u8; 3]>` to `&mut Poly<[u16]>`: the tail's element differs, so `[u8; 3]:
/// Unsize<[u16]>` does not hold and the coercion is refused (E0308), nothing recorded.
/// Should print: "test suite::struct_tail_unsize::a_struct_tail_of_another_element_is_refused ... ok".
#[test]
fn a_struct_tail_of_another_element_is_refused() {
    let fx = world();
    let (e, c) = param_case(&fx, "b_other_elem");
    assert_eq!(codes(&c), std::vec![DiagCode::Mismatch], "{:?}", c.diags);
    assert!(!adjusts(&fx, &c, e).contains("unsize"));
}

/// (c) `&Twice<[u8; 3]>` to `&Twice<[u8]>`: `D` is also the type of a field before the tail
/// ("`T` is not part of the type of any other fields"), so no unsizing: E0308.
/// Should print: "test suite::struct_tail_unsize::a_parameter_in_another_field_is_refused ... ok".
#[test]
fn a_parameter_in_another_field_is_refused() {
    let fx = world();
    let (e, c) = param_case(&fx, "c_twice");
    assert_eq!(codes(&c), std::vec![DiagCode::Mismatch], "{:?}", c.diags);
    assert!(!adjusts(&fx, &c, e).contains("unsize"));
}
