//! RFC 2229 disjoint field captures through `Box` dereferences (`(*b).field`, `(*s.command).prog`):
//! - Borrowing a field behind `Box` (`&(*b).a` / `&mut (*b).a`) captures the precise field through
//!   `CaptureProj::Deref`, staying disjoint from sibling fields behind the same `Box` (`(*b).b`).
//! - By-value captures (`move ||` or moving a non-`Copy` field out of `Box`) still truncate before
//!   `CaptureProj::Deref` so the `Box` itself is moved (RFC 2229: moving a part out of `Box` would
//!   leave the outer owner responsible for freeing the allocation).

use std::vec::Vec;

use super::check_body;
use crate::body::{Expr, IntSuffix};
use crate::columns::CaptureKind;
use crate::env_fields::CaptureProj;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ItemKey, LangItem, Mutbl, Name, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;

struct BoxWorld {
    fx: Fx,
    read_field: ItemKey,
    mut_field: ItemKey,
    root_box: ItemKey,
    move_box: ItemKey,
    move_non_copy_field: ItemKey,
    nested_box: ItemKey,
    move_drop_struct: ItemKey,
}

fn box_world() -> BoxWorld {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let copy = fx.tr("Copy", 1, Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    let drop = fx.tr("Drop", 1, Vec::new(), &[]);
    fx.lang(LangItem::Drop, drop);

    let p0 = fx.param(0);
    let u8_ = fx.u8();
    let unit = fx.unit();
    fx.imp("impl Copy for u8", 0, u8_, Some((copy, &[])), &[], Vec::new());

    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    fx.lang(LangItem::Deref, deref);
    let ref_self = fx.r(Mutbl::Not, p0);
    let target = fx.alias(deref, "Target", &[p0]);
    let ref_target = fx.r(Mutbl::Not, target);
    fx.trait_fn(deref, "deref", true, &[ref_self], ref_target);

    let deref_bound = fx.bound(deref, &[p0]);
    let deref_mut = fx.tr("DerefMut", 1, std::vec![deref_bound], &[]);
    fx.lang(LangItem::DerefMut, deref_mut);
    let mut_self = fx.r(Mutbl::Mut, p0);
    let mut_target = fx.r(Mutbl::Mut, target);
    fx.trait_fn(deref_mut, "deref_mut", true, &[mut_self], mut_target);

    let owned_box = fx.strukt("Box", 1, &[("ptr", p0)]);
    fx.lang(LangItem::OwnedBox, owned_box);
    let box_p0 = fx.adt(owned_box, &[p0]);
    fx.imp("impl Deref for Box", 1, box_p0, Some((deref, &[])), &[("Target", p0)], Vec::new());
    fx.imp("impl DerefMut for Box", 1, box_p0, Some((deref_mut, &[])), &[], Vec::new());

    let owned = fx.strukt("Owned", 0, &[("v", u8_)]);
    let owned_ty = fx.adt(owned, &[]);

    let cmd = fx.strukt("Cmd", 0, &[("prog", u8_), ("extra", owned_ty)]);
    let cmd_ty = fx.adt(cmd, &[]);
    let box_cmd = fx.adt(owned_box, &[cmd_ty]);
    let box_box_cmd = fx.adt(owned_box, &[box_cmd]);

    let started = fx.strukt("Started", 0, &[("command", box_cmd), ("limit", u8_)]);
    let started_ty = fx.adt(started, &[]);

    let dropped = fx.strukt("Dropped", 0, &[("command", box_cmd), ("limit", u8_)]);
    let dropped_ty = fx.adt(dropped, &[]);
    fx.imp("impl Drop for Dropped", 0, dropped_ty, Some((drop, &[])), &[], Vec::new());

    let read_field = fx.func("read_field", 0, &[started_ty], unit, Vec::new());
    let mut_field = fx.func("mut_field", 0, &[started_ty], unit, Vec::new());
    let root_box = fx.func("root_box", 0, &[box_cmd], unit, Vec::new());
    let move_box = fx.func("move_box", 0, &[box_cmd], unit, Vec::new());
    let move_non_copy_field = fx.func("move_non_copy_field", 0, &[box_cmd], unit, Vec::new());
    let nested_box = fx.func("nested_box", 0, &[box_box_cmd], unit, Vec::new());
    let move_drop_struct = fx.func("move_drop_struct", 0, &[dropped_ty], unit, Vec::new());
    fx.finish();
    BoxWorld { fx, read_field, mut_field, root_box, move_box, move_non_copy_field, nested_box, move_drop_struct }
}

/// Positive: reading `s.command.prog` inside a closure captures `&(*s.command).prog` (`len == 3`,
/// `[Field(0), Deref, Field(0)]`, `&u8`), disjoint from both `s.limit` and `(*s.command).extra`.
#[test]
fn box_deref_field_captures_through_box_deref() {
    let w = box_world();
    let mut b = Bb::new();
    let s = b.param("s");
    let ls = b.local(s);
    let cmd = b.e(Expr::Field { base: ls, name: Name::of("command") });
    let prog = b.e(Expr::Field { base: cmd, name: Name::of("prog") });
    let cbody = b.block(&[], Some(prog));
    let cl = b.closure(&[], cbody);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.read_field, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    assert_eq!(fields.len(), 1, "expected 1 captured place for `(*s.command).prog`");
    let f = fields[0];
    assert_eq!(f.base, s);
    assert_eq!(f.len, 3, "must capture `(*s.command).prog` (len 3)");
    assert_eq!(f.how, CaptureKind::ByRef);
    assert_eq!(f.proj(&c, 0).map(|(p, _)| p), Some(CaptureProj::Field { variant: 0, field: 0 }));
    assert_eq!(f.proj(&c, 1).map(|(p, _)| p), Some(CaptureProj::Deref));
    assert_eq!(f.proj(&c, 2).map(|(p, _)| p), Some(CaptureProj::Field { variant: 0, field: 0 }));
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "&u8");
}

/// Soundness: mutating `s.command.prog = 1` captures `&mut (*s.command).prog` (`len == 3`,
/// `CaptureKind::ByMutRef`, `&mut u8`), preserving mutable borrow through `Box` deref.
#[test]
fn box_deref_field_mutation_captures_field_by_mut_ref() {
    let w = box_world();
    let mut b = Bb::new();
    let s = b.param("s");
    let ls = b.local(s);
    let cmd = b.e(Expr::Field { base: ls, name: Name::of("command") });
    let prog = b.e(Expr::Field { base: cmd, name: Name::of("prog") });
    let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let assign = b.e(Expr::Assign { lhs: prog, rhs: one });
    let cl = b.closure(&[], assign);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.mut_field, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    assert_eq!(fields.len(), 1);
    let f = fields[0];
    assert_eq!(f.base, s);
    assert_eq!(f.len, 3);
    assert_eq!(f.how, CaptureKind::ByMutRef);
    assert_eq!(f.proj(&c, 0).map(|(p, _)| p), Some(CaptureProj::Field { variant: 0, field: 0 }));
    assert_eq!(f.proj(&c, 1).map(|(p, _)| p), Some(CaptureProj::Deref));
    assert_eq!(f.proj(&c, 2).map(|(p, _)| p), Some(CaptureProj::Field { variant: 0, field: 0 }));
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "&mut u8");
}

/// Positive: dereferencing a root `b: Box<Cmd>` (`b.prog`) captures `&(*b).prog` (`len == 2`,
/// `[Deref, Field(0)]`, `&u8`), disjoint from `(*b).extra`.
#[test]
fn box_deref_on_root_binding_captures_disjoint_field() {
    let w = box_world();
    let mut b = Bb::new();
    let bx = b.param("b");
    let lb = b.local(bx);
    let prog = b.e(Expr::Field { base: lb, name: Name::of("prog") });
    let cbody = b.block(&[], Some(prog));
    let cl = b.closure(&[], cbody);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.root_box, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    assert_eq!(fields.len(), 1);
    let f = fields[0];
    assert_eq!(f.base, bx);
    assert_eq!(f.len, 2, "root `Box` field read captures `(*b).prog` (len 2)");
    assert_eq!(f.how, CaptureKind::ByRef);
    assert_eq!(f.proj(&c, 0).map(|(p, _)| p), Some(CaptureProj::Deref));
    assert_eq!(f.proj(&c, 1).map(|(p, _)| p), Some(CaptureProj::Field { variant: 0, field: 0 }));
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "&u8");
}

/// Positive: nested `Box<Box<Cmd>>` (`bb.prog`) captures `&(**bb).prog` (`len == 3`,
/// `[Deref, Deref, Field(0)]`, `&u8`).
#[test]
fn nested_box_deref_captures_disjoint_field() {
    let w = box_world();
    let mut b = Bb::new();
    let bb = b.param("bb");
    let lbb = b.local(bb);
    let prog = b.e(Expr::Field { base: lbb, name: Name::of("prog") });
    let cbody = b.block(&[], Some(prog));
    let cl = b.closure(&[], cbody);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.nested_box, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    assert_eq!(fields.len(), 1);
    let f = fields[0];
    assert_eq!(f.base, bb);
    assert_eq!(f.len, 3);
    assert_eq!(f.how, CaptureKind::ByRef);
    assert_eq!(f.proj(&c, 0).map(|(p, _)| p), Some(CaptureProj::Deref));
    assert_eq!(f.proj(&c, 1).map(|(p, _)| p), Some(CaptureProj::Deref));
    assert_eq!(f.proj(&c, 2).map(|(p, _)| p), Some(CaptureProj::Field { variant: 0, field: 0 }));
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "&u8");
}

/// RFC 2229 `move` rule: `move || b.prog` on `b: Box<Cmd>` truncates before the `Box` deref and
/// captures whole `b` by value (`len == 0`, `ByValue`, `Box<Cmd>`).
#[test]
fn move_closure_over_root_box_captures_whole_box() {
    let w = box_world();
    let mut b = Bb::new();
    let bx = b.param("b");
    let lb = b.local(bx);
    let prog = b.e(Expr::Field { base: lb, name: Name::of("prog") });
    let cbody = b.block(&[], Some(prog));
    let cl = b.move_closure(&[], cbody);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.move_box, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    assert_eq!(fields.len(), 1);
    let f = fields[0];
    assert_eq!(f.base, bx);
    assert_eq!(f.len, 0, "`move` closure must truncate before `Box` deref");
    assert_eq!(f.how, CaptureKind::ByValue);
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "Box<Cmd>");
}

/// RFC 2229 by-value rule: a non-`move` closure moving a non-`Copy` field `b.extra` out of
/// `b: Box<Cmd>` truncates before the `Box` deref and captures whole `b` by value (`len == 0`).
#[test]
fn by_value_move_of_non_copy_field_in_box_captures_whole_box() {
    let w = box_world();
    let mut b = Bb::new();
    let bx = b.param("b");
    let lb = b.local(bx);
    let extra = b.e(Expr::Field { base: lb, name: Name::of("extra") });
    let cbody = b.block(&[], Some(extra));
    let cl = b.closure(&[], cbody);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.move_non_copy_field, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let fields: Vec<_> = c.env_fields_by::<crate::Places>(cl).collect();
    assert_eq!(fields.len(), 1);
    let f = fields[0];
    assert_eq!(f.base, bx);
    assert_eq!(f.len, 0, "moving non-Copy field out of `Box` must capture whole `b`");
    assert_eq!(f.how, CaptureKind::ByValue);
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "Box<Cmd>");
}

/// Negative / Drop soundness: a `move` closure reading `d.command.prog` on a struct `Dropped` that
/// implements `Drop` must still move `d` whole (`len == 0`, `ByValue`, `Dropped`) so `Drop::drop`
/// runs once on the whole struct (`restrict_precision_for_drop_types`).
#[test]
fn move_closure_over_box_field_of_drop_struct_captures_whole_struct() {
    let w = box_world();
    let mut b = Bb::new();
    let d = b.param("d");
    let ld = b.local(d);
    let cmd = b.e(Expr::Field { base: ld, name: Name::of("command") });
    let prog = b.e(Expr::Field { base: cmd, name: Name::of("prog") });
    let cbody = b.block(&[], Some(prog));
    let cl = b.move_closure(&[], cbody);
    let st = b.semi(cl);
    let root = b.block(&[st], None);

    let c = check_body(&w.fx, &b, w.move_drop_struct, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let range = c.capture_places_of(cl);
    assert_eq!(range.len(), 1);
    let row = range.start;
    assert_eq!(c.capture_places.column::<1>()[row], d);
    assert_eq!(c.capture_places.column::<3>()[row], 0, "`Drop` struct moved by `move` closure must capture whole `d`");
    assert_eq!(c.capture_places.column::<4>()[row], CaptureKind::ByValue);
    assert_eq!(show(&w.fx, c.types.view(), c.capture_places.column::<5>()[row]), "Dropped");
}
