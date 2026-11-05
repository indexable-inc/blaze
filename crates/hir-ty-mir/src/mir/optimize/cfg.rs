//! The passes that change the blocks of a body and not what they compute: rustc's
//! `SimplifyCfg` (`simplify.rs`), `SimplifyConstCondition` (`simplify_branches.rs`),
//! `RemoveUnneededDrops`, `UnreachablePropagation`, `AddCallGuards::CriticalCallEdges` and the
//! part of `CleanupPostBorrowck` that applies here.

use blaze_mem::Frame;
use la_arena::Arena;

use crate::{
    MemoryMap,
    mir::{
        BasicBlock, BasicBlockId, MirBody, Operand, OperandKind, Rvalue, StatementKind,
        SwitchTargets, Terminator, TerminatorKind,
        framed::{Bits, bits_in, block_id, block_index},
        transform::Ctx,
    },
    next_solver::ConstKind,
};
use rustc_type_ir::inherent::IntoKind;

use super::{renumber_blocks, successors_mut};

/// rustc's `CleanupPostBorrowck` and `EraseDerefTemps` for what lowering here makes:
/// `FakeRead`s and the false edges only borrow checking reads go, and a `CopyForDeref` is the
/// copy it is. Also the storage markers lowering gives parameters, which rustc's never have.
pub(super) fn cleanup_post_borrowck(body: &mut MirBody<'_>, _frame: &Frame) {
    let params = &body.param_locals;
    for (_, block) in body.basic_blocks.iter_mut() {
        if let Some(terminator) = &mut block.terminator
            && let TerminatorKind::FalseEdge { real_target: target, .. }
            | TerminatorKind::FalseUnwind { real_target: target, .. } = terminator.kind
        {
            terminator.kind = TerminatorKind::Goto { target };
        }
        block.statements.retain(|it| match it.kind {
            StatementKind::FakeRead(_) => false,
            StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                !params.contains(&local)
            }
            _ => true,
        });
        for statement in &mut block.statements {
            if let StatementKind::Assign(_, rvalue) = &mut statement.kind
                && let Rvalue::CopyForDeref(place) = rvalue
            {
                let place = place.clone(); // clone: a place's refcount, kept in the body
                *rvalue = Rvalue::Use(Operand { kind: OperandKind::Copy(place), span: None });
            }
        }
    }
}

/// rustc's `simplify_cfg`: goto chains collapsed, a block with one predecessor that jumps to
/// it merged into that one, a switch whose targets are all one block made a `goto`, and the
/// blocks nothing reaches removed.
pub(super) fn simplify_cfg(body: &mut MirBody<'_>, frame: &Frame) {
    let start = block_index(body.start_block);
    CfgSimplifier::new(body.basic_blocks.as_mut(), start, frame).simplify();
    remove_dead_blocks(body, start, frame);
}

struct CfgSimplifier<'a, 'f> {
    blocks: &'a mut [BasicBlock],
    pred_count: &'f mut [u32],
    frame: &'f Frame,
    /// `collapse_goto_chain`'s gotos, reused across calls.
    chain: Vec<(usize, Terminator), &'f Frame>,
}

impl<'a, 'f> CfgSimplifier<'a, 'f> {
    fn new(blocks: &'a mut [BasicBlock], start: usize, frame: &'f Frame) -> Self {
        let pred_count = frame.alloc_slice_fill_with(blocks.len(), |_| 0u32);
        pred_count[start] = 1;
        let mut order = Vec::with_capacity_in(blocks.len(), frame);
        reachable_into(blocks, start, &mut order, frame);
        for &block in &order {
            if let Some(terminator) = &blocks[block].terminator {
                for target in terminator.kind.all_successors() {
                    pred_count[block_index(target)] += 1;
                }
            }
        }
        CfgSimplifier { blocks, pred_count, frame, chain: Vec::new_in(frame) }
    }

    fn simplify(mut self) {
        for block in self.blocks.iter_mut() {
            block.statements.retain(|it| !matches!(it.kind, StatementKind::Nop));
        }
        let mut merged: Vec<usize, &Frame> = Vec::new_in(self.frame);
        loop {
            let mut changed = false;
            for bb in 0..self.blocks.len() {
                if self.pred_count[bb] == 0 {
                    continue;
                }
                let Some(mut terminator) = self.blocks[bb].terminator.take() else { continue };
                successors_mut(&mut terminator.kind, |successor| {
                    self.collapse_goto_chain(successor, &mut changed);
                });
                let mut inner_changed = true;
                merged.clear();
                while inner_changed {
                    inner_changed = false;
                    inner_changed |= self.simplify_branch(&mut terminator);
                    inner_changed |= self.merge_successor(&mut merged, &mut terminator);
                    changed |= inner_changed;
                }
                for &from in &merged {
                    let statements = std::mem::take(&mut self.blocks[from].statements);
                    self.blocks[bb].statements.extend(statements);
                }
                self.blocks[bb].terminator = Some(terminator);
            }
            if !changed {
                break;
            }
        }
    }

    fn take_terminator_if_simple_goto(&mut self, bb: usize) -> Option<Terminator> {
        let block = &mut self.blocks[bb];
        match &block.terminator {
            Some(Terminator { kind: TerminatorKind::Goto { .. }, .. })
                if block.statements.is_empty() =>
            {
                block.terminator.take()
            }
            _ => None,
        }
    }

    fn collapse_goto_chain(&mut self, start: &mut BasicBlockId, changed: &mut bool) {
        let mut terminators = std::mem::replace(&mut self.chain, Vec::new_in(self.frame));
        terminators.clear();
        let mut current = block_index(*start);
        while let Some(terminator) = self.take_terminator_if_simple_goto(current) {
            let TerminatorKind::Goto { target } = terminator.kind else { unreachable!() };
            terminators.push((current, terminator));
            current = block_index(target);
        }
        let last = current;
        *changed |= block_index(*start) != last;
        *start = block_id(last);
        while let Some((current, mut terminator)) = terminators.pop() {
            let TerminatorKind::Goto { target } = &mut terminator.kind else { unreachable!() };
            *changed |= block_index(*target) != last;
            *target = block_id(last);
            if self.pred_count[current] == 1 {
                self.pred_count[current] = 0;
            } else {
                self.pred_count[last] += 1;
                self.pred_count[current] -= 1;
            }
            self.blocks[current].terminator = Some(terminator);
        }
        self.chain = terminators;
    }

    fn merge_successor(
        &mut self,
        merged: &mut Vec<usize, &Frame>,
        terminator: &mut Terminator,
    ) -> bool {
        let target = match terminator.kind {
            TerminatorKind::Goto { target } if self.pred_count[block_index(target)] == 1 => {
                block_index(target)
            }
            _ => return false,
        };
        *terminator = match self.blocks[target].terminator.take() {
            Some(it) => it,
            None => return false,
        };
        merged.push(target);
        self.pred_count[target] = 0;
        true
    }

    fn simplify_branch(&mut self, terminator: &mut Terminator) -> bool {
        let TerminatorKind::SwitchInt { targets, .. } = &terminator.kind else { return false };
        let all = targets.all_targets();
        let first = all[0];
        if all.iter().any(|&it| it != first) {
            return false;
        }
        self.pred_count[block_index(first)] -= (all.len() - 1) as u32;
        terminator.kind = TerminatorKind::Goto { target: first };
        true
    }
}

/// The blocks reachable from `start`, in preorder, after what `out` holds.
fn reachable_into(
    blocks: &[BasicBlock],
    start: usize,
    out: &mut Vec<usize, &Frame>,
    frame: &Frame,
) {
    let mut seen: Bits<'_> = bits_in(blocks.len(), frame);
    let mut stack = Vec::with_capacity_in(blocks.len(), frame);
    stack.push(start);
    while let Some(block) = stack.pop() {
        if !seen.insert(block) {
            continue;
        }
        out.push(block);
        if let Some(terminator) = &blocks[block].terminator {
            for target in terminator.kind.all_successors().rev() {
                if !seen.contains(block_index(target)) {
                    stack.push(block_index(target));
                }
            }
        }
    }
}

/// rustc's `remove_dead_blocks`: the blocks `start` does not reach go, and of the reachable
/// blocks that are nothing but `unreachable` all but one go. The rest keep their order.
fn remove_dead_blocks(body: &mut MirBody<'_>, start: usize, frame: &Frame) {
    let count = body.basic_blocks.len();
    let blocks: &[BasicBlock] = body.basic_blocks.as_mut();
    let mut order = Vec::with_capacity_in(count, frame);
    reachable_into(blocks, start, &mut order, frame);
    let mut keep: Bits<'_> = bits_in(count, frame);
    for &block in &order {
        keep.insert(block);
    }
    let empty_unreachable = |block: &BasicBlock| {
        !block.is_cleanup
            && block.statements.is_empty()
            && matches!(
                block.terminator.as_ref().map(|it| &it.kind),
                Some(TerminatorKind::Unreachable)
            )
    };
    let map = frame.alloc_slice_fill_with(count, |_| None::<usize>);
    let mut kept = 0;
    let mut kept_unreachable = None;
    for (index, block) in blocks.iter().enumerate() {
        if !keep.contains(index) {
            continue;
        }
        if empty_unreachable(block) {
            if let Some(it) = kept_unreachable {
                map[index] = Some(it);
                continue;
            }
            kept_unreachable = Some(kept);
        }
        map[index] = Some(kept);
        kept += 1;
    }
    // Every block kept, each where it was: the map is the identity, and renumbering would
    // leave the body as it is.
    if kept == count {
        return;
    }
    let mut arena = Arena::with_capacity(kept);
    for (index, (_, block)) in std::mem::take(&mut body.basic_blocks).into_iter().enumerate() {
        // A repeated empty `unreachable` maps to the one kept before it.
        if map[index] == Some(arena.len()) {
            arena.alloc(block);
        }
    }
    body.basic_blocks = arena;
    renumber_blocks(body, map);
}

/// The value of a constant operand of a scalar type, as its bits.
pub(super) fn constant_bits(operand: &Operand) -> Option<u128> {
    match &operand.kind {
        OperandKind::Allocation { allocation } => {
            let allocation = allocation.as_ref();
            let bytes = &allocation.memory;
            if bytes.len() > 16 || !matches!(allocation.memory_map, MemoryMap::Empty) {
                return None;
            }
            let mut padded = [0u8; 16];
            padded[..bytes.len()].copy_from_slice(bytes);
            Some(u128::from_le_bytes(padded))
        }
        OperandKind::Constant { konst, .. } => match konst.as_ref().kind() {
            ConstKind::Value(value) => value.try_to_leaf().map(|it| it.to_bits_unchecked()),
            _ => None,
        },
        _ => None,
    }
}

fn is_constant(operand: &Operand) -> bool {
    matches!(operand.kind, OperandKind::Allocation { .. } | OperandKind::Constant { .. })
}

/// rustc's `SimplifyConstCondition`: a switch on a constant is a `goto`, and so is an assert
/// of a constant that holds. The constant may be written to the place the condition reads
/// by the statement just before.
pub(super) fn simplify_const_condition(body: &mut MirBody<'_>, _frame: &Frame) {
    for (_, block) in body.basic_blocks.iter_mut() {
        // Borrowed from the statements while the terminator, another field, is rewritten.
        let before = match block.statements.last().map(|it| &it.kind) {
            Some(StatementKind::Assign(place, Rvalue::Use(value))) if is_constant(value) => {
                Some((place, value))
            }
            _ => None,
        };
        let known = |operand: &Operand| -> Option<u128> {
            match &operand.kind {
                OperandKind::Copy(place) | OperandKind::Move(place) => {
                    let (written, value) = before?;
                    (written == place).then(|| constant_bits(value)).flatten()
                }
                _ => constant_bits(operand),
            }
        };
        let Some(terminator) = &mut block.terminator else { continue };
        let new = match &terminator.kind {
            TerminatorKind::SwitchInt { discr, targets } => match known(discr) {
                Some(value) => TerminatorKind::Goto { target: targets.target_for_value(value) },
                None => continue,
            },
            TerminatorKind::Assert { cond, expected, target, .. } => match known(cond) {
                Some(value) if (value != 0) == *expected => {
                    TerminatorKind::Goto { target: *target }
                }
                _ => continue,
            },
            _ => continue,
        };
        terminator.kind = new;
    }
}

/// rustc's `RemoveUnneededDrops`: a drop of what has nothing to drop is a `goto`.
pub(super) fn remove_unneeded_drops<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    frame: &Frame,
) {
    let mut changed = false;
    let mut blocks = std::mem::take(&mut body.basic_blocks);
    for (_, block) in blocks.iter_mut() {
        let Some(terminator) = &mut block.terminator else { continue };
        let TerminatorKind::Drop { place, target, .. } = &terminator.kind else { continue };
        let ty = place.as_ref().ty(body, &ctx.infcx, ctx.env).ty;
        if ctx.needs_drop(ctx.normalize(ty)) {
            continue;
        }
        terminator.kind = TerminatorKind::Goto { target: *target };
        changed = true;
    }
    body.basic_blocks = blocks;
    if changed {
        simplify_cfg(body, frame);
    }
}

/// rustc's `AddCallGuards::CriticalCallEdges`: a call that may unwind and returns to a block
/// with more than one predecessor returns to a new block that only jumps there.
pub(super) fn critical_call_edges(body: &mut MirBody<'_>, frame: &Frame) {
    let pred_count = frame.alloc_slice_fill_with(body.basic_blocks.len(), |_| 0u32);
    for (_, block) in body.basic_blocks.iter() {
        if let Some(terminator) = &block.terminator {
            for target in terminator.kind.all_successors() {
                pred_count[block_index(target)] = pred_count[block_index(target)].saturating_add(1);
            }
        }
    }
    let mut work: Vec<BasicBlockId, &Frame> = Vec::new_in(frame);
    work.extend(
        body.basic_blocks
            .iter()
            .filter(|(_, block)| {
                matches!(
                    block.terminator.as_ref().map(|it| &it.kind),
                    Some(TerminatorKind::Call { target: Some(target), cleanup: Some(_), .. })
                        if pred_count[block_index(*target)] > 1
                )
            })
            .map(|(id, _)| id),
    );
    for &source in &work {
        let (span, is_cleanup, destination) = {
            let block = &body.basic_blocks[source];
            let terminator = block.terminator.as_ref().expect("filtered");
            let TerminatorKind::Call { target: Some(target), .. } = terminator.kind else {
                unreachable!()
            };
            (terminator.span, block.is_cleanup, target)
        };
        let guard = body.basic_blocks.alloc(BasicBlock {
            statements: Vec::new(), // alloc: none, a kept block with no statements
            terminator: Some(Terminator {
                span,
                kind: TerminatorKind::Goto { target: destination },
            }),
            is_cleanup,
        });
        let terminator = body.basic_blocks[source].terminator.as_mut().expect("filtered");
        if let TerminatorKind::Call { target, .. } = &mut terminator.kind {
            *target = Some(guard);
        }
    }
}

/// rustc's `UnreachablePropagation`, without the `assume`s rustc leaves for LLVM where it takes
/// a target out of a switch: a block that can only go on to `unreachable` is unreachable
/// itself, and a switch loses the targets that are. A block with `asm!` in it is never taken
/// for unreachable, since a block that does not return is one that ends in `unreachable` here.
pub(super) fn unreachable_propagation(body: &mut MirBody<'_>, frame: &Frame) {
    let mut unreachable: Bits<'_> = bits_in(body.basic_blocks.len(), frame);
    let order = super::reverse_postorder(body, frame);
    let mut kept: Vec<(u128, BasicBlockId), &Frame> = Vec::new_in(frame);
    for &bb in order.iter().rev() {
        let block = &mut body.basic_blocks[bb];
        if block.statements.iter().any(|it| matches!(it.kind, StatementKind::InlineAsm(_))) {
            continue;
        }
        let Some(terminator) = &mut block.terminator else { continue };
        let is_unreachable = match &terminator.kind {
            TerminatorKind::Unreachable => true,
            TerminatorKind::Goto { target } if unreachable.contains(block_index(*target)) => {
                terminator.kind = TerminatorKind::Unreachable;
                true
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                let otherwise = targets.otherwise();
                let otherwise_unreachable = unreachable.contains(block_index(otherwise));
                kept.clear();
                kept.extend(
                    targets.iter().filter(|(_, it)| !unreachable.contains(block_index(*it))),
                );
                let new_targets = SwitchTargets::new(kept.iter().copied(), otherwise);
                let count = new_targets.all_targets().len();
                let new = match (count, otherwise_unreachable) {
                    (1, true) => Some(TerminatorKind::Unreachable),
                    (1, false) => Some(TerminatorKind::Goto { target: otherwise }),
                    (2, true) => Some(TerminatorKind::Goto { target: kept[0].1 }),
                    _ if count == targets.all_targets().len() => None,
                    _ => Some(TerminatorKind::SwitchInt {
                        discr: discr.clone(), // clone: the kept switch's operand, a handle
                        targets: new_targets,
                    }),
                };
                let fully = count == 1 && otherwise_unreachable;
                if let Some(new) = new {
                    terminator.kind = new;
                }
                fully
            }
            _ => false,
        };
        if is_unreachable {
            unreachable.insert(block_index(bb));
        }
    }
    for bb in unreachable.iter() {
        body.basic_blocks[block_id(bb)].statements.clear();
    }
}
