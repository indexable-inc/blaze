//! `MirBody::unreachable_after_calls`: rustc's `LintAndRemoveUninhabited`
//! (`compiler/rustc_mir_transform/src/lint_and_remove_uninhabited.rs`, MIT or Apache-2.0).
//!
//! rustc builds what follows a call into the call's target block, and the pass then takes the
//! target away from a call of an uninhabited type and looks for the first user code in that
//! block (`find_unreachable_code_from`). Lowering here builds nothing after such a call. So a
//! function with a call of an uninhabited type other than `!`, whose own return type is
//! inhabited, is lowered once more as a probe: each call of an uninhabited type keeps a target
//! block that no edge leads to and lowering goes on in it, as rustc's does. The probe's body
//! is only read for the entries; the body kept is the first one.
//!
//! The probe builds past calls only: code after a `return`, `break`, `continue` or a `loop`
//! that never ends is still not built, where rustc builds it into a block nothing reaches.

use blaze_mem::Frame;
use hir_def::HasModule;

use crate::mir::{
    BasicBlockId, MirSpan, OperandKind, Rvalue, StatementKind, TerminatorKind,
    UnreachableAfterCall, return_slot,
};
use crate::{inhabitedness::is_ty_uninhabited_from, next_solver::StoredTy};

use super::{
    FrameBlock, MirLowerCtx,
    framed::{self, FrameSet},
};

#[cfg(test)]
mod tests;

/// A call of an uninhabited type the probe lowered on past.
struct ProbedCall {
    /// The block the call ends.
    block: BasicBlockId,
    /// The block what follows the call went into, which no edge leads to.
    target: BasicBlockId,
    ty: StoredTy,
    span: MirSpan,
}

/// What the probe records while it lowers.
pub(super) struct Probe<'f> {
    calls: Vec<ProbedCall, &'f Frame>,
    /// The `()` assignments `push_assign_unit` wrote, by block and index: rustc writes those
    /// as a constant `()` and `find_unreachable_code_from` skips them, where a `()` the user
    /// wrote is an aggregate it does not skip.
    units: FrameSet<'f, (BasicBlockId, usize)>,
}

impl<'f> Probe<'f> {
    pub(super) fn new_in(frame: &'f Frame) -> Self {
        Probe { calls: Vec::new_in(frame), units: framed::set_in(frame) }
    }
}

/// How many edges lead into a block, and from where when it is one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Preds {
    None,
    One(BasicBlockId),
    Many,
}

impl Preds {
    fn add(&mut self, from: BasicBlockId) {
        *self = match *self {
            Preds::None => Preds::One(from),
            Preds::One(_) | Preds::Many => Preds::Many,
        };
    }
}

impl<'a, 'f, 'db> MirLowerCtx<'a, 'f, 'db> {
    /// The block lowering goes on in after the call of an uninhabited type that ends
    /// `current`: none while the kept body is built, a block nothing leads to in the probe.
    pub(super) fn after_uninhabited_call(
        &mut self,
        current: BasicBlockId,
        span: MirSpan,
    ) -> Option<BasicBlockId> {
        let MirSpan::ExprId(call) = span else { return None };
        let ty = self.infer.expr_ty(call);
        if !ty.is_never() {
            self.uninhabited_call = true;
        }
        self.probe.as_ref()?;
        let target = self.new_basic_block();
        let probe = self.probe.as_mut()?;
        probe.calls.push(ProbedCall { block: current, target, ty: ty.store(), span });
        Some(target)
    }

    /// Notes that the statement just pushed to the end of `block` is `push_assign_unit`'s.
    pub(super) fn note_assign_unit(&mut self, block: BasicBlockId) {
        if self.probe.is_none() {
            return;
        }
        let Some(at) = self.block(block).statements.len().checked_sub(1) else { return };
        if let Some(probe) = &mut self.probe {
            probe.units.insert((block, at));
        }
    }

    /// Whether the kept body needs the probe: rustc lints only in a function (`DefKind::Fn`
    /// or `AssocFn`) whose return type is inhabited, after a call of an uninhabited type that
    /// is not `!`.
    pub(super) fn needs_probe(&self) -> bool {
        self.uninhabited_call
            && self.owner.as_function().is_some()
            && !is_ty_uninhabited_from(
                &self.infcx,
                self.local(return_slot()).ty.as_ref(),
                self.owner.module(self.db),
                self.env,
            )
    }

    /// The probe's entries, in the order of the calls' blocks, as rustc's pass walks them.
    pub(super) fn unreachable_after_calls(&self) -> Vec<UnreachableAfterCall> {
        let Some(probe) = &self.probe else { return Vec::with_capacity(0) };
        let mut calls = Vec::new_in(self.frame);
        calls.extend(probe.calls.iter().filter(|it| !it.ty.as_ref().is_never()));
        if calls.is_empty() {
            return Vec::with_capacity(0);
        }
        calls.sort_unstable_by_key(|it| framed::block_index(it.block));
        let blocks = &self.building.blocks[..];
        let preds = predecessors(blocks, self.frame);
        // alloc: kept in the body, at most one entry a call
        let mut found = Vec::with_capacity(calls.len());
        for call in calls {
            if let Some((next, descr)) = first_user_code(blocks, &probe.units, &preds, call.target)
            {
                found.push(UnreachableAfterCall { call: call.span, next, descr, ty: call.ty });
            }
        }
        found.shrink_to_fit();
        found
    }
}

/// Every edge into each block, as rustc's `predecessors`: the successors, the imaginary edge
/// of a `FalseEdge` and the unwind edge.
fn predecessors<'f>(blocks: &[FrameBlock<'_>], frame: &'f Frame) -> Vec<Preds, &'f Frame> {
    let mut preds = Vec::with_capacity_in(blocks.len(), frame);
    preds.resize(blocks.len(), Preds::None);
    for (at, block) in blocks.iter().enumerate() {
        let Some(terminator) = &block.terminator else { continue };
        let from = framed::block_id(at);
        for to in terminator.kind.successors() {
            preds[framed::block_index(to)].add(from);
        }
        if let TerminatorKind::FalseEdge { imaginary_target, .. } = terminator.kind {
            preds[framed::block_index(imaginary_target)].add(from);
        }
        if let Some(to) = terminator.kind.unwind() {
            preds[framed::block_index(to)].add(from);
        }
    }
    preds
}

/// rustc's `find_unreachable_code_from`: the first statement or terminator from `block` on
/// that is user code, and whether it is a `let` ("definition") or not ("expression").
fn first_user_code(
    blocks: &[FrameBlock<'_>],
    units: &FrameSet<'_, (BasicBlockId, usize)>,
    preds: &[Preds],
    mut block: BasicBlockId,
) -> Option<(MirSpan, &'static str)> {
    // A chain of `Goto`s, each the only way into the next, that closes on itself is reached
    // from nowhere; it ends after as many steps as there are blocks.
    for _ in 0..blocks.len() {
        let data = &blocks[framed::block_index(block)];
        for (at, statement) in data.statements.iter().enumerate() {
            match &statement.kind {
                StatementKind::Assign(place, rvalue) => {
                    let returned =
                        place.local == return_slot() && place.projection.as_slice().is_empty();
                    if returned || units.contains(&(block, at)) || is_unit_constant(rvalue) {
                        continue;
                    }
                    return Some((statement.span, "expression"));
                }
                StatementKind::StorageLive(_) | StatementKind::StorageDead(_) => continue,
                StatementKind::FakeRead(_) => return Some((statement.span, "definition")),
                StatementKind::Deinit(_) | StatementKind::InlineAsm(_) | StatementKind::Nop => {
                    return Some((statement.span, "expression"));
                }
            }
        }
        let terminator = data.terminator.as_ref()?;
        match terminator.kind {
            TerminatorKind::Goto { target } | TerminatorKind::Drop { target, .. } => {
                if preds[framed::block_index(target)] != Preds::One(block) {
                    return None;
                }
                block = target;
            }
            TerminatorKind::Return => return None,
            _ => return Some((terminator.span, "expression")),
        }
    }
    None
}

/// Whether `rvalue` is a constant of type `()`, which rustc skips as the implicit `()` of a
/// block or a function.
fn is_unit_constant(rvalue: &Rvalue) -> bool {
    let Rvalue::Use(operand) = rvalue else { return false };
    match &operand.kind {
        OperandKind::Constant { ty, .. } => ty.as_ref().is_unit(),
        OperandKind::Allocation { allocation } => allocation.as_ref().ty.is_unit(),
        OperandKind::Copy(_) | OperandKind::Move(_) | OperandKind::Static(_) => false,
    }
}
