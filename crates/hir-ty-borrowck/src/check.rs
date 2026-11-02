//! The walk over a body that reports errors, as `MirBorrowckCtxt` in rustc's
//! `rustc_borrowck/src/lib.rs`: every access to a place is checked for being initialized,
//! for being allowed to mutate what it mutates and, with the loans in scope, for conflicting
//! with a loan.

use std::fmt;

use hir_ty::db::HirDatabase;
use hir_def::hir::{BindingAnnotation, BindingId, ExprId, Pat, Statement};
use rustc_type_ir::{
    ClauseKind, OutlivesClause, RegionKind,
    inherent::{GenericArgs as _, IntoKind},
};

use hir_ty::{
    mir::{
        AsmOperand, BasicBlockId, BinOp, BorrowKind, LocalId, MirBody, MirSpan, MutBorrowKind,
        Operand, OperandKind, PlaceElem, ProjectionElem, Rvalue, StatementKind, StoredPlace,
        TerminatorKind,
    },
    next_solver::{GenericArgKind, Region, Ty, TyKind},
};

use super::{
    Applicability, BorrowckDiagnostic, Child, Ctx, Mode, PartSpan, Sublevel, Suggestion,
    bitset::{BitSet, bits},
    cfg::{Cfg, Dominators, Location, index},
    intervals::IntervalSet,
    liveness,
    loans::{Activation, Bias, Depth, Loan, Loans, places_conflict},
    mem::{self, FMap, FSet, FVec, Frame},
    moves::{InitFlow, InitKind, Lookup, MoveData, Mpi, apply},
    decode::Decoded,
    names::Text,
    polonius,
    regions::{self, Category, RegionValues, Regions, UniversalKind},
};

/// `format!` into the body's frame: `text!(it, "..", ..)` of a [`Checker`] or a [`Draft`].
macro_rules! text {
    ($in:expr, $($arg:tt)*) => {
        $in.text(format_args!($($arg)*))
    };
}

/// A diagnostic while it is built: its texts and lists in the body's frame, so building it,
/// relabelling it, and dropping one a later error replaces take nothing from the global heap.
/// [`Draft::owned`] makes the [`BorrowckDiagnostic`] the result keeps.
pub(super) struct Draft<'f> {
    frame: &'f Frame,
    pub(super) code: &'static str,
    pub(super) span: MirSpan,
    pub(super) message: &'f str,
    pub(super) at_signature: bool,
    pub(super) labels: FVec<'f, (PartSpan, &'f str)>,
    pub(super) children: FVec<'f, DraftChild<'f>>,
    suggestions: FVec<'f, DraftSuggestion<'f>>,
}

/// A [`Child`] while drafted.
pub(super) struct DraftChild<'f> {
    level: Sublevel,
    pub(super) message: &'f str,
    spans: &'f [PartSpan],
    labels: &'f [(PartSpan, &'f str)],
}

/// A [`Suggestion`] while drafted.
struct DraftSuggestion<'f> {
    message: &'f str,
    parts: &'f [(PartSpan, &'f str)],
    applicability: Applicability,
    verbose: bool,
}

impl<'f> Draft<'f> {
    pub(super) fn new(
        frame: &'f Frame,
        code: &'static str,
        span: MirSpan,
        message: &'f str,
    ) -> Self {
        Draft {
            frame,
            code,
            span,
            message,
            at_signature: false,
            labels: Vec::new_in(frame),
            children: Vec::new_in(frame),
            suggestions: Vec::new_in(frame),
        }
    }

    pub(super) fn text(&self, args: fmt::Arguments<'_>) -> &'f str {
        Text::format(self.frame, args)
    }

    pub(super) fn label(&mut self, span: PartSpan, text: &'f str) {
        self.labels.push((span, text));
    }

    pub(super) fn note(&mut self, message: &'f str) {
        self.child(Sublevel::Note, message, &[], &[]);
    }

    pub(super) fn child(
        &mut self,
        level: Sublevel,
        message: &'f str,
        spans: &[PartSpan],
        labels: &[(PartSpan, &'f str)],
    ) {
        let (spans, labels) =
            (self.frame.alloc_slice_copy(spans), self.frame.alloc_slice_copy(labels));
        self.children.push(DraftChild { level, message, spans, labels });
    }

    pub(super) fn suggest(
        &mut self,
        message: &'f str,
        parts: &[(PartSpan, &'f str)],
        applicability: Applicability,
        verbose: bool,
    ) {
        let parts = self.frame.alloc_slice_copy(parts);
        self.suggestions.push(DraftSuggestion { message, parts, applicability, verbose });
    }

    /// The diagnostic the result keeps: each text and each list copied once at its length,
    /// one allocation each and none for an empty one (T1's `owned_blocks`).
    pub(super) fn owned(&self) -> BorrowckDiagnostic {
        fn labels(labels: &[(PartSpan, &str)]) -> Vec<(PartSpan, String)> {
            labels
                .iter()
                .map(|&(span, text)| (span, String::from(text))) // alloc: the owned diagnostic
                .collect() // alloc: the owned diagnostic
        }
        BorrowckDiagnostic {
            code: self.code,
            span: self.span,
            message: String::from(self.message), // alloc: the owned diagnostic
            at_signature: self.at_signature,
            labels: labels(&self.labels),
            children: self
                .children
                .iter()
                .map(|it| Child {
                    level: it.level,
                    message: String::from(it.message), // alloc: the owned diagnostic
                    spans: it.spans.to_vec(), // alloc: the owned diagnostic
                    labels: labels(it.labels),
                })
                .collect(), // alloc: the owned diagnostic
            suggestions: self
                .suggestions
                .iter()
                .map(|it| Suggestion {
                    message: String::from(it.message), // alloc: the owned diagnostic
                    parts: labels(it.parts),
                    applicability: it.applicability,
                    verbose: it.verbose,
                })
                .collect(), // alloc: the owned diagnostic
        }
    }
}

mod explain;
mod parts;
#[cfg(test)]
mod tests;

/// A place's local and projection, as the checks keep it in their sets and maps.
type PlaceKey<'p> = (LocalId, &'p [PlaceElem]);

fn raw(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

/// Checks `body`, given what the closures it makes need of it; in a closure, returns what it
/// needs of its creator, with how many regions its type has, and the captures it needs the
/// `mut` of.
pub(super) fn check<'o, 'f, 'db>(
    ctx: &Ctx<'f, 'db>,
    out_frame: &'o Frame,
    body: &MirBody<'db>,
    requirements: &FMap<'_, ExprId, (usize, FVec<'_, regions::Requirement<'db>>)>,
    mut_upvars: &FMap<'_, ExprId, FVec<'_, usize>>,
    out: &mut FVec<'o, BorrowckDiagnostic>,
    moved: &mut FVec<'o, UseOfMoved<'o>>,
) -> (usize, FVec<'o, regions::Requirement<'db>>, FVec<'o, usize>) {
    use crate::timings::{Phase, phase};
    let (cfg, decoded, moves, rustc_moves, upvar_sources, flow, dominators, capture_temps, index_spans) =
        phase(Phase::Moves, || {
            let frame = ctx.frame;
            let cfg = Cfg::new(frame, body);
            let decoded = Decoded::new(ctx, body, &cfg);
            let moves = MoveData::gather(ctx, body, &cfg, &decoded);
            let rustc_moves = frame_only(|| rustc_move_indices(ctx, body, &moves));
            let upvar_sources = frame_only(|| upvar_sources(ctx, body));
            let flow = InitFlow::compute(frame, body, &cfg, &moves);
            let dominators = Dominators::new(frame, &cfg);
            let capture_temps = frame_only(|| capture_temps(frame, body));
            let index_spans = frame_only(|| index_spans(frame, body));
            (cfg, decoded, moves, rustc_moves, upvar_sources, flow, dominators, capture_temps, index_spans)
        });
    let mut regions = phase(Phase::Constraints, || regions::build(ctx, body, &cfg, requirements));
    let count = ctx.infcx.num_region_vars();
    let live = phase(Phase::Liveness, || {
        liveness::region_liveness(ctx, body, &cfg, &decoded, &moves, &regions, count)
    });
    let (values, unnamed) = phase(Phase::Infer, || {
        let mut values = regions::infer(ctx.frame, &regions, count, cfg.points, &live);
        // rustc's member constraints: a region of a returned `impl Trait`'s hidden type that can
        // be none of the lifetimes it captures is an error, reported only when the body has no
        // other region error. One that can is made to outlive the least it can be, as rustc's
        // `apply_member_constraint`, which what it must outlive in turn may not.
        let chosen = frame_only(|| member_choices(ctx.frame, &regions, &values, count));
        let unnamed = chosen.iter().any(|(_, it)| it.is_none());
        let applied = mem::collect(
            ctx.frame,
            chosen.iter().filter_map(|&(member, choice)| {
                let choice = choice?;
                (member != choice
                    && !values.universals_of(member).any(|it| regions.universals[it].0 == choice))
                .then_some(regions::Outlives {
                    sup: member,
                    sub: choice,
                    location: None,
                    category: Category::Opaque,
                    span: MirSpan::Unknown,
                    from_closure: false,
                })
            }),
        );
        if !applied.is_empty() {
            regions.outlives.extend(applied);
            values = regions::infer(ctx.frame, &regions, count, cfg.points, &live);
        }
        (values, unnamed)
    });
    let unique = |assigned: LocalId, local: LocalId, projection: &[PlaceElem]| {
        capture_temps.contains_key(&assigned)
            && is_immutable_root(ctx, PlaceRef { local, projection })
    };
    let mut loans =
        phase(Phase::Loans, || Loans::gather(ctx, body, &cfg, &decoded, &regions, unique));
    phase(Phase::Scopes, || match ctx.mode {
        Mode::Polonius => {
            let live_loans = polonius::live_loans(ctx, body, &cfg, &regions, count, &live, &loans);
            loans.compute_polonius_scopes(ctx, body, &cfg, &live_loans);
        }
        _ => loans.compute_scopes(ctx, body, &cfg, &values),
    });
    let borrows_entry = phase(Phase::EntrySets, || loans.entry_sets(ctx, body, &cfg));
    crate::timings::count(|it| {
        it.bodies += 1;
        it.blocks += body.basic_blocks.len();
        it.points += cfg.points;
        it.locals += body.locals.len();
        it.regions += count;
        it.universals += regions.universals.len();
        it.constraints += regions.outlives.len();
        it.localized += regions.outlives.iter().filter(|it| it.location.is_some()).count();
        it.loans += loans.loans.len();
    });
    if crate::debugging(ctx.db, ctx.owner) {
        for (local, ty) in regions.local_tys.iter().enumerate() {
            eprintln!("_{local}: {ty:?}");
        }
        eprintln!("universals {:?}", regions.universals);
        if !regions.members.is_empty() {
            eprintln!("members {:?} choices {:?}", regions.members, regions.choices);
        }
        for test in &regions.type_tests {
            eprintln!("type test {:?}: '{} at {:?}", test.ty, test.lower, test.span);
        }
        for constraint in &regions.outlives {
            eprintln!("{constraint:?}");
        }
        for (id, loan) in loans.loans.iter().enumerate() {
            let points: Vec<usize> = values
                .points_of(loan.region)
                .iter()
                .filter(|&it| it < cfg.points)
                .collect(); // alloc: BLAZE_BORROWCK_DEBUG output only
            eprintln!(
                "loan {:?} {:?} at {:?} region {} points {points:?}",
                loan.local,
                loans.projection(id),
                loan.location,
                loan.region
            );
            for &(target, _) in &regions.universals {
                let path = regions::constraint_path(&regions, count, loan.region, target);
                if !path.is_empty() {
                    eprintln!("  path to {target}: {path:?}");
                }
            }
        }
    }
    let frame = ctx.frame;
    let locals = body.locals.len();
    let mut checker = frame_only(|| Checker {
        ctx,
        out_frame,
        body,
        cfg: &cfg,
        moves: &moves,
        dominators: &dominators,
        uninit: bits(frame, moves.len()),
        ever: bits(frame, locals),
        errors: Vec::new_in(frame),
        access_reported: mem::set(frame),
        uninit_reported: bits(frame, locals),
        move_reported: Vec::new_in(frame),
        move_reported_at: mem::map(frame),
        rustc_moves,
        upvar_sources,
        move_errors: Vec::new_in(frame),
        capture_temps,
        index_spans,
        assigned_local: super::return_slot(),
        regions: &regions,
        region_count: count,
        values: &values,
        live: &live,
        loans: &loans,
        in_scope: bits(frame, loans.loans.len()),
        dead_reported: mem::set(frame),
        reservation_reported: mem::set(frame),
        static_muts: static_muts(ctx, body, &decoded),
        mut_errors: mem::map(frame),
        requirements: Vec::new_in(out_frame),
        mut_upvars,
        used_mut: bits(frame, locals),
        used_mut_upvars: Vec::new_in(frame),
    });
    let walk = crate::timings::start();
    frame_only(|| {
        let (guards, guard_at) = guard_borrows(ctx, body);
        let index_borrows = index_borrows(ctx, body);
        let test_span = |span: MirSpan| match span {
            MirSpan::PatId(pat) => ctx.scan.tested_at(pat).map_or(span, MirSpan::ExprId),
            _ => span,
        };
        for &block in &cfg.order {
            flow.uninit_entry.load(index(block), checker.uninit.words_mut());
            flow.ever_entry.load(index(block), checker.ever.words_mut());
            borrows_entry.load(index(block), checker.in_scope.words_mut());
            let data = &body.basic_blocks[block];
            for (statement, it) in data.statements.iter().enumerate() {
                let location = Location { block, statement };
                let span = match &it.kind {
                    StatementKind::Assign(_, Rvalue::Discriminant(_)) => test_span(it.span),
                    StatementKind::Assign(_, Rvalue::CheckedBinaryOp(BinOp::Eq, ..))
                        if switched_literal(ctx, it.span) =>
                    {
                        test_span(it.span)
                    }
                    _ => it.span,
                };
                loans.before(&cfg, location, &mut checker.in_scope);
                checker.check_activations(location, span);
                if let Some(&guard) = guard_at.get(&it.span)
                    && let StatementKind::Assign(dest, rvalue) = &it.kind
                {
                    match rvalue {
                        Rvalue::Ref(BorrowKind::Mut { .. }, borrowed) => {
                            checker.guard_conflict(&guards[guard as usize], borrowed, false, span)
                        }
                        _ => checker.guard_conflict(&guards[guard as usize], dest, true, span),
                    }
                }
                if let Some(protected) = index_borrows.get(&it.span)
                    && let StatementKind::Assign(dest, rvalue) = &it.kind
                {
                    match rvalue {
                        Rvalue::Ref(BorrowKind::Mut { .. }, borrowed) => {
                            checker.index_conflict(protected, borrowed, false, span)
                        }
                        _ => checker.index_conflict(protected, dest, true, span),
                    }
                }
                checker.statement(location, &it.kind, span);
                apply(body, &moves, &cfg, location, &mut checker.uninit, &mut checker.ever);
                loans.after(&cfg, location, &mut checker.in_scope);
            }
            if let Some(terminator) = &data.terminator {
                let location = Location { block, statement: data.statements.len() };
                let span = match &terminator.kind {
                    TerminatorKind::SwitchInt { .. } => test_span(terminator.span),
                    _ => terminator.span,
                };
                loans.before(&cfg, location, &mut checker.in_scope);
                checker.check_activations(location, span);
                if let Some(&guard) = guard_at.get(&terminator.span)
                    && let TerminatorKind::Call { destination, .. } = &terminator.kind
                {
                    checker.guard_conflict(&guards[guard as usize], destination, true, span);
                }
                if let Some(protected) = index_borrows.get(&terminator.span)
                    && let TerminatorKind::Call { destination, .. } = &terminator.kind
                {
                    checker.index_conflict(protected, destination, true, span);
                }
                checker.terminator(location, &terminator.kind, span);
            }
        }
    });
    crate::timings::finish(Phase::Walk, walk);
    let errors = crate::timings::start();
    let unused_mut = frame_only(|| {
        checker.report_move_errors();
        let before = checker.errors.len();
        checker.report_region_errors();
        if unnamed && checker.errors.len() == before {
            checker.errors.push(Some(Draft {
                at_signature: true,
                ..checker.draft(
                    "E0700",
                    MirSpan::Unknown,
                    "hidden type for `impl Trait` captures lifetime that does not appear in bounds",
                )
            }));
        }
        checker.unused_mut()
    });
    crate::timings::finish(Phase::Errors, errors);
    frame_only(|| {
        out.extend(checker.errors.iter().flatten().map(Draft::owned));
        // rustc's `buffered_move_errors` is a `BTreeMap` by the moves each error is about.
        let mut reported = checker.move_reported;
        reported.sort_unstable_by(|a, b| a.moves.cmp(b.moves));
        for MoveReported { diagnostic, mut error, .. } in reported {
            error.at = out.len();
            out.push(diagnostic.owned());
            moved.push(error);
        }
        out.extend(unused_mut.iter().map(Draft::owned));
        let count =
            regions.creator_positions.values().filter(|&&it| it < regions::IMPL_ANON).count();
        let mut used = checker.used_mut_upvars;
        used.sort_unstable();
        used.dedup();
        (count, checker.requirements, mem::collect(out_frame, used.iter().copied()))
    })
}

/// rustc's `apply_member_constraint` for each region of a returned `impl Trait`'s hidden type:
/// the smallest of the choices that outlives what the region holds and is outlived by what
/// outlives it, of those comparable to all the others; `None` when there is none, and the
/// hidden type names a lifetime the `impl Trait` does not capture.
fn member_choices<'f>(
    frame: &'f Frame,
    regions: &regions::Regions<'_, '_>,
    values: &regions::RegionValues<'_>,
    count: usize,
) -> FVec<'f, (regions::Rv, Option<regions::Rv>)> {
    if regions.members.is_empty() {
        return Vec::new_in(frame);
    }
    let fn_body = regions.universals[1].0;
    let constraints = &regions.outlives;
    let outlived_by = crate::relation::Csr::build(
        frame,
        count,
        (0..constraints.len())
            .filter(|&at| constraints.sup(at) < count && constraints.sub(at) < count)
            .map(|at| (constraints.sub(at) as u32, constraints.sup(at) as u32)),
    );
    let choices =
        mem::collect(frame, regions.choices.iter().filter_map(|&it| regions.universal(it)));
    let mut chosen = Vec::with_capacity_in(regions.members.len(), frame);
    let mut lower: FVec<'_, usize> = Vec::new_in(frame);
    let mut upper: FVec<'_, usize> = Vec::new_in(frame);
    let mut seen = bits(frame, count);
    let mut stack: FVec<'_, regions::Rv> = Vec::new_in(frame);
    let mut fits: FVec<'_, usize> = Vec::new_in(frame);
    for &member in &regions.members {
        lower.clear();
        lower.extend(values.universals_of(member).filter(|&it| it != 1));
        // A region of the signature written into the hidden type is that region on both sides.
        upper.clear();
        upper.extend(regions.universal(member));
        seen.clear();
        stack.push(member);
        while let Some(at) = stack.pop() {
            for sup in outlived_by.of(at).iter().map(|&it| it as regions::Rv) {
                if seen.insert(sup) {
                    if sup != fn_body
                        && let Some(universal) = regions.universal(sup)
                    {
                        upper.push(universal);
                    }
                    stack.push(sup);
                }
            }
        }
        fits.clear();
        fits.extend(
            choices
                .iter()
                .copied()
                .filter(|&choice| lower.iter().all(|&it| regions.knows(choice, it)))
                .filter(|&choice| upper.iter().all(|&it| regions.knows(it, choice))),
        );
        let ordered = fits
            .iter()
            .copied()
            .filter(|&a| fits.iter().all(|&b| regions.knows(a, b) || regions.knows(b, a)));
        let min = ordered.reduce(|a, b| if regions.knows(a, b) { b } else { a });
        chosen.push((member, min.map(|it| regions.universals[it].0)));
    }
    chosen
}

/// Whether `expr` is the value of a block a `let` binds to a variable: `let c = { ..; expr };`.
fn let_bound_block_value(ctx: &Ctx<'_, '_>, expr: ExprId) -> bool {
    use hir_def::hir::Expr;
    let value = |mut at: ExprId| {
        while let Expr::Block { tail: Some(tail), .. } = ctx.store.expr(at) {
            at = tail;
        }
        at
    };
    ctx.store.exprs().any(|(_, it)| {
        let Expr::Block { statements, .. } = it else { return false };
        statements.iter().any(|statement| {
            matches!(
                statement,
                Statement::Let { pat, initializer: Some(init), .. }
                    if matches!(ctx.store.pat(pat), Pat::Bind { subpat: None, .. })
                        && matches!(ctx.store.expr(init), Expr::Block { .. })
                        && value(init) == expr
            )
        })
    })
}

/// A literal pattern rustc tests with a switch on the value rather than a call of `PartialEq`.
fn switched_literal(ctx: &Ctx<'_, '_>, span: MirSpan) -> bool {
    use hir_def::hir::{Expr, Literal};
    let MirSpan::PatId(pat) = span else { return false };
    let Pat::Lit(lit) = ctx.store.pat(pat) else { return false };
    matches!(
        ctx.store.expr(lit),
        Expr::Literal(Literal::Bool(_) | Literal::Char(_) | Literal::Int(..) | Literal::Uint(..))
    )
}

/// An `E0382` of a body, as rustc buffers it with those of the other bodies of the function:
/// rustc keeps one buffer of them for a function and the closures in it, keyed by the indices
/// of the moves each is about, each body numbering its own moves.
pub(super) struct UseOfMoved<'o> {
    /// The indices of the moves, as rustc numbers them in the body; `None` when one of them is
    /// a move of rust-analyzer's lowering that rustc's MIR does not have.
    pub(super) rustc_key: Option<FVec<'o, usize>>,
    /// The place used, as rustc's MIR has it: a capture is a field of the closure.
    pub(super) place: (LocalId, FVec<'o, PlaceElem>),
    /// Where the error is in what `check` reports.
    pub(super) at: usize,
}

/// Each move out's index as rustc's `MoveData` numbers its move outs: every operand moved,
/// and every `StorageDead`, in the order of the blocks and their statements. A move rustc's
/// MIR does not have has none: a drop, a capture copied into a local of the closure body, and
/// a temporary lowering moves an operand into where rustc moves the place itself.
fn rustc_move_indices<'f>(
    ctx: &Ctx<'f, '_>,
    body: &MirBody<'_>,
    moves: &MoveData<'_>,
) -> FVec<'f, Option<usize>> {
    let frame = ctx.frame;
    let locals = body.locals.len();
    let mut upvar_locals = bits(frame, locals);
    for upvar in ctx.upvars.iter() {
        upvar_locals.insert(raw(upvar.local));
    }
    let mut variables = bits(frame, locals);
    for &local in body.binding_locals.values() {
        variables.insert(raw(local));
    }
    // For each local, how often it is assigned and whether every assignment moves a place
    // into the whole of it.
    let mut assigned = mem::filled(frame, locals, (0usize, true));
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(place, rvalue) = &statement.kind {
                let entry = &mut assigned[raw(place.local)];
                entry.0 += 1;
                entry.1 &= place.projection.as_slice().is_empty()
                    && matches!(rvalue, Rvalue::Use(it) if matches!(it.kind, OperandKind::Move(_)));
            }
        }
        if let Some(terminator) = &block.terminator
            && let TerminatorKind::Call { destination, .. } = &terminator.kind
        {
            assigned[raw(destination.local)].1 = false;
        }
    }
    let lowering_temp = |local: LocalId| {
        !variables.contains(raw(local))
            && !body.param_locals.contains(&local)
            && !upvar_locals.contains(raw(local))
            && assigned[raw(local)] == (1, true)
    };
    let mut order = mem::collect(
        frame,
        (0..moves.moves.len()).filter(|&it| {
            let out = moves.moves[it];
            let local = moves.local(out.path);
            let block = &body.basic_blocks[out.location.block];
            match block.statements.get(out.location.statement) {
                Some(statement) => match &statement.kind {
                    StatementKind::StorageDead(_) => {
                        !lowering_temp(local) && !upvar_locals.contains(raw(local))
                    }
                    StatementKind::Assign(dest, _) if upvar_locals.contains(raw(dest.local)) => {
                        Some(local) != ctx.closure_local
                    }
                    _ => !lowering_temp(local),
                },
                None => {
                    !matches!(
                        block.terminator.as_ref().map(|it| &it.kind),
                        Some(TerminatorKind::Drop { .. } | TerminatorKind::DropAndReplace { .. })
                    ) && !lowering_temp(local)
                }
            }
        }),
    );
    order.sort_unstable_by_key(|&it| {
        let location = moves.moves[it].location;
        (index(location.block), location.statement, it)
    });
    let mut out = mem::filled(frame, moves.moves.len(), None);
    for (rustc, &it) in order.iter().enumerate() {
        out[it] = Some(rustc);
    }
    out
}

/// In a closure, the place of the closure each local lowering copies a capture into is
/// (`_2 = move (_1.0)`, `_2 = &(*_1).0`), as rustc's MIR uses that place itself.
fn upvar_sources<'f, 'b>(
    ctx: &Ctx<'f, '_>,
    body: &'b MirBody<'_>,
) -> FMap<'f, LocalId, PlaceRef<'b>> {
    let mut out = mem::map(ctx.frame);
    let Some(closure) = ctx.closure_local else { return out };
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            let StatementKind::Assign(dest, rvalue) = &statement.kind else { continue };
            if !dest.projection.as_slice().is_empty() || ctx.upvar_of_local(dest.local).is_none() {
                continue;
            }
            let source = match rvalue {
                Rvalue::Use(it) => match &it.kind {
                    OperandKind::Move(place) | OperandKind::Copy(place) => place,
                    _ => continue,
                },
                Rvalue::Ref(_, place) => place,
                _ => continue,
            };
            if source.local == closure {
                out.entry(dest.local).or_insert(PlaceRef::of(source));
            }
        }
    }
    out
}

/// Whether the variable `place` starts from is declared without `mut`: rustc's
/// `limit_capture_mutability` makes a closure's mutable borrow of such a place a unique one.
/// In a closure, a capture of its own captures is checked against how the variable it captured
/// is declared.
fn is_immutable_root(ctx: &Ctx<'_, '_>, place: PlaceRef<'_>) -> bool {
    if Some(place.local) == ctx.closure_local
        && let [ProjectionElem::Field(field), ..]
        | [ProjectionElem::Deref, ProjectionElem::Field(field), ..] = place.projection
        && let Some(upvar) = ctx.upvars.get(field.0 as usize)
    {
        return !upvar.mutable;
    }
    match ctx.upvar_of_local(place.local) {
        Some(upvar) => !ctx.upvars[upvar].mutable,
        None => ctx.is_immutable(place.local),
    }
}

/// A place as the checks pass it around.
#[derive(Clone, Copy)]
pub(super) struct PlaceRef<'p> {
    pub(super) local: LocalId,
    pub(super) projection: &'p [PlaceElem],
}

impl<'p> PlaceRef<'p> {
    fn of(place: &'p StoredPlace) -> Self {
        PlaceRef { local: place.local, projection: place.projection.as_slice() }
    }

    fn base(self, len: usize) -> Self {
        PlaceRef { local: self.local, projection: &self.projection[..len] }
    }

    fn key(self) -> PlaceKey<'p> {
        (self.local, self.projection)
    }

    pub(super) fn is_prefix_of(self, other: PlaceRef<'_>) -> bool {
        self.local == other.local && other.projection.starts_with(self.projection)
    }
}

fn mutability(kind: BorrowKind) -> &'static str {
    match kind {
        BorrowKind::Mut { .. } => "mutable",
        _ => "immutable",
    }
}

/// What `is_mutable` says of a capture the closure may not mutate that rustc reports where the
/// closure is made.
const REPORTED_BY_PARENT: usize = usize::MAX - 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Allowed {
    Yes,
    ExceptUpvars,
    No,
}

/// What a mutation that may be made needs the `mut` of, as rustc's `RootPlace`: the variable
/// mutated, a capture, or neither.
#[derive(Clone, Copy, Debug)]
enum Root {
    Local(Allowed),
    Upvar(usize, Allowed),
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Access {
    Read,
    /// A shared or a fake borrow.
    Borrow(BorrowKind),
    Mutate,
    MutableBorrow {
        closure_capture: bool,
        two_phase: bool,
    },
    Move,
    StorageDeadOrDrop,
    /// What a two-phase borrow does where it is first used, with the loan.
    Activation(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InitAction {
    Use,
    Borrow,
    MatchOn,
    Assignment,
    PartialAssignment,
}

impl InitAction {
    fn noun(self) -> &'static str {
        match self {
            InitAction::Use => "use",
            InitAction::Borrow => "borrow",
            InitAction::MatchOn => "use",
            InitAction::Assignment => "assign",
            InitAction::PartialAssignment => "assign to part",
        }
    }

    fn as_general_verb(self) -> &'static str {
        match self {
            InitAction::Use | InitAction::MatchOn => "used",
            InitAction::Borrow => "borrowed",
            InitAction::Assignment => "assigned",
            InitAction::PartialAssignment => "partially assigned",
        }
    }

    /// rustc's `as_verb_in_past_tense`.
    fn past_tense(self) -> &'static str {
        match self {
            InitAction::Use => "used",
            InitAction::Borrow => "borrowed",
            InitAction::MatchOn => "matched on",
            InitAction::Assignment => "assigned",
            InitAction::PartialAssignment => "partially assigned",
        }
    }
}

/// Why a move out of a place is not allowed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MoveErrorKind {
    /// Out of what a reference or a pointer points to: the length of the place up to it.
    Borrowed(usize),
    /// Out of a field of a value whose type implements `Drop`: the length of the value.
    Destructor(usize),
    /// Out of an element of a slice or an array, at an index.
    SliceOrArray(usize),
    /// Out of a variable in its `match` arm's guard.
    Guard,
}

struct MoveError<'f> {
    place: PlaceKey<'f>,
    location: Location,
    span: MirSpan,
    kind: MoveErrorKind,
}

/// An `E0382`, buffered by the moves it is about as rustc's `buffered_move_errors`.
struct MoveReported<'f, 'o> {
    /// The moves, sorted: the error's key in [`Checker::move_reported_at`].
    moves: &'f [usize],
    /// The place used, as the body has it.
    used: PlaceKey<'f>,
    diagnostic: Draft<'f>,
    error: UseOfMoved<'o>,
}

struct Checker<'a, 'f, 'o, 'db> {
    ctx: &'a Ctx<'f, 'db>,
    /// The owner's frame, for what `check` returns.
    out_frame: &'o Frame,
    body: &'a MirBody<'db>,
    cfg: &'a Cfg<'f>,
    moves: &'a MoveData<'f>,
    dominators: &'a Dominators<'f>,
    uninit: BitSet<'f>,
    ever: BitSet<'f>,
    /// In the order reported; `None` for one a later error replaced.
    errors: FVec<'f, Option<Draft<'f>>>,
    access_reported: FSet<'f, (PlaceKey<'f>, MirSpan)>,
    /// The locals an `E0381` was reported of.
    uninit_reported: BitSet<'f>,
    /// The `E0382`s, in the order first reported.
    move_reported: FVec<'f, MoveReported<'f, 'o>>,
    /// Where in `move_reported` the error of each set of moves is.
    move_reported_at: FMap<'f, &'f [usize], u32>,
    /// For each move out, its index in rustc's numbering, if rustc's MIR has it.
    rustc_moves: FVec<'f, Option<usize>>,
    /// In a closure, the place of the closure each local a capture is copied into is.
    upvar_sources: FMap<'f, LocalId, PlaceRef<'a>>,
    move_errors: FVec<'f, MoveError<'f>>,
    /// The temporaries that hold a borrow a closure captures, with the span of the closure.
    capture_temps: FMap<'f, LocalId, MirSpan>,
    /// The index expression each index temporary is evaluated for.
    index_spans: FMap<'f, LocalId, MirSpan>,
    /// The local the statement being checked assigns, if it assigns one.
    assigned_local: LocalId,
    /// The temporaries that point to a `static mut`.
    static_muts: BitSet<'f>,
    regions: &'a Regions<'f, 'db>,
    region_count: usize,
    values: &'a RegionValues<'f>,
    /// The points each region is live at by liveness alone, rustc's `liveness_constraints`.
    live: &'a [IntervalSet<'f>],
    loans: &'a Loans<'f>,
    /// The loans in scope where the checker is.
    in_scope: BitSet<'f>,
    /// The places a "does not live long enough" error was reported of, with the borrow.
    dead_reported: FSet<'f, (LocalId, MirSpan)>,
    /// The places a two-phase borrow's reservation was reported a conflict of, which its
    /// activation does not report again.
    reservation_reported: FSet<'f, PlaceKey<'f>>,
    /// For each variable declared without `mut` borrowed mutably, its error and how many
    /// borrows it is about.
    mut_errors: FMap<'f, LocalId, (usize, usize)>,
    /// In a closure, what it needs of its creator's regions.
    requirements: FVec<'o, regions::Requirement<'db>>,
    /// For each closure this body makes, the captures it needs the `mut` of.
    mut_upvars: &'a FMap<'a, ExprId, FVec<'a, usize>>,
    /// The variables a mutation needs the `mut` of, rustc's `used_mut`.
    used_mut: BitSet<'f>,
    /// In a closure, the captures a mutation needs the `mut` of.
    used_mut_upvars: FVec<'f, usize>,
}

impl<'a, 'f, 'o, 'db> Checker<'a, 'f, 'o, 'db> {
    /// `place` as a key the checker keeps: its projection copied into the body's frame.
    fn kept(&self, place: PlaceRef<'_>) -> PlaceKey<'f> {
        let projection: &'f [PlaceElem] = self.ctx.frame.alloc_slice_copy(place.projection);
        (place.local, projection)
    }

    /// A guard that assigns or mutably borrows what its `match` tests, or a place it is in,
    /// could change which arm matches (E0510); one that touches what its arm binds by value
    /// conflicts with the shared reference it sees that through.
    fn guard_conflict(
        &mut self,
        guard: &Guard<'_, '_>,
        place: &StoredPlace,
        assign: bool,
        span: MirSpan,
    ) {
        let projection = place.projection.as_slice();
        let local = place.local;
        let place = PlaceRef { local, projection };
        if guard.bindings.contains(&local) {
            let what = self.quoted(place, "value");
            // The guard sees the variable through a shared reference, so assigning it is not
            // the reassignment of an immutable variable (E0384) but a mutability error.
            if assign {
                let key = (self.kept(place), span);
                self.access_reported.insert(key);
                self.report(
                    "E0594",
                    span,
                    text!(
                        self,
                        "cannot assign to {what}, as it is immutable for the pattern guard"
                    ),
                );
            } else {
                self.report(
                    "E0596",
                    span,
                    text!(
                        self,
                        "cannot borrow {what} as mutable, as it is immutable for the pattern guard"
                    ),
                );
            }
        } else if guard.tested.iter().any(|(it, fake)| *it == local && fake.starts_with(projection))
        {
            let what = self.quoted(place, "value");
            let message = match assign {
                true => text!(self, "cannot assign {what} in match guard"),
                false => text!(self, "cannot mutably borrow {what} in match guard"),
            };
            self.report("E0510", span, message);
        } else if guard.bound.iter().any(|(it, bound)| {
            *it == local && (bound.starts_with(projection) || projection.starts_with(bound))
        }) {
            let what = self.quoted(place, "value");
            match assign {
                true => self.report(
                    "E0506",
                    span,
                    text!(self, "cannot assign to {what} because it is borrowed"),
                ),
                false => self.report(
                    "E0502",
                    span,
                    text!(
                        self,
                        "cannot borrow {what} as mutable because it is also borrowed as immutable"
                    ),
                ),
            }
        }
    }

    fn index_conflict(
        &mut self,
        protected: &[PlaceKey<'_>],
        place: &StoredPlace,
        assign: bool,
        span: MirSpan,
    ) {
        let projection = place.projection.as_slice();
        let local = place.local;
        if !protected.iter().any(|(it, fake)| *it == local && fake.starts_with(projection)) {
            return;
        }
        let what = self.quoted(PlaceRef { local, projection }, "value");
        let message = match assign {
            true => text!(self, "cannot assign {what} in indexing expression"),
            false => text!(self, "cannot mutably borrow {what} in indexing expression"),
        };
        if !self.errors.iter().flatten().any(|it| it.span == span && it.message == message) {
            self.report("E0510", span, message);
        }
    }

    fn field_with_regions(&self, place: PlaceRef<'_>) -> bool {
        place.projection.iter().enumerate().any(|(at, elem)| {
            if !matches!(elem, ProjectionElem::Field(_)) {
                return false;
            }
            let ty = self.ctx.place_ty(self.body, place.local, &place.projection[..=at]).ty;
            let mut regions = false;
            rustc_type_ir::fold_regions(self.ctx.infcx.interner, ty, |region, _| {
                regions = true;
                region
            });
            regions
        })
    }

    fn text(&self, args: fmt::Arguments<'_>) -> &'f str {
        Text::format(self.ctx.frame, args)
    }

    fn draft(&self, code: &'static str, span: MirSpan, message: &'f str) -> Draft<'f> {
        Draft::new(self.ctx.frame, code, span, message)
    }

    fn report(&mut self, code: &'static str, span: MirSpan, message: &'f str) {
        let diagnostic = self.draft(code, span, message);
        self.report_with(diagnostic);
    }

    fn report_with(&mut self, mut diagnostic: Draft<'f>) {
        use hir_def::hir::{ClosureKind, CoroutineSource, Expr};
        // An `async` closure's future is lowered around its body with no syntax of its own;
        // rustc points at the body.
        let span = diagnostic.span;
        diagnostic.span = match span {
            MirSpan::ExprId(expr)
                if let Expr::Closure {
                    body,
                    closure_kind: ClosureKind::Coroutine { source: CoroutineSource::Closure, .. },
                    ..
                } = self.ctx.store.expr(expr)
                    && let Expr::Block { tail: Some(tail), .. } = self.ctx.store.expr(body) =>
            {
                MirSpan::ExprId(tail)
            }
            _ => span,
        };
        for (at, _) in &mut diagnostic.labels {
            if *at == PartSpan::At(span) {
                *at = PartSpan::At(diagnostic.span);
            }
        }
        self.errors.push(Some(diagnostic));
    }

    fn is_union_literal(&self, place: PlaceRef<'_>, span: MirSpan) -> bool {
        let MirSpan::ExprId(expr) = span else { return false };
        if !matches!(place.projection, [ProjectionElem::Field(_)])
            || !self.ctx.scan.is_union_literal(expr)
        {
            return false;
        }
        let ty = self.body.locals[place.local].ty.as_ref();
        matches!(ty.kind(), TyKind::Adt(def, _) if def.is_union())
    }

    /// Whether the variable `place` is in, not behind a reference, is declared without `mut`.
    fn is_immutable_root(&self, place: PlaceRef<'_>) -> bool {
        is_immutable_root(self.ctx, place)
    }

    fn describe(&self, place: PlaceRef<'_>) -> Option<&'f str> {
        self.ctx.describe(self.body, place.local, place.projection)
    }

    fn quoted(&self, place: PlaceRef<'_>, otherwise: &'f str) -> &'f str {
        match self.describe(place) {
            Some(name) => text!(self, "`{name}`"),
            None => otherwise,
        }
    }

    fn statement(&mut self, location: Location, kind: &StatementKind, span: MirSpan) {
        match kind {
            StatementKind::Assign(place, rvalue) => {
                let place = PlaceRef::of(place);
                self.assigned_local = place.local;
                // Lowering copies each capture of a closure into a local of its own first,
                // which rustc uses in place.
                if span == MirSpan::Unknown
                    && place.projection.is_empty()
                    && self.ctx.upvar_of_local(place.local).is_some()
                {
                    return;
                }
                if let Rvalue::Ref(BorrowKind::Mut { kind: MutBorrowKind::Default }, borrowed) =
                    rvalue
                    && self.capture_temps.contains_key(&place.local)
                    && self.is_immutable_root(PlaceRef::of(borrowed))
                {
                    // rustc makes the mutable borrow of a variable without `mut` a closure
                    // captures a unique one, and reports the mutation in the closure.
                    let borrowed = PlaceRef::of(borrowed);
                    let access = Access::MutableBorrow { closure_capture: true, two_phase: false };
                    self.access_place(location, borrowed, span, access, Allowed::No, Depth::Deep);
                    self.check_if_path_or_subpath_is_moved(
                        location,
                        InitAction::Borrow,
                        borrowed,
                        span,
                    );
                } else {
                    // rustc reports a conflict of `x += 1` once, for its read, but twice
                    // through a field whose type has lifetimes (`*s.pointer += 1`): renumbering
                    // gives the read's and the write's places different field types.
                    let read = match span {
                        MirSpan::ExprId(expr) => match self.ctx.store.expr(expr) {
                            hir_def::hir::Expr::BinaryOp {
                                lhs,
                                op: Some(hir_def::hir::BinaryOp::Assignment { op: Some(_) }),
                                ..
                            } if self.field_with_regions(place) => MirSpan::ExprId(lhs),
                            _ => span,
                        },
                        _ => span,
                    };
                    self.consume_rvalue(location, rvalue, read);
                }
                self.mutate_destination(location, place, span);
            }
            // rustc reads a matched place only where no arm matches, and the bindings of a
            // `let`; lowering reads the matched place itself and every initializer, so these
            // say nothing rustc checks.
            // Except a `match`'s read of what it matches, rustc's `ForMatchedPlace`: a value
            // moved out of whole is used there, before any arm tests a part of it.
            StatementKind::FakeRead(place) => {
                if let MirSpan::ExprId(expr) = span
                    && self.ctx.scan.is_matched(expr)
                {
                    let place = PlaceRef::of(place);
                    self.check_if_full_path_is_moved(location, InitAction::Use, place, span);
                }
            }
            StatementKind::InlineAsm(asm) => {
                for operand in &asm.operands {
                    match operand {
                        AsmOperand::Reg { input, output, .. } => {
                            if let Some(input) = input {
                                self.consume_operand(location, input, span);
                            }
                            if let Some(output) = output {
                                self.mutate_place(location, PlaceRef::of(output), span);
                            }
                        }
                        AsmOperand::Const(value) | AsmOperand::Sym(value) => {
                            self.consume_operand(location, value, span)
                        }
                        AsmOperand::Label => (),
                    }
                }
            }
            StatementKind::StorageDead(local) => {
                let place = PlaceRef { local: *local, projection: &[] };
                let access = Access::StorageDeadOrDrop;
                self.access_place(location, place, span, access, Allowed::Yes, Depth::Shallow);
            }
            StatementKind::Deinit(_) | StatementKind::StorageLive(_) | StatementKind::Nop => (),
        }
    }

    fn terminator(&mut self, location: Location, kind: &TerminatorKind, span: MirSpan) {
        self.assigned_local = super::return_slot();
        match kind {
            TerminatorKind::SwitchInt { discr, .. } => self.consume_operand(location, discr, span),
            TerminatorKind::Drop { place, .. }
                if self.body.basic_blocks[location.block].is_cleanup
                    && place.projection.as_slice().is_empty()
                    && self.ctx.drop_glue(self.body.locals[place.local].ty.as_ref())
                        == hir_ty::drop::DropGlue::None =>
            {
                // rustc's unwind paths drop only what has drop glue and end no storage, so a
                // borrow of a `MetricsCounter` stays valid while the callbacks holding it drop.
            }
            TerminatorKind::Drop { place, target, .. } => {
                // The drop of the old value of an assignment, `x = y` lowered as `drop(x)` then
                // `x = y` where it goes: rustc's `Drop { replace: true }`, a write.
                let replaces = self.body.basic_blocks[*target].statements.first().is_some_and(
                    |it| matches!(&it.kind, StatementKind::Assign(assigned, _) if assigned == place),
                );
                let place = PlaceRef::of(place);
                let access = match replaces {
                    true => Access::Mutate,
                    false => Access::StorageDeadOrDrop,
                };
                self.access_place(location, place, span, access, Allowed::Yes, Depth::Drop);
            }
            TerminatorKind::Return => self.check_for_invalidation_at_exit(location, span),
            TerminatorKind::DropAndReplace { place, value, .. } => {
                self.consume_operand(location, value, span);
                self.mutate_place(location, PlaceRef::of(place), span);
            }
            TerminatorKind::Call { func, args, destination, .. } => {
                self.consume_operand(location, func, span);
                for arg in args.iter() {
                    self.consume_operand(location, arg, span);
                }
                self.mutate_destination(location, PlaceRef::of(destination), span);
            }
            TerminatorKind::TailCall { func, args, .. } => {
                self.consume_operand(location, func, span);
                for arg in args.iter() {
                    self.consume_operand(location, arg, span);
                }
                // The frame is gone before the callee runs, as at a `return`.
                self.check_for_invalidation_at_exit(location, span);
            }
            TerminatorKind::Assert { cond, msg, .. } => {
                self.consume_operand(location, cond, span);
                msg.for_each_operand(&mut |operand| {
                    if let Some(place) = operand_place(operand) {
                        self.check_if_path_or_subpath_is_moved(
                            location,
                            InitAction::Use,
                            place,
                            span,
                        );
                    }
                });
            }
            TerminatorKind::Yield { value, resume_arg, .. } => {
                self.consume_operand(location, value, span);
                self.mutate_place(location, PlaceRef::of(resume_arg), span);
            }
            TerminatorKind::Goto { .. }
            | TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. } => (),
        }
        // A loop's way back is rustc's `FalseUnwind` at the loop's head, which may unwind.
        let loops = matches!(
            kind,
            TerminatorKind::Goto { target } if self.dominators.dominates(*target, location.block)
        );
        if loops
            || matches!(
                kind,
                TerminatorKind::Call { .. }
                    | TerminatorKind::Drop { .. }
                    | TerminatorKind::DropAndReplace { .. }
                    | TerminatorKind::Assert { .. }
            )
        {
            self.check_for_invalidation_at_unwind(location, span);
        }
    }

    /// rustc's `check_for_invalidation_at_exit` at the `resume` its unwind paths end in, which
    /// rust-analyzer's MIR does not have: a terminator that may unwind takes there every loan
    /// in scope, and those whose region outlives a universal one are still in scope at the
    /// `resume`, so each of them is reported, not only the first a `StorageDead` meets.
    fn check_for_invalidation_at_unwind(&mut self, location: Location, span: MirSpan) {
        // The reports read no `in_scope`.
        let in_scope = std::mem::take(&mut self.in_scope);
        let loans = self.loans;
        for id in in_scope.iter() {
            let loan = &loans.loans[id];
            if loans.projection(id).contains(&ProjectionElem::Deref) {
                continue;
            }
            let outlives_universal = self
                .values
                .universals_of(loan.region)
                .any(|it| self.regions.universals[it].1 != UniversalKind::FnBody);
            // The unwind path drops the local the loan is of, a destructor that may see it.
            let borrowed = PlaceRef { local: loan.local, projection: loans.projection(id) };
            if !outlives_universal {
                continue;
            }
            match self.destructor_ty(borrowed) {
                Some(dropped) => {
                    let whole = PlaceRef { local: loan.local, projection: &[] };
                    self.report_conflicts_with_destructor(id, location, whole, span, dropped)
                }
                None => self.report_does_not_live_long_enough(id, location, span, None),
            }
        }
        self.in_scope = in_scope;
    }

    /// Whether `span` is a `&raw const` or `&raw mut`, which lowering builds from a borrow.
    fn is_raw_borrow(&self, span: MirSpan) -> bool {
        matches!(span, MirSpan::ExprId(expr) if matches!(
            self.ctx.store.expr(expr),
            hir_def::hir::Expr::Ref { rawness: hir_def::hir::type_ref::Rawness::RawPtr, .. }
        ))
    }

    /// rustc's `is_disaligned`: whether `place` is in a `#[repr(packed)]` struct, not behind a
    /// reference, that may leave it less aligned than its type asks for, as one whose layout
    /// is not known (of a type parameter) may.
    fn is_disaligned(&self, place: PlaceRef<'_>) -> bool {
        let mut pack: Option<u64> = None;
        for at in (0..place.projection.len()).rev() {
            if matches!(place.projection[at], ProjectionElem::Deref) {
                break;
            }
            let base = self.ctx.place_ty(self.body, place.local, &place.projection[..at]);
            if base.variant_id.is_none()
                && let TyKind::Adt(adt, _) = base.ty.kind()
                && let Some(packed) = adt.repr(self.ctx.db).pack
            {
                let packed = packed.bytes();
                pack = Some(pack.map_or(packed, |it| it.min(packed)));
            }
        }
        let Some(pack) = pack else { return false };
        let ty = self.ctx.place_ty(self.body, place.local, place.projection).ty;
        let env = hir_ty::ParamEnvAndCrate { param_env: self.ctx.env, krate: self.ctx.krate };
        // An unsized value's alignment is known only for a slice or `str` tail, not a `dyn`.
        let known_tail = || {
            let normalize = |ty| liveness::normalize_erased(self.ctx, ty).unwrap_or(ty);
            let tail = hir_ty::layout::struct_tail_erasing_lifetimes(self.ctx.db, ty, &normalize);
            matches!(tail.kind(), TyKind::Slice(_) | TyKind::Str)
        };
        match self.ctx.db.layout_of_ty(ty.store(), env.store()) {
            Ok(layout) => layout.align.abi.bytes() > pack || !(layout.is_sized() || known_tail()),
            Err(_) => true,
        }
    }

    fn mutate_destination(&mut self, location: Location, place: PlaceRef<'_>, span: MirSpan) {
        if self.is_union_literal(place, span) {
            // Lowering builds a union literal by assigning its field, where rustc assigns the
            // union.
            self.mutate_place(location, place.base(0), span);
        } else {
            self.mutate_place(location, place, span);
        }
    }

    fn consume_rvalue(&mut self, location: Location, rvalue: &Rvalue, span: MirSpan) {
        match rvalue {
            Rvalue::Ref(..) if self.loans.promoted.contains(&location) => (),
            // Lowering computes `offset_of!` as the address of the field of a value at address
            // 0, where rustc has `NullOp::OffsetOf`, which accesses no place.
            Rvalue::Ref(..)
                if matches!(span, MirSpan::ExprId(expr)
                    if matches!(self.ctx.store.expr(expr), hir_def::hir::Expr::OffsetOf(_))) => {}
            Rvalue::Ref(kind, place) => {
                let place = PlaceRef::of(place);
                let (access, depth, action) = match kind {
                    BorrowKind::Shared => (Access::Borrow(*kind), Depth::Deep, InitAction::Borrow),
                    BorrowKind::Shallow => {
                        (Access::Borrow(*kind), Depth::ShallowArtificial, InitAction::MatchOn)
                    }
                    BorrowKind::Mut { kind } => (
                        Access::MutableBorrow {
                            closure_capture: *kind == MutBorrowKind::ClosureCapture,
                            two_phase: *kind == MutBorrowKind::TwoPhasedBorrow,
                        },
                        Depth::Deep,
                        InitAction::Borrow,
                    ),
                };
                self.access_place(location, place, span, access, Allowed::No, depth);
                self.check_if_path_or_subpath_is_moved(location, action, place, span);
                if !matches!(kind, BorrowKind::Shallow)
                    && !self.is_raw_borrow(span)
                    && self.is_disaligned(place)
                {
                    self.report(
                        "E0793",
                        span,
                        "reference to field of packed struct is unaligned",
                    );
                }
            }
            Rvalue::Discriminant(place) if self.ctx.cast_moves(self.body, place, span) => {
                let place = PlaceRef::of(place);
                self.check_movable_place(location, place, span);
                self.access_place(location, place, span, Access::Move, Allowed::Yes, Depth::Deep);
                self.check_if_path_or_subpath_is_moved(location, InitAction::Use, place, span);
            }
            Rvalue::Len(place) | Rvalue::Discriminant(place) | Rvalue::CopyForDeref(place) => {
                let place = PlaceRef::of(place);
                let depth = match rvalue {
                    Rvalue::Len(_) => Depth::ShallowArtificial,
                    Rvalue::Discriminant(_) => Depth::Shallow,
                    _ => Depth::Deep,
                };
                self.access_place(location, place, span, Access::Read, Allowed::No, depth);
                self.check_if_path_or_subpath_is_moved(location, InitAction::Use, place, span);
            }
            Rvalue::Aggregate(hir_ty_mir::mir::AggregateKind::Closure(ty), operands) => {
                self.propagate_used_mut_upvars(ty.as_ref(), operands);
                rvalue
                    .for_each_operand(&mut |operand| self.consume_operand(location, operand, span))
            }
            _ if super::compares(rvalue) => rvalue.for_each_operand(&mut |operand| {
                if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
                    let span = operand.span().unwrap_or(span);
                    let place = PlaceRef::of(place);
                    self.access_place(
                        location,
                        place,
                        span,
                        Access::Read,
                        Allowed::No,
                        Depth::Deep,
                    );
                    self.check_if_path_or_subpath_is_moved(location, InitAction::Use, place, span);
                }
            }),
            _ => rvalue
                .for_each_operand(&mut |operand| self.consume_operand(location, operand, span)),
        }
    }

    fn consume_operand(&mut self, location: Location, operand: &Operand, span: MirSpan) {
        let span = operand.span().unwrap_or(span);
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place)
                if self.ctx.moved_place(self.body, operand).is_none() =>
            {
                let place = PlaceRef::of(place);
                self.access_place(location, place, span, Access::Read, Allowed::No, Depth::Deep);
                self.check_if_path_or_subpath_is_moved(location, InitAction::Use, place, span);
            }
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                let place = PlaceRef::of(place);
                self.check_movable_place(location, place, span);
                self.access_place(location, place, span, Access::Move, Allowed::Yes, Depth::Deep);
                self.check_if_path_or_subpath_is_moved(location, InitAction::Use, place, span);
            }
            OperandKind::Constant { .. }
            | OperandKind::Allocation { .. }
            | OperandKind::Static(_) => (),
        }
    }

    fn mutate_place(&mut self, location: Location, place: PlaceRef<'_>, span: MirSpan) {
        self.check_if_assigned_path_is_moved(location, place, span);
        // An immutable variable can be assigned while it has never been initialized.
        if place.projection.is_empty() && self.ctx.is_immutable(place.local) {
            if self.ever.contains(place.local.into_raw().into_u32() as usize)
                && !self.access_reported.contains(&(place.key(), span))
            {
                self.report_illegal_reassignment(place, span);
            }
            return;
        }
        self.access_place(location, place, span, Access::Mutate, Allowed::No, Depth::Shallow);
    }

    fn access_place(
        &mut self,
        location: Location,
        place: PlaceRef<'_>,
        span: MirSpan,
        access: Access,
        allowed: Allowed,
        depth: Depth,
    ) {
        self.bounds_check_reads(location, place, span);
        self.access_place_only(location, place, span, access, allowed, depth);
    }

    /// What rustc's bounds check of each index of a slice in `place` reads, at the index
    /// expression, before `place` is accessed (`len_of_slice_or_array`): a slice behind a
    /// `Copy` local has its length read from the local (`PtrMetadata(copy _1)`), another
    /// slice's is a shallow read of the slice (`&raw const (fake) *_1`, `ArrayLength`), so
    /// `&mut list[i]` uses `*list`, which a mutable borrow of it still live forbids (E0503).
    /// An array's length is its constant; its `FakeRead(ForIndex)` checks only that the array
    /// is initialized.
    fn bounds_check_reads(&mut self, location: Location, place: PlaceRef<'_>, span: MirSpan) {
        for (at, elem) in place.projection.iter().enumerate() {
            let ProjectionElem::Index(index) = elem else { continue };
            let base = place.base(at);
            let span = self.index_spans.get(index).copied().unwrap_or(span);
            let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
            let (read, depth) = match ty.kind() {
                TyKind::Slice(_) => match base.projection {
                    [ProjectionElem::Deref]
                        if self
                            .ctx
                            .place_ty(self.body, base.local, &[])
                            .ty
                            .is_trivially_pure_clone_copy() =>
                    {
                        (PlaceRef { local: base.local, projection: &[] }, Depth::Deep)
                    }
                    _ => (base, Depth::ShallowArtificial),
                },
                _ => continue,
            };
            self.access_place_only(location, read, span, Access::Read, Allowed::No, depth);
        }
    }

    fn access_place_only(
        &mut self,
        location: Location,
        place: PlaceRef<'_>,
        span: MirSpan,
        access: Access,
        allowed: Allowed,
        depth: Depth,
    ) {
        if self.access_reported.contains(&(place.key(), span)) {
            return;
        }
        let mutability_error = self.check_access_permissions(place, span, access, allowed);
        let conflict_error = self.check_access_for_conflict(location, place, span, access, depth);
        if mutability_error || conflict_error {
            let key = (self.kept(place), span);
            self.access_reported.insert(key);
        }
    }

    /// rustc's `check_access_for_conflict`: the first loan in scope the access conflicts with,
    /// reported.
    fn check_access_for_conflict(
        &mut self,
        location: Location,
        place: PlaceRef<'_>,
        span: MirSpan,
        access: Access,
        depth: Depth,
    ) -> bool {
        // Only a loan of the place's local can conflict with it; `of_local` lists them in the
        // order they were made, the order of `in_scope`.
        let loans = self.loans;
        for &id in loans.of_local(place.local) {
            let id = id as usize;
            if !self.in_scope.contains(id) {
                continue;
            }
            let loan = &loans.loans[id];
            if !places_conflict(
                self.ctx,
                self.body,
                loans,
                id,
                place.local,
                place.projection,
                depth,
                Bias::Overlap,
            ) {
                continue;
            }
            match (access, loan.kind) {
                (Access::Read | Access::Borrow(_), BorrowKind::Shared | BorrowKind::Shallow)
                | (Access::Borrow(BorrowKind::Shallow), BorrowKind::Mut { .. }) => continue,
                (Access::Activation(activating), _) if activating == id => continue,
                (Access::Activation(_), _) if self.reservation_reported.contains(&place.key()) => {
                    return false;
                }
                // A two-phase borrow's reservation may share with shared borrows, rust-lang/rust#56254.
                (
                    Access::MutableBorrow { two_phase: true, .. },
                    BorrowKind::Shared | BorrowKind::Shallow,
                )
                | (Access::Move, BorrowKind::Shallow) => continue,
                (Access::Read | Access::Borrow(_), BorrowKind::Mut { .. }) => {
                    if !self.is_active(loan, location) {
                        continue;
                    }
                    match access {
                        Access::Borrow(kind) => {
                            self.report_conflicting_borrow(location, place, span, kind, id)
                        }
                        _ => {
                            let what = self.quoted(place, "value");
                            let mut diagnostic = self.draft(
                                "E0503",
                                span,
                                text!(self, "cannot use {what} because it was mutably borrowed"),
                            );
                            self.use_while_borrowed_parts(&mut diagnostic, location, id);
                            self.report_with(diagnostic);
                        }
                    }
                }
                (Access::MutableBorrow { closure_capture, two_phase }, _) => {
                    if two_phase {
                        let key = self.kept(place);
                        self.reservation_reported.insert(key);
                    }
                    let kind = match (closure_capture, two_phase) {
                        (true, _) => MutBorrowKind::ClosureCapture,
                        (false, true) => MutBorrowKind::TwoPhasedBorrow,
                        (false, false) => MutBorrowKind::Default,
                    };
                    let kind = BorrowKind::Mut { kind };
                    self.report_conflicting_borrow(location, place, span, kind, id);
                }
                (Access::Activation(activating), _) => {
                    let kind = self.loans.loans[activating].kind;
                    self.report_conflicting_borrow(location, place, span, kind, id);
                }
                (Access::Mutate, _) => {
                    let what = self.quoted(place, "value");
                    let mut diagnostic = self.draft(
                        "E0506",
                        span,
                        text!(self, "cannot assign to {what} because it is borrowed"),
                    );
                    self.assign_to_borrowed_parts(&mut diagnostic, location, place, id);
                    self.report_with(diagnostic);
                }
                (Access::Move, _) => {
                    let what = self.quoted(place, "value");
                    let mut diagnostic = self.draft(
                        "E0505",
                        span,
                        text!(self, "cannot move out of {what} because it is borrowed"),
                    );
                    self.move_out_while_borrowed_parts(&mut diagnostic, location, place, id);
                    self.report_with(diagnostic);
                }
                (Access::StorageDeadOrDrop, _) => {
                    let borrowed = PlaceRef { local: loan.local, projection: loans.projection(id) };
                    match self.destructor_ty(borrowed) {
                        Some(dropped) if !borrowed.is_prefix_of(place) => self
                            .report_conflicts_with_destructor(id, location, place, span, dropped),
                        _ => {
                            let whole = place.projection.is_empty().then_some(place.local);
                            self.report_does_not_live_long_enough(id, location, span, whole)
                        }
                    }
                }
            }
            return true;
        }
        false
    }

    /// rustc's `is_active`: a two-phase borrow acts as a shared one between where it is made
    /// and where it is first used.
    fn is_active(&self, loan: &Loan, location: Location) -> bool {
        let activation = match loan.activation {
            Activation::NotTwoPhase => return true,
            Activation::NotActivated => return false,
            Activation::At(it) => it,
        };
        if self.dominators.location_dominates(activation, location) {
            return true;
        }
        let reserved =
            Location { block: loan.location.block, statement: loan.location.statement + 1 };
        !self.dominators.location_dominates(reserved, location)
    }

    fn check_activations(&mut self, location: Location, span: MirSpan) {
        let loans = self.loans;
        for &id in loans.activated_at(self.cfg.point(location)) {
            let id = id as usize;
            let place = PlaceRef { local: loans.loans[id].local, projection: loans.projection(id) };
            self.access_place(
                location,
                place,
                span,
                Access::Activation(id),
                Allowed::Yes,
                Depth::Deep,
            );
        }
    }

    /// rustc's `check_for_invalidation_at_exit`: a loan of a local still in scope where the
    /// body returns outlives it.
    fn check_for_invalidation_at_exit(&mut self, location: Location, span: MirSpan) {
        // The reports read no `in_scope`.
        let in_scope = std::mem::take(&mut self.in_scope);
        for id in in_scope.iter() {
            if !self.loans.projection(id).contains(&ProjectionElem::Deref) {
                self.report_does_not_live_long_enough(id, location, span, None);
            }
        }
        self.in_scope = in_scope;
    }

    fn report_conflicting_borrow(
        &mut self,
        location: Location,
        place: PlaceRef<'_>,
        span: MirSpan,
        new: BorrowKind,
        issued: usize,
    ) {
        use MutBorrowKind::ClosureCapture;
        let issued_kind = self.loans.loans[issued].kind;
        let what = self.quoted(place, "value");
        let (code, message) = match (new, issued_kind) {
            (BorrowKind::Shared, BorrowKind::Mut { kind: ClosureCapture })
            | (BorrowKind::Mut { .. }, BorrowKind::Mut { kind: ClosureCapture })
                if !matches!(new, BorrowKind::Mut { kind: ClosureCapture }) =>
            {
                (
                    "E0501",
                    text!(
                        self,
                        "cannot borrow {what} as {} because previous closure requires unique access",
                        mutability(new)
                    ),
                )
            }
            (
                BorrowKind::Mut { kind: ClosureCapture },
                BorrowKind::Mut { kind: ClosureCapture },
            ) => {
                (
                    "E0524",
                    text!(self, "two closures require unique access to {what} at the same time"),
                )
            }
            (BorrowKind::Mut { kind: ClosureCapture }, _) => (
                "E0500",
                text!(self, "closure requires unique access to {what} but it is already borrowed"),
            ),
            (BorrowKind::Mut { .. }, BorrowKind::Mut { .. }) => {
                ("E0499", text!(self, "cannot borrow {what} as mutable more than once at a time"))
            }
            _ => (
                "E0502",
                text!(
                    self,
                    "cannot borrow {what} as {} because it is also borrowed as {}",
                    mutability(new),
                    mutability(issued_kind)
                ),
            ),
        };
        let span = match self.capture_temps.get(&self.assigned_local) {
            Some(&closure) => closure,
            None => span,
        };
        let mut diagnostic = self.draft(code, span, message);
        self.conflicting_borrow_parts(&mut diagnostic, location, place, new, issued);
        self.report_with(diagnostic);
    }

    /// rustc's `classify_drop_access_kind` being `Destructor`: the place is reached through a
    /// field of a value whose type implements `Drop`, that type.
    fn destructor_ty(&self, place: PlaceRef<'_>) -> Option<Ty<'db>> {
        let mut ty = self.ctx.place_ty(self.body, place.local, &[]);
        for elem in place.projection {
            if let ProjectionElem::Field(_) | ProjectionElem::Downcast(_) = elem
                && let TyKind::Adt(def, _) = ty.ty.kind()
                && self.ctx.has_destructor(def.def_id())
            {
                return Some(ty.ty);
            }
            ty = self.ctx.project(ty, elem);
        }
        None
    }

    /// rustc's `report_borrow_conflicts_with_destructor`: the drop at `drop` of `place` needs
    /// loan `id`'s place, inside a value of type `dropped`, which implements `Drop`.
    fn report_conflicts_with_destructor(
        &mut self,
        id: usize,
        location: Location,
        place: PlaceRef<'_>,
        drop: MirSpan,
        dropped: Ty<'db>,
    ) {
        let loan = &self.loans.loans[id];
        if !self.dead_reported.insert((loan.local, loan.span)) {
            return;
        }
        let mut diagnostic =
            self.draft("E0713", loan.span, "borrow may still be in use when destructor runs");
        self.conflicts_with_destructor_parts(&mut diagnostic, location, id, place, drop, dropped);
        self.report_with(diagnostic);
    }

    /// rustc's `report_borrowed_value_does_not_live_long_enough`: loan `id` is still in use at
    /// `location`, where what it borrows is dropped at `drop`; `dropped` is the variable that
    /// is, when it is a whole one.
    fn report_does_not_live_long_enough(
        &mut self,
        id: usize,
        location: Location,
        drop: MirSpan,
        dropped: Option<LocalId>,
    ) {
        let loans = self.loans;
        let loan = &loans.loans[id];
        if !self.dead_reported.insert((loan.local, loan.span)) {
            return;
        }
        let place = PlaceRef { local: loan.local, projection: loans.projection(id) };
        // rustc's `borrow_spans` knows a capture only by a closure's or a coroutine's aggregate:
        // what an `async` closure borrows is blamed as any other borrow of a local.
        let capture = self.capture_temps.get(&loan.assigned).copied().filter(|it| {
            !matches!(
                it,
                MirSpan::ExprId(expr) if matches!(
                    self.ctx.store.expr(*expr),
                    hir_def::hir::Expr::Closure {
                        closure_kind: hir_def::hir::ClosureKind::CoroutineClosure(_),
                        ..
                    }
                )
            )
        });
        // A loan whose region outlives a universal one must be valid for it, and the error
        // says why: it is returned, or it goes to where a closure escapes to.
        let universal = self
            .values
            .universals_of(loan.region)
            .find(|&it| self.regions.universals[it].1 != UniversalKind::FnBody);
        if let Some(universal) = universal {
            let target = self.regions.universals[universal].0;
            let path =
                regions::constraint_path(self.regions, self.region_count, loan.region, target);
            // Through the lifetime chosen for an `impl Trait`'s hidden type it is rustc's
            // `OpaqueType` constraint that the loan breaks, which makes it E0597.
            let chosen = path.iter().any(|it| it.category == Category::Opaque);
            if !chosen
                && let Some(blamed) = regions::blame_with(&path, self.regions.returns_static_opaque)
            {
                match (capture, blamed.category) {
                    // Not into an `impl Trait` that captures no lifetime, which rustc's
                    // `OpaqueType` constraint makes E0597.
                    (Some(closure), Category::Return | Category::CallArgument)
                        if !blamed.from_closure && !self.regions.returns_static_opaque =>
                    {
                        let what = self.quoted(place, "value");
                        let mut diagnostic = self.draft(
                            "E0373",
                            closure,
                            text!(self, "closure may outlive the current function, but it borrows {what}, which is owned by the current function"),
                        );
                        if let MirSpan::ExprId(expr) = closure {
                            let capture = self.capture_of(loan.assigned).map(|(_, it)| it);
                            let returned = (matches!(blamed.category, Category::Return)
                                && blamed.span != MirSpan::Unknown)
                                .then(|| self.tail(blamed.span));
                            self.escaping_closure_parts(
                                &mut diagnostic,
                                expr,
                                capture,
                                &what,
                                returned,
                            );
                        }
                        self.report_with(diagnostic);
                        return;
                    }
                    // A `const` or `static` item's value is rustc's `UseAsConst`, not returned.
                    (None, Category::Return)
                        if !blamed.from_closure
                            && !self.regions.returns_static_opaque
                            && !matches!(
                                self.ctx.owner,
                                hir_ty::InferBodyId::DefWithBodyId(
                                    hir_def::DefWithBodyId::ConstId(_)
                                        | hir_def::DefWithBodyId::StaticId(_)
                                )
                            ) =>
                    {
                        let span = if blamed.span == MirSpan::Unknown {
                            loan.span
                        } else {
                            self.tail(blamed.span)
                        };
                        let described = self.describe(place);
                        let local = described.is_some() && place.projection.is_empty();
                        let message = match local {
                            true => "cannot return reference to local variable",
                            false => {
                                "returns a value referencing data owned by the current function"
                            }
                        };
                        let mut diagnostic = self.draft("E0515", span, message);
                        diagnostic.label(
                            PartSpan::At(span),
                            match local {
                                true => "returns a reference to data owned by the current function",
                                false => {
                                    "returns a value referencing data owned by the current function"
                                }
                            },
                        );
                        if span != loan.span {
                            let note = match described {
                                Some(name) => text!(self, "`{name}` is borrowed here"),
                                None => "temporary value created here",
                            };
                            diagnostic.label(PartSpan::At(loan.span), note);
                        }
                        self.report_with(diagnostic);
                        return;
                    }
                    _ => (),
                }
            }
        }
        match self.describe(place) {
            // rustc's `LaterUseKind::ClosureCapture`: the closure is a block's value a `let`
            // binds, so what uses the borrow after the block is that `let`'s read.
            Some(_)
                if let Some(MirSpan::ExprId(closure)) = capture
                    && let_bound_block_value(self.ctx, closure) =>
            {
                let what = self.quoted(place, "value");
                let mut diagnostic = self.draft(
                    "E0373",
                    MirSpan::ExprId(closure),
                    text!(self, "closure may outlive the current block, but it borrows {what}, which is owned by the current block"),
                );
                let capture = self.capture_of(loan.assigned).map(|(_, it)| it);
                self.escaping_closure_parts(&mut diagnostic, closure, capture, &what, None);
                self.report_with(diagnostic);
            }
            Some(name) => {
                let mut diagnostic = self.draft(
                    "E0597",
                    loan.span,
                    text!(self, "`{name}` does not live long enough"),
                );
                self.local_does_not_live_long_enough_parts(
                    &mut diagnostic,
                    location,
                    id,
                    &name,
                    drop,
                    dropped,
                );
                self.report_with(diagnostic);
            }
            // At the temporary, as rustc's: a pattern's borrow, `match Some(s()) {
            // Some(ref m) => .. }`, at `Some(s())`, not at `ref m`.
            None => {
                let span = match loan.span {
                    MirSpan::ExprId(_) => loan.span,
                    _ => temporary_span(self.body, loan.local).unwrap_or(loan.span),
                };
                let mut diagnostic =
                    self.draft("E0716", span, "temporary value dropped while borrowed");
                self.temporary_does_not_live_long_enough_parts(&mut diagnostic, location, id, drop);
                self.report_with(diagnostic);
            }
        }
    }

    /// What rustc's `def_descr` calls the function being checked.
    fn owner_descr(&self) -> &'static str {
        let hir_ty::InferBodyId::DefWithBodyId(hir_def::DefWithBodyId::FunctionId(function)) =
            self.ctx.owner
        else {
            return "function";
        };
        match hir_def::Lookup::lookup(&function, self.ctx.db).container {
            hir_def::ItemContainerId::ImplId(_) | hir_def::ItemContainerId::TraitId(_) => {
                if hir_def::signatures::FunctionSignature::of(self.ctx.db, function)
                    .has_self_param()
                {
                    "method"
                } else {
                    "associated function"
                }
            }
            _ => "function",
        }
    }

    /// Whether the region `rv` is in the type of a parameter, which rustc's E0521 names as
    /// where the data comes from; not in `f: impl FnOnce(&'a T)`, whose type is a parameter.
    fn parameter_names(&self, rv: regions::Rv) -> bool {
        // A closure's reference to itself is not one of its parameters.
        let own = usize::from(self.ctx.closure_local.is_some());
        self.body.param_locals.iter().skip(own).any(|&local| {
            let ty = self.regions.local_tys[local.into_raw().into_u32() as usize];
            let mut found = false;
            rustc_type_ir::fold_regions(self.ctx.infcx.interner, ty, |region, _| {
                found |= regions::rv_of(region) == Some(rv);
                region
            });
            found
        })
    }

    /// Whether the body is an `async` closure's, whose captured variables rustc says cannot
    /// escape as a lifetime that may not live long enough.
    fn async_closure(&self) -> bool {
        self.body.param_locals.first().is_some_and(|&env| {
            matches!(
                self.regions.local_tys[env.into_raw().into_u32() as usize].kind(),
                TyKind::Ref(_, inner, _) if matches!(inner.kind(), TyKind::CoroutineClosure(..))
            )
        })
    }

    /// Whether the region `rv` is in the type of what a closure captures or of one of its
    /// parameters, which rustc's E0521 names as where the data comes from or goes; the
    /// closure's reference to itself is neither, nor is `'static`.
    fn closure_variable_names(&self, rv: regions::Rv) -> bool {
        if rv == self.regions.static_rv {
            return false;
        }
        let tys = &self.regions.local_tys;
        let Some((&env, params)) = self.body.param_locals.split_first() else { return false };
        let env = tys[env.into_raw().into_u32() as usize];
        let env = match env.kind() {
            TyKind::Ref(_, inner, _) => inner,
            _ => env,
        };
        let captured = match env.kind() {
            TyKind::Closure(_, args) => Some(args.as_closure().tupled_upvars_ty()),
            TyKind::Coroutine(_, args) => Some(args.as_coroutine().tupled_upvars_ty()),
            TyKind::CoroutineClosure(_, args) => {
                Some(args.as_coroutine_closure().tupled_upvars_ty())
            }
            _ => None,
        };
        let interner = self.ctx.infcx.interner;
        captured
            .into_iter()
            .chain(params.iter().map(|&it| tys[it.into_raw().into_u32() as usize]))
            .any(|ty| {
                let mut found = false;
                rustc_type_ir::fold_regions(interner, ty, |region, _| {
                    found |= regions::rv_of(region) == Some(rv);
                    region
                });
                found
            })
    }

    /// The universal regions made to outlive what the signature does not say they outlive,
    /// and the type parameters made to outlive what they are not known to.
    fn report_region_errors(&mut self) {
        if !self.regions.check_universals {
            return;
        }
        let universals = &self.regions.universals;
        let in_closure = self.ctx.closure_local.is_some() || self.ctx.inline_const;
        let mut higher_ranked = mem::set(self.ctx.frame);
        for (longer, &(longer_rv, kind)) in universals.iter().enumerate() {
            if matches!(kind, UniversalKind::Static | UniversalKind::FnBody) {
                continue;
            }
            // As rustc, only the representative of the universal regions that outlive each
            // other is checked against all it outlives; the others only against it.
            let mut failing: FVec<'f, usize> = Vec::new_in(self.ctx.frame);
            match self.representative(longer) {
                Some(representative) => {
                    if !self.regions.knows(longer, representative) {
                        failing.push(representative);
                    }
                }
                None => failing.extend(
                    self.values
                        .universals_of(longer_rv)
                        .filter(|&it| !self.regions.knows(longer, it)),
                ),
            }
            let placeholder = kind == UniversalKind::Placeholder;
            if failing.is_empty() || in_closure && !placeholder && self.propagate(longer, &failing)
            {
                continue;
            }
            let shorter = failing[0];
            let (shorter_rv, shorter_kind) = universals[shorter];
            let path =
                regions::constraint_path(self.regions, self.region_count, longer_rv, shorter_rv);
            let Some(blamed) = regions::blame(&path) else { continue };
            let named_anon = |a: UniversalKind, b: UniversalKind| {
                a == UniversalKind::Named && b == UniversalKind::AnonParam
            };
            // In a closure, rustc's `report_region_error` names what of the closure's own goes
            // to its creator by how it gets there.
            let escapes = in_closure
                && matches!(shorter_kind, UniversalKind::Creator | UniversalKind::Static);
            let (code, message) = if placeholder
                && self.regions.related_placeholders.contains(&longer)
                && !self.regions.normalized_placeholders.contains(&longer)
            {
                ("E0308", "mismatched types")
            } else if placeholder {
                ("", "higher-ranked lifetime error")
            } else if escapes
                && blamed.category == Category::Return
                && self.regions.closure_env == Some(longer)
                && self.ctx.closure_kind == Some(rustc_type_ir::ClosureKind::FnMut)
                && !self.async_closure()
            {
                ("", "captured variable cannot escape `FnMut` closure body")
            } else if escapes
                && matches!(blamed.category, Category::Assignment | Category::CallArgument)
                && (self.closure_variable_names(longer_rv)
                    || self.closure_variable_names(shorter_rv))
            {
                ("E0521", "borrowed data escapes outside of closure")
            } else if !self.regions.is_impl_item
                && !in_closure
                && (named_anon(kind, shorter_kind) || named_anon(shorter_kind, kind))
                && !self.regions.unsuggested.contains(&if kind == UniversalKind::AnonParam {
                    longer
                } else {
                    shorter
                })
            {
                ("E0621", "explicit lifetime required in the type of a parameter")
            } else if blamed.category == Category::CallArgument
                && shorter_kind == UniversalKind::Static
                && self.parameter_names(longer_rv)
            {
                ("E0521", text!(self, "borrowed data escapes outside of {}", self.owner_descr()))
            } else {
                ("", "lifetime may not live long enough")
            };
            let span = match blamed.category {
                Category::Return => self.tail(blamed.span),
                _ => blamed.span,
            };
            // A predicate proven twice, each time for placeholders of its own, is one error: of
            // a function's where clauses, at its path and at its call, it is the call's.
            let span = match span {
                MirSpan::ExprId(callee) if placeholder => self
                    .ctx
                    .store
                    .exprs()
                    .find_map(|(call, it)| {
                        matches!(it, hir_def::hir::Expr::Call { callee: c, .. } if c == callee)
                            .then_some(MirSpan::ExprId(call))
                    })
                    .unwrap_or(span),
                _ => span,
            };
            if placeholder && !higher_ranked.insert(span) {
                continue;
            }
            if blamed.category == Category::Signature {
                self.errors
                    .push(Some(Draft { at_signature: true, ..self.draft(code, span, message) }));
                continue;
            }
            self.report(code, span, message);
        }
        let mut reported = mem::set(self.ctx.frame);
        for test in &self.regions.type_tests {
            if self.type_test_holds(test.ty, test.lower) {
                continue;
            }
            for universal in self.values.universals_of(test.lower) {
                let kind = universals[universal].1;
                if kind == UniversalKind::FnBody
                    || self.regions.type_outlives_known(test.ty, universal)
                {
                    continue;
                }
                // rustc's `try_promote_type_test`: in a closure, the type parameter must
                // outlive each of its creator's regions that outlive this one, which the
                // creator proves.
                if in_closure {
                    if !rustc_type_ir::TypeVisitableExt::has_free_regions(&test.ty) {
                        for upper in self.non_local_upper_bounds(universal) {
                            let shorter = match universals[upper].1 {
                                UniversalKind::Static => None,
                                _ => match self.regions.creator_positions.get(&upper) {
                                    Some(&it) => Some(it),
                                    None => continue,
                                },
                            };
                            self.requirements.push(regions::Requirement {
                                subject: Some(test.ty),
                                longer: 0,
                                shorter,
                                category: Category::Boring,
                                span: test.span,
                            });
                        }
                    }
                    continue;
                }
                if reported.insert((test.ty, test.span)) {
                    let what = match test.ty.kind() {
                        TyKind::Alias(_, _) => "associated type",
                        _ => "parameter type",
                    };
                    let (code, message) = match kind {
                        _ if test.forall => {
                            ("", text!(self, "the {what} does not live long enough"))
                        }
                        UniversalKind::Static => {
                            ("E0310", text!(self, "the {what} may not live long enough"))
                        }
                        UniversalKind::Named => {
                            ("E0309", text!(self, "the {what} may not live long enough"))
                        }
                        _ => ("E0311", text!(self, "the {what} may not live long enough")),
                    };
                    self.errors.push(Some(Draft {
                        at_signature: test.signature,
                        ..self.draft(code, test.span, message)
                    }));
                }
                break;
            }
        }
    }

    /// rustc's `try_propagate_universal_region_error`, for every universal region `longer`
    /// is made to outlive that it is not known to: what a closure needs of a region that
    /// outlives one of its creator's becomes a requirement on the creator's regions, from the
    /// largest of them `longer` outlives to the smallest that outlive the shorter one. `false`
    /// when `longer` outlives none of the creator's, which is an error in the closure.
    fn propagate(&mut self, longer: usize, failing: &[usize]) -> bool {
        let known = self.regions;
        let universals = &known.universals;
        let non_local =
            |it: usize| matches!(universals[it].1, UniversalKind::Creator | UniversalKind::Static);
        let knows = |a: usize, b: usize| known.knows(a, b);
        let frame = self.ctx.frame;
        let longer_minus: FVec<'f, usize> = if non_local(longer) {
            let mut it = Vec::with_capacity_in(1, frame);
            it.push(longer);
            it
        } else {
            let lower = mem::collect(
                frame,
                (0..universals.len()).filter(|&it| non_local(it) && knows(longer, it)),
            );
            mem::collect(
                frame,
                lower.iter().copied().filter(|&it| {
                    !lower.iter().any(|&other| other != it && knows(other, it) && !knows(it, other))
                }),
            )
        };
        if longer_minus.is_empty() {
            return false;
        }
        let longer_rv = universals[longer].0;
        for &shorter in failing {
            let path = regions::constraint_path(
                known,
                self.region_count,
                longer_rv,
                universals[shorter].0,
            );
            let Some(blamed) = regions::blame(&path) else { continue };
            let shorter_plus = self.non_local_upper_bounds(shorter);
            for &minus in &longer_minus {
                if universals[minus].1 == UniversalKind::Static {
                    continue;
                }
                let outlived = mem::collect(
                    frame,
                    shorter_plus.iter().copied().filter(|&it| knows(minus, it)),
                );
                let chosen = if outlived.is_empty() { &shorter_plus } else { &outlived };
                let Some(&longer) = known.creator_positions.get(&minus) else { continue };
                for &plus in chosen {
                    let shorter = match universals[plus].1 {
                        UniversalKind::Static => None,
                        _ => match known.creator_positions.get(&plus) {
                            Some(&it) => Some(it),
                            None => continue,
                        },
                    };
                    self.requirements.push(regions::Requirement {
                        subject: None,
                        longer,
                        shorter,
                        category: blamed.category,
                        span: blamed.span,
                    });
                }
            }
        }
        true
    }

    /// rustc's representative of the strongly connected universal regions `universal` is one
    /// of, where it is not that region itself: the first made, a closure's creator's before
    /// its own. Not the function body's.
    fn representative(&self, universal: usize) -> Option<usize> {
        let universals = &self.regions.universals;
        let rank = |it: usize| {
            let local = !matches!(universals[it].1, UniversalKind::Static | UniversalKind::Creator);
            (local, it)
        };
        let rv = universals[universal].0;
        let representative = self
            .values
            .universals_of(rv)
            .filter(|&it| {
                universals[it].1 != UniversalKind::FnBody
                    && self.values.universals_of(universals[it].0).any(|it| it == universal)
            })
            .chain([universal])
            .min_by_key(|&it| rank(it))?;
        (representative != universal).then_some(representative)
    }

    /// rustc's `non_local_upper_bounds`: `universal` if it is the creator's, else the smallest
    /// of the creator's regions known to outlive it (`'static` at least).
    fn non_local_upper_bounds(&self, universal: usize) -> FVec<'f, usize> {
        let frame = self.ctx.frame;
        let known = self.regions;
        let universals = &known.universals;
        let non_local =
            |it: usize| matches!(universals[it].1, UniversalKind::Creator | UniversalKind::Static);
        if non_local(universal) {
            let mut it = Vec::with_capacity_in(1, frame);
            it.push(universal);
            return it;
        }
        let upper = mem::collect(
            frame,
            (0..universals.len()).filter(|&it| non_local(it) && known.knows(it, universal)),
        );
        mem::collect(
            frame,
            upper.iter().copied().filter(|&it| {
                !upper
                    .iter()
                    .any(|&other| other != it && known.knows(it, other) && !known.knows(other, it))
            }),
        )
    }

    /// The expression a block's value comes from, where rustc points at a returned value.
    fn tail(&self, span: MirSpan) -> MirSpan {
        let mut span = span;
        while let MirSpan::ExprId(expr) = span
            && let hir_def::hir::Expr::Block { tail: Some(tail), .. } = self.ctx.store.expr(expr)
        {
            span = MirSpan::ExprId(tail);
        }
        span
    }

    /// rustc's `eval_verify_bound` for `ty: lower`, `ty` a type parameter: one of its bounds,
    /// or the function body every parameter outlives, outlives `lower` as region inference
    /// left them.
    fn type_test_holds(&self, ty: Ty<'db>, lower: usize) -> bool {
        if let TyKind::Alias(_, alias) = ty.kind() {
            return self.alias_test_holds(ty, alias, lower, 0);
        }
        let fn_body = self.regions.universals[1].0;
        self.regions
            .type_bounds_of(ty)
            .chain([fn_body])
            .any(|bound| self.outlives_all(bound, lower))
    }

    /// Whether `longer` outlives every universal region `shorter` does, as region inference
    /// left them: each of `shorter`'s is known to be outlived by one of `longer`'s.
    fn outlives_all(&self, longer: regions::Rv, shorter: regions::Rv) -> bool {
        self.values.universals_of(shorter).all(|needed| {
            self.values.universals_of(longer).any(|have| self.regions.knows(have, needed))
        })
    }

    /// rustc's `alias_bound`: an associated type outlives `lower` where a where clause or the
    /// trait says it outlives a region that does, or where all its arguments do.
    fn alias_test_holds(
        &self,
        ty: Ty<'db>,
        alias: rustc_type_ir::AliasTy<hir_ty::next_solver::DbInterner<'db>>,
        lower: usize,
        depth: usize,
    ) -> bool {
        let interner = self.ctx.infcx.interner;
        let frame = self.ctx.frame;
        let outlives = |bound: usize| self.outlives_all(bound, lower);
        let erase = |ty: Ty<'db>| {
            rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner))
        };
        let erased = erase(ty);
        let regions_of = |ty: Ty<'db>| {
            let mut out: FVec<'_, Option<regions::Rv>> = Vec::new_in(frame);
            rustc_type_ir::fold_regions(interner, ty, |region, _| {
                out.push(match region.kind() {
                    RegionKind::ReStatic => Some(self.regions.static_rv),
                    _ => regions::rv_of(region),
                });
                region
            });
            out
        };
        let knows_all = |longer: usize, shorter: usize| self.outlives_all(longer, shorter);
        // rustc's `VerifyIfEq`: a bound on the same associated type, with equal regions.
        let mine = regions_of(ty);
        if self.regions.type_bounds.iter().any(|&(bounded, region)| {
            erase(bounded) == erased
                && regions_of(bounded).iter().zip(&mine).all(|pair| match pair {
                    (Some(x), Some(y)) => x == y || (knows_all(*x, *y) && knows_all(*y, *x)),
                    _ => false,
                })
                && outlives(region)
        }) {
            return true;
        }
        let bounds =
            rustc_type_ir::Interner::item_bounds(interner, regions::alias_def_id(alias.kind));
        for clause in bounds.iter_instantiated(interner, alias.args) {
            if let ClauseKind::TypeOutlives(OutlivesClause(bounded, region)) =
                clause.skip_norm_wip().kind().skip_binder()
                && erase(bounded) == erased
            {
                match region.kind() {
                    RegionKind::ReStatic => return true,
                    _ if regions::rv_of(region).is_some_and(outlives) => return true,
                    _ => (),
                }
            }
        }
        if depth > 4 {
            return false;
        }
        let mut components: FVec<'_, regions::Rv> = Vec::new_in(frame);
        let mut params: FVec<'_, Ty<'db>> = Vec::new_in(frame);
        let captured = match alias.kind {
            rustc_type_ir::AliasTyKind::Opaque { def_id } => {
                regions::opaque_captures(frame, self.ctx.db, def_id, alias.args.len())
            }
            _ => None,
        };
        for (index, arg) in alias.args.iter().enumerate() {
            if captured.as_ref().is_some_and(|it| !it[index]) {
                continue;
            }
            match arg.kind() {
                GenericArgKind::Lifetime(region) => components.extend(regions::rv_of(region)),
                GenericArgKind::Type(it) => {
                    regions::outlives_components(it, &mut components, &mut params);
                    regions::alias_components(it, &mut params);
                }
                GenericArgKind::Const(_) => (),
            }
        }
        components.into_iter().all(outlives)
            && params.into_iter().all(|it| match it.kind() {
                TyKind::Alias(_, inner) => self.alias_test_holds(it, inner, lower, depth + 1),
                _ => self.type_test_holds(it, lower),
            })
    }

    fn check_access_permissions(
        &mut self,
        place: PlaceRef<'_>,
        span: MirSpan,
        access: Access,
        allowed: Allowed,
    ) -> bool {
        let (error, root) = match access {
            Access::MutableBorrow { closure_capture, two_phase } => {
                let allowed = if closure_capture { Allowed::Yes } else { allowed };
                match self.is_mutable(place, allowed) {
                    Ok(root) => {
                        self.add_used_mut(place.local, root);
                        return false;
                    }
                    Err(root) => (Access::MutableBorrow { closure_capture, two_phase }, root),
                }
            }
            Access::Mutate => match self.is_mutable(place, allowed) {
                Ok(root) => {
                    self.add_used_mut(place.local, root);
                    return false;
                }
                Err(root) => (Access::Mutate, root),
            },
            Access::Read
            | Access::Borrow(_)
            | Access::Move
            | Access::StorageDeadOrDrop
            | Access::Activation(_) => return false,
        };
        // Only a variable that has been initialized can be said to be mutated.
        if !self.ever.contains(place.local.into_raw().into_u32() as usize)
            || root == REPORTED_BY_PARENT
        {
            return false;
        }
        let span = match self.capture_temps.get(&self.assigned_local) {
            Some(&closure) if matches!(access, Access::MutableBorrow { .. }) => closure,
            _ => span,
        };
        self.report_mutability_error(place, root, span, error);
        true
    }

    /// rustc's `propagate_closure_used_mut_upvar`: what the closure of type `ty`, made of
    /// `operands`, mutates of its captures by value needs their `mut` here.
    fn propagate_used_mut_upvars(&mut self, ty: Ty<'db>, operands: &[Operand]) {
        let db = self.ctx.db;
        let expr = match ty.kind() {
            TyKind::Closure(def, _) => def.0.loc(db).expr,
            TyKind::Coroutine(def, _) => def.0.loc(db).expr,
            TyKind::CoroutineClosure(def, _) => def.0.loc(db).expr,
            _ => return,
        };
        // A closure lowering does not support is not checked, and may mutate any of them.
        let fields = self.mut_upvars.get(&expr).map(|it| it.as_slice());
        let count = fields.map_or(operands.len(), <[usize]>::len);
        for at in 0..count {
            let field = fields.map_or(at, |it| it[at]);
            let Some(OperandKind::Copy(place) | OperandKind::Move(place)) =
                operands.get(field).map(|it| &it.kind)
            else {
                continue;
            };
            let place = PlaceRef::of(place);
            if place.projection.is_empty()
                && !self.ctx.bindings.contains_key(&place.local)
                && self.ctx.upvar_of_local(place.local).is_none()
            {
                // A capture by reference is a borrow, which needs the `mut` itself; one by
                // value of a copy of what a reference points to is of what it points to.
                let ty = self.ctx.place_ty(self.body, place.local, &[]).ty;
                if !matches!(ty.kind(), TyKind::Ref(_, _, rustc_ast_ir::Mutability::Mut))
                    && let Some(source) = self.temporary_source(place.local)
                {
                    self.propagate_used_mut_place(source);
                }
                continue;
            }
            self.propagate_used_mut_place(place);
        }
    }

    /// The place the temporary `local` is a reference to or a copy of.
    fn temporary_source(&self, local: LocalId) -> Option<PlaceRef<'a>> {
        let body = self.body;
        body.basic_blocks.iter().flat_map(|(_, block)| &block.statements).find_map(|statement| {
            match &statement.kind {
                StatementKind::Assign(place, rvalue)
                    if place.local == local && place.projection.as_slice().is_empty() =>
                {
                    match rvalue {
                        Rvalue::Ref(_, source)
                        | Rvalue::Use(Operand {
                            kind: OperandKind::Copy(source) | OperandKind::Move(source),
                            ..
                        }) => Some(PlaceRef::of(source)),
                        _ => None,
                    }
                }
                _ => None,
            }
        })
    }

    /// A mutation in a closure of what it captures by value, `place`: of a variable here, of
    /// what this closure captures, or through a mutable reference, which needs neither.
    fn propagate_used_mut_place(&mut self, place: PlaceRef<'_>) {
        if let Some(upvar) = self.upvar_field_projection(place) {
            self.used_mut_upvars.push(upvar);
            return;
        }
        for len in (0..place.projection.len()).rev() {
            let base = place.base(len);
            if matches!(place.projection[len], ProjectionElem::Deref)
                && matches!(
                    self.ctx.place_ty(self.body, base.local, base.projection).ty.kind(),
                    TyKind::Ref(_, _, rustc_ast_ir::Mutability::Mut)
                )
            {
                return;
            }
            if let Some(upvar) = self.upvar_field_projection(base) {
                self.used_mut_upvars.push(upvar);
                return;
            }
        }
        self.used_mut.insert(raw(place.local));
    }

    /// rustc's `lint_unused_mut`: the variables declared `mut` that no mutation needs the `mut`
    /// of. As rustc's `gather_used_muts`, a variable moved into a temporary that needs its
    /// `mut` needs it too, and one never assigned is left to other lints.
    fn unused_mut(&mut self) -> FVec<'f, Draft<'f>> {
        let frame = self.ctx.frame;
        // rustc lints no body its type check found an error in, and a type rust-analyzer could
        // not find may be such an error, as an array length whose evaluation fails.
        if self.ctx.infer.has_errors {
            return Vec::new_in(frame);
        }
        let body = self.body;
        let mut temporaries = bits(frame, body.locals.len());
        for (local, _) in body.locals.iter() {
            if self.used_mut.contains(raw(local)) && !self.ctx.bindings.contains_key(&local) {
                temporaries.insert(raw(local));
            }
        }
        let mut assigned = bits(frame, body.locals.len());
        let mut moved_into_temporary = |operand: &Operand| {
            if let OperandKind::Move(moved) = &operand.kind
                && moved.projection.as_slice().is_empty()
            {
                self.used_mut.insert(raw(moved.local));
            }
        };
        for &block in &self.cfg.order {
            let data = &body.basic_blocks[block];
            for statement in &data.statements {
                if let StatementKind::Assign(place, rvalue) = &statement.kind {
                    assigned.insert(raw(place.local));
                    if temporaries.contains(raw(place.local)) {
                        rvalue.for_each_operand(&mut moved_into_temporary);
                    }
                }
            }
            if let Some(TerminatorKind::Call { destination, args, .. }) =
                data.terminator.as_ref().map(|it| &it.kind)
            {
                assigned.insert(raw(destination.local));
                if temporaries.contains(raw(destination.local)) {
                    args.iter().for_each(&mut moved_into_temporary);
                }
            }
        }
        // An `async fn` or `async` closure moves its arguments into the future it makes, whose
        // body binds them as its own variables.
        let moves_arguments = match self.ctx.closure_expr {
            Some(expr) => matches!(
                self.ctx.store.expr(expr),
                hir_def::hir::Expr::Closure {
                    closure_kind: hir_def::hir::ClosureKind::CoroutineClosure(_),
                    ..
                }
            ),
            None => matches!(
                self.ctx.owner,
                hir_ty::InferBodyId::DefWithBodyId(hir_def::DefWithBodyId::FunctionId(function))
                    if hir_def::signatures::FunctionSignature::of(self.ctx.db, function).is_async()
            ),
        };
        let mut unused = mem::collect(
            frame,
            self.ctx.bindings.iter().map(|(&local, &binding)| (local, binding)).filter(
                |&(local, binding)| {
                    let data = &self.ctx.store[binding];
                    data.mode == BindingAnnotation::Mutable
                        && !data.name.as_str().starts_with('_')
                        && !self.used_mut.contains(raw(local))
                        && self.ctx.upvar_of_local(local).is_none()
                        && match body.param_locals.contains(&local) {
                            true => !moves_arguments,
                            false => assigned.contains(raw(local)),
                        }
                },
            ),
        );
        unused.sort_unstable_by_key(|&(local, _)| raw(local));
        let mut seen = mem::set(frame);
        unused.retain(|&(_, binding)| seen.insert(binding));
        mem::collect(
            frame,
            unused.iter().map(|&(_, binding)| {
                Draft::new(
                    frame,
                    "unused_mut",
                    MirSpan::BindingId(binding),
                    "variable does not need to be mutable",
                )
            }),
        )
    }

    /// rustc's `add_used_mut`: a mutation that needs the `mut` of the variable `local`, which
    /// may have been initialized before, or of what a closure captures.
    fn add_used_mut(&mut self, local: LocalId, root: Root) {
        match root {
            Root::Local(allowed)
                if allowed != Allowed::Yes
                    && self.ever.contains(local.into_raw().into_u32() as usize) =>
            {
                self.used_mut.insert(raw(local));
            }
            Root::Upvar(upvar, allowed) if allowed != Allowed::Yes => {
                self.used_mut_upvars.push(upvar)
            }
            _ => (),
        }
    }

    /// Whether `place` may be mutated, with what that needs the `mut` of, or the longest part
    /// of it that may not.
    fn is_mutable(&self, place: PlaceRef<'_>, allowed: Allowed) -> Result<Root, usize> {
        let Some((elem, _)) = place.projection.split_last() else {
            if let Some(upvar) = self.ctx.upvar_of_local(place.local) {
                return self.is_upvar_mutable(upvar, allowed, 0);
            }
            return match (self.ctx.is_immutable(place.local), allowed) {
                (true, Allowed::No) => Err(0),
                _ => Ok(Root::Local(allowed)),
            };
        };
        let len = place.projection.len();
        let base = place.base(len - 1);
        match elem {
            ProjectionElem::Deref => {
                // rustc reaches a `static mut` through a `*mut` pointer, lowering through a
                // reference.
                if base.projection.is_empty() && self.static_muts.contains(raw(base.local)) {
                    return Ok(Root::Other);
                }
                let base_ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                match base_ty.kind() {
                    TyKind::Ref(_, _, rustc_ast_ir::Mutability::Not) => Err(len),
                    TyKind::Ref(_, _, rustc_ast_ir::Mutability::Mut) => {
                        let mode = match self.upvar_field_projection(place) {
                            Some(upvar) if self.ctx.upvars[upvar].by_ref => allowed,
                            _ => Allowed::Yes,
                        };
                        self.is_mutable(base, mode)
                    }
                    TyKind::RawPtr(_, rustc_ast_ir::Mutability::Not) => Err(len),
                    TyKind::RawPtr(_, rustc_ast_ir::Mutability::Mut) => Ok(Root::Other),
                    _ => self.is_mutable(base, allowed),
                }
            }
            _ => match self.upvar_field_projection(place) {
                Some(upvar) => self.is_upvar_mutable(upvar, allowed, len),
                None => self.is_mutable(base, allowed),
            },
        }
    }

    fn is_upvar_mutable(&self, upvar: usize, allowed: Allowed, len: usize) -> Result<Root, usize> {
        let upvar_data = &self.ctx.upvars[upvar];
        if !upvar_data.mutable && allowed != Allowed::Yes {
            // What is behind a reference the closure cannot mutate, rustc reports where the
            // closure borrows it.
            return Err(if upvar_data.behind_deref { REPORTED_BY_PARENT } else { len });
        }
        // Mutating what a closure captured mutates the closure: not one called by `&self`.
        match self.ctx.closure_kind {
            Some(rustc_type_ir::ClosureKind::Fn) => Err(usize::MAX),
            _ => Ok(Root::Upvar(upvar, allowed)),
        }
    }

    /// The capture of a closure `place` is, as rustc's `is_upvar_field_projection`: `(*_1).0`
    /// or the local lowering copied it into, or what either points to for a capture by
    /// reference.
    fn upvar_field_projection(&self, place: PlaceRef<'_>) -> Option<usize> {
        let (place, by_ref) = match place.projection.split_last() {
            Some((ProjectionElem::Deref, _)) => (place.base(place.projection.len() - 1), true),
            _ => (place, false),
        };
        let upvar = if place.projection.is_empty() {
            self.ctx.upvar_of_local(place.local)?
        } else if Some(place.local) == self.ctx.closure_local {
            match place.projection {
                [ProjectionElem::Deref, ProjectionElem::Field(field)]
                | [ProjectionElem::Field(field)] => field.0 as usize,
                _ => return None,
            }
        } else {
            return None;
        };
        (!by_ref || self.ctx.upvars.get(upvar)?.by_ref).then_some(upvar)
    }

    fn report_mutability_error(
        &mut self,
        place: PlaceRef<'_>,
        root: usize,
        span: MirSpan,
        access: Access,
    ) {
        let (code, verb) = match access {
            Access::Mutate => ("E0594", "assign to"),
            _ => ("E0596", "borrow"),
        };
        // rustc's `access_place_error_reported`: a borrow of a variable that is this error is
        // not also one that does not live long enough.
        if place.projection.is_empty() {
            self.dead_reported.insert((place.local, span));
        }
        let name = self.quoted(place, "data");
        let reason = if root == usize::MAX {
            match access {
                Access::Mutate => ", as `Fn` closures cannot mutate their captured variables",
                _ => ", as it is a captured variable in a `Fn` closure",
            }
        } else {
            let root_place = place.base(root.min(place.projection.len()));
            match root_place.projection.last() {
                Some(ProjectionElem::Deref) => {
                    let base = root_place.base(root_place.projection.len() - 1);
                    let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                    if matches!(ty.kind(), TyKind::RawPtr(..)) {
                        ", which is behind a `*const` pointer"
                    } else if self.describe(root_place).is_some() {
                        ", which is behind a `&` reference"
                    } else {
                        " as it is behind a `&` reference"
                    }
                }
                _ => ", as it is not declared as mutable",
            }
        };
        let message = match access {
            Access::Mutate => text!(self, "cannot {verb} {name}{reason}"),
            _ => text!(self, "cannot {verb} {name} as mutable{reason}"),
        };
        // rustc's `buffer_mut_error`: the mutable borrows of a variable declared without `mut`
        // are one error, at the declaration once there are two.
        let binding = self.ctx.bindings.get(&place.local).copied();
        if code == "E0596"
            && root == 0
            && let Some(binding) = binding
            && self.ctx.store[binding].mode == BindingAnnotation::Unannotated
            && self.ctx.upvar_of_local(place.local).is_none()
        {
            if let Some((index, count)) = self.mut_errors.get_mut(&place.local) {
                *count += 1;
                let count = *count;
                if let Some(error) = &mut self.errors[*index] {
                    if count == 2 {
                        error.span = MirSpan::BindingId(binding);
                        error.label(PartSpan::At(error.span), "not mutable");
                    }
                    if count < 10 {
                        error.label(PartSpan::At(span), "cannot borrow as mutable");
                    }
                    // rustc's note, added once all the borrows are counted.
                    if count > 10 {
                        let note =
                            text!(error, "...and {} other attempted mutable borrows", count - 10);
                        match error.children.last_mut() {
                            Some(last) if count > 11 => last.message = note,
                            _ => error.note(note),
                        }
                    }
                }
                return;
            }
            self.mut_errors.insert(place.local, (self.errors.len(), 1));
        }
        let mut diagnostic = self.draft(code, span, message);
        // Behind a pointer rustc's labels name the pointer, which is not ported.
        let behind_pointer = root != usize::MAX
            && matches!(
                place.base(root.min(place.projection.len())).projection.last(),
                Some(ProjectionElem::Deref)
            );
        if !behind_pointer {
            let not_declared_mut = root != usize::MAX;
            self.mutability_parts(&mut diagnostic, place, code == "E0594", not_declared_mut);
        }
        self.report_with(diagnostic);
    }

    fn report_illegal_reassignment(&mut self, place: PlaceRef<'_>, span: MirSpan) {
        let name = self.describe(place).unwrap_or_default();
        let from_argument = self.body.param_locals.contains(&place.local);
        let message = if from_argument {
            text!(self, "cannot assign to immutable argument `{name}`")
        } else {
            text!(self, "cannot assign twice to immutable variable `{name}`")
        };
        let mut diagnostic = self.draft("E0384", span, message);
        self.reassignment_parts(&mut diagnostic, place, from_argument);
        self.report_with(diagnostic);
    }

    /// The path of `place`, or the nearest above it, with how much of `place` it covers.
    fn move_path_closest_to(&self, place: PlaceRef<'_>) -> (usize, Mpi) {
        let path = self.moves.find(self.ctx, self.body, place.local, place.projection).path();
        (self.moves.projection(path).len(), path)
    }

    fn move_path_for(&self, place: PlaceRef<'_>) -> Option<Mpi> {
        match self.moves.find(self.ctx, self.body, place.local, place.projection) {
            Lookup::Exact(path) => Some(path),
            Lookup::Parent(_) => None,
        }
    }

    fn check_if_full_path_is_moved(
        &mut self,
        location: Location,
        action: InitAction,
        place: PlaceRef<'_>,
        span: MirSpan,
    ) {
        let (_, path) = self.move_path_closest_to(place);
        if self.uninit.contains(path) {
            self.report_use_of_moved_or_uninitialized(location, action, place, span, path);
        }
    }

    fn check_if_path_or_subpath_is_moved(
        &mut self,
        location: Location,
        action: InitAction,
        place: PlaceRef<'_>,
        span: MirSpan,
    ) {
        let (_, path) = self.move_path_closest_to(place);
        if self.uninit.contains(path) {
            self.report_use_of_moved_or_uninitialized(location, action, place, span, path);
            return;
        }
        // A subslice of an array is moved if an element of it is, which a move of the array's
        // elements has a path for (rustc's `check_if_subslice_element_is_moved`).
        if let Some((&ProjectionElem::Subslice { from, to }, _)) = place.projection.split_last() {
            let base = place.base(place.projection.len() - 1);
            if let TyKind::Array(_, len) =
                self.ctx.place_ty(self.body, base.local, base.projection).ty.kind()
            {
                let Some(len) = hir_ty::consteval::try_const_usize(self.ctx.db, len) else {
                    return;
                };
                let Some(array) = self.move_path_for(base) else { return };
                let range = from..(len as u64).saturating_sub(to);
                let moved = self.moves.children(array).find_map(|child| {
                    match self.moves.projection(child).last() {
                        Some(ProjectionElem::ConstantIndex { offset, .. })
                            if range.contains(offset) =>
                        {
                            self.moves.first_in(child, &self.uninit)
                        }
                        _ => None,
                    }
                });
                if let Some(moved) = moved {
                    self.report_use_of_moved_or_uninitialized(location, action, base, span, moved);
                }
                return;
            }
        }
        if let Some(path) = self.move_path_for(place) {
            if let Some(moved) = self.moves.first_in(path, &self.uninit) {
                self.report_use_of_moved_or_uninitialized(location, action, place, span, moved);
            }
        }
    }

    fn check_if_assigned_path_is_moved(
        &mut self,
        location: Location,
        place: PlaceRef<'_>,
        span: MirSpan,
    ) {
        // rustc reads an array before it indexes it (`FakeRead(ForIndex)`, where it knows the
        // length), which lowering leaves out: assigning to `a[i]` uses `a`.
        for (len, elem) in place.projection.iter().enumerate() {
            if let ProjectionElem::Index(_) = elem
                && let base = place.base(len)
                && let TyKind::Array(..) =
                    self.ctx.place_ty(self.body, base.local, base.projection).ty.kind()
            {
                self.check_if_path_or_subpath_is_moved(location, InitAction::Use, base, span);
            }
        }
        for len in (0..place.projection.len()).rev() {
            let base = place.base(len);
            match place.projection[len] {
                ProjectionElem::Index(_)
                | ProjectionElem::ConstantIndex { .. }
                | ProjectionElem::Subslice { .. }
                | ProjectionElem::Downcast(_) => (),
                ProjectionElem::Deref => {
                    self.check_if_full_path_is_moved(location, InitAction::Use, base, span);
                    break;
                }
                ProjectionElem::Field(_) => {
                    let ty = self.ctx.place_ty(self.body, base.local, base.projection);
                    match ty.ty.kind() {
                        TyKind::Adt(def, _) if self.ctx.has_destructor(def.def_id()) => {
                            self.check_if_path_or_subpath_is_moved(
                                location,
                                InitAction::Assignment,
                                base,
                                span,
                            );
                            break;
                        }
                        TyKind::Adt(..) | TyKind::Tuple(..) => {
                            self.check_parent_of_field(location, base, span);
                        }
                        _ => (),
                    }
                }
            }
        }
    }

    fn check_parent_of_field(&mut self, location: Location, base: PlaceRef<'_>, span: MirSpan) {
        let mut shortest = None;
        let mut len = base.projection.len();
        loop {
            let prefix = base.base(len);
            if let Some(path) = self.move_path_for(prefix)
                && self.uninit.contains(path)
            {
                shortest = Some((prefix, path));
            }
            if len == 0 || base.projection[len - 1] == ProjectionElem::Deref {
                break;
            }
            len -= 1;
        }
        let Some((_, path)) = shortest else { return };
        let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
        if let TyKind::Adt(def, _) = ty.kind()
            && def.is_union()
        {
            let (mut seen, mut stack) = self.reach_buffers();
            let moves = self.moves;
            if moves.moves_of(path).iter().any(|&it| {
                self.reaches(moves.moves[it as usize].location, location, &mut seen, &mut stack)
            }) {
                return;
            }
        }
        self.report_use_of_moved_or_uninitialized(
            location,
            InitAction::PartialAssignment,
            base,
            span,
            path,
        );
    }

    /// Whether control can go from `from` to `to`, in the caller's `seen` and `stack`, which
    /// it clears.
    fn reaches(
        &self,
        from: Location,
        to: Location,
        seen: &mut BitSet<'_>,
        stack: &mut FVec<'_, BasicBlockId>,
    ) -> bool {
        if from.block == to.block && from.statement < to.statement {
            return true;
        }
        seen.clear();
        stack.clear();
        stack.extend_from_slice(self.cfg.successors(from.block));
        while let Some(block) = stack.pop() {
            if block == to.block {
                return true;
            }
            if !seen.insert(index(block)) {
                continue;
            }
            stack.extend_from_slice(self.cfg.successors(block));
        }
        false
    }

    /// Buffers for [`Checker::reaches`].
    fn reach_buffers(&self) -> (BitSet<'f>, FVec<'f, BasicBlockId>) {
        let frame = self.ctx.frame;
        (bits(frame, self.body.basic_blocks.len()), Vec::new_in(frame))
    }

    fn check_movable_place(&mut self, location: Location, place: PlaceRef<'_>, span: MirSpan) {
        let mut ty = hir_ty_mir::mir::PlaceTy::from_ty(self.body.locals[place.local].ty.as_ref());
        if ty.ty.references_non_lt_error() {
            return;
        }
        if self.ctx.moves_out_of_guard(place.local, place.projection, span) {
            let moved = self.ctx.place_ty(self.body, place.local, place.projection).ty;
            if !moved.references_non_lt_error() && !self.ctx.is_copy(moved) {
                let kind = MoveErrorKind::Guard;
                let place = self.kept(place);
                self.move_errors.push(MoveError { place, location, span, kind });
            }
            return;
        }
        for (len, elem) in place.projection.iter().enumerate() {
            let kind = match (elem, ty.ty.kind()) {
                (ProjectionElem::Deref, TyKind::Ref(..) | TyKind::RawPtr(..)) => {
                    Some(MoveErrorKind::Borrowed(len))
                }
                (ProjectionElem::Field(_) | ProjectionElem::Downcast(_), TyKind::Adt(def, _))
                    if !def.is_box() && self.ctx.has_destructor(def.def_id()) =>
                {
                    Some(MoveErrorKind::Destructor(len))
                }
                (
                    ProjectionElem::Index(_)
                    | ProjectionElem::ConstantIndex { .. }
                    | ProjectionElem::Subslice { .. },
                    TyKind::Slice(_),
                ) => Some(MoveErrorKind::SliceOrArray(len)),
                (ProjectionElem::Index(_), TyKind::Array(..)) => {
                    Some(MoveErrorKind::SliceOrArray(len))
                }
                _ => None,
            };
            if let Some(kind) = kind {
                let moved = self.ctx.place_ty(self.body, place.local, place.projection).ty;
                if moved.references_non_lt_error() || self.ctx.is_copy(moved) {
                    return;
                }
                let place = self.kept(place);
                self.move_errors.push(MoveError { place, location, span, kind });
                return;
            }
            ty = self.ctx.project(ty, elem);
            if ty.ty.references_non_lt_error() {
                return;
            }
        }
    }

    fn report_use_of_moved_or_uninitialized(
        &mut self,
        location: Location,
        action: InitAction,
        used: PlaceRef<'_>,
        span: MirSpan,
        path: Mpi,
    ) {
        let (move_outs, looped) = self.moved_indexes(location, path);
        if move_outs.is_empty() {
            // rustc reports only variables: lowering leaves some temporaries unassigned, as
            // the `()` an assignment evaluates to.
            if self.ctx.local_name(used.local).is_none()
                || !self.uninit_reported.insert(raw(used.local))
            {
                return;
            }
            let name = self.quoted(used, "value");
            let message = match action {
                InitAction::PartialAssignment | InitAction::Assignment => {
                    text!(self, "partially assigned binding {name} isn't fully initialized")
                }
                _ => text!(self, "{} binding {name} isn't initialized", action.as_general_verb()),
            };
            // A capture is used where the closure is made, which rustc points at.
            let made = self.body.basic_blocks[location.block]
                .statements
                .get(location.statement)
                .filter(|it| {
                    matches!(
                        it.kind,
                        StatementKind::Assign(
                            _,
                            Rvalue::Aggregate(hir_ty_mir::mir::AggregateKind::Closure(_), _)
                        )
                    )
                })
                .map(|it| it.span);
            let span =
                self.capture_temps.get(&self.assigned_local).copied().or(made).unwrap_or(span);
            self.report("E0381", span, message);
            return;
        }
        let reported_at = self.move_reported_at.get(&move_outs.as_slice()).copied();
        if let Some(at) = reported_at {
            let (local, projection) = self.move_reported[at as usize].used;
            if used.is_prefix_of(PlaceRef { local, projection }) {
                return;
            }
        }
        let depth = self.moves.projection(path).len();
        let moved = depth < used.projection.len() || self.moves.local(path) != used.local;
        let partially = !moved && depth > used.projection.len();
        let name = self.quoted(used, "value");
        let what = if partially { "partially moved value" } else { "moved value" };
        let message = text!(self, "{} of {what}: {name}", action.noun());
        let mut diagnostic = self.draft("E0382", span, message);
        self.use_of_moved_parts(
            &mut diagnostic,
            location,
            action,
            used,
            path,
            &move_outs,
            looped,
            partially,
        );
        let out_frame = self.out_frame;
        let rustc_key = move_outs.iter().all(|&it| self.rustc_moves[it].is_some()).then(|| {
            let mut key =
                mem::collect(out_frame, move_outs.iter().filter_map(|&it| self.rustc_moves[it]));
            key.sort_unstable();
            key
        });
        let place = match self.upvar_sources.get(&used.local) {
            Some(source) => {
                let mut projection = Vec::with_capacity_in(
                    source.projection.len() + used.projection.len(),
                    out_frame,
                );
                projection.extend_from_slice(source.projection);
                projection.extend_from_slice(used.projection);
                (source.local, projection)
            }
            None => (used.local, mem::collect(out_frame, used.projection.iter().copied())),
        };
        let error = UseOfMoved { rustc_key, place, at: 0 };
        let used = self.kept(used);
        let moves: &'f [usize] = move_outs.leak();
        let reported = MoveReported { moves, used, diagnostic, error };
        match reported_at {
            Some(at) => self.move_reported[at as usize] = reported,
            None => {
                self.move_reported_at.insert(moves, self.move_reported.len() as u32);
                self.move_reported.push(reported);
            }
        }
    }

    /// The moves out of `path` or a path above it that reach `location` without an init in
    /// between, as rustc's `get_moved_indexes`, and whether they were found only through a
    /// loop's way back, its `traversed_back_edge`.
    fn moved_indexes(&self, location: Location, path: Mpi) -> (FVec<'f, usize>, bool) {
        let frame = self.ctx.frame;
        let above = |it: Mpi| self.moves.is_ancestor_or_self(it, path);
        let mut stack: FVec<'_, Location> = Vec::new_in(frame);
        let mut back_edges: FVec<'_, Location> = Vec::new_in(frame);
        for predecessor in self.cfg.predecessor_locations(self.body, location) {
            if self.dominators.location_dominates(location, predecessor) {
                back_edges.push(predecessor);
            } else {
                stack.push(predecessor);
            }
        }
        let is_argument = self.body.param_locals.contains(&self.moves.local(path));
        let mut visited = bits(frame, self.cfg.points);
        let mut result: FVec<'f, usize> = Vec::new_in(frame);
        let mut reached_start = false;
        let mut visit = |result: &mut FVec<'f, usize>, location: Location| -> bool {
            if !visited.insert(self.cfg.point(location)) {
                return true;
            }
            let is_storage_dead = matches!(
                self.body.basic_blocks[location.block].statements.get(location.statement),
                Some(statement) if matches!(statement.kind, StatementKind::StorageDead(_))
            );
            let point = self.cfg.point(location);
            if !is_storage_dead {
                for &move_out in self.moves.moves_at(point) {
                    let move_out = move_out as usize;
                    if above(self.moves.moves[move_out].path) {
                        result.push(move_out);
                        return true;
                    }
                }
            }
            for &init in self.moves.inits_at(point) {
                let init = self.moves.inits[init as usize];
                if matches!(init.kind, InitKind::Deep | InitKind::NonPanicPathOnly)
                    && above(init.path)
                {
                    return true;
                }
            }
            false
        };
        while let Some(location) = stack.pop() {
            if visit(&mut result, location) {
                continue;
            }
            let mut has_predecessor = false;
            for predecessor in self.cfg.predecessor_locations(self.body, location) {
                if self.dominators.location_dominates(location, predecessor) {
                    back_edges.push(predecessor);
                } else {
                    stack.push(predecessor);
                }
                has_predecessor = true;
            }
            if !has_predecessor {
                reached_start = true;
            }
        }
        let looped = (is_argument || !reached_start) && result.is_empty();
        if looped {
            while let Some(location) = back_edges.pop() {
                if visit(&mut result, location) {
                    continue;
                }
                back_edges.extend(self.cfg.predecessor_locations(self.body, location));
            }
        }
        result.sort_unstable();
        result.dedup();
        (result, looped)
    }

    fn report_move_errors(&mut self) {
        let frame = self.ctx.frame;
        let errors = std::mem::replace(&mut self.move_errors, Vec::new_in(frame));
        let mut reported_groups: FSet<'_, (ExprId, LocalId, &'f [PlaceElem])> = mem::set(frame);
        for error in errors {
            let mut span = error.span;
            // A variable bound by a pattern is reported where the value matched on is, once
            // for the variables of the pattern that move out of the same place, as rustc's
            // `MovesFromPlace` groups them: `(D(s), &t)` is two errors.
            let statement = self.body.basic_blocks[error.location.block]
                .statements
                .get(error.location.statement);
            if let Some(statement) = statement
                && !matches!(error.kind, MoveErrorKind::Guard)
                && let StatementKind::Assign(
                    target,
                    Rvalue::Use(Operand { kind: OperandKind::Move(_), .. }),
                ) = &statement.kind
                && target.projection.as_slice().is_empty()
                && let Some(&binding) = self.ctx.bindings.get(&target.local)
            {
                match self.ctx.scan.scrutinee_of(binding) {
                    Some(Some(scrutinee)) => {
                        let len = match error.kind {
                            MoveErrorKind::Borrowed(len)
                            | MoveErrorKind::Destructor(len)
                            | MoveErrorKind::SliceOrArray(len) => len,
                            MoveErrorKind::Guard => 0,
                        };
                        let projection = error.place.1;
                        let base = &projection[..len.min(projection.len())];
                        if !reported_groups.insert((scrutinee, error.place.0, base)) {
                            continue;
                        }
                        span = MirSpan::ExprId(scrutinee);
                    }
                    Some(None) => span = statement.span,
                    None => (),
                }
            }
            let message = self.move_error_message(&error);
            let code = match error.kind {
                MoveErrorKind::Borrowed(len) if self.is_slice_or_array(&error, len + 1) => "E0508",
                MoveErrorKind::Borrowed(_) | MoveErrorKind::Guard => "E0507",
                MoveErrorKind::SliceOrArray(_) => "E0508",
                MoveErrorKind::Destructor(_) => "E0509",
            };
            self.report(code, span, message);
        }
    }

    fn is_slice_or_array(&self, error: &MoveError<'_>, len: usize) -> bool {
        let ty = self.ctx.place_ty(self.body, error.place.0, &error.place.1[..len]).ty;
        matches!(ty.kind(), TyKind::Slice(_) | TyKind::Array(..))
    }

    /// A slice or array type as rustc writes it: `[A; 1]`, not the length's representation.
    fn array_name(&self, ty: Ty<'_>) -> &'f str {
        match ty.kind() {
            TyKind::Array(element, len) => {
                match hir_ty::consteval::try_const_usize(self.ctx.db, len) {
                    Some(len) => text!(self, "[{element:?}; {len}]"),
                    None => text!(self, "[{element:?}; _]"),
                }
            }
            _ => text!(self, "{ty:?}"),
        }
    }

    fn move_error_message(&self, error: &MoveError<'_>) -> &'f str {
        let place = PlaceRef { local: error.place.0, projection: error.place.1 };
        match error.kind {
            MoveErrorKind::Borrowed(len) if self.is_slice_or_array(error, len + 1) => {
                let ty = self.ctx.place_ty(self.body, place.local, &place.projection[..len + 1]).ty;
                let kind = if matches!(ty.kind(), TyKind::Slice(_)) { "slice" } else { "array" };
                text!(self, "cannot move out of type `{}`, a non-copy {kind}", self.array_name(ty))
            }
            MoveErrorKind::Borrowed(len) => {
                let base = place.base(len);
                let base_ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                let behind = match base_ty.kind() {
                    TyKind::Ref(_, _, rustc_ast_ir::Mutability::Mut) => "a mutable reference",
                    TyKind::Ref(..) => "a shared reference",
                    _ => "a raw pointer",
                };
                match self.describe(place) {
                    Some(name) => {
                        text!(self, "cannot move out of `{name}` which is behind {behind}")
                    }
                    None => text!(self, "cannot move out of {behind}"),
                }
            }
            MoveErrorKind::Destructor(len) => {
                let base = place.base(len);
                let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                text!(self, "cannot move out of type `{ty:?}`, which implements the `Drop` trait")
            }
            MoveErrorKind::Guard => {
                text!(self, "cannot move out of {} in pattern guard", self.quoted(place, "value"))
            }
            MoveErrorKind::SliceOrArray(len) => {
                let base = place.base(len);
                let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                let kind = if matches!(ty.kind(), TyKind::Slice(_)) { "slice" } else { "array" };
                text!(self, "cannot move out of type `{}`, a non-copy {kind}", self.array_name(ty))
            }
        }
    }
}

/// What a `match` arm's guard must leave alone: the places the `match` tests and the
/// references they are behind, which rustc fake-borrows shallowly through every guard, and
/// what the arm binds by value, which a guard sees through shared references, as it sees
/// the arm's bindings.
struct Guard<'f, 'b> {
    tested: &'f [PlaceKey<'b>],
    bound: FVec<'f, PlaceKey<'b>>,
    bindings: FVec<'f, LocalId>,
}

/// The [`Guard`]s of a body's `match` arms, and the index of the one of each expression and
/// pattern in an arm's guard.
fn guard_borrows<'f, 'b>(
    ctx: &'f Ctx<'_, '_>,
    body: &'b MirBody<'_>,
) -> (FVec<'f, Guard<'f, 'b>>, FMap<'f, MirSpan, u32>) {
    use hir_def::hir::Expr;
    fn copied(operand: &Operand) -> Option<&StoredPlace> {
        match &operand.kind {
            OperandKind::Copy(place) => Some(place),
            _ => None,
        }
    }
    let crate::scan::GuardedMatches { matches, tested_by, bound_by, guards: guarded } =
        &ctx.scan.guarded;
    let frame: &'f Frame = ctx.frame;
    let mut out = mem::map(frame);
    if *matches == 0 {
        return (Vec::new_in(frame), out);
    }
    // For each `match`, the places it tests; for each guarded arm, what it binds by value and
    // the locals of its variables.
    let mut tested_of: FVec<'_, FVec<'f, PlaceKey<'b>>> =
        mem::collect(frame, (0..*matches).map(|_| Vec::new_in(frame)));
    let mut arms_of: FVec<'_, (FVec<'f, PlaceKey<'b>>, FVec<'f, LocalId>)> =
        mem::collect(frame, (0..guarded.len()).map(|_| (Vec::new_in(frame), Vec::new_in(frame))));
    let (mut guards, mut stack, mut pats) =
        (Vec::new_in(frame), Vec::new_in(frame), Vec::new_in(frame));
    let add = |set: &mut FVec<'f, PlaceKey<'b>>, local, projection: &'b [PlaceElem]| {
        if !set.iter().any(|&(it, other)| it == local && other == projection) {
            set.push((local, projection));
        }
    };
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            let MirSpan::PatId(pat) = statement.span else { continue };
            let StatementKind::Assign(dest, rvalue) = &statement.kind else { continue };
            if let Some(&(index, arm)) = bound_by.get(&pat) {
                if let Some(arm) = arm
                    && dest.projection.as_slice().is_empty()
                    && !arms_of[arm].1.contains(&dest.local)
                {
                    arms_of[arm].1.push(dest.local);
                }
                let source = match rvalue {
                    Rvalue::Use(Operand {
                        kind: OperandKind::Copy(place) | OperandKind::Move(place),
                        ..
                    }) => Some((place, true)),
                    Rvalue::Ref(_, place) => Some((place, false)),
                    _ => None,
                };
                if let Some((place, by_value)) = source {
                    let projection = place.projection.as_slice();
                    for (len, elem) in projection.iter().enumerate() {
                        if let ProjectionElem::Deref = elem {
                            add(&mut tested_of[index], place.local, &projection[..len]);
                        }
                    }
                    if let Some(arm) = arm
                        && by_value
                    {
                        add(&mut arms_of[arm].0, place.local, projection);
                    }
                }
                continue;
            }
            let Some(&index) = tested_by.get(&pat) else { continue };
            let tested: [Option<&StoredPlace>; 2] = match rvalue {
                Rvalue::Discriminant(place) | Rvalue::Len(place) => [Some(place), None],
                Rvalue::CheckedBinaryOp(_, a, b) => [copied(a), copied(b)],
                _ => continue,
            };
            let set = &mut tested_of[index];
            for place in tested.into_iter().flatten() {
                let projection = place.projection.as_slice();
                add(set, place.local, projection);
                for (len, elem) in projection.iter().enumerate() {
                    if let ProjectionElem::Deref = elem {
                        add(set, place.local, &projection[..len]);
                    }
                }
            }
        }
    }
    let tested_of = mem::collect(frame, tested_of.into_iter().map(|it| &*it.leak()));
    for (arm, &(index, guard)) in guarded.iter().enumerate() {
        let tested: &'f [PlaceKey<'b>] = tested_of[index];
        let (bound, bindings) =
            std::mem::replace(&mut arms_of[arm], (Vec::new_in(frame), Vec::new_in(frame)));
        if tested.is_empty() && bound.is_empty() && bindings.is_empty() {
            continue;
        }
        let it = guards.len() as u32;
        guards.push(Guard { tested, bound, bindings });
        stack.push(guard);
        while let Some(expr) = stack.pop() {
            out.insert(MirSpan::ExprId(expr), it);
            match ctx.store.expr(expr) {
                Expr::Match { arms, .. } => pats.extend(arms.iter().map(|arm| arm.pat)),
                Expr::Let { pat, .. } => pats.push(pat),
                Expr::Block { statements, .. } => {
                    pats.extend(statements.iter().filter_map(|statement| match statement {
                        Statement::Let { pat, .. } => Some(pat),
                        _ => None,
                    }))
                }
                _ => {}
            }
            for pat in pats.drain(..) {
                ctx.store.walk_pats(pat, &mut |pat| {
                    out.insert(MirSpan::PatId(pat), it);
                });
            }
            ctx.store.walk_child_exprs(expr, |it| stack.push(it));
        }
    }
    (guards, out)
}

/// The places rustc fake-borrows shallowly while an index is evaluated, by each expression in
/// that index: in `x[1][i]` with `x: &[&[i32]]`, the bounds check of the slice `x[1]` indexes
/// must hold until `i` is evaluated, so the references it is behind, `x`, are left alone
/// (rustc's `add_fake_borrows_of_base`).
fn index_borrows<'f, 'b, 'db>(
    ctx: &Ctx<'f, 'db>,
    body: &'b MirBody<'db>,
) -> FMap<'f, MirSpan, FVec<'f, PlaceKey<'b>>> {
    use hir_def::hir::Expr;
    fn operand(it: &Operand) -> Option<&StoredPlace> {
        match &it.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => Some(place),
            _ => None,
        }
    }
    let frame = ctx.frame;
    let mut evaluated_in = mem::map(frame);
    let mut places: FVec<'_, &'b StoredPlace> = Vec::new_in(frame);
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            match &statement.kind {
                StatementKind::StorageLive(local) => {
                    if let MirSpan::ExprId(expr) = statement.span {
                        evaluated_in.insert(*local, expr);
                    }
                }
                StatementKind::Assign(dest, rvalue) => {
                    places.push(dest);
                    match rvalue {
                        Rvalue::Use(it)
                        | Rvalue::Repeat(it, _)
                        | Rvalue::Cast(_, it, _)
                        | Rvalue::UnaryOp(_, it) => places.extend(operand(it)),
                        Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => {
                            places.extend(operand(a));
                            places.extend(operand(b));
                        }
                        Rvalue::Ref(_, place)
                        | Rvalue::Len(place)
                        | Rvalue::Discriminant(place)
                        | Rvalue::CopyForDeref(place) => places.push(place),
                        Rvalue::Aggregate(_, operands) => {
                            places.extend(operands.iter().filter_map(operand))
                        }
                        _ => (),
                    }
                }
                _ => (),
            }
        }
        if let Some(terminator) = &block.terminator
            && let TerminatorKind::Call { func, args, destination, .. } = &terminator.kind
        {
            places.push(destination);
            places.extend(operand(func));
            places.extend(args.iter().filter_map(operand));
        }
    }
    let is_slice = |local, projection: &[PlaceElem]| {
        matches!(ctx.place_ty(body, local, projection).ty.kind(), TyKind::Slice(_))
    };
    let mut protected: FMap<'_, ExprId, FVec<'f, PlaceKey<'b>>> = mem::map(frame);
    let mut seen = mem::set(frame);
    let mut indexes: FVec<'_, (usize, LocalId)> = Vec::new_in(frame);
    let mut fake: FVec<'_, PlaceKey<'b>> = Vec::new_in(frame);
    for place in places {
        let projection = place.projection.as_slice();
        indexes.clear();
        indexes.extend(projection.iter().enumerate().filter_map(|(at, elem)| match elem {
            ProjectionElem::Index(local) => Some((at, *local)),
            _ => None,
        }));
        if indexes.len() < 2 || !seen.insert((place.local, projection)) {
            continue;
        }
        for (n, &(at, _)) in indexes[..indexes.len() - 1].iter().enumerate() {
            if !is_slice(place.local, &projection[..at]) {
                continue;
            }
            fake.clear();
            for len in (0..at).rev() {
                match &projection[len] {
                    ProjectionElem::Deref => fake.push((place.local, &projection[..len])),
                    ProjectionElem::Index(_) if is_slice(place.local, &projection[..len]) => break,
                    _ => (),
                }
            }
            for &(_, local) in &indexes[n + 1..] {
                if let Some(&expr) = evaluated_in.get(&local)
                    && let Expr::Index { index, .. } = ctx.store.expr(expr)
                {
                    let set = protected.entry(index).or_insert_with(|| Vec::new_in(frame));
                    for it in fake.iter() {
                        if !set.contains(it) {
                            set.push(*it);
                        }
                    }
                }
            }
        }
    }
    let mut out: FMap<'f, MirSpan, FVec<'f, PlaceKey<'b>>> = mem::map(frame);
    let mut stack: FVec<'_, ExprId> = Vec::new_in(frame);
    for (&index, fake) in protected.iter() {
        stack.push(index);
        while let Some(expr) = stack.pop() {
            let set = out.entry(MirSpan::ExprId(expr)).or_insert_with(|| Vec::new_in(frame));
            for it in fake.iter() {
                if !set.contains(it) {
                    set.push(*it);
                }
            }
            ctx.store.walk_child_exprs(expr, |it| stack.push(it));
        }
    }
    out
}

/// The span of the index expression each index temporary is evaluated for, where lowering
/// starts its storage.
fn index_spans<'f>(frame: &'f Frame, body: &MirBody<'_>) -> FMap<'f, LocalId, MirSpan> {
    let statements = body.basic_blocks.iter().flat_map(|(_, block)| &block.statements);
    let mut out = mem::map(frame);
    out.extend(statements.filter_map(|statement| match statement.kind {
        StatementKind::StorageLive(local) => Some((local, statement.span)),
        _ => None,
    }));
    out
}

fn capture_temps<'f>(frame: &'f Frame, body: &MirBody<'_>) -> FMap<'f, LocalId, MirSpan> {
    let mut variables = bits(frame, body.locals.len());
    for &local in body.binding_locals.values() {
        variables.insert(raw(local));
    }
    let mut temps = mem::map(frame);
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(
                _,
                Rvalue::Aggregate(hir_ty_mir::mir::AggregateKind::Closure(_), operands),
            ) = &statement.kind
            {
                for operand in operands.iter() {
                    if let OperandKind::Move(place) = &operand.kind
                        && place.projection.as_slice().is_empty()
                        && !variables.contains(raw(place.local))
                    {
                        temps.insert(place.local, statement.span);
                    }
                }
            }
        }
    }
    temps
}

/// Where the temporary `local` is made: the statement or call that assigns it.
fn temporary_span(body: &MirBody<'_>, local: LocalId) -> Option<MirSpan> {
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(place, _) = &statement.kind
                && place.local == local
                && place.projection.as_slice().is_empty()
                && statement.span != MirSpan::Unknown
            {
                return Some(statement.span);
            }
        }
        if let Some(terminator) = &block.terminator
            && let TerminatorKind::Call { destination, .. } = &terminator.kind
            && destination.local == local
            && destination.projection.as_slice().is_empty()
            && terminator.span != MirSpan::Unknown
        {
            return Some(terminator.span);
        }
    }
    None
}

/// The locals a `static mut` is read into whole, in any block: the decode's constants.
fn static_muts<'f>(ctx: &Ctx<'f, '_>, body: &MirBody<'_>, decoded: &Decoded<'_>) -> BitSet<'f> {
    let mut locals = bits(ctx.frame, body.locals.len());
    for constant in decoded.constants.iter().filter(|it| it.mutable) {
        locals.insert(raw(constant.local));
    }
    locals
}

fn operand_place(operand: &Operand) -> Option<PlaceRef<'_>> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => Some(PlaceRef::of(place)),
        _ => None,
    }
}

/// Runs `f`, a part of a body's check that takes memory only from the frames; in tests, adds
/// what it took from the global allocator to [`tests::GLOBAL`].
#[inline]
fn frame_only<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(test)]
    {
        let (out, used) = blaze_alloc_count::measure(f);
        tests::GLOBAL.with(|it| it.set(it.get() + used.allocations));
        out
    }
    #[cfg(not(test))]
    f()
}
