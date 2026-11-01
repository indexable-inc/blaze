//! The body surgery of rustc's `inline_call` and its `Integrator`: the callee's locals and
//! blocks appended to the caller's, its parameters the call's arguments, its return place a
//! local the caller copies out of, its unwinding the call's, its spans in a scope of its own.

use std::ops::Range;

use blaze_hash::HashMap;
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{IntoKind, Ty as _};
use smallvec::SmallVec;

use crate::{
    mir::{
        BasicBlock, BasicBlockId, BorrowKind, FieldIndex, InlinedBinding, InlinedPromoted,
        InlinedScope, InlinedSpan, Local, LocalId, MirBody, MirSpan, MutBorrowKind, Operand,
        OperandKind, Place, ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace,
        Terminator, TerminatorKind, return_slot, transform::Ctx,
    },
    next_solver::{Region, RegionExt, Ty, TyKind},
};

use super::super::{block_id, block_index, local_index, map_place, successors_mut};
use super::operand_ty;

/// What [`inline_call`] needs of the call besides the bodies.
pub(super) struct Site<'a> {
    pub(super) block: BasicBlockId,
    /// The call passes a closure's arguments as a tuple, which the closure's body takes one
    /// by one.
    pub(super) spread: bool,
    /// The callee's index in the caller's [`Inlined::callees`](crate::mir::Inlined::callees).
    pub(super) callee: u32,
    /// For each of the callee's own inlined callees, its index in the caller's.
    pub(super) nested: &'a [u32],
}

/// The temporaries of the call: each live from the call on and dead where it returns.
struct Temps {
    span: MirSpan,
    /// Statements before the jump into the callee.
    before: SmallVec<[Statement; 8]>,
    /// The temporaries, in the order they were made.
    made: SmallVec<[LocalId; 8]>,
}

impl Temps {
    fn new_temp<'db>(&mut self, body: &mut MirBody<'db>, ty: Ty<'db>) -> LocalId {
        let local = body.locals.alloc(Local { ty: ty.store() });
        self.before.push(Statement { kind: StatementKind::StorageLive(local), span: self.span });
        self.made.push(local);
        local
    }

    fn assign(&mut self, place: StoredPlace, rvalue: Rvalue) {
        self.before.push(Statement { kind: StatementKind::Assign(place, rvalue), span: self.span });
    }

    /// rustc's `create_temp_if_necessary`: a temporary the argument is moved out of is the
    /// parameter itself.
    fn argument<'db>(
        &mut self,
        ctx: &Ctx<'_, 'db>,
        body: &mut MirBody<'db>,
        arg: Operand,
    ) -> LocalId {
        if let OperandKind::Move(place) = &arg.kind
            && place.projection.is_empty()
            && place.local != return_slot()
            && !body.param_locals.contains(&place.local)
        {
            return place.local;
        }
        let ty = operand_ty(ctx, body, &arg);
        let local = self.new_temp(body, ty);
        self.assign(Place::from(local).store(), Rvalue::Use(arg));
        local
    }
}

/// The spans of the callee as [`MirSpan::Inlined`] spans of the caller.
struct Spans<'a> {
    /// The scope of this call.
    scope: u32,
    /// Where the callee's own inlined spans start among the caller's.
    base: u32,
    own: HashMap<MirSpan, u32>,
    spans: &'a mut Vec<InlinedSpan>,
}

impl Spans<'_> {
    fn map(&mut self, span: MirSpan) -> MirSpan {
        if let MirSpan::Inlined(at) = span {
            return MirSpan::Inlined(self.base + at);
        }
        let next = self.spans.len() as u32;
        let at = *self.own.entry(span).or_insert(next);
        if at == next {
            self.spans.push(InlinedSpan { scope: self.scope, span });
        }
        MirSpan::Inlined(at)
    }

    fn operand(&mut self, operand: &mut Operand) {
        if let Some(span) = &mut operand.span {
            *span = self.map(*span);
        }
    }
}

/// The call that ends `site.block` replaced by `callee`, the callee's body instantiated for
/// the call, whose parameters the caller checked the arguments against. Says which blocks
/// are the callee's now.
pub(super) fn inline_call<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    site: Site<'_>,
    mut callee: MirBody<'db>,
) -> Range<usize> {
    let interner = ctx.infcx.interner;
    let (span, args, destination, target, cleanup) =
        match body.basic_blocks[site.block].terminator.take() {
            Some(Terminator {
                span,
                kind: TerminatorKind::Call { args, destination, target, cleanup, .. },
            }) => (span, args, destination, target, cleanup),
            // The inliner took the call from this terminator; rustc's `inline_call` has a
            // `bug!` here. The body stays as it was, the call a call.
            other => {
                body.basic_blocks[site.block].terminator = other;
                return 0..0;
            }
        };
    let return_block = target.map(|target| {
        let is_cleanup = body.basic_blocks[target].is_cleanup;
        body.basic_blocks.alloc(BasicBlock {
            statements: Vec::new(), // alloc: empty; reserved to its final size once below
            terminator: Some(Terminator { span, kind: TerminatorKind::Goto { target } }),
            is_cleanup,
        })
    });
    let mut temps = Temps { span, before: SmallVec::new(), made: SmallVec::new() };

    // `a[*i] = f(i)`, where `f` may change `i`: which place it is is decided before the call.
    let needs_borrow = destination
        .projection
        .as_slice()
        .iter()
        .any(|it| matches!(it, ProjectionElem::Deref | ProjectionElem::Index(_)));
    let destination_ty = destination.as_ref().ty(body, &ctx.infcx, ctx.env).ty;
    let dest = if needs_borrow {
        let ty =
            Ty::new_ref(interner, Region::new_erased(interner), destination_ty, Mutability::Mut);
        let temp = temps.new_temp(body, ty);
        let borrow = Rvalue::Ref(BorrowKind::Mut { kind: MutBorrowKind::Default }, destination);
        temps.assign(Place::from(temp).store(), borrow);
        Place::from(temp).project(ProjectionElem::Deref).store()
    } else {
        destination
    };

    let mut params: Vec<LocalId> = Vec::with_capacity(callee.param_locals.len());
    let mut args = args.into_vec();
    if site.spread
        && let Some(tuple) = args.pop()
    {
        for receiver in args {
            params.push(temps.argument(ctx, body, receiver));
        }
        let tuple = temps.argument(ctx, body, tuple);
        if let TyKind::Tuple(fields) = body.locals[tuple].ty.as_ref().kind() {
            for index in 0..fields.len() {
                let field =
                    Place::from(tuple).project(ProjectionElem::Field(FieldIndex(index as u32)));
                let field = Operand { kind: OperandKind::Move(field.store()), span: None };
                params.push(temps.argument(ctx, body, field));
            }
        }
    } else {
        for arg in args {
            params.push(temps.argument(ctx, body, arg));
        }
    }
    // `_0` is a place with projections in the callee, so it is a local of the caller: the
    // destination where that is one no parameter is, else a new one copied into it.
    let direct = dest.projection.is_empty() && !params.contains(&dest.local);
    let destination_local = if direct { dest.local } else { temps.new_temp(body, destination_ty) };

    // `_0` is the destination, a parameter its argument, any other local a new one of the
    // caller; a new one the callee never marks live or dead is live for the whole call.
    let mut marked: SmallVec<[bool; 64]> = SmallVec::from_elem(false, callee.locals.len());
    for (_, block) in callee.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::StorageLive(local) | StatementKind::StorageDead(local) =
                statement.kind
            {
                marked[local_index(local)] = true;
            }
        }
    }
    let first_new = body.locals.len();
    let mut map: Vec<LocalId> = Vec::with_capacity(callee.locals.len());
    let mut always_live: SmallVec<[LocalId; 8]> = SmallVec::new();
    for (local, decl) in std::mem::take(&mut callee.locals) {
        let mapped = if local == return_slot() {
            destination_local
        } else if let Some(at) = callee.param_locals.iter().position(|it| *it == local) {
            params[at]
        } else {
            let new = body.locals.alloc(decl);
            if !marked[local_index(local)] {
                always_live.push(new);
            }
            new
        };
        map.push(mapped);
    }
    let lookup = |local: LocalId| map[local_index(local)];
    for &local in &always_live {
        temps.before.push(Statement { kind: StatementKind::StorageLive(local), span });
    }

    // The scope of this call, then the callee's own scopes and spans.
    let scope = body.inlined.scopes.len() as u32;
    body.inlined.scopes.push(InlinedScope { callee: site.callee, call_site: span });
    let scope_base = scope + 1;
    let base = body.inlined.spans.len() as u32;
    body.inlined.spans.extend(
        callee
            .inlined
            .spans
            .iter()
            .map(|it| InlinedSpan { scope: scope_base + it.scope, span: it.span }),
    );
    let mut spans = Spans { scope, base, own: HashMap::default(), spans: &mut body.inlined.spans };
    body.inlined.scopes.extend(callee.inlined.scopes.iter().map(|it| InlinedScope {
        callee: site.nested[it.callee as usize],
        call_site: spans.map(it.call_site),
    }));

    let offset = body.basic_blocks.len();
    let map_block = |block: BasicBlockId| block_id(offset + block_index(block));
    callee.walk_places(|place| map_place(place, &mut |local| lookup(local)));
    for (_, block) in callee.basic_blocks.iter_mut() {
        let is_cleanup = block.is_cleanup;
        for statement in &mut block.statements {
            statement.span = spans.map(statement.span);
            statement.kind.for_each_operand_mut(&mut |it| spans.operand(it));
            if let StatementKind::StorageLive(local) | StatementKind::StorageDead(local) =
                &mut statement.kind
            {
                *local = lookup(*local);
            }
        }
        let Some(terminator) = &mut block.terminator else { continue };
        terminator.span = spans.map(terminator.span);
        terminator.kind.for_each_operand_mut(&mut |it| spans.operand(it));
        successors_mut(&mut terminator.kind, |it| *it = map_block(*it));
        let replaced = match &terminator.kind {
            TerminatorKind::Return => Some(match return_block {
                Some(target) => TerminatorKind::Goto { target },
                None => TerminatorKind::Unreachable,
            }),
            TerminatorKind::UnwindResume => cleanup.map(|target| TerminatorKind::Goto { target }),
            _ => None,
        };
        if let Some(kind) = replaced {
            terminator.kind = kind;
        } else if !is_cleanup
            && let Some(unwind @ None) = terminator.kind.unwind_mut()
        {
            // Unwinding out of the callee is unwinding out of the call. In a cleanup block no
            // unwind edge is one that terminates, and stays one.
            *unwind = cleanup;
        }
    }
    let callee_start = map_block(callee.start_block);

    let own_bindings = callee.binding_locals.iter().map(|(binding, &local)| InlinedBinding {
        scope,
        binding,
        local: lookup(local),
    });
    let nested_bindings = callee.inlined.bindings.iter().map(|it| InlinedBinding {
        scope: scope_base + it.scope,
        binding: it.binding,
        local: lookup(it.local),
    });
    body.inlined.bindings.extend(own_bindings.chain(nested_bindings));
    // A promoted constant is of the expression store of the body it was written in, and is
    // instantiated with that body's arguments: those of its callee.
    let own_promoted = callee.promoted_locals.iter().map(|&(local, expr)| InlinedPromoted {
        local: lookup(local),
        callee: site.callee,
        expr,
    });
    let nested_promoted = callee.inlined.promoted.iter().map(|it| InlinedPromoted {
        local: lookup(it.local),
        callee: site.nested[it.callee as usize],
        expr: it.expr,
    });
    body.inlined.promoted.extend(own_promoted.chain(nested_promoted));

    if let Some(block) = return_block {
        let statements = &mut body.basic_blocks[block].statements;
        statements.reserve(1 + always_live.len() + temps.made.len());
        if !direct {
            let moved = Operand {
                kind: OperandKind::Move(Place::from(destination_local).store()),
                span: None,
            };
            statements
                .push(Statement { kind: StatementKind::Assign(dest, Rvalue::Use(moved)), span });
        }
        for &local in always_live.iter().rev().chain(temps.made.iter().rev()) {
            statements.push(Statement { kind: StatementKind::StorageDead(local), span });
        }
    }
    let caller_block = &mut body.basic_blocks[site.block];
    caller_block.statements.extend(temps.before.drain(..));
    caller_block.terminator =
        Some(Terminator { span, kind: TerminatorKind::Goto { target: callee_start } });

    body.promoted_constants.extend(callee.promoted_constants.iter().map(|&it| lookup(it)));
    body.unreached.extend(callee.unreached.iter().map(|&it| map_block(it)));
    body.panic_unwinds.extend(
        callee.panic_unwinds.iter().map(|&(block, at, to)| (map_block(block), at, map_block(to))),
    );
    // The callee's scopes end before any of the caller's that is open at the call.
    let open = body.scope_ends.len();
    body.scope_ends.extend(
        callee.scope_ends.iter().map(|&it| lookup(it)).filter(|it| local_index(*it) >= first_new),
    );
    let ended = body.scope_ends.len() - open;
    body.scope_ends.rotate_right(ended);

    for (_, block) in std::mem::take(&mut callee.basic_blocks) {
        body.basic_blocks.alloc(block);
    }
    offset..body.basic_blocks.len()
}
