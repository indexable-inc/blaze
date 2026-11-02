//! rustc's `CopyProp` (`copy_prop.rs`) and the part of its `SsaLocals` (`ssa.rs`) it uses.
//!
//! A local is SSA when it is assigned once, as a parameter, by an assignment to the whole of
//! it or as where a call returns, that assignment comes before every use on every path, and
//! nothing changes it otherwise: no `&mut`, no assignment to a part, no drop. A shared borrow
//! is allowed of a local with no cell in it. Of
//!
//! ```text
//! _a = rvalue
//! _b = move? _a
//! _c = move? _b
//! ```
//!
//! with all three SSA, `_b` and `_c` are `_a`: every use of them reads `_a`, the copies go,
//! and what moved out of one of them copies.
//!
//! Promoted locals are never SSA here: the code generator gives them their constant and skips
//! what assigns them.

use blaze_mem::Frame;

use crate::mir::{
    LocalId, MirBody, OperandKind, ProjectionElem, Rvalue, StatementKind,
    framed::{Bits, bits_in, block_index, local_index},
    return_slot,
    transform::Ctx,
};

use super::{
    Dominators, Location, Mention, local_id, locals::remove_unused_definitions, map_place,
    reverse_postorder, visit_statement, visit_terminator,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Def {
    Argument,
    Assignment(Location),
    /// Returned to by the call ending `call`, which goes on to `target`.
    CallReturn {
        target: Option<crate::mir::BasicBlockId>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Assignments {
    Empty,
    One(Def),
    Many,
}

impl Assignments {
    fn insert(&mut self, def: Def) {
        *self = match *self {
            Assignments::Empty => Assignments::One(def),
            _ => Assignments::Many,
        };
    }
}

fn def_dominates(
    def: Def,
    at: Location,
    dominators: &Dominators<'_>,
    single_pred: &Bits<'_>,
) -> bool {
    match def {
        Def::Argument => true,
        Def::Assignment(def) if def.block == at.block => def.index < at.index,
        Def::Assignment(def) => dominators.dominates(def.block, at.block),
        // The value is there on the edge into `target`; with the call as the only way in, it
        // is there wherever `target` dominates.
        Def::CallReturn { target: Some(target) } => {
            single_pred.contains(block_index(target)) && dominators.dominates(target, at.block)
        }
        Def::CallReturn { target: None } => false,
    }
}

struct Ssa<'f> {
    assignments: Vec<Assignments, &'f Frame>,
    /// The SSA locals in the order of their assignments, in reverse postorder.
    order: Vec<LocalId, &'f Frame>,
    borrowed: Bits<'f>,
}

fn ssa_locals<'f, 'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, frame: &'f Frame) -> Ssa<'f> {
    let count = body.locals.len();
    let dominators = Dominators::new(body, frame);
    let preds = frame.alloc_slice_fill_with(body.basic_blocks.len(), |_| 0u32);
    for (_, block) in body.basic_blocks.iter() {
        if let Some(terminator) = &block.terminator {
            for target in terminator.kind.all_successors() {
                preds[block_index(target)] += 1;
            }
        }
    }
    let mut single_pred: Bits<'_> = bits_in(preds.len(), frame);
    for (block, &it) in preds.iter().enumerate() {
        if it == 1 {
            single_pred.insert(block);
        }
    }
    let mut assignments = Vec::with_capacity_in(count, frame);
    assignments.resize(count, Assignments::Empty);
    // A local goes in once, as a parameter or when its first assignment is seen.
    let mut order = Vec::with_capacity_in(count, frame);
    let mut borrowed: Bits<'f> = bits_in(count, frame);
    for &param in &body.param_locals {
        assignments[local_index(param)] = Assignments::One(Def::Argument);
        order.push(param);
    }
    for &(local, _) in &body.promoted_locals {
        assignments[local_index(local)] = Assignments::Many;
    }
    for &(local, _) in &body.upvars_in_place {
        assignments[local_index(local)] = Assignments::Many;
    }
    let check_dominates = |assignments: &mut [Assignments], local: LocalId, at: Location| {
        let slot = &mut assignments[local_index(local)];
        let dominated = match *slot {
            Assignments::One(def) => def_dominates(def, at, &dominators, &single_pred),
            _ => false,
        };
        if !dominated {
            *slot = Assignments::Many;
        }
    };
    let mut on_mention = |local: LocalId, mention: Mention, at: Location| match mention {
        Mention::Assigned => {
            assignments[local_index(local)].insert(Def::Assignment(at));
            if let Assignments::One(_) = assignments[local_index(local)] {
                order.push(local);
            }
        }
        Mention::CallReturned => {
            let target = match &body.basic_blocks[at.block].terminator.as_ref().map(|it| &it.kind) {
                Some(crate::mir::TerminatorKind::Call { target, .. }) => *target,
                _ => None,
            };
            assignments[local_index(local)].insert(Def::CallReturn { target });
            if let Assignments::One(_) = assignments[local_index(local)] {
                order.push(local);
            }
        }
        Mention::AssignedPart | Mention::Mutated => {
            assignments[local_index(local)] = Assignments::Many;
        }
        Mention::SharedBorrow => {
            borrowed.insert(local_index(local));
            check_dominates(&mut assignments[..], local, at);
        }
        Mention::Read => check_dominates(&mut assignments[..], local, at),
        Mention::Storage => (),
    };
    let rpo = reverse_postorder(body, frame);
    for &block in rpo.iter() {
        let data = &body.basic_blocks[block];
        for (index, statement) in data.statements.iter().enumerate() {
            visit_statement(&statement.kind, Location { block, index }, &mut on_mention);
        }
        if let Some(terminator) = &data.terminator {
            let at = Location { block, index: data.statements.len() };
            visit_terminator(&terminator.kind, at, &mut on_mention);
        }
    }
    for (local, decl) in body.locals.iter() {
        if borrowed.contains(local_index(local))
            && !crate::drop::is_freeze(&ctx.infcx, decl.ty.as_ref(), ctx.env)
        {
            assignments[local_index(local)] = Assignments::Many;
        }
    }
    order.retain(|&it| matches!(assignments[local_index(it)], Assignments::One(_)));
    Ssa { assignments, order, borrowed }
}

/// rustc's `compute_copy_classes`: for each local, the local it is a copy of, itself if none.
fn copy_classes<'f>(
    ssa: &Ssa<'_>,
    body: &MirBody<'_>,
    frame: &'f Frame,
) -> Vec<LocalId, &'f Frame> {
    let mut copies = Vec::with_capacity_in(body.locals.len(), frame);
    copies.extend((0..body.locals.len()).map(local_id));
    let is_ssa =
        |local: LocalId| matches!(ssa.assignments[local_index(local)], Assignments::One(_));
    let is_temp = |local: LocalId| local != return_slot() && !body.param_locals.contains(&local);
    for &local in &ssa.order {
        let Assignments::One(Def::Assignment(at)) = ssa.assignments[local_index(local)] else {
            continue;
        };
        let Some(StatementKind::Assign(target, Rvalue::Use(operand))) =
            body.basic_blocks[at.block].statements.get(at.index).map(|it| &it.kind)
        else {
            continue;
        };
        debug_assert_eq!(target.as_ref().as_local(), Some(local));
        let (OperandKind::Copy(place) | OperandKind::Move(place)) = &operand.kind else {
            continue;
        };
        let Some(rhs) = place.as_ref().as_local() else { continue };
        if body.locals[local].ty != body.locals[rhs].ty || !is_ssa(rhs) {
            continue;
        }
        let head = copies[local_index(rhs)];
        if ssa.borrowed.contains(local_index(local)) {
            continue;
        }
        if local == return_slot() {
            // The return slot keeps its number: the class is renamed to it instead.
            if !is_temp(head) {
                continue;
            }
            for it in copies.iter_mut() {
                if *it == head {
                    *it = return_slot();
                }
            }
        } else {
            copies[local_index(local)] = head;
        }
    }
    copies
}

/// rustc's `CopyProp`. With `lifetimes` (rustc's `emit_lifetime_markers`), storage markers of
/// a class are kept where rustc would keep them; here they all go, which rustc does when
/// `lifetimes` is off and which is always correct.
pub(super) fn copy_prop<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    lifetimes: bool,
    frame: &Frame,
) {
    let _ = lifetimes;
    let ssa = ssa_locals(ctx, body, frame);
    let copies = copy_classes(&ssa, body, frame);
    // Only probed: which locals are in a class of more than one.
    let mut unified: Bits<'_> = bits_in(body.locals.len(), frame);
    for (index, &head) in copies.iter().enumerate() {
        if local_id(index) != head {
            unified.insert(local_index(head));
            unified.insert(index);
        }
    }
    if unified.is_empty() {
        return;
    }
    let mut lookup = |local: LocalId| copies[local_index(local)];
    for (_, block) in body.basic_blocks.iter_mut() {
        for statement in &mut block.statements {
            if let StatementKind::StorageLive(local) | StatementKind::StorageDead(local) =
                statement.kind
                && unified.contains(local_index(local))
            {
                statement.kind = StatementKind::Nop;
            }
        }
    }
    // A move out of a local of a class, or a part of one not behind a pointer, copies: the
    // head may be read again.
    let copy_moves = |operand: &mut crate::mir::Operand| {
        if let OperandKind::Move(place) = &operand.kind
            && place.projection.as_slice().first() != Some(&ProjectionElem::Deref)
            && unified.contains(local_index(place.local))
        {
            // clone: a place's refcount, kept in the body
            operand.kind = OperandKind::Copy(place.clone());
        }
    };
    let mut copy_moves = copy_moves;
    for (_, block) in body.basic_blocks.iter_mut() {
        for statement in &mut block.statements {
            statement.kind.for_each_operand_mut(&mut copy_moves);
        }
        if let Some(terminator) = &mut block.terminator {
            terminator.kind.for_each_operand_mut(&mut copy_moves);
        }
    }
    body.walk_places(|place| map_place(place, &mut lookup));
    // Every binding keeps its slot: the map is the one a rebuild would make.
    for (_, local) in body.binding_locals.iter_mut() {
        *local = lookup(*local);
    }
    for (_, block) in body.basic_blocks.iter_mut() {
        for statement in &mut block.statements {
            if let StatementKind::Assign(to, Rvalue::Use(operand)) = &statement.kind
                && let OperandKind::Copy(from) | OperandKind::Move(from) = &operand.kind
                && to == from
            {
                statement.kind = StatementKind::Nop;
            }
        }
        block.statements.retain(|it| !matches!(it.kind, StatementKind::Nop));
    }
    remove_unused_definitions(body, frame);
}
