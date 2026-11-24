//! What an `.await` keeps of the values waiting on the walk's stack (ASYNC.md P8; main's
//! rulings of 2026-11-16, async-P8-4: the flow layer is the one owner of what crosses a
//! suspension; the walk never invents a slot that lives across one).
//!
//! An operand evaluated before an `.await` in the same expression (`f(&guard(), x.await)`,
//! `a + x.await`) is a program temporary: the flow layer names it as a local of its own (the
//! Reference, "Temporary scopes": operands are held in temporaries while the others are
//! evaluated) and saves it when it is live across the suspension. The walk holds it as a value
//! on its stack; before the loop it LOOKS UP that local in the flow's table by the value's
//! expression (walk/state_names.rs `waiting_local`), WRITES the value to a slot named as that
//! local (placed at the local's offset in the state by the split), and on the Ready edge READS
//! it back. A waiting value the flow saves no local for is refused, naming its expression.
//!
//! A value made again after a resume is never written (blaze-lir coroutine.rs `REMADE`: a
//! slot's address, a constant, a function's or data's address): a place operand whose pointer
//! is a slot's address keeps its bytes in that slot, which is the local's own (named where the
//! walk made it) or refused by the split when unnamed and read after the suspension.

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::func::{SlotId, SlotKind};
use blaze_lir::{Flags, Op, Ty, Value};
use blaze_own::Vec;
use blaze_typeck::{CtorKind, Def, Expr, ExprId, ItemKey, Lit, PNode};

use super::val_stack::Owns;
use super::{PTR_BITS, Repr, V, Walk, malformed};
use crate::BuildError;

/// One waiting value preserved across a suspend: either a runtime value spilled into the flow's
/// saved slot, or a pure compile-time constant aggregate (`At::Const` in `blaze-flow-lower`, so
/// it has no flow local) whose temporary slot is ended before the suspend and re-materialized on
/// the Ready edge.
#[derive(Clone, Copy)]
pub(super) enum Spilled {
    Slot { at: usize, slot: SlotId, ty: Ty, form: Form },
    Remat { at: usize, kind: RematConst },
}

#[derive(Clone, Copy)]
pub(super) enum RematConst {
    UnitAdt { layout: u32, variant: u32 },
    Const { expr: ExprId, item: ItemKey },
    ConstBlock { expr: ExprId },
    StrLit { expr: ExprId },
    CStrLit { expr: ExprId },
}

#[derive(Clone, Copy)]
enum Form {
    Scalar,
    Place(i32),
    Unsized(i32),
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether `x` is made again after a resume (blaze-lir coroutine.rs `plan`'s `remade`).
    fn remade(&self, x: Value) -> bool {
        let view = self.b.func().view();
        matches!(view.op(x.index()), Op::SlotAddr | Op::Iconst | Op::Null | Op::FuncAddr | Op::DataAddr)
    }

    /// Whether waiting value `at` is an unadjusted pure constant aggregate (`At::Const` in
    /// `blaze-flow-lower`) that the walk materialized into a scratch temp slot and can safely
    /// end before the suspend and re-materialize on the Ready edge.
    fn remat_const_for(&mut self, at: usize) -> Result<Option<RematConst>, BuildError> {
        let Some(mut cur) = self.val_expr(at) else { return Ok(None) };
        loop {
            if !self.adjust_rows(cur).is_empty() {
                return Ok(None);
            }
            match self.expr(cur)? {
                Expr::Paren(inner) => cur = inner,
                _ => break,
            }
        }
        let Repr::Agg(layout) = self.expr_repr(cur)? else {
            return Ok(None);
        };
        let kind = match self.expr(cur)? {
            Expr::Path(_) => {
                let t = self.expr_ty(cur)?;
                match (self.node_at(cur, t)?, self.expr_def(cur)) {
                    (_, Some(Def::Const(item))) => Some(RematConst::Const { expr: cur, item }),
                    (PNode::Adt(..), Some(Def::Ctor { kind: CtorKind::Unit, variant, .. })) => Some(RematConst::UnitAdt { layout, variant }),
                    (PNode::Adt(..), Some(Def::Adt(_))) => Some(RematConst::UnitAdt { layout, variant: 0 }),
                    _ => None,
                }
            }
            Expr::ConstBlock { .. } => Some(RematConst::ConstBlock { expr: cur }),
            Expr::Lit(Lit::Str) => Some(RematConst::StrLit { expr: cur }),
            Expr::Lit(Lit::CStr) => Some(RematConst::CStrLit { expr: cur }),
            _ => None,
        };
        Ok(kind)
    }

    /// Before `.await` `e`'s loop: each value waiting on the stack that is not made again after
    /// a resume, written to the slot of the flow local that holds it. Without a laid state (this
    /// crate's unit tests) nothing is written: the split keeps the values itself, as before.
    pub(super) fn spill_waiting(&mut self, e: ExprId) -> Result<Vec<Spilled, A>, BuildError> {
        let mut out: Vec<Spilled, A> = Vec::new_in(self.alloc);
        if self.names.is_none() {
            return Ok(out);
        }
        for at in 0..self.vals.len() {
            let Some(&val) = self.vals.get(at) else { continue };
            if let V::Place(p, 0) = val
                && let Some(slot) = self.whole_slot((p, 0))
                && self.is_temp_slot(slot)
                && !self.slot_is_named(slot)
                && !self.slot_held(slot)
                && self.val_tys.get(at) == Some(&Owns::Nothing)
                && self.vals_slot_count(slot) == 1
                && let Some(kind) = self.remat_const_for(at)?
            {
                self.b.slot_dead(slot);
                self.release_dead_slot(slot);
                out.push(Spilled::Remat { at, kind });
                continue;
            }
            let (x, form) = match val {
                V::None => continue,
                V::Scalar(x) => (x, Form::Scalar),
                V::Place(p, off) => (p, Form::Place(off)),
                V::Unsized(p, off) => (p, Form::Unsized(off)),
            };
            if self.remade(x) {
                continue;
            }
            let ty = self.b.ty(x);
            let bits = ty.bits(PTR_BITS);
            if bits == 0 || bits % 8 != 0 {
                return Err(malformed("a value waiting across an `.await` of no byte width (await_spill.rs)"));
            }
            let bytes = bits / 8;
            let (id, row) = self.waiting_local(e, self.val_expr(at), bytes)?;
            let align = u8::try_from(bytes.next_power_of_two().trailing_zeros()).unwrap_or(4).min(4);
            let slot = match self.spill_slot(e, id)? {
                Some(slot) => slot,
                None => {
                    let slot = self.b.slot(row.size, align, SlotKind::Temp);
                    self.record_spill(id, slot);
                    slot
                }
            };
            let p = self.b.slot_addr(slot);
            self.b.store(Flags::NONE, x, p, 0, MemAttrs::aligned(align));
            out.push(Spilled::Slot { at, slot, ty, form });
        }
        Ok(out)
    }

    /// On an `.await`'s Ready edge: each value [`Walk::spill_waiting`] wrote, read back into its
    /// place on the stack (or re-materialized for a pure constant aggregate).
    pub(super) fn reload_waiting(&mut self, spilled: &[Spilled]) -> Result<(), BuildError> {
        for &s in spilled {
            match s {
                Spilled::Slot { at, slot, ty, form } => {
                    let bytes = ty.bits(PTR_BITS) / 8;
                    let align = u8::try_from(bytes.next_power_of_two().trailing_zeros()).unwrap_or(4).min(4);
                    let p = self.b.slot_addr(slot);
                    let x = self.b.load(ty, Flags::NONE, p, 0, MemAttrs::aligned(align));
                    if let Some(dst) = self.vals.get_mut(at) {
                        *dst = match form {
                            Form::Scalar => V::Scalar(x),
                            Form::Place(off) => V::Place(x, off),
                            Form::Unsized(off) => V::Unsized(x, off),
                        };
                    }
                }
                Spilled::Remat { at, kind } => {
                    let v = match kind {
                        RematConst::UnitAdt { layout, variant } => {
                            let (_, _, tag) = self.variant_rows(layout, variant)?;
                            let dst = self.temp(Repr::Agg(layout));
                            self.write_tag(tag, variant, dst);
                            V::Place(dst.0, dst.1)
                        }
                        RematConst::Const { expr, item } => self.const_value(expr, item)?,
                        RematConst::ConstBlock { expr } => {
                            let adts = self.adts;
                            let owner = self.body.owner;
                            let record = adts.const_block_value(owner, expr, self.args).ok_or(BuildError::NoConstBlockValue { expr, owner })?;
                            self.const_record_value(expr, record)?
                        }
                        RematConst::StrLit { expr } => self.str_lit(expr)?,
                        RematConst::CStrLit { expr } => self.c_str_lit(expr)?,
                    };
                    if let Some(dst) = self.vals.get_mut(at) {
                        *dst = v;
                    }
                }
            }
        }
        Ok(())
    }
}
