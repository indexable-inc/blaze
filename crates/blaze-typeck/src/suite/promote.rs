//! Constant promotion (rules/promote.rs, ASYNC.md D-A10): a shared borrow of a promotable
//! value of a `Freeze` type with no drop glue is listed in `Checked::promoted`, so the builder
//! makes it a `'static` slot instead of a frame temporary.
//!
//! The Reference, "Destructors", "Constant promotion": "Promotion of a value expression to a
//! `'static` slot occurs when the expression could be written in a constant and borrowed, and
//! that borrow could be dereferenced where the expression was originally written, without
//! changing the runtime behavior. That is, the promoted expression can be evaluated at
//! compile-time and the resulting value does not contain interior mutability or destructors".
//! The pinned core marks the waker vtable's constructor promotable (core/src/task/wake.rs):
//! `#[rustc_promotable] ... pub const fn new(clone: .., wake: .., wake_by_ref: .., drop: ..) ->
//! Self`, and marks interior mutability by the auto trait (core/src/marker.rs): `#[lang =
//! "freeze"] pub unsafe auto trait Freeze {}` with `impl<T: PointeeSized> !Freeze for
//! UnsafeCell<T> {}`.
//!
//! The world:
//! ```text
//! #[lang = "sized"] trait Sized {}
//! #[lang = "freeze"] unsafe auto trait Freeze {}
//! struct RawWakerVTable { clone: u64, wake: u64, wake_by_ref: u64, drop: u64 }
//! #[rustc_promotable] const fn new(a: u64, b: u64, c: u64, d: u64) -> RawWakerVTable;
//! struct UnsafeCell<T> { value: T }
//! impl<T> !Freeze for UnsafeCell<T> {}
//! struct Cell { value: UnsafeCell<u64> }
//! #[rustc_promotable] const fn cell(v: u64) -> Cell;
//! ```

use std::vec::Vec;

use super::check_body;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::{ImplPolarity, TypeAttrs};

/// `fn f() -> &'static <made> { &<ctor>(1, 2, 3, 4) }` (or `&cell(1)` for `cell`), checked in
/// the module doc's world; `promotable` sets `#[rustc_promotable]` on the called fn. Returns
/// the record and the borrow's expression.
fn borrow_of_call(cell: bool, promotable: bool) -> (crate::Checked<'static>, crate::ids::ExprId) {
    let (c, inner, _) = borrows_of_call(cell, promotable, false);
    (c, inner)
}

/// As [`borrow_of_call`]; with `nested` the body is `&&<ctor>(..)` returned as
/// `&'static &'static <made>`. Returns the record, the inner borrow and the outer one (the
/// inner again without `nested`).
fn borrows_of_call(cell: bool, promotable: bool, nested: bool) -> (crate::Checked<'static>, crate::ids::ExprId, crate::ids::ExprId) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let freeze = fx.tr("Freeze", 1, Vec::new(), &[]);
    fx.auto(freeze);
    fx.lang(LangItem::Freeze, freeze);
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let vt = fx.strukt("RawWakerVTable", 0, &[("clone", u64_), ("wake", u64_), ("wake_by_ref", u64_), ("drop", u64_)]);
    let vt_ty = fx.adt(vt, &[]);
    let p0 = fx.param(0);
    let uc = fx.strukt("UnsafeCell", 1, &[("value", p0)]);
    let uc_p0 = fx.adt(uc, &[p0]);
    let neg = fx.imp("!Freeze for UnsafeCell<T>", 1, uc_p0, Some((freeze, &[])), &[], Vec::new());
    fx.set_polarity(neg, ImplPolarity::Negative);
    let uc_u64 = fx.adt(uc, &[u64_]);
    let cell_s = fx.strukt("Cell", 0, &[("value", uc_u64)]);
    let cell_ty = fx.adt(cell_s, &[]);
    let callee: ItemKey = if cell {
        fx.func("cell", 0, &[u64_], cell_ty, Vec::new())
    } else {
        fx.func("new", 0, &[u64_, u64_, u64_, u64_], vt_ty, Vec::new())
    };
    if promotable {
        fx.set_attrs(callee, TypeAttrs(TypeAttrs::PROMOTABLE));
    }
    let mut out = if cell { fx.r(Mutbl::Not, cell_ty) } else { fx.r(Mutbl::Not, vt_ty) };
    if nested {
        out = fx.r(Mutbl::Not, out);
    }
    let owner = fx.func("f", 0, &[], out, Vec::new());
    fx.finish();
    let mut bb = Bb::new();
    let (path, n) = if cell { (bb.path(&["cell"]), 1) } else { (bb.path(&["new"]), 4) };
    let mut args = Vec::new();
    for _ in 0..n {
        args.push(bb.int(None));
    }
    let call = bb.call(path, &args);
    let r = bb.borrow(Mutbl::Not, call);
    let outer = if nested { bb.borrow(Mutbl::Not, r) } else { r };
    let root = bb.block(&[], Some(outer));
    let c = check_body(&fx, &bb, owner, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "the body checks clean: {:?}", c.diags);
    (c, r, outer)
}

/// PROMOTED: `&new(1, 2, 3, 4)` of a `#[rustc_promotable]` fn returning a struct of four
/// words (core's `RawWakerVTable::new`) is listed; without the attribute it is not (a rule that
/// promoted every call of a `const fn` would list it: a call may have effects a `'static`
/// slot evaluated once cannot repeat). Before rules/promote.rs nothing was listed and the
/// builder borrowed a frame temporary, returned dangling. Prints nothing; passes as
/// `test blaze-typeck: exit 0; ..`.
#[test]
fn raw_waker_vtable_new_is_promoted() {
    let (c, r) = borrow_of_call(false, true);
    assert_eq!(&c.promoted[..], &[r][..], "`&RawWakerVTable::new(..)` is a promoted borrow");
    let (c, _) = borrow_of_call(false, false);
    assert!(c.promoted.is_empty(), "a call of a fn not marked `#[rustc_promotable]` is not promoted: {:?}", c.promoted);
}

/// NOT PROMOTED: `&cell(1)` of a `#[rustc_promotable]` fn whose value holds an `UnsafeCell`
/// (`!Freeze`) is not listed: "the resulting value does not contain interior mutability" fails.
/// A rule that skipped the `Freeze` check would list it.
#[test]
fn a_cell_is_not_promoted() {
    let (c, _) = borrow_of_call(true, true);
    assert!(c.promoted.is_empty(), "a value with interior mutability is never promoted: {:?}", c.promoted);
}

/// PROMOTED, NESTED: `&&new(1, 2, 3, 4)` lists both borrows: the outer one's operand is a shared
/// borrow of a promotable `Freeze` value, which could be written in a constant. ripgrep's
/// `ignore` crate, `const DEFAULT_TYPES: &[(&[&str], &[&str])] = &[(&["ada"], &["*.adb"]), ..]`:
/// with only the inner borrows listed, the outer array was a frame temporary of 214 tuples, the
/// flow 4509 statements and 3449 origins, its "returned" borrow refused by tier one, and dense tier
/// two (16 GB of bits) never ended. A rule without the `Ref` arm lists only the inner borrow.
/// `&&cell(1)` lists neither: the inner value holds an `UnsafeCell`, and a rule that took the
/// nested borrow without its operand's `Freeze` check would list the outer one (its type, `&Cell`,
/// is a shared reference).
#[test]
fn a_borrow_of_a_promoted_borrow_is_promoted() {
    let (c, inner, outer) = borrows_of_call(false, true, true);
    let mut both = std::vec![inner, outer];
    both.sort_unstable_by_key(|e| e.0);
    assert_eq!(&c.promoted[..], &both[..], "`&&RawWakerVTable::new(..)`: both borrows are promoted");
    let (c, _, _) = borrows_of_call(true, true, true);
    assert!(c.promoted.is_empty(), "`&&cell(1)`: interior mutability under the inner borrow promotes neither: {:?}", c.promoted);
}
