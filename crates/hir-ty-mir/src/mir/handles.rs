//! `#![blaze::handles]` (docs/v2/plan/design/edition.md, change 1): a handle is a value of a
//! type that implements nightly's `core::clone::Share` (the `blaze_handle` lang item; `Arc`,
//! `Rc`, `Sender`, `SyncSender` and any type a user implements it for), and a move of a whole
//! local holding one that is used again on some later path is a copy, `Clone::clone(&x)`, made
//! right before the move. The last use on every path stays a move. A closure that owns its
//! captures moves each one into itself, so it takes a copy of a handle that is used after it,
//! and a move in a loop, which the next turn reaches, copies. There is no mark at the use.
//!
//! It runs on the built body, before borrow checking, so the borrow checker sees the copies and
//! every other move is checked as before: a `Vec` used after a move is still E0382. On a
//! program rustc accepts a whole local is never live after it is moved, so the pass changes
//! nothing there; without the attribute it does not run.
//!
//! "Used again" is backward liveness ([`super::optimize::live_on_entry`], dead store
//! elimination's), where a drop is not a use: built MIR drops every variable at the end of its
//! scope, moved or not. A move out of a part of a local (`x.0`) is not made a copy: whether
//! the part is used again is not what local liveness says.
//!
//! `#[blaze::norefcount]` on a function makes each such copy in its body, and in the closures
//! and `async` blocks in it, an error ([`norefcount_errors`], which the borrow checker reports).

use base_db::Db;
use blaze_mem::{Frame, Tag, frame};
use hir_def::{
    CallableDefId, FunctionId, HasModule, TraitId, expr_store::ExpressionStore,
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use intern::sym;
use rustc_type_ir::inherent::Ty as _;

use crate::{
    display::{DisplayTarget, HirDisplay},
    next_solver::{DbInterner, GenericArgs, Region, RegionExt, Ty, infer::select::EvaluationResult},
};

use super::{
    BasicBlock, BasicBlockId, BorrowKind, Local, LocalId, MirBody, MirLowerError, MirSpan,
    Mutability, Operand, OperandKind, Place, Rvalue, Statement, StatementKind, Terminator,
    TerminatorKind,
    framed::{self, Bits, bits_in, block_index, local_index},
    optimize::{
        Location, Mention, exit_into, live_on_entry, reverse_postorder, transfer, visit_operand,
        visit_place, visit_statement, visit_terminator,
    },
    transform::Ctx,
};

type Result<'db, T> = std::result::Result<T, MirLowerError<'db>>;

/// The mentions of one statement or terminator, reused for each.
type Mentions<'f> = Vec<(LocalId, Mention), &'f Frame>;

/// A move the pass makes a copy: of the whole local `local`, the `operand`th operand, in
/// `for_each_operand` order, of statement `index` of `block`, or of its terminator when `index`
/// is its number of statements.
#[derive(Debug, Clone, Copy)]
struct Site {
    block: BasicBlockId,
    index: usize,
    operand: usize,
    local: LocalId,
}

/// Makes each move of a handle that is used again a copy, when the body's crate says
/// `#![blaze::handles]`; any other body is left as it is.
pub(super) fn copy_handles<'db>(db: &'db Db, body: &mut MirBody<'db>) -> Result<'db, ()> {
    let krate = body.owner.krate(db);
    if !hir_def::nameres::crate_def_map(db, krate).handles() {
        return Ok(());
    }
    let lang = DbInterner::new_with(db, krate).lang_items();
    let clone_fn = lang
        .Clone
        .and_then(|it| it.trait_items(db).method_by_name(&Name::new_symbol_root(sym::clone)));
    let (Some(handle), Some(clone_fn)) = (lang.Handle, clone_fn) else {
        // alloc: error path, the crate's name
        let name = krate.extra_data(db).display_name.as_ref().map(|it| it.to_string());
        // alloc: error path
        return Err(MirLowerError::NotSupported(format!(
            "`#![blaze::handles]` in crate `{}`: its `core` has no `#[lang = \"blaze_handle\"]` \
             trait (nightly's `core::clone::Share`) or no `Clone::clone`, which say what a \
             handle is and how one is copied",
            name.unwrap_or_default()
        )));
    };
    frame::task(Tag::Mir, |frame| {
        let ctx = Ctx::new(db, body.owner, frame);
        let sites = sites(&ctx, body, handle, frame);
        if !sites.is_empty() {
            rewrite(&ctx, body, &sites, clone_fn, frame);
        }
    });
    Ok(())
}

/// Liveness across a statement, backwards: what it writes whole, then what it reads.
fn statement_effect(
    live: &mut Bits<'_>,
    statement: &StatementKind,
    at: Location,
    mentions: &mut Mentions<'_>,
) {
    mentions.clear();
    visit_statement(statement, at, &mut |local, mention, _| mentions.push((local, mention)));
    for &(local, mention) in mentions.iter() {
        if mention == Mention::Assigned {
            transfer(live, local, mention);
        }
    }
    for &(local, mention) in mentions.iter() {
        if mention != Mention::Assigned {
            transfer(live, local, mention);
        }
    }
}

/// Liveness across a terminator, backwards. A drop uses nothing: the value is gone either way.
/// A drop and replace of a whole local writes it.
fn terminator_effect(
    live: &mut Bits<'_>,
    kind: &TerminatorKind,
    at: Location,
    mentions: &mut Mentions<'_>,
) {
    match kind {
        TerminatorKind::Drop { .. } => (),
        TerminatorKind::DropAndReplace { place, value, .. } => {
            if place.projection.as_slice().is_empty() {
                live.remove(local_index(place.local));
            } else {
                visit_place(place, Mention::Mutated, at, &mut |local, mention, _| {
                    transfer(live, local, mention)
                });
            }
            visit_operand(value, at, &mut |local, mention, _| transfer(live, local, mention));
        }
        _ => {
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
    }
}

/// Of one operand: whether it moves a whole local, and the local its place is in.
fn operand_facts(operand: &Operand) -> (bool, Option<LocalId>) {
    match &operand.kind {
        OperandKind::Move(place) => (place.projection.as_slice().is_empty(), Some(place.local)),
        OperandKind::Copy(place) => (false, Some(place.local)),
        _ => (false, None),
    }
}

/// Whether `local` holds a handle: a type that implements the handle trait and is not `Copy`
/// (a `&T` is `Share` and `Copy`, and is never copied by this pass).
fn is_handle<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    handle: TraitId,
    local: LocalId,
    known: &mut [Option<bool>],
) -> bool {
    let slot = &mut known[local_index(local)];
    if let Some(it) = *slot {
        return it;
    }
    let ty = ctx.normalize(body.locals[local].ty.as_ref());
    let it = !ctx.infcx.type_is_copy_modulo_regions(ctx.env, ty)
        && matches!(
            ctx.infcx.type_implements_trait(handle, [ty], ctx.env),
            EvaluationResult::EvaluatedToOk | EvaluationResult::EvaluatedToOkModuloRegions
        );
    *slot = Some(it);
    it
}

/// The moves of a handle that is used again: live after the statement or terminator that moves
/// it (other than as what it writes whole), or read by a later operand of it. Ordered by block,
/// position and operand.
fn sites<'f, 'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    handle: TraitId,
    frame: &'f Frame,
) -> Vec<Site, &'f Frame> {
    let locals = body.locals.len();
    let order = reverse_postorder(body, frame);
    let entry = {
        let mut statement_mentions: Mentions<'_> = Vec::new_in(frame);
        let mut terminator_mentions: Mentions<'_> = Vec::new_in(frame);
        live_on_entry(
            body,
            &order,
            frame,
            |live, statement, at| statement_effect(live, statement, at, &mut statement_mentions),
            |live, kind, at| terminator_effect(live, kind, at, &mut terminator_mentions),
        )
    };
    let mut known: Vec<Option<bool>, &Frame> = Vec::with_capacity_in(locals, frame);
    known.resize(locals, None);
    let mut live: Bits<'_> = bits_in(locals, frame);
    let mut mentions: Mentions<'_> = Vec::new_in(frame);
    let mut operands: Vec<(bool, Option<LocalId>), &Frame> = Vec::new_in(frame);
    let mut sites = Vec::new_in(frame);
    for &bb in order.iter() {
        let block = &body.basic_blocks[bb];
        if block.is_cleanup || body.unreached.contains(&bb) {
            continue;
        }
        exit_into(&entry, block, &mut live);
        if let Some(terminator) = &block.terminator {
            let at = Location { block: bb, index: block.statements.len() };
            let written = match &terminator.kind {
                TerminatorKind::Call { destination, .. }
                    if destination.projection.as_slice().is_empty() =>
                {
                    Some(destination.local)
                }
                _ => None,
            };
            operands.clear();
            terminator.kind.for_each_operand(&mut |it| operands.push(operand_facts(it)));
            let point = Point { ctx, body, handle, live: &live, written, at };
            point.push_sites(&operands, &mut known, &mut sites);
            terminator_effect(&mut live, &terminator.kind, at, &mut mentions);
        }
        for (index, statement) in block.statements.iter().enumerate().rev() {
            let at = Location { block: bb, index };
            let written = match &statement.kind {
                StatementKind::Assign(place, _) if place.projection.as_slice().is_empty() => {
                    Some(place.local)
                }
                _ => None,
            };
            operands.clear();
            statement.kind.for_each_operand(&mut |it| operands.push(operand_facts(it)));
            let point = Point { ctx, body, handle, live: &live, written, at };
            point.push_sites(&operands, &mut known, &mut sites);
            statement_effect(&mut live, &statement.kind, at, &mut mentions);
        }
    }
    // Each site is one operand of one point, so the order is total.
    sites.sort_unstable_by_key(|it: &Site| (block_index(it.block), it.index, it.operand));
    sites
}

/// One statement or terminator, with what is live after it.
struct Point<'a, 'c, 'l, 'db> {
    ctx: &'a Ctx<'c, 'db>,
    body: &'a MirBody<'db>,
    handle: TraitId,
    live: &'a Bits<'l>,
    /// The local it writes whole, whose liveness after it is of the new value.
    written: Option<LocalId>,
    at: Location,
}

impl Point<'_, '_, '_, '_> {
    fn push_sites(
        &self,
        operands: &[(bool, Option<LocalId>)],
        known: &mut [Option<bool>],
        sites: &mut Vec<Site, &Frame>,
    ) {
        for (operand, &(whole, local)) in operands.iter().enumerate() {
            let Some(local) = local.filter(|_| whole) else { continue };
            let live_after =
                self.written != Some(local) && self.live.contains(local_index(local));
            let read_later = operands[operand + 1..].iter().any(|&(_, it)| it == Some(local));
            if (live_after || read_later)
                && is_handle(self.ctx, self.body, self.handle, local, known)
            {
                sites.push(Site { block: self.at.block, index: self.at.index, operand, local });
            }
        }
    }
}

/// Puts `Clone::clone(&x)` before each site, into a local the site's operand then moves.
fn rewrite<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    sites: &[Site],
    clone_fn: FunctionId,
    frame: &Frame,
) {
    // Where a statement that may panic unwinds, by its position: the copies before it unwind
    // there too, and the positions move as blocks split.
    let mut unwinds = framed::map_in(frame);
    unwinds.extend(body.panic_unwinds.iter().map(|&(block, at, unwind)| ((block, at), unwind)));
    let mut moved_to = framed::map_in(frame);
    let mut pending: Vec<Statement, &Frame> = Vec::new_in(frame);
    let mut rest = sites;
    while let Some(first) = rest.first() {
        let id = first.block;
        let count = rest.iter().take_while(|it| it.block == id).count();
        let (mut here, after) = rest.split_at(count);
        rest = after;
        pending.clear();
        pending.extend(body.basic_blocks[id].statements.drain(..));
        let terminator = body.basic_blocks[id].terminator.take();
        let terminator_unwind = terminator.as_ref().and_then(|it| it.kind.unwind());
        let total = pending.len();
        let mut current = id;
        for (at, mut statement) in pending.drain(..).enumerate() {
            let unwind = unwinds.get(&(id, at)).copied().or(terminator_unwind);
            while let Some(&site) = here.first().filter(|it| it.index == at) {
                here = &here[1..];
                let copy = new_copy(body, site.local);
                let span = replace_operand(site.operand, copy, &mut |f| {
                    statement.kind.for_each_operand_mut(&mut |it| f(it))
                })
                .unwrap_or(statement.span);
                let after = total - at;
                current = copy_before(
                    ctx, body, current, site.local, copy, span, unwind, clone_fn, after,
                );
            }
            let to = (current, body.basic_blocks[current].statements.len());
            if to != (id, at) {
                moved_to.insert((id, at), to);
            }
            body.basic_blocks[current].statements.push(statement);
        }
        if let Some(mut terminator) = terminator {
            for &site in here {
                let copy = new_copy(body, site.local);
                let span = replace_operand(site.operand, copy, &mut |f| {
                    terminator.kind.for_each_operand_mut(&mut |it| f(it))
                })
                .unwrap_or(terminator.span);
                let unwind = terminator_unwind;
                current =
                    copy_before(ctx, body, current, site.local, copy, span, unwind, clone_fn, 0);
            }
            body.basic_blocks[current].terminator = Some(terminator);
        }
    }
    for entry in body.panic_unwinds.iter_mut() {
        if let Some(&(block, at)) = moved_to.get(&(entry.0, entry.1)) {
            entry.0 = block;
            entry.1 = at;
        }
    }
}

/// A new local of the type of `handle`, for its copy.
fn new_copy(body: &mut MirBody<'_>, handle: LocalId) -> LocalId {
    let ty = body.locals[handle].ty.as_ref().store();
    body.locals.alloc(Local { ty })
}

/// Makes the `operand`th operand a move of `copy`, and returns the span it was written at.
fn replace_operand(
    operand: usize,
    copy: LocalId,
    for_each: &mut dyn FnMut(&mut dyn FnMut(&mut Operand)),
) -> Option<MirSpan> {
    let mut seen = 0;
    let mut span = None;
    for_each(&mut |it: &mut Operand| {
        if seen == operand {
            span = it.span;
            it.kind = OperandKind::Move(Place::from(copy).store());
        }
        seen += 1;
    });
    span
}

/// Ends `block` in `copy = Clone::clone(&handle)` and returns the block it goes on to, made
/// with room for the `after` statements that follow.
#[allow(clippy::too_many_arguments, reason = "one call site per kind of point")]
fn copy_before<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    block: BasicBlockId,
    handle: LocalId,
    copy: LocalId,
    span: MirSpan,
    unwind: Option<BasicBlockId>,
    clone_fn: FunctionId,
    after: usize,
) -> BasicBlockId {
    let interner = ctx.infcx.interner;
    let ty: Ty<'db> = body.locals[handle].ty.as_ref();
    let reference_ty = Ty::new_ref(interner, Region::new_erased(interner), ty, Mutability::Not);
    let reference = body.locals.alloc(Local { ty: reference_ty.store() });
    let statements = &mut body.basic_blocks[block].statements;
    statements.push(StatementKind::StorageLive(reference).with_span(span));
    statements.push(
        StatementKind::Assign(
            Place::from(reference).store(),
            Rvalue::Ref(BorrowKind::Shared, Place::from(handle).store()),
        )
        .with_span(span),
    );
    statements.push(StatementKind::StorageLive(copy).with_span(span));
    // alloc: the statements of a kept block, at its size
    let mut next_statements = Vec::with_capacity(after + 1);
    next_statements.push(StatementKind::StorageDead(reference).with_span(span));
    let next = body.basic_blocks.alloc(BasicBlock {
        statements: next_statements,
        terminator: None,
        is_cleanup: false,
    });
    let func = Operand::from_fn(ctx.db, clone_fn, GenericArgs::new_from_slice(&[ty.into()]));
    body.basic_blocks[block].terminator = Some(Terminator {
        span,
        kind: TerminatorKind::Call {
            func,
            // alloc: the call's one argument, in the kept body
            args: Box::new([Operand {
                kind: OperandKind::Move(Place::from(reference).store()),
                span: None,
            }]),
            destination: Place::from(copy).store(),
            target: Some(next),
            cleanup: unwind,
            // What marks a copy this pass made: lowering makes every call it lowers from the
            // source `from_hir_call`.
            from_hir_call: false,
            intrinsic: None,
        },
    });
    next
}

/// A copy of a handle `#![blaze::handles]` made in a `#[blaze::norefcount]` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpliedCopy {
    /// The move that became the copy, as it is written.
    pub span: MirSpan,
    /// The error, naming the function, the handle, its type and the rule.
    pub message: String,
}

impl ImpliedCopy {
    /// The code the error is reported with.
    pub const CODE: &'static str = "NOREFCOUNT";
}

/// The copies [`copy_handles`] made in `body`, a body of `function` or of a closure or `async`
/// block in it, each an error because `function` is `#[blaze::norefcount]`, in block order.
pub fn norefcount_errors<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    function: FunctionId,
    out: &mut Vec<ImpliedCopy>,
) {
    let krate = body.owner.krate(db);
    if !hir_def::nameres::crate_def_map(db, krate).handles() {
        return;
    }
    let lang = DbInterner::new_with(db, krate).lang_items();
    let Some(clone_fn) = lang
        .Clone
        .and_then(|it| it.trait_items(db).method_by_name(&Name::new_symbol_root(sym::clone)))
    else {
        return;
    };
    for (_, block) in body.basic_blocks.iter() {
        let Some(Terminator {
            span,
            kind: TerminatorKind::Call { func, args, from_hir_call: false, .. },
        }) = &block.terminator
        else {
            continue;
        };
        if !matches!(func.fn_def(), Some((CallableDefId::FunctionId(it), _)) if it == clone_fn) {
            continue;
        }
        let [Operand { kind: OperandKind::Move(reference), .. }] = &args[..] else { continue };
        let handle = block.statements.iter().rev().find_map(|it| match &it.kind {
            StatementKind::Assign(place, Rvalue::Ref(BorrowKind::Shared, handle))
                if place.local == reference.local && place.projection.as_slice().is_empty() =>
            {
                Some(handle.local)
            }
            _ => None,
        });
        let Some(handle) = handle else { continue };
        let name = body
            .binding_locals
            .iter()
            .find(|&(_, &it)| it == handle)
            .map(|(binding, _)| {
                let owner = body.owner.expression_store_owner(db);
                ExpressionStore::of(db, owner)[binding].name.as_str().to_owned() // alloc: error path
            })
            .unwrap_or_else(|| "a temporary".to_owned()); // alloc: error path
        // alloc: error path
        let ty = body.locals[handle]
            .ty
            .as_ref()
            .display(db, DisplayTarget::from_crate(db, krate))
            .to_string();
        // alloc: error path
        let function = FunctionSignature::of(db, function).name.as_str().to_owned();
        out.push(ImpliedCopy {
            span: *span,
            // alloc: error path
            message: format!(
                "`{function}` is `#[blaze::norefcount]`, but `{name}`, a `{ty}` handle, is used \
                 again after this move, so `#![blaze::handles]` would copy it here: make this its \
                 last use, or clone it where it is written"
            ),
        });
    }
}
