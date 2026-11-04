//! `BoxInPlace`, blaze's own pass (rustc has none): `Box::new(value)` of a value of at least
//! [`BOX_IN_PLACE_MIN`] bytes builds the value in the box instead of on the stack, at every
//! level that optimizes (`Debug` and `Release`).
//!
//! `Box::new` is an ordinary by-value function: its argument is built in a local of the caller,
//! then std's body allocates (`box_new_uninit`) and moves the value in, a `memcpy` of all of it
//! and a stack slot that size. This pass takes the allocation out of std's body and puts it
//! before the value is built, then builds the value in the box's place and makes the `Box` of
//! the pointer, as `Box::new`'s body does after its write. Rust does not promise when the
//! allocator is called, so the order is free to change; what stays the same:
//!
//! - the value and what drops when: the value is built by the same rvalue or call, into the
//!   box's place, and nothing else moves;
//! - the allocation: std's own `box_new_uninit` with std's own `<T as SizedTypeProperties>::
//!   LAYOUT`, taken from the body of `Box::new` itself, so a zero-sized value and a failed
//!   allocation (`handle_alloc_error`) are what std does;
//! - unwinding: a call that builds the value and unwinds frees the box (a drop of it as a
//!   `Box<MaybeUninit<T>>`, which deallocates and drops nothing), then unwinds where the call
//!   unwound before.
//!
//! The one difference is the point of the pass: the allocation comes before the value is built,
//! so a failed allocation stops the program before the argument's own code runs, not after.
//! With the default `handle_alloc_error`, which aborts, nothing else can tell. With an
//! allocation-error hook that panics, what would have been built is not, and what it would
//! have been built from is still where it was; so that nothing drops differently, the pass
//! takes only a value with nothing to drop, built from inputs with nothing to drop.
//!
//! What it takes: a call of std's inherent `Box::<T>::new` whose argument is a temporary, `_a`,
//! of a type at least [`BOX_IN_PLACE_MIN`] bytes where the body is generic, with nothing to
//! drop, mentioned nowhere but where it is built, the call, and its storage markers, built
//! - by an assignment (an aggregate, an array repeat, a cast) in the block of the call; a
//!   tuple or struct aggregate whose fields are temporaries built the same way earlier in the
//!   block has them built in their fields of the box too (`Box::new(Big([0; 1024]))` builds
//!   the array in the box), and writes its other fields one by one; or
//! - by a call that returns into the block of the `Box::new` call, the only way into it.
//!
//! What it leaves (and `blaze::box_large_new` still reports): a place moved in (a local, a
//! field), which is already on the stack; a value built over several blocks (an `if` or a
//! `match`); a value whose inputs have something to drop. `Arc::new`, `Rc::new` and
//! `Vec::push` are not rewritten: their allocation is not the first thing their body does.

use blaze_mem::Frame;
use hir_def::{AdtId, FunctionId, ItemContainerId, Lookup, signatures::FunctionSignature};
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{IntoKind, Ty as _};
use smallvec::SmallVec;

use crate::{
    db::HirDatabase,
    mir::{
        AggregateKind, BasicBlock, BasicBlockId, CastKind, FieldIndex, Local, LocalId, MirBody,
        MirLowerError, MirSpan, Operand, OperandKind, Place, ProjectionElem, Rvalue, Statement,
        StatementKind, Terminator, TerminatorKind, VariantId, return_slot, transform::Ctx,
    },
    next_solver::{ConstExt, GenericArg, GenericArgs, Ty, TyKind},
};

use super::{Mention, Result, block_index, inline, inst_simplify::callee, local_index, visit_body};

/// The size from which `Box::new(value)` builds the value in the box: 4 KiB, the default of
/// `blaze::box_large_new`'s `large-box-threshold`, which stops reporting what this pass builds
/// in place.
pub const BOX_IN_PLACE_MIN: u64 = 4 << 10;

/// What std's `Box::new` allocates with, read from its body: `box_new_uninit` and its layout
/// argument, in terms of `Box::new`'s own `T`.
struct Alloc {
    func: Operand,
    layout: Operand,
    /// `*mut u8`, what `box_new_uninit` returns.
    raw_ty: crate::next_solver::StoredTy,
}

/// A value built in the box by an assignment in the block of the call: the statement at
/// `index` assigns `local`, which becomes the place `path` (fields from the box's value) in
/// the box. With `split`, the statement is an aggregate some of whose fields are placed values
/// themselves, and writes its other fields one by one.
struct Placed {
    index: usize,
    local: LocalId,
    path: SmallVec<[FieldIndex; 4]>,
    split: bool,
}

/// How the argument of the `Box::new` call is built.
enum Built {
    /// By assignments in the block of the call: the argument's first, then the fields of
    /// aggregates built in place too, each before the aggregate that reads it.
    Assign(SmallVec<[Placed; 4]>),
    /// By the call that ends this block and returns into the block of the `Box::new` call.
    Call(BasicBlockId),
}

struct Site {
    /// The block the `Box::new` call ends.
    block: BasicBlockId,
    /// The argument's temporary.
    arg: LocalId,
    built: Built,
}

pub(super) fn box_in_place<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    frame: &Frame,
) -> Result<'db, ()> {
    let sites = find(ctx, body, frame);
    if sites.is_empty() {
        return Ok(());
    }
    let mut alloc: Option<(FunctionId, Alloc)> = None;
    for site in sites {
        let Some(TerminatorKind::Call { func, .. }) =
            body.basic_blocks[site.block].terminator.as_ref().map(|it| &it.kind)
        else {
            continue;
        };
        let Some((new, args)) = func.fn_item() else { continue };
        if alloc.as_ref().is_none_or(|(it, _)| *it != new) {
            alloc = Some((new, std_alloc(ctx, new)?));
        }
        let (_, std) = alloc.as_ref().expect("set above");
        let Some(layout) = instantiate_operand(ctx, &std.layout, args) else {
            return Err(MirLowerError::ImplementationError(format!(
                "BoxInPlace: the layout argument of `box_new_uninit` in `alloc::boxed::Box::<T>::new` \
                 ({new:?}) does not instantiate with the generic arguments of the call in bb{} of {:?}",
                block_index(site.block),
                body.owner
            )));
        };
        rewrite(ctx, body, site, std, layout)?;
    }
    Ok(())
}

/// The `Box::new` calls of `body` this pass rewrites, in block order.
fn find<'f, 'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    frame: &'f Frame,
) -> Vec<Site, &'f Frame> {
    let mut sites = Vec::new_in(frame);
    let count = body.locals.len();
    let built = frame.alloc_slice_fill_with(count, |_| 0u32);
    let read = frame.alloc_slice_fill_with(count, |_| 0u32);
    let other = frame.alloc_slice_fill_with(count, |_| 0u32);
    visit_body(body, |local, mention, _| {
        let at = local_index(local);
        match mention {
            Mention::Assigned | Mention::CallReturned => built[at] += 1,
            Mention::Read => read[at] += 1,
            Mention::Storage => {}
            Mention::AssignedPart | Mention::SharedBorrow | Mention::Mutated => other[at] += 1,
        }
    });
    let blocks = body.basic_blocks.len();
    let preds = frame.alloc_slice_fill_with(blocks, |_| 0u32);
    for (_, data) in body.basic_blocks.iter() {
        if let Some(terminator) = &data.terminator {
            for target in terminator.kind.all_successors() {
                preds[block_index(target)] += 1;
            }
        }
    }
    // A temporary built once and read once, as a whole, by what this pass rewrites.
    let single = |local: LocalId| {
        let at = local_index(local);
        built[at] == 1
            && read[at] == 1
            && other[at] == 0
            && local != return_slot()
            && !body.param_locals.contains(&local)
            && !body.binding_locals.values().any(|it| *it == local)
            && !body.promoted_locals.iter().any(|it| it.0 == local)
            && !body.upvars_in_place.iter().any(|it| it.0 == local)
    };
    // A block is rewritten for one site at most.
    let taken = frame.alloc_slice_fill_with(blocks, |_| false);
    for (block, data) in body.basic_blocks.iter() {
        if data.is_cleanup {
            continue;
        }
        let Some(Terminator {
            kind: TerminatorKind::Call { func, args, target: Some(_), .. }, ..
        }) = &data.terminator
        else {
            continue;
        };
        let [arg] = &args[..] else { continue };
        let OperandKind::Move(place) = &arg.kind else { continue };
        let Some(arg) = place.as_ref().as_local() else { continue };
        if !callee(func).is_some_and(|it| is_box_new(ctx, it)) {
            continue;
        }
        if !single(arg) {
            continue;
        }
        let ty = body.locals[arg].ty.as_ref();
        if ctx.size_of(ty).is_none_or(|size| size < BOX_IN_PLACE_MIN) {
            continue;
        }
        // A value with something to drop is not taken: built before the allocation, a failed
        // allocation whose hook panics would have dropped it.
        if ctx.needs_drop(ty) {
            continue;
        }
        let Some(built) = built_by(ctx, body, block, arg, &single, preds) else { continue };
        let other_block = match &built {
            Built::Assign(_) => None,
            Built::Call(it) => Some(*it),
        };
        if taken[block_index(block)] || other_block.is_some_and(|it| taken[block_index(it)]) {
            continue;
        }
        taken[block_index(block)] = true;
        if let Some(it) = other_block {
            taken[block_index(it)] = true;
        }
        sites.push(Site { block, arg, built });
    }
    sites
}

/// How `arg`, read by the `Box::new` call that ends `block`, is built, if it is a way this pass
/// takes.
fn built_by<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    block: BasicBlockId,
    arg: LocalId,
    single: &dyn Fn(LocalId) -> bool,
    preds: &[u32],
) -> Option<Built> {
    let data = &body.basic_blocks[block];
    let assigned_in = |local: LocalId, before: usize| {
        data.statements[..before].iter().position(|it| {
            matches!(&it.kind, StatementKind::Assign(place, _) if place.as_ref().as_local() == Some(local))
        })
    };
    if let Some(index) = assigned_in(arg, data.statements.len()) {
        // alloc: inline `SmallVec`, on the stack for up to four placed values
        let mut plan: SmallVec<[Placed; 4]> = SmallVec::new();
        plan.push(Placed { index, local: arg, path: SmallVec::new(), split: false });
        // Each temporary is built once, so each statement joins the plan once: bounded by the
        // block's statements.
        let mut at = 0;
        while at < plan.len() {
            let index = plan[at].index;
            let built_ty = body.locals[plan[at].local].ty.as_ref();
            let StatementKind::Assign(_, rvalue) = &data.statements[index].kind else {
                return None;
            };
            let mut moves_drop = false;
            rvalue.for_each_operand(&mut |it| moves_drop |= moves_something_to_drop(ctx, body, it));
            if moves_drop {
                return None;
            }
            if let Rvalue::Aggregate(kind, operands) = rvalue
                && fieldwise(ctx, kind, built_ty)
            {
                for (field, operand) in operands.iter().enumerate() {
                    let OperandKind::Move(place) = &operand.kind else { continue };
                    let Some(local) = place.as_ref().as_local() else { continue };
                    if !single(local) {
                        continue;
                    }
                    let Some(built) = assigned_in(local, index) else { continue };
                    plan[at].split = true;
                    let mut path = plan[at].path.clone(); // clone: a few field indexes
                    path.push(FieldIndex(field as u32));
                    plan.push(Placed { index: built, local, path, split: false });
                }
            }
            at += 1;
        }
        // What comes after the allocation, which goes before the first statement of the plan,
        // may neither unwind nor run code.
        let first = plan.iter().map(|it| it.index).min().expect("the argument's own");
        if data.statements[first..].iter().any(|it| matches!(it.kind, StatementKind::InlineAsm(_)))
        {
            return None;
        }
        // Nor may it set or unset a local with something to drop: the call's cleanup, which the
        // allocation unwinds to, expects what was done before the call done.
        let sets_drop = data.statements[first..].iter().enumerate().any(|(offset, it)| {
            let place = match &it.kind {
                StatementKind::Assign(place, _) | StatementKind::Deinit(place) => place,
                _ => return false,
            };
            !plan.iter().any(|p| p.index == first + offset)
                && ctx.needs_drop(body.locals[place.as_ref().local].ty.as_ref())
        });
        if sets_drop {
            return None;
        }
        return Some(Built::Assign(plan));
    }
    // Built by a call that returns here: the one way into this block.
    if preds[block_index(block)] != 1 {
        return None;
    }
    let (pred, _) = body.basic_blocks.iter().find(|(_, it)| {
        it.terminator.as_ref().is_some_and(|it| it.kind.all_successors().any(|it| it == block))
    })?;
    if pred == block || body.basic_blocks[pred].is_cleanup {
        return None;
    }
    let Some(TerminatorKind::Call { args, destination, target: Some(target), .. }) =
        body.basic_blocks[pred].terminator.as_ref().map(|it| &it.kind)
    else {
        return None;
    };
    if *target != block || destination.as_ref().as_local() != Some(arg) {
        return None;
    }
    if data.statements.iter().any(|it| matches!(it.kind, StatementKind::InlineAsm(_))) {
        return None;
    }
    if args.iter().any(|it| moves_something_to_drop(ctx, body, it)) {
        return None;
    }
    Some(Built::Call(pred))
}

/// An aggregate, of type `ty`, whose fields are written one by one as it is: a tuple or a
/// struct, no discriminant to set, and not `#[repr(packed)]`, whose fields may be misaligned.
fn fieldwise<'db>(ctx: &Ctx<'_, 'db>, kind: &AggregateKind, ty: Ty<'db>) -> bool {
    match kind {
        AggregateKind::Tuple(_) => true,
        AggregateKind::Adt(VariantId::StructId(_), _) => {
            matches!(ctx.normalize(ty).kind(), TyKind::Adt(adt, _) if adt.repr(ctx.db).pack.is_none())
        }
        _ => false,
    }
}

/// `operand` moves a value with something to drop: one a panic before it is moved would leave
/// for the caller's cleanup, which expects it moved.
fn moves_something_to_drop<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    operand: &Operand,
) -> bool {
    match &operand.kind {
        OperandKind::Move(place) => ctx.needs_drop(place.as_ref().ty(body, &ctx.infcx, ctx.env).ty),
        _ => false,
    }
}

/// `func` is std's inherent `Box::<T>::new`.
pub(super) fn is_box_new(ctx: &Ctx<'_, '_>, func: FunctionId) -> bool {
    let db = ctx.db;
    let Some(owned_box) = ctx.infcx.interner.lang_items().OwnedBox else { return false };
    if FunctionSignature::of(db, func).name.as_str() != "new" {
        return false;
    }
    let ItemContainerId::ImplId(impl_) = func.lookup(db).container else { return false };
    if db.impl_trait(impl_).is_some() {
        return false;
    }
    matches!(
        db.impl_self_ty(impl_).skip_binder().kind(),
        TyKind::Adt(adt, _) if adt.def_id() == AdtId::StructId(owned_box)
    )
}

/// The allocation std's `Box::new` (`new`) starts with: its body calls `box_new_uninit` with a
/// constant layout before anything else. A body that does not is an error naming what it does
/// instead: the pass is not to quietly stop building values in place.
fn std_alloc<'db>(ctx: &Ctx<'_, 'db>, new: FunctionId) -> Result<'db, Alloc> {
    let db = ctx.db;
    let std = db.runtime_mir_body(new.into())?;
    let refuse = |what: String| {
        MirLowerError::ImplementationError(format!(
            "BoxInPlace: the body of `alloc::boxed::Box::<T>::new` ({new:?}) does not start by calling \
             `box_new_uninit` with a constant layout, which the pass takes the allocation from: {what}"
        ))
    };
    // The constants assigned to locals on the way to the first call.
    let mut constants: SmallVec<[(LocalId, &Operand); 4]> = SmallVec::new();
    let mut block = std.start_block;
    // Bounded by the blocks: each step goes to another block or ends.
    for _ in 0..std.basic_blocks.len() {
        let data = &std.basic_blocks[block];
        for statement in &data.statements {
            if let StatementKind::Assign(place, Rvalue::Use(operand)) = &statement.kind
                && let Some(local) = place.as_ref().as_local()
                && is_constant(operand)
            {
                constants.push((local, operand));
            }
        }
        match data.terminator.as_ref().map(|it| &it.kind) {
            Some(TerminatorKind::Goto { target }) => block = *target,
            Some(TerminatorKind::Call { func, args, destination, .. }) => {
                let Some(alloc) = callee(func) else {
                    return Err(refuse(format!(
                        "bb{} calls something not named",
                        block_index(block)
                    )));
                };
                let name = FunctionSignature::of(db, alloc).name.as_str().to_owned(); // alloc: a name for an error
                if name != "box_new_uninit" {
                    return Err(refuse(format!("its first call is of `{name}`")));
                }
                let [arg] = &args[..] else {
                    return Err(refuse(format!(
                        "`box_new_uninit` is given {} arguments",
                        args.len()
                    )));
                };
                let layout = match &arg.kind {
                    _ if is_constant(arg) => arg,
                    OperandKind::Copy(place) | OperandKind::Move(place) => {
                        let local = place.as_ref().as_local();
                        let found = constants.iter().rev().find(|it| Some(it.0) == local);
                        match found {
                            Some((_, it)) => *it,
                            None => {
                                return Err(refuse(format!(
                                    "the layout is _{}, not a constant",
                                    local_index(place.local)
                                )));
                            }
                        }
                    }
                    _ => return Err(refuse("the layout is a static".to_owned())), // alloc: error path
                };
                let raw_ty = std.locals[destination.local].ty.clone(); // clone: a refcount
                // clone: two operands kept for the rewrites of this body
                // Without std's span: the call is the caller's `Box::new`, given the call's span.
                let func = Operand { kind: func.kind.clone(), span: None }; // clone: a refcount
                return Ok(Alloc { func, layout: layout.clone(), raw_ty });
            }
            _ => {
                return Err(refuse(format!(
                    "bb{} ends in something other than a jump or a call",
                    block_index(block)
                )));
            }
        }
    }
    Err(refuse("its blocks loop before any call".to_owned())) // alloc: error path
}

fn is_constant(operand: &Operand) -> bool {
    matches!(operand.kind, OperandKind::Allocation { .. } | OperandKind::Constant { .. })
}

/// `operand`, a constant of `Box::new`'s body, with `Box::new`'s generic parameters replaced by
/// `args`, the call's.
fn instantiate_operand<'db>(
    ctx: &Ctx<'_, 'db>,
    operand: &Operand,
    args: GenericArgs<'db>,
) -> Option<Operand> {
    let kind = match &operand.kind {
        OperandKind::Constant { konst, ty } => OperandKind::Constant {
            konst: inline::fold(ctx, konst.as_ref(), Some(args))?.store(),
            ty: inline::fold(ctx, ty.as_ref(), Some(args))?.store(),
        },
        // An allocation is concrete: it names no parameter.
        OperandKind::Allocation { .. } => operand.kind.clone(), // clone: a refcount
        _ => return None,
    };
    Some(Operand { kind, span: None })
}

fn statement(kind: StatementKind, span: MirSpan) -> Statement {
    Statement { kind, span }
}

fn move_of(local: LocalId) -> Operand {
    Operand { kind: OperandKind::Move(Place::from(local).store()), span: None }
}

/// `site` rewritten: the allocation before the value is built, the value built in the box's
/// place, the `Box` made of the pointer where the call was.
fn rewrite<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    site: Site,
    std: &Alloc,
    layout: Operand,
) -> Result<'db, ()> {
    let interner = ctx.infcx.interner;
    let Some(Terminator {
        span,
        kind: TerminatorKind::Call { destination, target: Some(target), cleanup: box_cleanup, .. },
    }) = body.basic_blocks[site.block].terminator.take()
    else {
        unreachable!(
            "`find` took a `Box::new` call with a target in bb{}",
            block_index(site.block)
        );
    };
    let value_ty = body.locals[site.arg].ty.as_ref();
    let box_ty = ctx.normalize(destination.as_ref().ty(body, &ctx.infcx, ctx.env).ty);
    let raw = body.locals.alloc(Local { ty: std.raw_ty.clone() }); // clone: a refcount
    let typed_ty = Ty::new_ptr(interner, value_ty, Mutability::Mut);
    let typed = body.locals.alloc(Local { ty: typed_ty.store() });
    let to_typed = Rvalue::Cast(CastKind::PtrToPtr, move_of(raw), typed_ty.store());
    let to_box = Rvalue::Cast(CastKind::Transmute, move_of(typed), box_ty.store());
    // Each arm below calls it once, so it moves `layout` into the call.
    let allocate = move |target: BasicBlockId, cleanup: Option<BasicBlockId>| Terminator {
        span,
        kind: TerminatorKind::Call {
            func: std.func.clone(),   // clone: the function's constant, a refcount
            args: Box::new([layout]), // alloc: a call's arguments, kept in the body
            destination: Place::from(raw).store(),
            target: Some(target),
            cleanup,
            from_hir_call: false,
            intrinsic: None,
        },
    };
    match site.built {
        Built::Assign(plan) => {
            // bb: ..; StorageLive(raw); raw = box_new_uninit(layout) -> [return: built, unwind]
            // built: typed = raw as *mut T; ..; (*typed).<path> = <rvalue>; ..;
            //        dest = typed as Box<T>; goto target
            let first = plan.iter().map(|it| it.index).min().expect("the argument's own");
            let rest = body.basic_blocks[site.block].statements.split_off(first); // alloc: the block's tail, moved to the new block
            let mut statements = Vec::with_capacity(rest.len() + 8); // alloc: a block's statements, kept in the body
            statements.push(statement(StatementKind::StorageLive(typed), span));
            statements
                .push(statement(StatementKind::Assign(Place::from(typed).store(), to_typed), span));
            statements.push(statement(StatementKind::StorageDead(raw), span));
            let in_box = Place::from(typed).project(ProjectionElem::Deref);
            let at_path = |path: &[FieldIndex]| {
                path.iter()
                    .fold(in_box, |place, &field| place.project(ProjectionElem::Field(field)))
            };
            let placed = |local: LocalId| plan.iter().any(|it| it.local == local);
            for (offset, it) in rest.into_iter().enumerate() {
                let Some(placed_here) = plan.iter().find(|p| p.index == first + offset) else {
                    statements.push(it);
                    continue;
                };
                let StatementKind::Assign(_, rvalue) = it.kind else {
                    unreachable!("`find` placed an assignment in bb{}", block_index(site.block));
                };
                let to = at_path(&placed_here.path);
                match rvalue {
                    Rvalue::Aggregate(_, operands) if placed_here.split => {
                        for (field, operand) in operands.into_vec().into_iter().enumerate() {
                            let is_placed = matches!(&operand.kind, OperandKind::Move(place)
                                if place.as_ref().as_local().is_some_and(placed));
                            if is_placed {
                                continue;
                            }
                            let to =
                                to.project(ProjectionElem::Field(FieldIndex(field as u32))).store();
                            statements.push(statement(
                                StatementKind::Assign(to, Rvalue::Use(operand)),
                                it.span,
                            ));
                        }
                    }
                    rvalue => statements
                        .push(statement(StatementKind::Assign(to.store(), rvalue), it.span)),
                }
            }
            statements.push(statement(StatementKind::Assign(destination, to_box), span));
            statements.push(statement(StatementKind::StorageDead(typed), span));
            let built = body.basic_blocks.alloc(BasicBlock {
                statements,
                terminator: Some(Terminator { span, kind: TerminatorKind::Goto { target } }),
                is_cleanup: false,
            });
            let data = &mut body.basic_blocks[site.block];
            data.statements.push(statement(StatementKind::StorageLive(raw), span));
            data.terminator = Some(allocate(built, box_cleanup));
        }
        Built::Call(pred) => {
            // pred: .. StorageLive(raw); raw = box_new_uninit(layout) -> [return: write, unwind]
            // write: typed = raw as *mut T; (*typed) = f(..) -> [return: bb, unwind: free]
            // free: boxed = typed as Box<MaybeUninit<T>>; drop(boxed) -> unwind
            // bb: ..; dest = typed as Box<T>; goto target
            let Some(Terminator {
                span: call_span,
                kind:
                    TerminatorKind::Call {
                        func,
                        args,
                        target: call_target,
                        cleanup,
                        from_hir_call,
                        intrinsic,
                        ..
                    },
            }) = body.basic_blocks[pred].terminator.take()
            else {
                unreachable!("`find` took the call that ends bb{}", block_index(pred));
            };
            let resume = match cleanup {
                Some(it) => it,
                None => body.basic_blocks.alloc(BasicBlock {
                    statements: Vec::new(), // alloc: empty
                    terminator: Some(Terminator { span, kind: TerminatorKind::UnwindResume }),
                    is_cleanup: true,
                }),
            };
            let free = free_block(ctx, body, typed, value_ty, box_ty, resume, span)?;
            let write = body.basic_blocks.alloc(BasicBlock {
                // alloc: a block's statements, kept in the body
                statements: vec![
                    statement(StatementKind::StorageLive(typed), span),
                    statement(StatementKind::Assign(Place::from(typed).store(), to_typed), span),
                    statement(StatementKind::StorageDead(raw), span),
                ],
                terminator: Some(Terminator {
                    span: call_span,
                    kind: TerminatorKind::Call {
                        func,
                        args,
                        destination: Place::from(typed).project(ProjectionElem::Deref).store(),
                        target: call_target,
                        cleanup: Some(free),
                        from_hir_call,
                        intrinsic,
                    },
                }),
                is_cleanup: false,
            });
            let data = &mut body.basic_blocks[pred];
            data.statements.push(statement(StatementKind::StorageLive(raw), span));
            data.terminator = Some(allocate(write, cleanup));
            let data = &mut body.basic_blocks[site.block];
            data.statements.push(statement(StatementKind::Assign(destination, to_box), span));
            data.statements.push(statement(StatementKind::StorageDead(typed), span));
            data.terminator = Some(Terminator { span, kind: TerminatorKind::Goto { target } });
        }
    }
    Ok(())
}

/// The cleanup block that frees the box `typed` points to when building its value unwinds: the
/// pointer as a `Box<MaybeUninit<T>>` (`box_ty`'s allocator), dropped, which deallocates and
/// drops nothing; then `resume`.
fn free_block<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    typed: LocalId,
    value_ty: Ty<'db>,
    box_ty: Ty<'db>,
    resume: BasicBlockId,
    span: MirSpan,
) -> Result<'db, BasicBlockId> {
    let interner = ctx.infcx.interner;
    let lang = interner.lang_items();
    let TyKind::Adt(adt, args) = box_ty.kind() else {
        return Err(MirLowerError::ImplementationError(format!(
            "BoxInPlace: `alloc::boxed::Box::<T>::new` returns `{box_ty:?}` in {:?}, not a `Box`",
            body.owner
        )));
    };
    let Some(maybe_uninit) = lang.MaybeUninit else {
        return Err(MirLowerError::ImplementationError(format!(
            "BoxInPlace: no `maybe_uninit` lang item, which freeing the box of an unwinding \
             `Box::new` argument in {:?} needs",
            body.owner
        )));
    };
    let uninit = Ty::new_adt(
        interner,
        AdtId::UnionId(maybe_uninit),
        GenericArgs::new_from_slice(&[GenericArg::from(value_ty)]),
    );
    let mut box_args: SmallVec<[GenericArg<'db>; 2]> = SmallVec::new();
    box_args.push(GenericArg::from(uninit));
    box_args.extend(args.as_slice().iter().skip(1).copied());
    let uninit_box = Ty::new_adt(interner, adt.def_id(), GenericArgs::new_from_slice(&box_args));
    let boxed = body.locals.alloc(Local { ty: uninit_box.store() });
    let to_box = Rvalue::Cast(CastKind::Transmute, move_of(typed), uninit_box.store());
    Ok(body.basic_blocks.alloc(BasicBlock {
        statements: vec![statement(
            StatementKind::Assign(Place::from(boxed).store(), to_box),
            span,
        )], // alloc: a block's statements, kept in the body
        terminator: Some(Terminator {
            span,
            kind: TerminatorKind::Drop {
                place: Place::from(boxed).store(),
                target: resume,
                unwind: None,
            },
        }),
        is_cleanup: true,
    }))
}
