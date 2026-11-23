//! The walk's side of the flow's table of identities while a coroutine body is built into a
//! state laid from types (ASYNC.md P8; main's ruling of 2026-11-16, async-P8-4).
//!
//! The flow layer is the ONE owner of which locals cross a suspension and of what each local
//! stands for (blaze-flow-lower identity.rs). The walk never invents a slot that lives across a
//! suspension: where it makes a slot for something the program names (a binding's home, its
//! drop flag, a temporary held for its scope, an `.await`'s future and value), it LOOKS UP the
//! identity in the plan's table ([`crate::coroutine_state::StateNames`]) and, when the flow
//! saves that local, names the slot with the local's offset: blaze-lir `split_in` places it
//! there. Every other slot is the walk's own: the split keeps it on the stack when no code
//! after a suspension reaches it, and otherwise refuses it ([`UNNAMED_ACROSS`], at the
//! expression whose task made the slot): a gap of the flow body, or a slot the walk should end
//! (`slot_dead`) before the suspend, fixed there and never placed by guess.
//!
//! A name only ever PLACES a slot; it never drops one from the state. So a key the walk
//! computes wrongly can at worst put a slot in a field the flow made for another value of the
//! same expression, never lose bytes the program reads after a resume (those are found by the
//! split's own reach rule and refused when unnamed).
//!
//! Which slot is which (the wiring's first lines): the `.await`'s `Pin` slot is ended right
//! after the poll call and remade at the loop's head each poll (never saved; the flow's `pin`
//! is storage-dead before its `Suspend`); the `Poll` result's slot is ended on the Pending edge
//! and written by the next poll (never saved; the flow's `r` is storage-dead on its pending
//! edge); a binding's drop flag is its binding's flag byte in the state (`StateName::flag`); a
//! binding's field-flag word (`rest`, a partly moved binding) and a temporary's drop flag have
//! no row: refused by name if a suspension's later code reaches them.

use core::alloc::Allocator;

use blaze_flow_lower::identity::{Identity, Role};
use blaze_lir::builder::MemAttrs;
use blaze_lir::coroutine::{CoroutineError, SlotName};
use blaze_lir::func::{SlotId, SlotKind};
use blaze_lir::{Flags, Op, Ty, Value};
use blaze_own::Vec;
use blaze_typeck::ExprId;

use super::{Task, Walk, malformed};
use crate::BuildError;
use crate::coroutine_state::{StateName, StateNames, StatePlant, UNNAMED_ACROSS};

/// The table, what the walk named, and where each slot and stack value came from.
pub(super) struct Names<'n, A: Allocator> {
    table: StateNames<'n>,
    /// Each slot named: the slot, the identity, whether it is that local's drop flag, its offset.
    named: Vec<(SlotId, Identity, bool, u32), A>,
    /// Beside the walk's value stack: the expression whose task pushed each value.
    val_at: Vec<ExprId, A>,
    /// The expression of the task being run.
    at: ExprId,
    /// Per run of tasks of one expression: the first slot index made in it, and the expression.
    epochs: Vec<(u32, ExprId), A>,
    /// The slots walk/await_spill.rs wrote waiting values to, by the local they are: a value
    /// waiting across two `.await`s is written to its one slot again.
    spills: Vec<(Identity, SlotId), A>,
    plant: StatePlant,
}

impl<'n, A: Allocator + Copy> Names<'n, A> {
    pub(super) fn new(table: StateNames<'n>, plant: StatePlant, root: ExprId, alloc: A) -> Self {
        Names {
            table,
            named: Vec::new_in(alloc),
            val_at: Vec::new_in(alloc),
            at: root,
            epochs: Vec::new_in(alloc),
            spills: Vec::new_in(alloc),
            plant,
        }
    }

    /// The names for blaze-lir `split_in`, in `alloc`.
    pub(super) fn slot_names<B: Allocator>(&self, alloc: B) -> Vec<SlotName, B> {
        let mut out = Vec::with_capacity_in(self.named.len(), alloc);
        out.extend(self.named.iter().map(|&(slot, _, _, offset)| SlotName { slot, offset }));
        out
    }

    /// The split's refusal as the walk names it: an unnamed slot read after a suspension at
    /// the expression whose task made it; any other refusal as it is.
    pub(super) fn split_err(&self, error: CoroutineError) -> BuildError {
        match error {
            CoroutineError::UnnamedAcrossSuspend { slot } => {
                let i = self.epochs.partition_point(|&(first, _)| first <= slot);
                match i.checked_sub(1).map(|i| self.epochs[i].1) {
                    Some(expr) => BuildError::Unsupported { expr, what: UNNAMED_ACROSS },
                    None => malformed("a slot read after a suspension made before any task of the body"),
                }
            }
            other => BuildError::Coroutine(other),
        }
    }
}

/// The expression a task is about, when it has one.
pub(super) fn task_expr(t: &Task) -> Option<ExprId> {
    match *t {
        Task::Eval(e)
        | Task::Raw(e)
        | Task::Adjust(e, _)
        | Task::DerefCall(e, _)
        | Task::LoadAdjusted(e, _)
        | Task::SpillAdjusted(e)
        | Task::Place(e)
        | Task::PlaceDerefs(e, _)
        | Task::Intrinsic(e, _)
        | Task::DropValue(e)
        | Task::ScopeEnd(e)
        | Task::TempScopeEnd(e)
        | Task::BlockExit(e)
        | Task::LoopEnd(e)
        | Task::Break(e, _)
        | Task::Match(e)
        | Task::Call(e)
        | Task::OpCall(e)
        | Task::Unary(e, _)
        | Task::Deref(e)
        | Task::Binary(e, _)
        | Task::Assign(e)
        | Task::AssignOp(e, _)
        | Task::Cast(e)
        | Task::Field(e)
        | Task::FieldPlace(e)
        | Task::IndexPlace(e)
        | Task::IndexCall(e)
        | Task::LoadPlace(e)
        | Task::Spill(e)
        | Task::ArgCopy(e)
        | Task::Repeat(e)
        | Task::StructBase(e)
        | Task::InitDefaults(e)
        | Task::FormatArgs(e)
        | Task::Await(e)
        | Task::Asm(e) => Some(e),
        Task::AsmLabel { body, .. } => Some(body),
        Task::If { expr, .. }
        | Task::IfThen { expr, .. }
        | Task::IfElse { expr, .. }
        | Task::LetLink { expr, .. }
        | Task::Short { expr, .. } => Some(expr),
        Task::Let(_, init) => init,
        Task::Stmt(_)
        | Task::Discard
        | Task::TempEnd
        | Task::PushNone
        | Task::CondLink(_)
        | Task::LetElse(_)
        | Task::LetElseEnd(..)
        | Task::Return(_)
        | Task::Arm { .. }
        | Task::Guard { .. }
        | Task::GuardAlts { .. }
        | Task::ArmMoves { .. }
        | Task::ArmEnd { .. }
        | Task::MatchEnd
        | Task::ShortEnd(_)
        | Task::InitAt { .. }
        | Task::LitBuilt { .. }
        | Task::DerefPlace
        | Task::AsmLabelEnd(_)
        | Task::AsmJoin(_)
        | Task::Ref => None,
    }
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Before task `t` runs: its expression is the one the slots and values it makes come from.
    pub(super) fn note_task(&mut self, t: &Task) {
        let Some(x) = task_expr(t) else { return };
        let slots = self.b.func().view().slots.len() as u32;
        let Some(n) = self.names.as_mut() else { return };
        n.at = x;
        if n.epochs.last().is_none_or(|&(_, e)| e != x) {
            n.epochs.push((slots, x));
        }
    }

    /// Beside `push_val`: the value came from the running task's expression.
    pub(super) fn note_push(&mut self) {
        if let Some(n) = self.names.as_mut() {
            n.val_at.push(n.at);
        }
    }

    /// Beside `pop_val`, `truncate_vals` and `remove_val`: the stack is now `len` long, without
    /// the value at `removed` when one was taken from the middle.
    pub(super) fn note_stack(&mut self, len: usize, removed: Option<usize>) {
        if let Some(n) = self.names.as_mut() {
            if let Some(at) = removed.filter(|&at| at < n.val_at.len()) {
                n.val_at.remove_shift(at);
            }
            n.val_at.truncate(len);
        }
    }

    /// The expression whose task pushed the value at `at` on the stack.
    pub(super) fn val_expr(&self, at: usize) -> Option<ExprId> {
        self.names.as_ref().and_then(|n| n.val_at.get(at).copied())
    }

    /// Names `slot` as the field of saved local `id` (or its drop flag, `flag`) when the flow
    /// saves `id` and no other slot holds it yet. A slot named before by another identity is
    /// renamed (a later, sharper key: `hold_temp`'s expression over `temp`'s task).
    fn name(&mut self, slot: SlotId, id: Identity, flag: bool) {
        let Some(n) = self.names.as_mut() else { return };
        let Some(row) = n.table.get(id) else { return };
        let offset = match (flag, row.flag) {
            (false, _) => row.offset,
            (true, Some(f)) => f,
            (true, None) => return,
        };
        if let Some(i) = n.named.iter().position(|r| r.0 == slot) {
            if n.named[i].1 == id && n.named[i].2 == flag {
                return;
            }
            n.named.remove_shift(i);
        }
        if n.named.iter().any(|r| r.1 == id && r.2 == flag) {
            return;
        }
        n.named.push((slot, id, flag, offset));
    }

    /// Names `slot` as saved local `id`'s field (bind, `.await`'s future).
    pub(super) fn name_slot(&mut self, slot: SlotId, id: Identity) {
        self.name(slot, id, false);
    }

    /// Names `slot` as saved local `id`'s drop flag byte (`own`).
    pub(super) fn name_flag(&mut self, slot: SlotId, id: Identity) {
        self.name(slot, id, true);
    }

    /// Names the slot whose whole storage `at` is, if it is one, as saved local `id`'s field.
    pub(super) fn name_at(&mut self, at: (Value, i32), id: Identity) {
        if self.names.is_none() || at.1 != 0 {
            return;
        }
        let view = self.b.func().view();
        let ix = at.0.index();
        if ix < view.len() && view.op(ix) == Op::SlotAddr {
            let slot = SlotId(view.data[ix]);
            self.name_slot(slot, id);
        }
    }

    /// A fresh slot of `temp`, named as the running task's expression's value.
    pub(super) fn name_temp(&mut self, slot: SlotId) {
        if let Some(x) = self.names.as_ref().map(|n| n.at) {
            self.name_slot(slot, Identity::Temp(x, Role::Value));
        }
    }

    /// The saved local the value of expression `x` waiting across `.await` `e` is held in: the
    /// one of its value, borrow or result roles the flow saves. Refused by name (at `x`, or at
    /// `e` when the walk does not know `x`): none saved (the flow does not hold what the walk
    /// holds across the suspension), two saved (the walk cannot tell which), or a field
    /// smaller than the value.
    pub(super) fn waiting_local(&self, e: ExprId, x: Option<ExprId>, bytes: u32) -> Result<(Identity, StateName), BuildError> {
        let refused = |at: ExprId| BuildError::Unsupported { expr: at, what: UNNAMED_ACROSS };
        let n = self.names.as_ref().ok_or(malformed("a waiting value looked up outside a laid coroutine body"))?;
        let x = x.ok_or(refused(e))?;
        let mut found = None;
        for role in [Role::Value, Role::Borrow, Role::Result] {
            let id = Identity::Temp(x, role);
            if let Some(row) = n.table.get(id) {
                if found.is_some() {
                    return Err(BuildError::Unsupported { expr: x, what: "a value waiting across an `.await` whose expression has two saved locals in the flow: the walk cannot tell which holds it" });
                }
                found = Some((id, row));
            }
        }
        let (id, row) = found.ok_or(refused(x))?;
        if row.size < bytes {
            return Err(BuildError::Unsupported { expr: x, what: "a value waiting across an `.await` wider than the saved local the flow holds it in" });
        }
        Ok((id, row))
    }

    /// Where waiting values of saved local `id` are written: the spill slot made for it at an
    /// earlier `.await` (`Ok(Some)`), none yet (`Ok(None)`: the caller makes it and
    /// [`Walk::record_spill`]s it), or refused at `e` when another slot of the walk already
    /// holds `id` (a slot `bind` or `temp` named: two homes for one flow local).
    pub(super) fn spill_slot(&self, e: ExprId, id: Identity) -> Result<Option<SlotId>, BuildError> {
        let n = self.names.as_ref().ok_or(malformed("a waiting value looked up outside a laid coroutine body"))?;
        if let Some(&(_, slot)) = n.spills.iter().find(|s| s.0 == id) {
            return Ok(Some(slot));
        }
        if n.named.iter().any(|r| r.1 == id && !r.2) {
            return Err(BuildError::Unsupported {
                expr: id.expr().unwrap_or(e),
                what: "a value waiting across an `.await` whose saved local already has a slot of the walk: two homes for one flow local",
            });
        }
        Ok(None)
    }

    /// `slot` made and named for waiting values of saved local `id`.
    pub(super) fn record_spill(&mut self, id: Identity, slot: SlotId) {
        self.name_slot(slot, id);
        if let Some(n) = self.names.as_mut() {
            n.spills.push((id, slot));
        }
    }

    /// THE PLANT (this crate's test builds only, `StatePlant::BuilderSlot`): before `.await`
    /// `e`'s loop, a slot of the walk's own written, to be read on the Ready edge
    /// ([`Walk::plant_after`]): a builder-made slot across the suspension, which the split must
    /// refuse at `e`.
    pub(super) fn plant_before(&mut self) -> Option<SlotId> {
        let planted = cfg!(test) && self.names.as_ref().is_some_and(|n| n.plant == StatePlant::BuilderSlot);
        if !planted {
            return None;
        }
        let slot = self.b.slot(8, 3, SlotKind::Temp);
        let at = self.b.slot_addr(slot);
        let v = self.b.iconst(Ty::I64, 7);
        self.b.store(Flags::NONE, v, at, 0, MemAttrs::aligned(3));
        Some(slot)
    }

    /// The plant's read after the resume.
    pub(super) fn plant_after(&mut self, slot: Option<SlotId>) {
        if let Some(slot) = slot {
            let at = self.b.slot_addr(slot);
            let _ = self.b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::aligned(3));
        }
    }
}
