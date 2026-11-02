//! rustc's `DeadStoreElimination` (`dead_store_elimination.rs`) with the liveness it runs on,
//! `MaybeTransitiveLiveLocals`: an assignment to a local that is not read after, on any path
//! and unwinding included, goes, and so do the assignments only it read. A copy into a call
//! of a local not read after it is a move.
//!
//! A local that is ever borrowed is always live, and so are the return slot, what a debugger
//! is told of, the promoted locals and the captures of an `async` body. A copy into a call
//! becomes a move only of a whole local, where rustc also takes a part of one that is not in a
//! packed struct.

use blaze_mem::Frame;

use crate::mir::{
    BasicBlock, BasicBlockId, LocalId, MirBody, OperandKind, ProjectionElem, Rvalue,
    StatementKind, TerminatorKind,
    framed::{Bits, bits_in, block_index, local_index},
    return_slot,
};

use super::{Location, Mention, reverse_postorder, visit_statement, visit_terminator};

/// The mentions of one statement or terminator, reused for each.
type Mentions<'f> = Vec<(LocalId, Mention), &'f Frame>;

fn is_safe_to_remove(rvalue: &Rvalue) -> bool {
    !matches!(rvalue, Rvalue::Cast(crate::mir::CastKind::PointerExposeAddress, _, _))
}

fn is_indirect(place: &crate::mir::StoredPlace) -> bool {
    place.projection.as_slice().contains(&ProjectionElem::Deref)
}

/// The local an assignment writes, when it is one this pass may take out.
fn removable_destination(statement: &StatementKind, always_live: &Bits<'_>) -> Option<LocalId> {
    let place = match statement {
        StatementKind::Assign(place, rvalue) if is_safe_to_remove(rvalue) => place,
        StatementKind::Deinit(place) => place,
        _ => return None,
    };
    (!is_indirect(place) && !always_live.contains(local_index(place.local))).then_some(place.local)
}

/// Liveness across one mention, backwards. rustc ends a local's life at its `StorageDead`;
/// here only a write does, so a read that lowering misplaces after the `StorageDead` still
/// keeps the store it reads.
fn transfer(live: &mut Bits<'_>, local: LocalId, mention: Mention) {
    match mention {
        Mention::Assigned | Mention::CallReturned => {
            live.remove(local_index(local));
        }
        Mention::AssignedPart | Mention::Storage => (),
        Mention::Read | Mention::SharedBorrow | Mention::Mutated => {
            live.insert(local_index(local));
        }
    }
}

fn statement_effect(
    live: &mut Bits<'_>,
    statement: &StatementKind,
    at: Location,
    always_live: &Bits<'_>,
    mentions: &mut Mentions<'_>,
) {
    if let Some(local) = removable_destination(statement, always_live)
        && !live.contains(local_index(local))
    {
        return;
    }
    // The left-hand side is written after the right-hand side is read: kills first.
    mentions.clear();
    visit_statement(statement, at, &mut |local, mention, _| mentions.push((local, mention)));
    for &(local, mention) in mentions.iter() {
        if matches!(mention, Mention::Assigned | Mention::Storage) {
            transfer(live, local, mention);
        }
    }
    for &(local, mention) in mentions.iter() {
        if !matches!(mention, Mention::Assigned | Mention::Storage) {
            transfer(live, local, mention);
        }
    }
}

fn terminator_effect(
    live: &mut Bits<'_>,
    kind: &TerminatorKind,
    at: Location,
    mentions: &mut Mentions<'_>,
) {
    mentions.clear();
    visit_terminator(kind, at, &mut |local, mention, _| mentions.push((local, mention)));
    for &(local, mention) in mentions.iter() {
        if mention == Mention::CallReturned {
            transfer(live, local, mention);
        }
    }
    for &(local, mention) in mentions.iter() {
        if mention != Mention::CallReturned {
            transfer(live, local, mention);
        }
    }
}

/// Live on exit from `block`: live on entry to any of its successors.
fn exit_into(entry: &[Bits<'_>], block: &BasicBlock, live: &mut Bits<'_>) {
    live.clear();
    if let Some(terminator) = &block.terminator {
        for target in terminator.kind.all_successors() {
            live.union_with(&entry[block_index(target)]);
        }
    }
}

fn insert_index_locals(live: &mut Bits<'_>, place: &crate::mir::StoredPlace) {
    for elem in place.projection.as_slice() {
        if let ProjectionElem::Index(local) = elem {
            live.insert(local_index(*local));
        }
    }
}

pub(super) fn dead_store_elimination(body: &mut MirBody<'_>, frame: &Frame) {
    let locals = body.locals.len();
    let mut always_live: Bits<'_> = bits_in(locals, frame);
    always_live.insert(local_index(return_slot()));
    for local in body
        .binding_locals
        .values()
        .copied()
        .chain(body.promoted_locals.iter().map(|it| it.0))
        .chain(body.upvars_in_place.iter().map(|it| it.0))
    {
        always_live.insert(local_index(local));
    }
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            match &statement.kind {
                StatementKind::Assign(_, Rvalue::Ref(_, place)) if !is_indirect(place) => {
                    always_live.insert(local_index(place.local));
                }
                StatementKind::InlineAsm(_) => {
                    let at = Location { block: body.start_block, index: 0 };
                    visit_statement(&statement.kind, at, &mut |local, _, _| {
                        always_live.insert(local_index(local));
                    });
                }
                _ => (),
            }
        }
    }

    // Live on entry to each block, to a fixpoint, in postorder.
    let order = reverse_postorder(body, frame);
    let blocks = body.basic_blocks.len();
    let mut entry: Vec<Bits<'_>, &Frame> = Vec::with_capacity_in(blocks, frame);
    entry.extend((0..blocks).map(|_| bits_in(locals, frame)));
    let mut live: Bits<'_> = bits_in(locals, frame);
    let mut mentions: Mentions<'_> = Vec::new_in(frame);
    let mut changed = true;
    while changed {
        changed = false;
        for &bb in order.iter().rev() {
            let block = &body.basic_blocks[bb];
            exit_into(&entry, block, &mut live);
            if let Some(terminator) = &block.terminator {
                let at = Location { block: bb, index: block.statements.len() };
                terminator_effect(&mut live, &terminator.kind, at, &mut mentions);
            }
            for (index, statement) in block.statements.iter().enumerate().rev() {
                statement_effect(
                    &mut live,
                    &statement.kind,
                    Location { block: bb, index },
                    &always_live,
                    &mut mentions,
                );
            }
            if live != entry[block_index(bb)] {
                entry[block_index(bb)].copy_from(&live);
                changed = true;
            }
        }
    }

    let mut dead: Vec<Location, &Frame> = Vec::new_in(frame);
    let mut to_move: Vec<(BasicBlockId, usize), &Frame> = Vec::new_in(frame);
    let mut state: Bits<'_> = bits_in(locals, frame);
    for &bb in order.iter() {
        let block = &body.basic_blocks[bb];
        exit_into(&entry, block, &mut live);
        if let Some(terminator) = &block.terminator {
            let at = Location { block: bb, index: block.statements.len() };
            if let TerminatorKind::Call { args, destination, .. } = &terminator.kind {
                // Between reading the arguments and writing the destination.
                state.copy_from(&live);
                if destination.projection.as_slice().is_empty() {
                    state.remove(local_index(destination.local));
                } else if is_indirect(destination) {
                    state.insert(local_index(destination.local));
                }
                insert_index_locals(&mut state, destination);
                for (index, arg) in args.iter().enumerate().rev() {
                    if let OperandKind::Copy(place) = &arg.kind
                        && place.projection.as_slice().is_empty()
                        && !always_live.contains(local_index(place.local))
                        && !state.contains(local_index(place.local))
                    {
                        to_move.push((bb, index));
                    }
                    if let OperandKind::Copy(place) | OperandKind::Move(place) = &arg.kind {
                        state.insert(local_index(place.local));
                        insert_index_locals(&mut state, place);
                    }
                }
            }
            terminator_effect(&mut live, &terminator.kind, at, &mut mentions);
        }
        for (index, statement) in block.statements.iter().enumerate().rev() {
            let at = Location { block: bb, index };
            if let Some(local) = removable_destination(&statement.kind, &always_live)
                && !live.contains(local_index(local))
            {
                dead.push(at);
            }
            statement_effect(&mut live, &statement.kind, at, &always_live, &mut mentions);
        }
    }
    if dead.is_empty() && to_move.is_empty() {
        return;
    }
    let eliminated = !dead.is_empty();
    for &at in dead.iter() {
        body.basic_blocks[at.block].statements[at.index].kind = StatementKind::Nop;
    }
    for &(bb, index) in to_move.iter() {
        let Some(terminator) = &mut body.basic_blocks[bb].terminator else { continue };
        if let TerminatorKind::Call { args, .. } = &mut terminator.kind
            && let OperandKind::Copy(place) = &args[index].kind
        {
            // clone: a place's refcount, kept in the body
            args[index].kind = OperandKind::Move(place.clone());
        }
    }
    if eliminated {
        remove_unused_storage_markers(body, frame);
    }
}

/// rustc's `remove_unused_storage_annotations`: the storage markers of a local nothing else
/// mentions go, with the `Nop`s.
fn remove_unused_storage_markers(body: &mut MirBody<'_>, frame: &Frame) {
    let mut used: Bits<'_> = bits_in(body.locals.len(), frame);
    super::visit_body(body, |local, mention, _| {
        if mention != Mention::Storage {
            used.insert(local_index(local));
        }
    });
    for (_, block) in body.basic_blocks.iter_mut() {
        block.statements.retain(|it| match it.kind {
            StatementKind::Nop => false,
            StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                used.contains(local_index(local))
            }
            _ => true,
        });
    }
}
