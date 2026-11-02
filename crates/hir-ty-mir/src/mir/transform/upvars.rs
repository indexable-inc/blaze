//! An `async` body uses what it captured in place, as rustc's does: lowering copies each
//! capture into a local when the body starts (`capture = move (*future).i`, which borrowck
//! wants), and here that local becomes the capture itself. The copy and the local's storage
//! markers go; the local holds its value when the body starts and lives in the future's
//! prefix (`MirBody::upvars_in_place`), so it is never one of the locals the future keeps for
//! a yield. Drop elaboration then tracks it like any local, as rustc tracks the fields of its
//! coroutine argument.

use blaze_mem::Frame;

use crate::mir::{FieldIndex, MirBody, OperandKind, ProjectionElem, Rvalue, StatementKind};

pub(super) fn upvars_in_place(body: &mut MirBody<'_>, frame: &Frame) {
    if body.coroutine.is_none() {
        return;
    }
    let Some(&future) = body.param_locals.first() else { return };
    let start = body.start_block;
    let mut upvars = Vec::with_capacity_in(body.basic_blocks[start].statements.len(), frame);
    body.basic_blocks[start].statements.retain(|statement| {
        let StatementKind::Assign(to, Rvalue::Use(operand)) = &statement.kind else {
            return true;
        };
        let (OperandKind::Copy(from) | OperandKind::Move(from)) = &operand.kind else {
            return true;
        };
        let (&[ProjectionElem::Deref, ProjectionElem::Field(FieldIndex(index))], true) =
            (from.projection.as_slice(), to.projection.as_slice().is_empty())
        else {
            return true;
        };
        if from.local != future || upvars.iter().any(|&(it, _)| it == to.local) {
            return true;
        }
        upvars.push((to.local, index));
        false
    });
    if upvars.is_empty() {
        return;
    }
    for (_, block) in body.basic_blocks.iter_mut() {
        block.statements.retain(|statement| match statement.kind {
            StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                !upvars.iter().any(|&(it, _)| it == local)
            }
            _ => true,
        });
    }
    let mut kept = Vec::with_capacity(upvars.len());
    kept.extend_from_slice(&upvars);
    body.upvars_in_place = kept;
}
