//! Shared-reference reborrow coercion (`rules/coerce.rs::coerce_borrowed`) and RFC 2229 capture
//! precision (`capture_places.rs`):
//!
//! When a `&'a T` binding `pk` is passed to a parameter expecting `&Q` (e.g. `adjacent.get(pk)`
//! in `crates/blaze-resolve/src/graph.rs:150` `path_to_top`), `coerce_borrowed` emits a shared
//! reborrow `[Deref(None), Borrow(Not)]` (`&*pk`), matching rustc's `coerce_borrowed_pointer`.
//! Under RFC 2229 (`truncate_capture_for_optimization`), an inner non-`move` closure therefore
//! captures `*pk` by shared borrow (`len == 1`, `[Deref]`, stored `&N`, lifetime `'a`) rather
//! than borrowing the enclosing closure's local stack slot `pk` (`len == 0`, stored `&&N`).
//!
//! Soundness controls:
//! - Passing `pk` to a generic value parameter `fn use_val<T>(_: T)` or returning `|| pk` with
//!   an inferred return type does NOT trigger `coerce_borrowed` and still captures `pk` (`len ==
//!   0`, stored `&&N`).
//! - Passing a non-reference `Copy` local `x: u8` still captures `x` (`len == 0`, stored `&u8`).
//! - Mutating `pk` itself inside the closure merges `*pk` into ancestor `pk` (`len == 0`,
//!   `ByMutRef`, stored `&mut &N`).

use std::vec::Vec;

use super::check_body;
use crate::body::Expr;
use crate::columns::{AdjustKind, CaptureKind, Checked};
use crate::env_fields::CaptureProj;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ExprId, ItemKey, LangItem, List, Mutbl};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

struct ReborrowWorld {
    fx: Fx,
    node: ItemKey,
    pass_to_ref: ItemKey,
    pass_to_val: ItemKey,
    return_bare: ItemKey,
    copy_non_ref: ItemKey,
    mutate_ref_binding: ItemKey,
}

fn adjusts_of(c: &Checked<'_>, e: ExprId) -> Vec<AdjustKind> {
    c.adjusts
        .column::<0>()
        .iter()
        .zip(c.adjusts.column::<1>().iter())
        .filter(|&(&ex, _)| ex == e)
        .map(|(_, &k)| k)
        .collect()
}

fn reborrow_world() -> ReborrowWorld {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let copy = fx.tr("Copy", 1, Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);

    let p0 = fx.param(0);
    let u8_ = fx.u8();
    let unit = fx.unit();
    let ref_p0 = fx.r(Mutbl::Not, p0);
    fx.imp("impl Copy for u8", 0, u8_, Some((copy, &[])), &[], Vec::new());
    fx.imp("impl<T> Copy for &T", 1, ref_p0, Some((copy, &[])), &[], Vec::new());

    let node = fx.strukt("Node", 0, &[("id", u8_)]);
    let node_ty = fx.adt(node, &[]);
    let node_ref = fx.r(Mutbl::Not, node_ty);

    // fn get<Q>(k: &Q) {}
    fx.func("get", 1, &[ref_p0], unit, Vec::new());
    // fn use_val<T>(v: T) {}
    fx.func("use_val", 1, &[p0], unit, Vec::new());
    // fn use_u8(v: u8) {}
    fx.func("use_u8", 0, &[u8_], unit, Vec::new());

    let pass_to_ref = fx.func("pass_to_ref", 0, &[node_ref], unit, Vec::new());
    let pass_to_val = fx.func("pass_to_val", 0, &[node_ref], unit, Vec::new());
    let return_bare = fx.func("return_bare", 0, &[node_ref], unit, Vec::new());
    let copy_non_ref = fx.func("copy_non_ref", 0, &[u8_], unit, Vec::new());
    let mutate_ref_binding = fx.func("mutate_ref_binding", 0, &[node_ref, node_ref], unit, Vec::new());
    fx.finish();
    ReborrowWorld { fx, node, pass_to_ref, pass_to_val, return_bare, copy_non_ref, mutate_ref_binding }
}

/// Positive (`graph.rs:150` `path_to_top`): `|pk: &Node| { || get(pk) }` emits a shared reborrow
/// `[Deref(None), Borrow(Not)]` on `pk` and captures `*pk` (`len == 1`, `[Deref]`, stored `&Node`),
/// not `&pk` (`len == 0`, stored `&&Node`).
#[test]
fn shared_ref_arg_reborrows_and_captures_deref_place() {
    let w = reborrow_world();
    let mut b = Bb::new();
    let pk = b.param("pk");
    let lpk = b.local(pk);
    let get = b.path(&["get"]);
    let call = b.call(get, &[lpk]);
    let cl = b.closure(&[], call);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.pass_to_ref, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let adjs = adjusts_of(&c, lpk);
    assert_eq!(
        adjs,
        std::vec![
            AdjustKind::Deref { overloaded: None },
            AdjustKind::Borrow { mutbl: Mutbl::Not, two_phase: false, raw: false },
        ],
        "`get(pk)` must emit shared reborrow `[Deref, Borrow]` on `pk`"
    );

    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    assert_eq!(fields.len(), 1, "expected 1 captured place for `*pk`");
    let f = fields[0];
    assert_eq!(f.base, pk);
    assert_eq!(f.len, 1, "must capture `*pk` (len 1), not local slot `pk` (len 0)");
    assert_eq!(f.how, CaptureKind::ByRef);
    assert_eq!(f.proj(&c, 0).map(|(p, _)| p), Some(CaptureProj::Deref));
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "&Node");
}

/// Positive: `|| { let _x: &Node = pk; }` also reborrows `pk` and captures `*pk` (`&Node`).
#[test]
fn shared_ref_annotated_let_reborrows_and_captures_deref_place() {
    let w = reborrow_world();
    let mut b = Bb::new();
    let b_node = b.ty(PNode::Adt(w.node, List::EMPTY));
    let b_node_ref = b.ty(PNode::Ref(Mutbl::Not, b_node));
    let pk = b.param("pk");
    let lpk = b.local(pk);
    let x = b.bind("x");
    let let_x = b.let_(x, Some(b_node_ref), lpk);
    let body = b.block(&[let_x], None);
    let cl = b.closure(&[], body);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.pass_to_ref, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    assert_eq!(fields.len(), 1);
    let f = fields[0];
    assert_eq!(f.base, pk);
    assert_eq!(f.len, 1, "must capture `*pk` (len 1)");
    assert_eq!(f.how, CaptureKind::ByRef);
    assert_eq!(f.proj(&c, 0).map(|(p, _)| p), Some(CaptureProj::Deref));
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "&Node");
}

/// Soundness negative 1: `|| use_val(pk)` (generic `T` parameter, not `&Q`) must NOT reborrow
/// `pk` and must still capture local `pk` (`len == 0`, stored `&&Node`), matching rustc E0373.
#[test]
fn generic_value_param_does_not_reborrow_and_captures_local_ref() {
    let w = reborrow_world();
    let mut b = Bb::new();
    let pk = b.param("pk");
    let lpk = b.local(pk);
    let use_val = b.path(&["use_val"]);
    let call = b.call(use_val, &[lpk]);
    let cl = b.closure(&[], call);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.pass_to_val, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    assert!(adjusts_of(&c, lpk).is_empty(), "`use_val(pk)` must emit no reborrow adjustment");
    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    assert_eq!(fields.len(), 1);
    let f = fields[0];
    assert_eq!(f.base, pk);
    assert_eq!(f.len, 0, "must capture local `pk` (len 0), not `*pk`");
    assert_eq!(f.how, CaptureKind::ByRef);
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "&&Node");
}

/// Soundness negative 2: `|| pk` (inferred closure return type, not `&T`) must capture local `pk`
/// (`len == 0`, stored `&&Node`), matching rustc E0597.
#[test]
fn bare_ref_closure_return_captures_local_ref() {
    let w = reborrow_world();
    let mut b = Bb::new();
    let pk = b.param("pk");
    let lpk = b.local(pk);
    let cl = b.closure(&[], lpk);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.return_bare, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    assert_eq!(fields.len(), 1);
    let f = fields[0];
    assert_eq!(f.base, pk);
    assert_eq!(f.len, 0, "`|| pk` must capture local `pk` (len 0)");
    assert_eq!(f.how, CaptureKind::ByRef);
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "&&Node");
}

/// Soundness negative 3: non-reference `Copy` local (`x: u8`) is still captured by shared borrow
/// of local `x` (`len == 0`, stored `&u8`).
#[test]
fn copy_non_ref_local_still_captures_by_ref() {
    let w = reborrow_world();
    let mut b = Bb::new();
    let x = b.param("x");
    let lx = b.local(x);
    let use_u8 = b.path(&["use_u8"]);
    let call = b.call(use_u8, &[lx]);
    let cl = b.closure(&[], call);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.copy_non_ref, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    assert_eq!(fields.len(), 1);
    let f = fields[0];
    assert_eq!(f.base, x);
    assert_eq!(f.len, 0);
    assert_eq!(f.how, CaptureKind::ByRef);
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "&u8");
}

/// Soundness negative 4: mutating `pk` itself (`pk = other; get(pk);`) merges `*pk` into ancestor
/// `pk` (`len == 0`, `ByMutRef`, stored `&mut &Node`).
#[test]
fn mutating_ref_binding_merges_reborrow_into_mut_local_capture() {
    let w = reborrow_world();
    let mut b = Bb::new();
    let pk = b.param_mut("pk");
    let other = b.param("other");
    let lpk_lhs = b.local(pk);
    let lother = b.local(other);
    let assign = b.e(Expr::Assign { lhs: lpk_lhs, rhs: lother });
    let s1 = b.semi(assign);
    let lpk_arg = b.local(pk);
    let get = b.path(&["get"]);
    let call = b.call(get, &[lpk_arg]);
    let body = b.block(&[s1], Some(call));
    let cl = b.closure(&[], body);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.mutate_ref_binding, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    let pk_field = fields.iter().find(|f| f.base == pk).expect("`pk` must be captured");
    assert_eq!(pk_field.len, 0, "mutating `pk` must merge `*pk` into whole `pk` (len 0)");
    assert_eq!(pk_field.how, CaptureKind::ByMutRef);
    assert_eq!(show(&w.fx, c.types.view(), pk_field.stored.expect("place stored type")), "&mut &Node");
}
