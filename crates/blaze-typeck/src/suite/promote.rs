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

/// CONST FIELD PROMOTION: `&ALIGNED_BYTES.bytes` (hashbrown's `Group::static_empty`) and `&PAIR.0`
/// are promoted when the base is a promotable `Freeze` rvalue without drop glue and without a
/// `Deref` adjustment. A field of a `!Freeze` base (`&CELL_PAIR.1`) or through a reference deref
/// (`&REF_CONST.bytes`) is never promoted.
#[test]
fn const_field_projection_is_promoted_when_base_is_promotable_freeze() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Default::default(), &[]);
    fx.lang(LangItem::Sized, sized);
    let freeze = fx.tr("Freeze", 1, Default::default(), &[]);
    fx.auto(freeze);
    fx.lang(LangItem::Freeze, freeze);
    let u8_ = fx.u8();
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let align_arr = fx.array(u64_, 0);
    let bytes_arr = fx.array(u8_, 8);
    let aligned = fx.strukt("AlignedBytes", 0, &[("_align", align_arr), ("bytes", bytes_arr)]);
    let aligned_ty = fx.adt(aligned, &[]);
    fx.konst("ALIGNED_BYTES", aligned_ty);

    let pair_ty = fx.tuple(&[u8_, u64_]);
    fx.konst("PAIR", pair_ty);

    let p0 = fx.param(0);
    let uc = fx.strukt("UnsafeCell", 1, &[("value", p0)]);
    let uc_p0 = fx.adt(uc, &[p0]);
    let neg = fx.imp("!Freeze for UnsafeCell<T>", 1, uc_p0, Some((freeze, &[])), &[], Default::default());
    fx.set_polarity(neg, ImplPolarity::Negative);
    let uc_u8 = fx.adt(uc, &[u8_]);
    let cell_pair_ty = fx.tuple(&[uc_u8, u8_]);
    fx.konst("CELL_PAIR", cell_pair_ty);

    let aligned_ref = fx.r(Mutbl::Not, aligned_ty);
    fx.konst("REF_CONST", aligned_ref);

    let bytes_ref = fx.r(Mutbl::Not, bytes_arr);
    let u8_ref = fx.r(Mutbl::Not, u8_);
    let f_bytes = fx.func("f_bytes", 0, &[], bytes_ref, Default::default());
    let f_pair = fx.func("f_pair", 0, &[], u8_ref, Default::default());
    let f_cell = fx.func("f_cell", 0, &[], u8_ref, Default::default());
    let f_deref = fx.func("f_deref", 0, &[], bytes_ref, Default::default());
    fx.finish();

    // Case 1: `&ALIGNED_BYTES.bytes` -> promoted.
    let mut bb = Bb::new();
    let base = bb.path(&["ALIGNED_BYTES"]);
    let field = bb.e(crate::body::Expr::Field { base, name: crate::ids::Name::of("bytes") });
    let r = bb.borrow(Mutbl::Not, field);
    let root = bb.block(&[], Some(r));
    let c = check_body(&fx, &bb, f_bytes, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "f_bytes checks clean: {:?}", c.diags);
    assert_eq!(&c.promoted[..], &[r][..], "`&ALIGNED_BYTES.bytes` must be promoted");

    // Case 2: `&PAIR.0` -> promoted.
    let mut bb = Bb::new();
    let base = bb.path(&["PAIR"]);
    let field = bb.e(crate::body::Expr::TupleField { base, index: 0 });
    let r = bb.borrow(Mutbl::Not, field);
    let root = bb.block(&[], Some(r));
    let c = check_body(&fx, &bb, f_pair, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "f_pair checks clean: {:?}", c.diags);
    assert_eq!(&c.promoted[..], &[r][..], "`&PAIR.0` must be promoted");

    // Case 3: `&CELL_PAIR.1` -> base is `!Freeze`, NOT promoted.
    let mut bb = Bb::new();
    let base = bb.path(&["CELL_PAIR"]);
    let field = bb.e(crate::body::Expr::TupleField { base, index: 1 });
    let r = bb.borrow(Mutbl::Not, field);
    let root = bb.block(&[], Some(r));
    let c = check_body(&fx, &bb, f_cell, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "f_cell checks clean: {:?}", c.diags);
    assert!(c.promoted.is_empty(), "field of `!Freeze` base must not be promoted: {:?}", c.promoted);

    // Case 4: `&REF_CONST.bytes` -> auto-derefs `&AlignedBytes`, NOT promoted.
    let mut bb = Bb::new();
    let base = bb.path(&["REF_CONST"]);
    let field = bb.e(crate::body::Expr::Field { base, name: crate::ids::Name::of("bytes") });
    let r = bb.borrow(Mutbl::Not, field);
    let root = bb.block(&[], Some(r));
    let c = check_body(&fx, &bb, f_deref, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "f_deref checks clean: {:?}", c.diags);
    assert!(c.promoted.is_empty(), "field through deref must not be promoted: {:?}", c.promoted);
}

/// VALUE-BASED ENUM / STRUCT PROMOTION: matches Rust Reference "Constant promotion" ("these
/// properties are determined based on the value where possible, e.g. `&None` always has the type
/// `&'static Option<_>`, as it contains nothing disallowed").
///
/// Positive (`blaze-manifest/src/targets.rs:272` `&[CrateType::Bin]`):
/// - `&[CrateType::Bin]` and `&CrateType::Bin` where `CrateType` also has `Other(DropStr)` -> PROMOTED.
/// - `&Mixed::Clean(1)` (tuple variant) and `&Mixed::StructClean { n: 1 }` (struct variant) when
///   sibling variants hold `DropStr` or `UnsafeCell<u64>` -> PROMOTED.
/// - `&OptCell::None` when `OptCell::Some(UnsafeCell<u64>)` is `!Freeze` -> PROMOTED.
/// - `&Wrapper { ct: CrateType::Bin }` when `Wrapper` has no explicit `Drop` -> PROMOTED.
///
/// Negative soundness controls:
/// - `&CrateType::Other(mk_drop())` (even when `mk_drop` is `#[rustc_promotable]`) -> NOT promoted.
/// - `&Mixed::StructDirty { s: mk_drop() }` -> NOT promoted.
/// - `&OptCell::Some(mk_uc(1))` -> NOT promoted.
/// - `&DropEnum::Unit` (`impl Drop for DropEnum`) -> NOT promoted.
/// - `&NotFreezeEnum::Unit` (`impl !Freeze for NotFreezeEnum`) -> NOT promoted.
/// - `&UnsafeCell { value: 1 }` (`impl<T> !Freeze for UnsafeCell<T>`) -> NOT promoted.
/// - `&DropWrapper { ct: CrateType::Bin }` (`impl Drop for DropWrapper`) -> NOT promoted.
#[test]
fn value_based_enum_variant_promotion_and_soundness_controls() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let freeze = fx.tr("Freeze", 1, Vec::new(), &[]);
    fx.auto(freeze);
    fx.lang(LangItem::Freeze, freeze);
    let drop_tr = fx.tr("Drop", 1, Vec::new(), &[]);
    fx.lang(LangItem::Drop, drop_tr);

    let p0 = fx.param(0);
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let uc = fx.strukt("UnsafeCell", 1, &[("value", p0)]);
    let uc_p0 = fx.adt(uc, &[p0]);
    let neg_uc = fx.imp("!Freeze for UnsafeCell<T>", 1, uc_p0, Some((freeze, &[])), &[], Vec::new());
    fx.set_polarity(neg_uc, ImplPolarity::Negative);
    let uc_u64 = fx.adt(uc, &[u64_]);

    let drop_str = fx.strukt("DropStr", 0, &[("ptr", u64_)]);
    let drop_str_ty = fx.adt(drop_str, &[]);
    fx.imp("impl Drop for DropStr", 0, drop_str_ty, Some((drop_tr, &[])), &[], Vec::new());

    let mk_drop = fx.func("mk_drop", 0, &[], drop_str_ty, Vec::new());
    fx.set_attrs(mk_drop, TypeAttrs(TypeAttrs::PROMOTABLE));
    let mk_uc = fx.func("mk_uc", 0, &[u64_], uc_u64, Vec::new());
    fx.set_attrs(mk_uc, TypeAttrs(TypeAttrs::PROMOTABLE));

    // enum CrateType { Bin, Other(DropStr) }
    let (crate_type, _) = fx.enum_("CrateType", 0, &[("Bin", &[]), ("Other", &[drop_str_ty])]);
    let ct_ty = fx.adt(crate_type, &[]);
    let ct_slice = fx.ty(PNode::Slice(ct_ty));
    let ref_ct_slice = fx.r(Mutbl::Not, ct_slice);
    let ref_ct = fx.r(Mutbl::Not, ct_ty);

    // enum OptCell { None, Some(UnsafeCell<u64>) }
    let (opt_cell, _) = fx.enum_("OptCell", 0, &[("None", &[]), ("Some", &[uc_u64])]);
    let opt_cell_ty = fx.adt(opt_cell, &[]);
    let ref_opt_cell = fx.r(Mutbl::Not, opt_cell_ty);

    // enum Mixed { Clean(u64), Dirty(DropStr), StructClean { n: u64 }, StructDirty { s: DropStr } }
    let (mixed, _) = fx.enum_full(
        "Mixed",
        0,
        &[
            ("Clean", crate::world::CtorKind::Tuple, &[("0", u64_)]),
            ("Dirty", crate::world::CtorKind::Tuple, &[("0", drop_str_ty)]),
            ("StructClean", crate::world::CtorKind::Braces, &[("n", u64_)]),
            ("StructDirty", crate::world::CtorKind::Braces, &[("s", drop_str_ty)]),
        ],
    );
    let mixed_ty = fx.adt(mixed, &[]);
    let ref_mixed = fx.r(Mutbl::Not, mixed_ty);

    // struct Wrapper { ct: CrateType } (no Drop)
    let wrapper = fx.strukt("Wrapper", 0, &[("ct", ct_ty)]);
    let wrapper_ty = fx.adt(wrapper, &[]);
    let ref_wrapper = fx.r(Mutbl::Not, wrapper_ty);

    // struct DropWrapper { ct: CrateType }; impl Drop for DropWrapper
    let drop_wrapper = fx.strukt("DropWrapper", 0, &[("ct", ct_ty)]);
    let drop_wrapper_ty = fx.adt(drop_wrapper, &[]);
    fx.imp("impl Drop for DropWrapper", 0, drop_wrapper_ty, Some((drop_tr, &[])), &[], Vec::new());
    let ref_drop_wrapper = fx.r(Mutbl::Not, drop_wrapper_ty);

    // enum DropEnum { Unit }; impl Drop for DropEnum
    let (drop_enum, _) = fx.enum_("DropEnum", 0, &[("Unit", &[])]);
    let drop_enum_ty = fx.adt(drop_enum, &[]);
    fx.imp("impl Drop for DropEnum", 0, drop_enum_ty, Some((drop_tr, &[])), &[], Vec::new());
    let ref_drop_enum = fx.r(Mutbl::Not, drop_enum_ty);

    // enum NotFreezeEnum { Unit }; impl !Freeze for NotFreezeEnum
    let (nf_enum, _) = fx.enum_("NotFreezeEnum", 0, &[("Unit", &[])]);
    let nf_enum_ty = fx.adt(nf_enum, &[]);
    let neg_nf = fx.imp("!Freeze for NotFreezeEnum", 0, nf_enum_ty, Some((freeze, &[])), &[], Vec::new());
    fx.set_polarity(neg_nf, ImplPolarity::Negative);
    let ref_nf_enum = fx.r(Mutbl::Not, nf_enum_ty);

    let ref_uc = fx.r(Mutbl::Not, uc_u64);

    let f_arr_bin = fx.func("f_arr_bin", 0, &[], ref_ct_slice, Vec::new());
    let f_bin = fx.func("f_bin", 0, &[], ref_ct, Vec::new());
    let f_other = fx.func("f_other", 0, &[], ref_ct, Vec::new());
    let f_opt_none = fx.func("f_opt_none", 0, &[], ref_opt_cell, Vec::new());
    let f_opt_some = fx.func("f_opt_some", 0, &[], ref_opt_cell, Vec::new());
    let f_mixed_clean = fx.func("f_mixed_clean", 0, &[], ref_mixed, Vec::new());
    let f_mixed_sclean = fx.func("f_mixed_sclean", 0, &[], ref_mixed, Vec::new());
    let f_mixed_sdirty = fx.func("f_mixed_sdirty", 0, &[], ref_mixed, Vec::new());
    let f_wrapper = fx.func("f_wrapper", 0, &[], ref_wrapper, Vec::new());
    let f_drop_wrapper = fx.func("f_drop_wrapper", 0, &[], ref_drop_wrapper, Vec::new());
    let f_drop_enum = fx.func("f_drop_enum", 0, &[], ref_drop_enum, Vec::new());
    let f_nf_enum = fx.func("f_nf_enum", 0, &[], ref_nf_enum, Vec::new());
    let f_uc_lit = fx.func("f_uc_lit", 0, &[], ref_uc, Vec::new());
    fx.finish();

    // Positive 1: `&[CrateType::Bin]` (unsized to `&[CrateType]`) -> PROMOTED.
    {
        let mut bb = Bb::new();
        let bin = bb.path(&["CrateType", "Bin"]);
        let arr = bb.array(&[bin]);
        let r = bb.borrow(Mutbl::Not, arr);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_arr_bin, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert_eq!(&c.promoted[..], &[r][..], "`&[CrateType::Bin]` must be promoted");
    }

    // Positive 2: `&CrateType::Bin` -> PROMOTED.
    {
        let mut bb = Bb::new();
        let bin = bb.path(&["CrateType", "Bin"]);
        let r = bb.borrow(Mutbl::Not, bin);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_bin, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert_eq!(&c.promoted[..], &[r][..], "`&CrateType::Bin` must be promoted");
    }

    // Positive 3: `&OptCell::None` (sibling variant is `!Freeze`) -> PROMOTED.
    {
        let mut bb = Bb::new();
        let none = bb.path(&["OptCell", "None"]);
        let r = bb.borrow(Mutbl::Not, none);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_opt_none, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert_eq!(&c.promoted[..], &[r][..], "`&OptCell::None` must be promoted");
    }

    // Positive 4: `&Mixed::Clean(1)` (tuple variant without Drop) -> PROMOTED.
    {
        let mut bb = Bb::new();
        let ctor = bb.path(&["Mixed", "Clean"]);
        let one = bb.int(None);
        let call = bb.call(ctor, &[one]);
        let r = bb.borrow(Mutbl::Not, call);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_mixed_clean, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert_eq!(&c.promoted[..], &[r][..], "`&Mixed::Clean(1)` must be promoted");
    }

    // Positive 5: `&Mixed::StructClean { n: 1 }` -> PROMOTED.
    {
        let mut bb = Bb::new();
        let one = bb.int(None);
        let s = bb.struct_lit(&["Mixed", "StructClean"], &[("n", one)], None);
        let r = bb.borrow(Mutbl::Not, s);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_mixed_sclean, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert_eq!(&c.promoted[..], &[r][..], "`&Mixed::StructClean {{ n: 1 }}` must be promoted");
    }

    // Positive 6: `&Wrapper { ct: CrateType::Bin }` -> PROMOTED.
    {
        let mut bb = Bb::new();
        let bin = bb.path(&["CrateType", "Bin"]);
        let s = bb.struct_lit(&["Wrapper"], &[("ct", bin)], None);
        let r = bb.borrow(Mutbl::Not, s);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_wrapper, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert_eq!(&c.promoted[..], &[r][..], "`&Wrapper {{ ct: CrateType::Bin }}` must be promoted");
    }

    // Negative 1: `&CrateType::Other(mk_drop())` -> NOT promoted (active variant holds `DropStr`).
    {
        let mut bb = Bb::new();
        let mk = bb.path(&["mk_drop"]);
        let d = bb.call(mk, &[]);
        let ctor = bb.path(&["CrateType", "Other"]);
        let call = bb.call(ctor, &[d]);
        let r = bb.borrow(Mutbl::Not, call);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_other, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert!(c.promoted.is_empty(), "Drop variant `&CrateType::Other(mk_drop())` must NOT be promoted: {:?}", c.promoted);
    }

    // Negative 2: `&Mixed::StructDirty { s: mk_drop() }` -> NOT promoted.
    {
        let mut bb = Bb::new();
        let mk = bb.path(&["mk_drop"]);
        let d = bb.call(mk, &[]);
        let s = bb.struct_lit(&["Mixed", "StructDirty"], &[("s", d)], None);
        let r = bb.borrow(Mutbl::Not, s);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_mixed_sdirty, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert!(c.promoted.is_empty(), "Drop struct variant must NOT be promoted: {:?}", c.promoted);
    }

    // Negative 3: `&OptCell::Some(mk_uc(1))` -> NOT promoted (`!Freeze` argument).
    {
        let mut bb = Bb::new();
        let mk = bb.path(&["mk_uc"]);
        let one = bb.int(None);
        let cell = bb.call(mk, &[one]);
        let ctor = bb.path(&["OptCell", "Some"]);
        let call = bb.call(ctor, &[cell]);
        let r = bb.borrow(Mutbl::Not, call);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_opt_some, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert!(c.promoted.is_empty(), "`!Freeze` variant `&OptCell::Some(..)` must NOT be promoted: {:?}", c.promoted);
    }

    // Negative 4: `&DropEnum::Unit` (`impl Drop for DropEnum`) -> NOT promoted.
    {
        let mut bb = Bb::new();
        let u = bb.path(&["DropEnum", "Unit"]);
        let r = bb.borrow(Mutbl::Not, u);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_drop_enum, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert!(c.promoted.is_empty(), "enum with explicit `impl Drop` must NOT be promoted: {:?}", c.promoted);
    }

    // Negative 5: `&NotFreezeEnum::Unit` (`impl !Freeze for NotFreezeEnum`) -> NOT promoted.
    {
        let mut bb = Bb::new();
        let u = bb.path(&["NotFreezeEnum", "Unit"]);
        let r = bb.borrow(Mutbl::Not, u);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_nf_enum, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert!(c.promoted.is_empty(), "enum with explicit `impl !Freeze` must NOT be promoted: {:?}", c.promoted);
    }

    // Negative 6: `&UnsafeCell { value: 1 }` (struct literal of `!Freeze` ADT) -> NOT promoted.
    {
        let mut bb = Bb::new();
        let one = bb.int(None);
        let s = bb.struct_lit(&["UnsafeCell"], &[("value", one)], None);
        let r = bb.borrow(Mutbl::Not, s);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_uc_lit, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert!(c.promoted.is_empty(), "struct literal of `!Freeze` ADT must NOT be promoted: {:?}", c.promoted);
    }

    // Negative 7: `&DropWrapper { ct: CrateType::Bin }` (`impl Drop for DropWrapper`) -> NOT promoted.
    {
        let mut bb = Bb::new();
        let bin = bb.path(&["CrateType", "Bin"]);
        let s = bb.struct_lit(&["DropWrapper"], &[("ct", bin)], None);
        let r = bb.borrow(Mutbl::Not, s);
        let root = bb.block(&[], Some(r));
        let c = check_body(&fx, &bb, f_drop_wrapper, root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
        assert!(c.promoted.is_empty(), "struct with explicit `impl Drop` must NOT be promoted: {:?}", c.promoted);
    }
}
