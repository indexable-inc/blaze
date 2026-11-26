//! Closure captures through a `Box` field (`self.command.prog` where `self.command: Box<Cmd>`)
//! truncate just before the `Box` deref (`self.command`, `len = 1`) instead of widening the entire
//! root binding (`self`, `len = 0`). That preserves RFC 2229 disjointness with sibling fields
//! (`self.limit` moved earlier, as in `blaze-core/src/process.rs::exited`) while keeping every
//! emitted `CaptureProj` lowerable by `blaze-build` (which walks thin/wide `&`/`*`, never `Box`).

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

    let cmd = fx.strukt("Cmd", 0, &[("prog", u8_)]);
    let cmd_ty = fx.adt(cmd, &[]);
    let box_cmd = fx.adt(owned_box, &[cmd_ty]);

    let started = fx.strukt("Started", 0, &[("command", box_cmd), ("limit", u8_)]);
    let started_ty = fx.adt(started, &[]);

    let dropped = fx.strukt("Dropped", 0, &[("command", box_cmd), ("limit", u8_)]);
    let dropped_ty = fx.adt(dropped, &[]);
    fx.imp("impl Drop for Dropped", 0, dropped_ty, Some((drop, &[])), &[], Vec::new());

    let read_field = fx.func("read_field", 0, &[started_ty], unit, Vec::new());
    let mut_field = fx.func("mut_field", 0, &[started_ty], unit, Vec::new());
    let root_box = fx.func("root_box", 0, &[box_cmd], unit, Vec::new());
    let move_drop_struct = fx.func("move_drop_struct", 0, &[dropped_ty], unit, Vec::new());
    fx.finish();
    BoxWorld { fx, read_field, mut_field, root_box, move_drop_struct }
}

/// Positive: reading `s.command.prog` inside a closure captures `&s.command` (`len == 1`,
/// `[Field(0)]`, `&Box<Cmd>`), NOT whole `s` (`len == 0`), staying disjoint from `s.limit`.
#[test]
fn box_deref_field_truncates_at_box_not_root() {
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
    assert_eq!(fields.len(), 1, "expected 1 captured place for `s.command`");
    let f = fields[0];
    assert_eq!(f.base, s);
    assert_eq!(f.len, 1, "must truncate at `s.command` (len 1), not whole `s` (len 0)");
    assert_eq!(f.how, CaptureKind::ByRef);
    assert_eq!(f.proj(&c, 0).map(|(p, _)| p), Some(CaptureProj::Field { variant: 0, field: 0 }));
    assert_eq!(show(&w.fx, c.types.view(), f.stored.expect("place stored type")), "&Box<Cmd>");
}

/// Soundness: mutating `s.command.prog = 1` captures `&mut s.command` (`len == 1`,
/// `CaptureKind::ByMutRef`, `&mut Box<Cmd>`), never downgrading to `ByRef` across `Box` deref.
#[test]
fn box_deref_field_mutation_captures_box_by_mut_ref() {
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

    let range = c.capture_places_of(cl);
    assert_eq!(range.len(), 1);
    let row = range.start;
    assert_eq!(c.capture_places.column::<1>()[row], s);
    assert_eq!(c.capture_places.column::<3>()[row], 1);
    assert_eq!(c.capture_places.column::<4>()[row], CaptureKind::ByMutRef);
    assert_eq!(show(&w.fx, c.types.view(), c.capture_places.column::<5>()[row]), "&mut Box<Cmd>");
}

/// Control: dereferencing a root `b: Box<Cmd>` (`b.prog`) truncates at `len == 0` (the whole `Box`
/// binding), never emitting a `Box` `CaptureProj::Deref`.
#[test]
fn box_deref_on_root_binding_captures_whole_box() {
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

    let range = c.capture_places_of(cl);
    assert_eq!(range.len(), 1);
    let row = range.start;
    assert_eq!(c.capture_places.column::<1>()[row], bx);
    assert_eq!(c.capture_places.column::<3>()[row], 0, "root `Box` deref truncates to len 0");
    assert_eq!(c.capture_places.column::<4>()[row], CaptureKind::ByRef);
    assert_eq!(show(&w.fx, c.types.view(), c.capture_places.column::<5>()[row]), "&Box<Cmd>");
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
