//! rustc's `SimplifyLocals` (`simplify.rs`) and `RemoveStorageMarkers`.

use blaze_mem::Frame;
use la_arena::Arena;

use crate::mir::{
    CastKind, LocalId, MirBody, Rvalue, StatementKind,
    framed::{Bits, bits_in, block_id, local_index},
    return_slot,
};

use super::{Location, Mention, renumber_locals, visit_body, visit_statement};

/// rustc's `RemoveStorageMarkers`, which runs where no lifetimes are given to LLVM.
pub(super) fn remove_storage_markers(body: &mut MirBody<'_>, _frame: &Frame) {
    for (_, block) in body.basic_blocks.iter_mut() {
        block.statements.retain(|it| {
            !matches!(it.kind, StatementKind::StorageLive(_) | StatementKind::StorageDead(_))
        });
    }
}

/// rustc's `Rvalue::is_safe_to_remove`: an exposing cast is the only rvalue that does
/// something besides giving its value.
fn is_safe_to_remove(rvalue: &Rvalue) -> bool {
    !matches!(rvalue, Rvalue::Cast(CastKind::PointerExposeAddress, _, _))
}

/// How many times each local is used, as rustc's `UsedLocals` counts: not where an
/// assignment that could be removed writes it as a whole, and not in storage markers. The
/// return slot, the parameters and what a debugger is told of are always used.
struct UsedLocals<'f> {
    count: &'f mut [u32],
    always: Bits<'f>,
}

impl<'f> UsedLocals<'f> {
    fn new(body: &MirBody<'_>, frame: &'f Frame) -> UsedLocals<'f> {
        let mut always: Bits<'f> = bits_in(body.locals.len(), frame);
        always.insert(local_index(return_slot()));
        for &local in body.param_locals.iter().chain(body.binding_locals.values()) {
            always.insert(local_index(local));
        }
        // The code generator gives each promoted local its constant, and an `async` body's
        // captures are the future's fields.
        let kept = body.promoted_locals.iter().map(|it| it.0);
        for local in kept.chain(body.upvars_in_place.iter().map(|it| it.0)) {
            always.insert(local_index(local));
        }
        let count = frame.alloc_slice_fill_with(body.locals.len(), |_| 0u32);
        let mut used = UsedLocals { count, always };
        visit_body(body, |local, mention, at| used.mention(body, local, mention, at, 1));
        used
    }

    fn is_used(&self, local: LocalId) -> bool {
        self.always.contains(local_index(local)) || self.count[local_index(local)] != 0
    }

    fn mention(
        &mut self,
        body: &MirBody<'_>,
        local: LocalId,
        mention: Mention,
        at: Location,
        by: i32,
    ) {
        match mention {
            Mention::Storage => return,
            Mention::Assigned | Mention::AssignedPart => {
                let block = &body.basic_blocks[at.block];
                if let Some(StatementKind::Assign(_, rvalue)) =
                    block.statements.get(at.index).map(|it| &it.kind)
                    && is_safe_to_remove(rvalue)
                {
                    return;
                }
            }
            _ => (),
        }
        let count = &mut self.count[local_index(local)];
        *count = (*count as i32 + by) as u32;
    }

    /// What the use counts are without `statement`, which was at `at`.
    fn removed(&mut self, statement: &StatementKind, at: Location) {
        let removable = matches!(statement, StatementKind::Assign(_, it) if is_safe_to_remove(it));
        visit_statement(statement, at, &mut |local, mention, _| {
            if mention == Mention::Storage
                || matches!(mention, Mention::Assigned | Mention::AssignedPart) && removable
            {
                return;
            }
            let count = &mut self.count[local_index(local)];
            *count = count.saturating_sub(1);
        });
    }
}

/// rustc's `remove_unused_definitions_helper`: assignments to locals nothing reads, and their
/// storage markers, go, until no more do.
fn remove_unused_definitions_helper(used: &mut UsedLocals<'_>, body: &mut MirBody<'_>) {
    let mut modified = true;
    while modified {
        modified = false;
        // Removing statements never adds or removes a block.
        for at in 0..body.basic_blocks.len() {
            let block = block_id(at);
            let mut index = 0;
            while index < body.basic_blocks[block].statements.len() {
                let statement = &body.basic_blocks[block].statements[index];
                let keep = match &statement.kind {
                    StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                        used.is_used(*local)
                    }
                    StatementKind::Assign(place, rvalue) => {
                        used.is_used(place.local) || !is_safe_to_remove(rvalue)
                    }
                    _ => true,
                };
                if keep {
                    index += 1;
                    continue;
                }
                let statement = body.basic_blocks[block].statements.remove(index);
                used.removed(&statement.kind, Location { block, index });
                modified = true;
            }
        }
    }
}

/// What rustc's `CopyProp` ends with: unused definitions removed, the locals kept.
pub(super) fn remove_unused_definitions(body: &mut MirBody<'_>, frame: &Frame) {
    let mut used = UsedLocals::new(body, frame);
    remove_unused_definitions_helper(&mut used, body);
}

/// rustc's `SimplifyLocals`: what no one uses goes, and the locals left are numbered again in
/// their order.
pub(super) fn simplify_locals(body: &mut MirBody<'_>, frame: &Frame) {
    let mut used = UsedLocals::new(body, frame);
    remove_unused_definitions_helper(&mut used, body);
    let kept = body.locals.iter().filter(|(local, _)| used.is_used(*local)).count();
    if kept == body.locals.len() {
        return;
    }
    let map = frame.alloc_slice_fill_with(body.locals.len(), |_| None::<LocalId>);
    // The kept locals moved, in their order, into an arena of exactly their number.
    let mut new_locals = Arena::with_capacity(kept);
    for (local, decl) in std::mem::take(&mut body.locals).into_iter() {
        if used.is_used(local) {
            map[local_index(local)] = Some(new_locals.alloc(decl));
        }
    }
    renumber_locals(body, new_locals, map);
}
