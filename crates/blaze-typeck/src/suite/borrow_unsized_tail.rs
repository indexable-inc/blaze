//! Borrowing an expression under an expected reference to an unsized-tail struct (`&Path`,
//! `&OsStr`, `&CStr`, `&dyn Trait`): `expr_borrow` passes `Expectation::rvalue_hint(inner)`
//! (`Expectation::RvalueLikeUnsized(Path)`) rather than `Expectation::HasType(Path)`.
//!
//! `blaze-manifest/src/source.rs:79, 81, 82`: `SourceId::for_path(&path()?)?` where `for_path`
//! takes `&Path` (`struct Path { inner: OsStr }` -> `[u8]` tail) and `path()?` desugars to a
//! `match` whose first arm diverges (`!`) and whose second arm has type `PathBuf: Deref<Target =
//! Path>`. Before the fix `expr_borrow` only recognized bare `str` / `[T]` and passed
//! `Expectation::HasType(Path)` into `match`, causing `IncompatibleArms` (`found PathBuf,
//! expected Path`) on the second arm.

use std::vec::Vec;

use super::check_body;
use crate::body::IntSuffix;
use crate::diag::{DiagCode, Node};
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// World: `Slice([u8])`, `Path(Slice)`, `PathBuf: Deref<Target = Path>`, `Other` (no `Deref`),
/// `for_path(&Path) -> u32`, `id<T>(T) -> T`.
/// Returns `(fx, [ok_match, ok_call, bad_match])`.
fn world() -> (Fx, [ItemKey; 3]) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    fx.lang(LangItem::Deref, deref);

    let bool_ = fx.ty(PNode::Bool);
    let u8_ = fx.u8();
    let u32_ = fx.u32();
    let p0 = fx.param(0);
    let slice_u8 = fx.ty(PNode::Slice(u8_));

    let slice_k = fx.strukt("Slice", 0, &[("inner", slice_u8)]);
    let slice_ty = fx.adt(slice_k, &[]);
    let path_k = fx.strukt("Path", 0, &[("inner", slice_ty)]);
    let path_ty = fx.adt(path_k, &[]);
    let r_path = fx.r(Mutbl::Not, path_ty);

    let path_buf_k = fx.strukt("PathBuf", 0, &[]);
    let path_buf_ty = fx.adt(path_buf_k, &[]);
    fx.imp("impl Deref for PathBuf", 0, path_buf_ty, Some((deref, &[])), &[("Target", path_ty)], Vec::new());

    let other_k = fx.strukt("Other", 0, &[]);
    let other_ty = fx.adt(other_k, &[]);

    fx.func("for_path", 0, &[r_path], u32_, Vec::new());
    fx.func("id", 1, &[p0], p0, Vec::new());

    let ok_match = fx.func("ok_match", 0, &[bool_, path_buf_ty], u32_, Vec::new());
    let ok_call = fx.func("ok_call", 0, &[path_buf_ty], u32_, Vec::new());
    let bad_match = fx.func("bad_match", 0, &[bool_, other_ty], u32_, Vec::new());
    fx.finish();
    (world, [ok_match, ok_call, bad_match])
}

/// `for_path(&match flag { _ => return 0u32, _ => pb })` (`&path()?` shape): clean.
/// Before the fix: `IncompatibleArms` (`found PathBuf, expected Path`) on the second arm.
#[test]
fn borrow_of_match_under_unsized_struct_tail_deref_coerces() {
    let (fx, [ok_match, ..]) = world();
    let mut b = Bb::new();
    let flag = b.param("flag");
    let pb = b.param("pb");
    let scrut = b.local(flag);
    let w0 = b.wild();
    let w1 = b.wild();
    let zero = b.int(Some(IntSuffix::Uint(UintTy::U32)));
    let early = b.ret(Some(zero));
    let pb_use = b.local(pb);
    let m = b.match_(scrut, &[(w0, early), (w1, pb_use)]);
    let borrowed = b.borrow(Mutbl::Not, m);
    let callee = b.path(&["for_path"]);
    let call = b.call(callee, &[borrowed]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, ok_match, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "ok_match: {:?}", c.diags);
}

/// `for_path(&id(pb))`: `&` passes `RvalueLikeUnsized(Path)` to `id(pb)`; `expected_inputs`
/// recognizes `Path` as an unsized struct tail, so `T = PathBuf` and `&PathBuf` derefs to `&Path`.
#[test]
fn borrow_of_generic_call_under_unsized_struct_tail_deref_coerces() {
    let (fx, [_, ok_call, _]) = world();
    let mut b = Bb::new();
    let pb = b.param("pb");
    let id = b.path(&["id"]);
    let pb_use = b.local(pb);
    let id_call = b.call(id, &[pb_use]);
    let borrowed = b.borrow(Mutbl::Not, id_call);
    let callee = b.path(&["for_path"]);
    let call = b.call(callee, &[borrowed]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, ok_call, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "ok_call: {:?}", c.diags);
}

/// Negative control: `for_path(&match flag { _ => return 0u32, _ => other })` where `Other` has
/// no `Deref<Target = Path>`: refused with `Mismatch` at `&match ..`, never `IncompatibleArms`.
#[test]
fn borrow_of_match_without_deref_to_unsized_tail_is_mismatch_not_incompatible_arms() {
    let (fx, [_, _, bad_match]) = world();
    let mut b = Bb::new();
    let flag = b.param("flag");
    let other = b.param("other");
    let scrut = b.local(flag);
    let w0 = b.wild();
    let w1 = b.wild();
    let zero = b.int(Some(IntSuffix::Uint(UintTy::U32)));
    let early = b.ret(Some(zero));
    let other_use = b.local(other);
    let m = b.match_(scrut, &[(w0, early), (w1, other_use)]);
    let borrowed = b.borrow(Mutbl::Not, m);
    let callee = b.path(&["for_path"]);
    let call = b.call(callee, &[borrowed]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, bad_match, root, Plant::None, InferPlant::None);
    let got: Vec<_> = c.diags.iter().map(|d| (d.code, d.node)).collect();
    assert_eq!(got, std::vec![(DiagCode::Mismatch, Some(Node::Expr(borrowed)))], "bad_match: {:?}", c.diags);
}
