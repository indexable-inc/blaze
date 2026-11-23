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
use blaze_typeck::ExprId;

use super::{PTR_BITS, V, Walk, malformed};
use crate::BuildError;

/// One waiting value written before a suspend: its place on the stack, the slot holding it, its
/// type, and how it is rebuilt.
#[derive(Clone, Copy)]
pub(super) struct Spilled {
    at: usize,
    slot: SlotId,
    ty: Ty,
    form: Form,
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

    /// Before `.await` `e`'s loop: each value waiting on the stack that is not made again after
    /// a resume, written to the slot of the flow local that holds it. Without a laid state (this
    /// crate's unit tests) nothing is written: the split keeps the values itself, as before.
    pub(super) fn spill_waiting(&mut self, e: ExprId) -> Result<Vec<Spilled, A>, BuildError> {
        let mut out: Vec<Spilled, A> = Vec::new_in(self.alloc);
        if self.names.is_none() {
            return Ok(out);
        }
        for at in 0..self.vals.len() {
            let (x, form) = match self.vals[at] {
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
            let align = (bytes.next_power_of_two().trailing_zeros() as u8).min(4);
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
            out.push(Spilled { at, slot, ty, form });
        }
        Ok(out)
    }

    /// On an `.await`'s Ready edge: each value [`Walk::spill_waiting`] wrote, read back into its
    /// place on the stack.
    pub(super) fn reload_waiting(&mut self, spilled: &[Spilled]) {
        for s in spilled {
            let bytes = s.ty.bits(PTR_BITS) / 8;
            let align = (bytes.next_power_of_two().trailing_zeros() as u8).min(4);
            let p = self.b.slot_addr(s.slot);
            let x = self.b.load(s.ty, Flags::NONE, p, 0, MemAttrs::aligned(align));
            self.vals[s.at] = match s.form {
                Form::Scalar => V::Scalar(x),
                Form::Place(off) => V::Place(x, off),
                Form::Unsized(off) => V::Unsized(x, off),
            };
        }
    }
}
