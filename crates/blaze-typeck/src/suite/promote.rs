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
    let out = if cell { fx.r(Mutbl::Not, cell_ty) } else { fx.r(Mutbl::Not, vt_ty) };
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
    let root = bb.block(&[], Some(r));
    let c = check_body(&fx, &bb, owner, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "the body checks clean: {:?}", c.diags);
    (c, r)
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

/// SHARED AUTOREF PROMOTION: `Fork::B.boxed()` autorefs `Fork::B` via
/// `AdjustKind::Borrow { mutbl: Not, raw: false }` (no `Expr::Ref` in the AST). Because `Fork`
/// is `Freeze` with no drop glue and `Fork::B` is a unit variant ctor, the receiver is listed
/// in `Checked::promoted` so the builder emits a `'static` constant row instead of a dangling
/// stack temporary (the tempo-chainspec `EthereumHardfork::Frontier.boxed()` SIGTRAP). A
/// `&mut self` autoref (`Fork::B.touch_mut()`) and a `!Freeze` receiver (`cell(1).peek()`) are
/// never promoted.
#[test]
fn shared_autoref_of_unit_variant_is_promoted() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Default::default(), &[]);
    fx.lang(LangItem::Sized, sized);
    let freeze = fx.tr("Freeze", 1, Default::default(), &[]);
    fx.auto(freeze);
    fx.lang(LangItem::Freeze, freeze);
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let (fork, _) = fx.enum_("Fork", 0, &[("A", &[]), ("B", &[])]);
    let fork_ty = fx.adt(fork, &[]);
    let fork_ref = fx.r(Mutbl::Not, fork_ty);
    let fork_mut = fx.r(Mutbl::Mut, fork_ty);
    let fork_imp = fx.imp("impl Fork", 0, fork_ty, None, &[], Default::default());
    fx.impl_fn(fork_imp, "boxed", true, &[fork_ref], fork_ref);
    fx.impl_fn(fork_imp, "touch_mut", true, &[fork_mut], u64_);

    let p0 = fx.param(0);
    let uc = fx.strukt("UnsafeCell", 1, &[("value", p0)]);
    let uc_p0 = fx.adt(uc, &[p0]);
    let neg = fx.imp("!Freeze for UnsafeCell<T>", 1, uc_p0, Some((freeze, &[])), &[], Default::default());
    fx.set_polarity(neg, ImplPolarity::Negative);
    let uc_u64 = fx.adt(uc, &[u64_]);
    let cell_s = fx.strukt("Cell", 0, &[("value", uc_u64)]);
    let cell_ty = fx.adt(cell_s, &[]);
    let cell_ref = fx.r(Mutbl::Not, cell_ty);
    let cell_imp = fx.imp("impl Cell", 0, cell_ty, None, &[], Default::default());
    fx.impl_fn(cell_imp, "peek", true, &[cell_ref], u64_);
    let mk_cell = fx.func("cell", 0, &[u64_], cell_ty, Default::default());
    fx.set_attrs(mk_cell, TypeAttrs(TypeAttrs::PROMOTABLE));

    let f_shared = fx.func("f_shared", 0, &[], fork_ref, Default::default());
    let f_mut = fx.func("f_mut", 0, &[], u64_, Default::default());
    let f_cell = fx.func("f_cell", 0, &[], u64_, Default::default());
    fx.finish();

    // `Fork::B.boxed()` -> receiver `Fork::B` is promoted.
    let mut bb = Bb::new();
    let b = bb.path(&["Fork", "B"]);
    let call = bb.method(b, "boxed", &[]);
    let root = bb.block(&[], Some(call));
    let c = check_body(&fx, &bb, f_shared, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "f_shared checks clean: {:?}", c.diags);
    assert_eq!(&c.promoted[..], &[b][..], "`Fork::B.boxed()` promotes its shared-autoref receiver");

    // `Fork::B.touch_mut()` -> `&mut` autoref is NOT promoted.
    let mut bb = Bb::new();
    let b = bb.path(&["Fork", "B"]);
    let call = bb.method(b, "touch_mut", &[]);
    let root = bb.block(&[], Some(call));
    let c = check_body(&fx, &bb, f_mut, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "f_mut checks clean: {:?}", c.diags);
    assert!(c.promoted.is_empty(), "a `&mut` autoref is never promoted: {:?}", c.promoted);

    // `cell(1).peek()` -> `!Freeze` receiver is NOT promoted.
    let mut bb = Bb::new();
    let path = bb.path(&["cell"]);
    let one = bb.int(None);
    let made = bb.call(path, &[one]);
    let call = bb.method(made, "peek", &[]);
    let root = bb.block(&[], Some(call));
    let c = check_body(&fx, &bb, f_cell, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "f_cell checks clean: {:?}", c.diags);
    assert!(c.promoted.is_empty(), "a `!Freeze` autoref is never promoted: {:?}", c.promoted);
}
