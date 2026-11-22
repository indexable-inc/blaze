//! An expected argument type whose STRUCT TAIL is unsized is only the argument's hint, never
//! its type (rules/mod.rs `rvalue_hint`, `struct_tail`).
//!
//! The site is turso core/btree_dump.rs:51:
//! `fn open(..) -> Result<Arc<RwLock<dyn InternalVirtualTableCursor>>> { ..
//! Ok(Arc::new(RwLock::new(cursor))) }` (lock_api's `struct RwLock<R, T: ?Sized> { raw: R,
//! data: UnsafeCell<T> }`). turso c1851p gave "found `RwLock<RawRwLock, BtreeDumpCursor>`,
//! expected `RwLock<RawRwLock, dyn ..>` (E0308)" and "`dyn ..: Sized` (E0277)".
//!
//! The rules:
//! - rustc_hir_typeck expectation.rs `rvalue_hint` matches `struct_tail_raw(ty)`: `ty::Slice |
//!   ty::Str | ty::Dynamic` give `ExpectRvalueLikeUnsized`. `check_argument_types` then coerces
//!   to `expectation.only_has_type().unwrap_or(formal_input_ty)`, which is the formal here.
//! - The Reference, "Type coercions", "Unsized Coercions": "`Foo<..., T, ...>` to `Foo<...,
//!   U, ...>`, when `Foo` is a struct, `T` implements `Unsize<U>`, the last field of `Foo` has
//!   a type involving `T`, if that field has type `Bar<T>`, then `Bar<T>` implements
//!   `Unsize<Bar<U>>`, `T` is not part of the type of any other fields". A `Foo<T>` that
//!   implements `CoerceUnsized<Foo<U>>` coerces to it.
//!
//! ```text
//! #[lang = "sized"] trait Sized {}  #[lang = "unsize"] trait Unsize<U: ?Sized> {}
//! #[lang = "coerce_unsized"] trait CoerceUnsized<U> {}
//! trait Tr {}  struct C;  impl Tr for C {}
//! struct P<T: ?Sized>;  impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<P<U>> for P<T> {}
//! struct Cell<T: ?Sized> { value: T }
//! struct Lock<R, T: ?Sized> { raw: R, data: Cell<T> }
//! struct Bad<T: ?Sized> { first: P<T>, data: Cell<T> }      // `T` in a field before the tail
//! fn new<T>(v: T) -> P<T>;  fn lock_new<T>(v: T) -> Lock<u8, T>;  fn bad_new<T>(v: T) -> Bad<T>;
//! fn a(c: C) -> P<Lock<u8, dyn Tr>> { new(lock_new(c)) }   // clean (turso's shape)
//! fn b(c: C) -> P<Bad<dyn Tr>> { new(bad_new(c)) }         // the twin: E0308, `Bad` cannot unsize
//! fn d(c: C) -> P<Lock<u8, C>> { new(lock_new(c)) }        // a sized tail: `HasType`, clean
//! ```
//!
//! CONTROL (rule 23): with `rvalue_hint` matching the type itself instead of its
//! `struct_tail`, `a` is E0308 + E0277 (`dyn Tr: Sized`) again and the first test fails.

use std::vec::Vec;

use super::check_body;
use crate::fixture::{Bb, Fx};
use crate::ids::{List, LangItem};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

fn world() -> Fx {
    let mut world = Fx::new();
    // Built through `&mut Fx`, as `rung_7c` is: on `Fx` itself `fx.adt(..)` picks `World::adt`.
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce = fx.tr("CoerceUnsized", 2, Vec::new(), &[]);
    fx.lang(LangItem::CoerceUnsized, coerce);
    let tr = fx.tr("Tr", 1, Vec::new(), &[]);
    let (p0, p1, u8_) = (fx.param(0), fx.param(1), fx.u8());

    let c_k = fx.strukt("C", 0, &[]);
    let c = fx.adt(c_k, &[]);
    fx.imp("impl Tr for C", 0, c, Some((tr, &[])), &[], Vec::new());

    let p_k = fx.strukt("P", 1, &[]);
    let (p_t, p_u) = (fx.adt(p_k, &[p0]), fx.adt(p_k, &[p1]));
    let t_unsize_u = fx.bound(unsize, &[p0, p1]);
    let cu = fx.imp("impl CoerceUnsized<P<U>> for P<T>", 2, p_t, Some((coerce, &[p_u])), &[], Vec::from([t_unsize_u]));
    fx.unsized_param(cu, 0);
    fx.unsized_param(cu, 1);

    let cell_k = fx.strukt("Cell", 1, &[("value", p0)]);
    let (cell_t, cell_p1) = (fx.adt(cell_k, &[p0]), fx.adt(cell_k, &[p1]));
    let lock_k = fx.strukt("Lock", 2, &[("raw", p0), ("data", cell_p1)]);
    let p_of_t = fx.adt(p_k, &[p0]);
    let bad_k = fx.strukt("Bad", 1, &[("first", p_of_t), ("data", cell_t)]);

    let lock_u8_t = fx.adt(lock_k, &[u8_, p0]);
    let bad_t = fx.adt(bad_k, &[p0]);
    fx.func("new", 1, &[p0], p_t, Vec::new());
    fx.func("lock_new", 1, &[p0], lock_u8_t, Vec::new());
    fx.func("bad_new", 1, &[p0], bad_t, Vec::new());

    let dyn_tr = {
        let ex_tr = fx.ty(PNode::ExTrait(tr, List::EMPTY));
        fx.t.dynamic(&[ex_tr]).expect("dyn")
    };
    let lock_dyn = fx.adt(lock_k, &[u8_, dyn_tr]);
    let bad_dyn = fx.adt(bad_k, &[dyn_tr]);
    let lock_c = fx.adt(lock_k, &[u8_, c]);
    let (ret_a, ret_b, ret_d) = (fx.adt(p_k, &[lock_dyn]), fx.adt(p_k, &[bad_dyn]), fx.adt(p_k, &[lock_c]));
    fx.func("a", 0, &[c], ret_a, Vec::new());
    fx.func("b", 0, &[c], ret_b, Vec::new());
    fx.func("d", 0, &[c], ret_d, Vec::new());
    fx.finish();
    world
}

/// The codes of `fn <owner>(c: C) -> .. { new(<inner>(c)) }`.
fn codes(fx: &Fx, owner: &str, inner: &str) -> Vec<&'static str> {
    let mut b = Bb::new();
    let c = b.param("c");
    let new = b.path(&["new"]);
    let make = b.path(&[inner]);
    let c_use = b.local(c);
    let made = b.call(make, &[c_use]);
    let call = b.call(new, &[made]);
    let root = b.block(&[], Some(call));
    let checked = check_body(fx, &b, fx.key_of(owner), root, Plant::None, InferPlant::None);
    checked.diags.iter().map(|d| d.code.code()).collect()
}

/// turso's shape. Prints nothing; fails with "a: [..]" (E0308 and E0277 before the fix).
#[test]
fn an_expected_input_with_an_unsized_struct_tail_is_only_a_hint() {
    let fx = world();
    let got = codes(&fx, "a", "lock_new");
    assert!(got.is_empty(), "a: {got:?}");
}

/// The twin rustc refuses: `T` is in `Bad`'s field before the tail, so `P<Bad<C>>` does not
/// coerce to `P<Bad<dyn Tr>>`. rustc says E0308. The checker may name the unmet
/// `CoerceUnsized` instead, so this asserts only that it is never clean.
#[test]
fn a_struct_whose_other_field_names_the_parameter_does_not_unsize() {
    let fx = world();
    let got = codes(&fx, "b", "bad_new");
    assert!(!got.is_empty(), "b is clean: the hint let `Bad<C>` through");
}

/// A sized tail is still the argument's type (`HasType`): clean.
#[test]
fn control_a_sized_tail_is_the_arguments_type() {
    let fx = world();
    let got = codes(&fx, "d", "lock_new");
    assert!(got.is_empty(), "d: {got:?}");
}
