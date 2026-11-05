//! The passes that rewrite one statement or terminator at a time: rustc's `InstSimplify`,
//! `LowerSliceLenCalls` and `SingleUseConsts`.
//!
//! Of `InstSimplify`, what is here: comparisons with a constant `bool`, `&*r` of a reference
//! `r` of that type, a cast to the type the value has, `[x; 1]`, `Clone::clone` of a
//! primitive behind `&`, and switch targets that are the otherwise target. What is not:
//! `size_of_val`/`align_of_val`, `raw_eq`, intrinsic asserts and calls known not to unwind,
//! which rustc rewrites with intrinsics this MIR does not have, and the pointer aggregates it
//! has no rvalue for.

use blaze_mem::Frame;
use hir_def::{ItemContainerId, Lookup};
use rustc_type_ir::inherent::IntoKind;

use crate::{
    mir::{
        BasicBlock, BinOp, BorrowKind, CastKind, MirBody, MutBorrowKind, Operand, OperandKind,
        Place, ProjectionElem, Rvalue, Statement, StatementKind, SwitchTargets, TerminatorKind,
        UnOp,
        framed::{Bits, bits_in, local_index},
        transform::Ctx,
    },
    next_solver::{Ty, TyKind},
};

use super::{Location, Mention, visit_body};

fn operand_ty<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, operand: &Operand) -> Option<Ty<'db>> {
    if let OperandKind::Static(_) = operand.kind {
        return None;
    }
    Some(ctx.normalize(ctx.operand_ty(body, operand)))
}

fn constant_bool(operand: &Operand) -> Option<bool> {
    let bits = match &operand.kind {
        OperandKind::Allocation { allocation } => {
            let allocation = allocation.as_ref();
            if !matches!(allocation.ty.kind(), TyKind::Bool) {
                return None;
            }
            super::cfg::constant_bits(operand)?
        }
        OperandKind::Constant { ty, .. } if matches!(ty.as_ref().kind(), TyKind::Bool) => {
            super::cfg::constant_bits(operand)?
        }
        _ => return None,
    };
    Some(bits != 0)
}

/// The function a call calls, when it names one.
pub(super) fn callee(func: &Operand) -> Option<hir_def::FunctionId> {
    let ty = match &func.kind {
        OperandKind::Allocation { allocation } => allocation.as_ref().ty,
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        _ => return None,
    };
    match ty.kind() {
        TyKind::FnDef(def, _) => match def.0 {
            crate::CallableDefId::FunctionId(it) => Some(it),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn inst_simplify<'db>(ctx: &Ctx<'_, 'db>, body: &mut MirBody<'db>, _frame: &Frame) {
    let clone_trait = ctx.infcx.interner.lang_items().Clone;
    let mut blocks = std::mem::take(&mut body.basic_blocks);
    for (_, block) in blocks.iter_mut() {
        for statement in &mut block.statements {
            let StatementKind::Assign(place, rvalue) = &mut statement.kind else { continue };
            simplify_bool_cmp(rvalue);
            simplify_ref_deref(ctx, body, place.as_ref(), rvalue);
            simplify_cast(ctx, body, rvalue);
            simplify_repeat_once(ctx, body, rvalue);
        }
        if let Some(clone_trait) = clone_trait {
            simplify_primitive_clone(ctx, body, clone_trait, block);
        }
        if let Some(terminator) = &mut block.terminator {
            simplify_duplicate_switch_targets(&mut terminator.kind);
        }
    }
    body.basic_blocks = blocks;
}

fn simplify_bool_cmp(rvalue: &mut Rvalue) {
    let Rvalue::BinaryOp(op @ (BinOp::Eq | BinOp::Ne), a, b) = &*rvalue else { return };
    // Each arm keeps one operand of the rvalue it replaces: a place's refcount or a handle.
    let new = match (op, constant_bool(a), constant_bool(b)) {
        // clone: the kept operand
        (BinOp::Eq, _, Some(true)) | (BinOp::Ne, _, Some(false)) => Rvalue::Use(a.clone()),
        // clone: the kept operand
        (BinOp::Eq, Some(true), _) | (BinOp::Ne, Some(false), _) => Rvalue::Use(b.clone()),
        (BinOp::Eq, Some(false), _) | (BinOp::Ne, Some(true), _) => {
            Rvalue::UnaryOp(UnOp::Not, b.clone()) // clone: the kept operand
        }
        (BinOp::Eq, _, Some(false)) | (BinOp::Ne, _, Some(true)) => {
            Rvalue::UnaryOp(UnOp::Not, a.clone()) // clone: the kept operand
        }
        _ => return,
    };
    *rvalue = new;
}

/// `&*r` is `r` when `r` is a reference of that type. A two-phase borrow is left alone, as
/// rustc leaves it: it is not a full `&mut` reborrow.
fn simplify_ref_deref<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    _to: Place<'db>,
    rvalue: &mut Rvalue,
) {
    let Rvalue::Ref(kind, place) = &*rvalue else { return };
    if matches!(kind, BorrowKind::Mut { kind: MutBorrowKind::TwoPhasedBorrow }) {
        return;
    }
    let elems = place.projection.as_slice();
    let Some((ProjectionElem::Deref, base)) = elems.split_last() else { return };
    let base =
        Place { local: place.local, projection: super::super::Projection::new_from_slice(base) };
    let base_ty = ctx.normalize(base.ty(body, &ctx.infcx, ctx.env).ty);
    let (pointee, mutability) = match base_ty.kind() {
        TyKind::Ref(_, pointee, mutability) => (pointee, mutability),
        _ => return,
    };
    let wanted = match kind {
        BorrowKind::Shared | BorrowKind::Shallow => rustc_ast_ir::Mutability::Not,
        BorrowKind::Mut { .. } => rustc_ast_ir::Mutability::Mut,
    };
    if mutability != wanted {
        return;
    }
    let _ = pointee;
    *rvalue = Rvalue::Use(Operand { kind: OperandKind::Copy(base.store()), span: None });
}

fn simplify_cast<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, rvalue: &mut Rvalue) {
    let Rvalue::Cast(kind, operand, ty) = rvalue else { return };
    if !matches!(kind, CastKind::IntToInt | CastKind::FloatToFloat | CastKind::PtrToPtr) {
        return;
    }
    let Some(from) = operand_ty(ctx, body, operand) else { return };
    if from == ctx.normalize(ty.as_ref()) {
        *rvalue = Rvalue::Use(operand.clone()); // clone: the kept operand
    }
}

fn simplify_repeat_once<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, rvalue: &mut Rvalue) {
    let Rvalue::Repeat(operand, count) = rvalue else { return };
    if crate::consteval::try_const_usize(ctx.db, count.as_ref()) != Some(1) {
        return;
    }
    let Some(element) = operand_ty(ctx, body, operand) else { return };
    *rvalue = Rvalue::Aggregate(
        crate::mir::AggregateKind::Array(element.store()),
        // alloc: clone: the kept aggregate's one operand, in the body
        Box::new([operand.clone()]),
    );
}

/// `Clone::clone(&x)` of a primitive `x` is `x`.
fn simplify_primitive_clone<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    clone_trait: hir_def::TraitId,
    block: &mut BasicBlock,
) {
    let Some(terminator) = &mut block.terminator else { return };
    let TerminatorKind::Call { func, args, destination, target: Some(target), .. } =
        &terminator.kind
    else {
        return;
    };
    let [arg] = &args[..] else { return };
    let Some(function) = callee(func) else { return };
    if function.lookup(ctx.db).container != ItemContainerId::TraitId(clone_trait) {
        return;
    }
    let Some(arg_ty) = operand_ty(ctx, body, arg) else { return };
    let TyKind::Ref(_, inner, rustc_ast_ir::Mutability::Not) = arg_ty.kind() else { return };
    if !inner.is_trivially_pure_clone_copy() {
        return;
    }
    let (OperandKind::Copy(place) | OperandKind::Move(place)) = &arg.kind else { return };
    let value = place.as_ref().project(ProjectionElem::Deref).store();
    block.statements.push(Statement {
        kind: StatementKind::Assign(
            destination.clone(), // clone: a place's refcount, kept in the body
            Rvalue::Use(Operand { kind: OperandKind::Copy(value), span: None }),
        ),
        span: terminator.span,
    });
    terminator.kind = TerminatorKind::Goto { target: *target };
}

/// rustc's `simplify_duplicate_switch_targets`.
pub(super) fn simplify_duplicate_switch_targets(kind: &mut TerminatorKind) {
    let TerminatorKind::SwitchInt { targets, .. } = kind else { return };
    let otherwise = targets.otherwise();
    if targets.iter().any(|it| it.1 == otherwise) {
        *targets = SwitchTargets::new(targets.iter().filter(|it| it.1 != otherwise), otherwise);
    }
}

/// rustc's `LowerSliceLenCalls`: `<[T]>::len(r)` is the length of `*r`. rustc reads it with
/// `PtrMetadata(r)`; this MIR has `Len`, which reads the same.
pub(super) fn lower_slice_len_calls<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    _frame: &Frame,
) {
    let Some(slice_len) = ctx.infcx.interner.lang_items().SliceLen else { return };
    for (_, block) in body.basic_blocks.iter_mut() {
        let Some(terminator) = &mut block.terminator else { continue };
        let TerminatorKind::Call { func, args, destination, target: Some(target), .. } =
            &terminator.kind
        else {
            continue;
        };
        let [arg] = &args[..] else { continue };
        if callee(func) != Some(slice_len) {
            continue;
        }
        let (OperandKind::Copy(place) | OperandKind::Move(place)) = &arg.kind else { continue };
        let slice = place.as_ref().project(ProjectionElem::Deref).store();
        block.statements.push(Statement {
            // clone: a place's refcount, kept in the body
            kind: StatementKind::Assign(destination.clone(), Rvalue::Len(slice)),
            span: terminator.span,
        });
        terminator.kind = TerminatorKind::Goto { target: *target };
    }
}

fn is_constant(operand: &Operand) -> bool {
    matches!(operand.kind, OperandKind::Allocation { .. } | OperandKind::Constant { .. })
}

/// rustc's `SingleUseConsts`: a local assigned a constant once and read once is that
/// constant where it is read, and one never read is not assigned. A local a debugger is told
/// of keeps its constant, where rustc moves the constant into the debug info.
pub(super) fn single_use_consts(body: &mut MirBody<'_>, frame: &Frame) {
    let count = body.locals.len();
    let mut ineligible: Bits<'_> = bits_in(count, frame);
    for &local in body.param_locals.iter().chain(body.binding_locals.values()) {
        ineligible.insert(local_index(local));
    }
    for local in
        body.promoted_locals.iter().map(|it| it.0).chain(body.upvars_in_place.iter().map(|it| it.0))
    {
        ineligible.insert(local_index(local));
    }
    ineligible.insert(local_index(crate::mir::return_slot()));
    let init = frame.alloc_slice_fill_with(count, |_| None::<Location>);
    let uses = frame.alloc_slice_fill_with(count, |_| None::<Location>);
    // What `visit_body` says of a local that is not its constant assignment or a read of the
    // whole of it as an operand makes it ineligible.
    let whole_reads = frame.alloc_slice_fill_with(count, |_| 0u32);
    for (block, data) in body.basic_blocks.iter() {
        for (index, statement) in data.statements.iter().enumerate() {
            let at = Location { block, index };
            if let StatementKind::Assign(place, Rvalue::Use(operand)) = &statement.kind
                && let Some(local) = place.as_ref().as_local()
                && is_constant(operand)
            {
                if init[local_index(local)].replace(at).is_some() {
                    ineligible.insert(local_index(local));
                }
            }
            statement.kind.for_each_operand(&mut |operand| {
                note_use(operand, at, uses, &mut ineligible, whole_reads)
            });
        }
        if let Some(terminator) = &data.terminator {
            let at = Location { block, index: data.statements.len() };
            terminator.kind.for_each_operand(&mut |operand| {
                note_use(operand, at, uses, &mut ineligible, whole_reads)
            });
        }
    }
    let mentions = frame.alloc_slice_fill_with(count, |_| 0u32);
    visit_body(body, |local, mention, _| {
        if mention != Mention::Storage {
            mentions[local_index(local)] += 1;
        }
    });
    for local in 0..count {
        let Some(at) = init[local] else { continue };
        // The constant assignment and the one read are all there is of it.
        if ineligible.contains(local) || mentions[local] != 1 + whole_reads[local] {
            continue;
        }
        let statement = &mut body.basic_blocks[at.block].statements[at.index];
        let StatementKind::Assign(_, Rvalue::Use(operand)) =
            std::mem::replace(&mut statement.kind, StatementKind::Nop)
        else {
            unreachable!()
        };
        let Some(use_at) = uses[local] else { continue };
        let target = super::local_id(local);
        let mut operand = Some(operand);
        let mut replace = |it: &mut Operand| {
            if let OperandKind::Copy(place) | OperandKind::Move(place) = &it.kind
                && place.as_ref().as_local() == Some(target)
                && let Some(new) = operand.take()
            {
                *it = new;
            }
        };
        let block = &mut body.basic_blocks[use_at.block];
        match block.statements.get_mut(use_at.index) {
            Some(statement) => statement.kind.for_each_operand_mut(&mut replace),
            None => {
                if let Some(terminator) = &mut block.terminator {
                    terminator.kind.for_each_operand_mut(&mut replace)
                }
            }
        }
    }
    for (_, block) in body.basic_blocks.iter_mut() {
        block.statements.retain(|it| !matches!(it.kind, StatementKind::Nop));
    }
}

fn note_use(
    operand: &Operand,
    at: Location,
    uses: &mut [Option<Location>],
    ineligible: &mut Bits<'_>,
    whole_reads: &mut [u32],
) {
    let (OperandKind::Copy(place) | OperandKind::Move(place)) = &operand.kind else { return };
    let Some(local) = place.as_ref().as_local() else { return };
    whole_reads[local_index(local)] += 1;
    if uses[local_index(local)].replace(at).is_some() {
        ineligible.insert(local_index(local));
    }
}
