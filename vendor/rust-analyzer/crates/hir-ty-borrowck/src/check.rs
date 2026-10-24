//! The walk over a body that reports errors, as `MirBorrowckCtxt` in rustc's
//! `rustc_borrowck/src/lib.rs`: every access to a place is checked for being initialized,
//! for being allowed to mutate what it mutates and, with the loans in scope, for conflicting
//! with a loan.

use hir_def::hir::{BindingAnnotation, BindingId, ExprId, Pat, Statement};
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::{
    ClauseKind, OutlivesPredicate, RegionKind,
    inherent::{GenericArgs as _, IntoKind, Region as _},
};

use hir_ty::{
    mir::{
        AsmOperand, BinOp, BorrowKind, LocalId, MirBody, MirSpan, MutBorrowKind, Operand,
        OperandKind, PlaceElem, ProjectionElem, Rvalue, StatementKind, StoredPlace, TerminatorKind,
    },
    next_solver::{GenericArgKind, Region, Ty, TyKind},
};

use super::{
    BorrowckDiagnostic, Ctx, Mode,
    bitset::BitSet,
    cfg::{Cfg, Dominators, Location, index},
    liveness,
    loans::{Activation, Bias, Depth, Loan, Loans, places_conflict},
    moves::{InitFlow, InitKind, Lookup, MoveData, Mpi, apply},
    polonius,
    regions::{self, Category, RegionValues, Regions, UniversalKind},
};

/// Checks `body`, given what the closures it makes need of it; in a closure, returns what it
/// needs of its creator, with how many regions its type has, and the captures it needs the
/// `mut` of.
pub(super) fn check<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    requirements: &FxHashMap<ExprId, (usize, Vec<regions::Requirement<'db>>)>,
    mut_upvars: &FxHashMap<ExprId, Vec<usize>>,
    out: &mut Vec<BorrowckDiagnostic>,
) -> (usize, Vec<regions::Requirement<'db>>, Vec<usize>) {
    let cfg = Cfg::new(body);
    let moves = MoveData::gather(ctx, body, &cfg);
    let flow = InitFlow::compute(body, &cfg, &moves);
    let dominators = Dominators::new(&cfg);
    let capture_temps = capture_temps(body);
    let mut regions = regions::build(ctx, body, &cfg, requirements);
    let count = ctx.infcx.num_region_vars();
    let live = liveness::region_liveness(ctx, body, &cfg, &moves, &regions, count);
    let live_loans_input = (ctx.mode == Mode::Polonius).then(|| live.clone());
    let again = (!regions.members.is_empty()).then(|| live.clone());
    let mut values = regions::infer(&regions, count, cfg.points, live);
    // rustc's member constraints: a region of a returned `impl Trait`'s hidden type that can
    // be none of the lifetimes it captures is an error, reported only when the body has no
    // other region error. One that can is made to outlive the least it can be, as rustc's
    // `apply_member_constraint`, which what it must outlive in turn may not.
    let chosen = member_choices(&regions, &values, count);
    let unnamed = chosen.iter().any(|(_, it)| it.is_none());
    let applied: Vec<regions::Outlives> = chosen
        .iter()
        .filter_map(|&(member, choice)| {
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
        })
        .collect();
    if let Some(live) = again
        && !applied.is_empty()
    {
        regions.outlives.extend(applied);
        values = regions::infer(&regions, count, cfg.points, live);
    }
    let unique = |assigned: LocalId, local: LocalId, projection: &[PlaceElem]| {
        capture_temps.contains_key(&assigned)
            && is_immutable_root(ctx, PlaceRef { local, projection })
    };
    let mut loans = Loans::gather(ctx, body, &cfg, &regions, unique);
    match live_loans_input {
        Some(liveness) => {
            let live = polonius::live_loans(ctx, body, &cfg, &regions, count, &liveness, &loans);
            loans.compute_polonius_scopes(body, &cfg, &live);
        }
        None => loans.compute_scopes(body, &cfg, &values),
    }
    let borrows_entry = loans.entry_sets(ctx, body, &cfg);
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
        for loan in &loans.loans {
            let points: Vec<usize> =
                values.values[loan.region].iter().filter(|&it| it < cfg.points).collect();
            eprintln!(
                "loan {:?} {:?} at {:?} region {} points {points:?}",
                loan.local, loan.projection, loan.location, loan.region
            );
            for &(target, _) in &regions.universals {
                let path = regions::constraint_path(&regions, count, loan.region, target);
                if !path.is_empty() {
                    eprintln!("  path to {target}: {path:?}");
                }
            }
        }
    }
    let mut checker = Checker {
        ctx,
        body,
        cfg: &cfg,
        moves: &moves,
        dominators: &dominators,
        uninit: BitSet::default(),
        ever: BitSet::default(),
        errors: Vec::new(),
        access_reported: FxHashSet::default(),
        uninit_reported: FxHashSet::default(),
        move_reported: FxHashMap::default(),
        move_errors: Vec::new(),
        scrutinees: None,
        capture_temps,
        assigned_local: super::return_slot(),
        regions: &regions,
        region_count: count,
        values: &values,
        loans: &loans,
        in_scope: BitSet::default(),
        dead_reported: FxHashSet::default(),
        reservation_reported: FxHashSet::default(),
        static_muts: static_muts(ctx, body),
        union_literals: union_literals(ctx),
        matched: matched(ctx),
        mut_errors: FxHashMap::default(),
        requirements: Vec::new(),
        mut_upvars,
        used_mut: FxHashSet::default(),
        used_mut_upvars: Vec::new(),
    };
    let guard_borrows = guard_borrows(ctx, body);
    let index_borrows = index_borrows(ctx, body);
    let tested_at = tested_at(ctx);
    let test_span = |span: MirSpan| match span {
        MirSpan::PatId(pat) => tested_at.get(&pat).map_or(span, |&it| MirSpan::ExprId(it)),
        _ => span,
    };
    for &block in &cfg.order {
        checker.uninit = flow.uninit_entry[index(block)].clone();
        checker.ever = flow.ever_entry[index(block)].clone();
        checker.in_scope = borrows_entry[index(block)].clone();
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
            if let Some(guard) = guard_borrows.get(&it.span)
                && let StatementKind::Assign(dest, rvalue) = &it.kind
            {
                match rvalue {
                    Rvalue::Ref(BorrowKind::Mut { .. }, borrowed) => {
                        checker.guard_conflict(guard, borrowed, false, span)
                    }
                    _ => checker.guard_conflict(guard, dest, true, span),
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
            apply(body, &moves, location, &mut checker.uninit, &mut checker.ever);
            loans.after(ctx, body, location, &mut checker.in_scope);
        }
        if let Some(terminator) = &data.terminator {
            let location = Location { block, statement: data.statements.len() };
            let span = match &terminator.kind {
                TerminatorKind::SwitchInt { .. } => test_span(terminator.span),
                _ => terminator.span,
            };
            loans.before(&cfg, location, &mut checker.in_scope);
            checker.check_activations(location, span);
            if let Some(guard) = guard_borrows.get(&terminator.span)
                && let TerminatorKind::Call { destination, .. } = &terminator.kind
            {
                checker.guard_conflict(guard, destination, true, span);
            }
            if let Some(protected) = index_borrows.get(&terminator.span)
                && let TerminatorKind::Call { destination, .. } = &terminator.kind
            {
                checker.index_conflict(protected, destination, true, span);
            }
            checker.terminator(location, &terminator.kind, span);
        }
    }
    checker.report_move_errors();
    let before = checker.errors.len();
    checker.report_region_errors();
    if unnamed && checker.errors.len() == before {
        checker.errors.push(Some(BorrowckDiagnostic {
            code: "E0700",
            span: MirSpan::Unknown,
            message:
                "hidden type for `impl Trait` captures lifetime that does not appear in bounds"
                    .to_owned(),
            at_signature: true,
        }));
    }
    let unused_mut = checker.unused_mut();
    out.extend(checker.errors.into_iter().flatten());
    out.extend(checker.move_reported.into_values().map(|(_, _, error)| error));
    out.extend(unused_mut);
    let count = regions.creator_positions.values().filter(|&&it| it < regions::IMPL_ANON).count();
    let mut used_mut_upvars = checker.used_mut_upvars;
    used_mut_upvars.sort_unstable();
    used_mut_upvars.dedup();
    (count, checker.requirements, used_mut_upvars)
}

/// rustc's `apply_member_constraint` for each region of a returned `impl Trait`'s hidden type:
/// the smallest of the choices that outlives what the region holds and is outlived by what
/// outlives it, of those comparable to all the others; `None` when there is none, and the
/// hidden type names a lifetime the `impl Trait` does not capture.
fn member_choices(
    regions: &regions::Regions<'_>,
    values: &regions::RegionValues,
    count: usize,
) -> Vec<(regions::Rv, Option<regions::Rv>)> {
    let fn_body = regions.universals[1].0;
    let mut outlived_by: Vec<Vec<regions::Rv>> = vec![Vec::new(); count];
    for constraint in &regions.outlives {
        if constraint.sup < count && constraint.sub < count {
            outlived_by[constraint.sub].push(constraint.sup);
        }
    }
    let choices: Vec<usize> =
        regions.choices.iter().filter_map(|&it| regions.universal(it)).collect();
    let mut chosen = Vec::new();
    for &member in &regions.members {
        let lower: Vec<usize> = values.universals_of(member).filter(|&it| it != 1).collect();
        // A region of the signature written into the hidden type is that region on both sides.
        let mut upper: Vec<usize> = regions.universal(member).into_iter().collect();
        let mut seen = FxHashSet::default();
        let mut stack = vec![member];
        while let Some(at) = stack.pop() {
            for &sup in &outlived_by[at] {
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
        let fits: Vec<usize> = choices
            .iter()
            .copied()
            .filter(|&choice| lower.iter().all(|&it| regions.knows(choice, it)))
            .filter(|&choice| upper.iter().all(|&it| regions.knows(it, choice)))
            .collect();
        let ordered = fits
            .iter()
            .copied()
            .filter(|&a| fits.iter().all(|&b| regions.knows(a, b) || regions.knows(b, a)));
        let min = ordered.reduce(|a, b| if regions.knows(a, b) { b } else { a });
        chosen.push((member, min.map(|it| regions.universals[it].0)));
    }
    chosen
}

/// The `match` expressions, and what they match, whose arms test the value: rustc reads the
/// matched place on the edge taken when no arm matches, which a match of bindings alone lacks.
fn matched(ctx: &Ctx<'_, '_>) -> FxHashSet<ExprId> {
    let mut found = FxHashSet::default();
    for (expr, it) in ctx.store.exprs() {
        if let hir_def::hir::Expr::Match { expr: scrutinee, arms } = it
            && (arms.is_empty() || arms.iter().any(|arm| !irrefutable(ctx, arm.pat)))
        {
            found.insert(expr);
            found.insert(*scrutinee);
        }
    }
    found
}

/// The scrutinee of the `match` each pattern of an arm is in: rustc reads a discriminant and
/// switches on a value at the scrutinee's span, where rust-analyzer gives the pattern's.
fn tested_at(ctx: &Ctx<'_, '_>) -> FxHashMap<hir_def::hir::PatId, ExprId> {
    let mut found = FxHashMap::default();
    for (_, it) in ctx.store.exprs() {
        if let hir_def::hir::Expr::Match { expr: scrutinee, arms } = it {
            for arm in arms.iter() {
                ctx.store.walk_pats(arm.pat, &mut |pat| {
                    found.insert(pat, *scrutinee);
                });
            }
        }
    }
    found
}

/// Whether `expr` is the value of a block a `let` binds to a variable: `let c = { ..; expr };`.
fn let_bound_block_value(ctx: &Ctx<'_, '_>, expr: ExprId) -> bool {
    use hir_def::hir::Expr;
    let value = |mut at: ExprId| {
        while let Expr::Block { tail: Some(tail), .. } = ctx.store[at] {
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
                    if matches!(ctx.store[*pat], Pat::Bind { subpat: None, .. })
                        && matches!(ctx.store[*init], Expr::Block { .. })
                        && value(*init) == expr
            )
        })
    })
}

/// A literal pattern rustc tests with a switch on the value rather than a call of `PartialEq`.
fn switched_literal(ctx: &Ctx<'_, '_>, span: MirSpan) -> bool {
    use hir_def::hir::{Expr, Literal};
    let MirSpan::PatId(pat) = span else { return false };
    let Pat::Lit(lit) = ctx.store[pat] else { return false };
    matches!(
        ctx.store[lit],
        Expr::Literal(Literal::Bool(_) | Literal::Char(_) | Literal::Int(..) | Literal::Uint(..))
    )
}

fn irrefutable(ctx: &Ctx<'_, '_>, pat: hir_def::hir::PatId) -> bool {
    let struct_like = || {
        matches!(
            ctx.infer.variant_resolution_for_pat(pat),
            Some(hir_def::VariantId::StructId(_) | hir_def::VariantId::UnionId(_))
        )
    };
    match &ctx.store[pat] {
        Pat::Wild | Pat::Rest | Pat::Bind { subpat: None, .. } => true,
        Pat::Bind { subpat: Some(inner), .. }
        | Pat::Ref { pat: inner, .. }
        | Pat::Box { inner }
        | Pat::Deref { inner } => irrefutable(ctx, *inner),
        Pat::Tuple { args, .. } => args.iter().all(|&it| irrefutable(ctx, it)),
        Pat::TupleStruct { args, .. } => {
            struct_like() && args.iter().all(|&it| irrefutable(ctx, it))
        }
        Pat::Record { args, .. } => struct_like() && args.iter().all(|it| irrefutable(ctx, it.pat)),
        _ => false,
    }
}

fn union_literals(ctx: &Ctx<'_, '_>) -> FxHashSet<ExprId> {
    let mut found = FxHashSet::default();
    for (expr, it) in ctx.store.exprs() {
        if let hir_def::hir::Expr::RecordLit { fields, .. } = it {
            found.insert(expr);
            found.extend(fields.iter().map(|field| field.expr));
        }
    }
    found
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

    fn key(self) -> (LocalId, Vec<PlaceElem>) {
        (self.local, self.projection.to_vec())
    }

    fn is_prefix_of(self, other: PlaceRef<'_>) -> bool {
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

struct MoveError {
    place: (LocalId, Vec<PlaceElem>),
    location: Location,
    span: MirSpan,
    kind: MoveErrorKind,
}

struct Checker<'a, 'b, 'db> {
    ctx: &'a Ctx<'b, 'db>,
    body: &'a MirBody<'db>,
    cfg: &'a Cfg,
    moves: &'a MoveData,
    dominators: &'a Dominators,
    uninit: BitSet,
    ever: BitSet,
    /// In the order reported; `None` for one a later error replaced.
    errors: Vec<Option<BorrowckDiagnostic>>,
    access_reported: FxHashSet<((LocalId, Vec<PlaceElem>), MirSpan)>,
    uninit_reported: FxHashSet<LocalId>,
    /// The `E0382` reported for a set of moves, with the place it is about.
    move_reported: FxHashMap<Vec<usize>, ((LocalId, Vec<PlaceElem>), Location, BorrowckDiagnostic)>,
    move_errors: Vec<MoveError>,
    scrutinees: Option<FxHashMap<BindingId, Option<ExprId>>>,
    /// The temporaries that hold a borrow a closure captures, with the span of the closure.
    capture_temps: FxHashMap<LocalId, MirSpan>,
    /// The local the statement being checked assigns, if it assigns one.
    assigned_local: LocalId,
    /// The temporaries that point to a `static mut`.
    static_muts: FxHashSet<LocalId>,
    regions: &'a Regions<'db>,
    region_count: usize,
    values: &'a RegionValues,
    loans: &'a Loans,
    /// The loans in scope where the checker is.
    in_scope: BitSet,
    /// The places a "does not live long enough" error was reported of, with the borrow.
    dead_reported: FxHashSet<(LocalId, MirSpan)>,
    /// The places a two-phase borrow's reservation was reported a conflict of, which its
    /// activation does not report again.
    reservation_reported: FxHashSet<(LocalId, Vec<PlaceElem>)>,
    /// Record literals and the values of their fields, whose lowering assigns a field of the
    /// literal where rustc assigns the whole of it (it matters for a union).
    union_literals: FxHashSet<ExprId>,
    /// The scrutinees of `match`es and the `match`es.
    matched: FxHashSet<ExprId>,
    /// For each variable declared without `mut` borrowed mutably, its error and how many
    /// borrows it is about.
    mut_errors: FxHashMap<LocalId, (usize, usize)>,
    /// In a closure, what it needs of its creator's regions.
    requirements: Vec<regions::Requirement<'db>>,
    /// For each closure this body makes, the captures it needs the `mut` of.
    mut_upvars: &'a FxHashMap<ExprId, Vec<usize>>,
    /// The variables a mutation needs the `mut` of, rustc's `used_mut`.
    used_mut: FxHashSet<LocalId>,
    /// In a closure, the captures a mutation needs the `mut` of.
    used_mut_upvars: Vec<usize>,
}

impl<'a, 'db> Checker<'a, '_, 'db> {
    /// A guard that assigns or mutably borrows what its `match` tests, or a place it is in,
    /// could change which arm matches (E0510); one that touches what its arm binds by value
    /// conflicts with the shared reference it sees that through.
    fn guard_conflict(&mut self, guard: &Guard, place: &StoredPlace, assign: bool, span: MirSpan) {
        let projection = place.projection.as_slice();
        let local = place.local;
        let what = self.quoted(PlaceRef { local, projection }, "value");
        if guard.bindings.contains(&local) {
            // The guard sees the variable through a shared reference, so assigning it is not
            // the reassignment of an immutable variable (E0384) but a mutability error.
            if assign {
                self.access_reported.insert((PlaceRef { local, projection }.key(), span));
                self.report(
                    "E0594",
                    span,
                    format!("cannot assign to {what}, as it is immutable for the pattern guard"),
                );
            } else {
                self.report(
                    "E0596",
                    span,
                    format!(
                        "cannot borrow {what} as mutable, as it is immutable for the pattern guard"
                    ),
                );
            }
        } else if guard.tested.iter().any(|(it, fake)| *it == local && fake.starts_with(projection))
        {
            let message = match assign {
                true => format!("cannot assign {what} in match guard"),
                false => format!("cannot mutably borrow {what} in match guard"),
            };
            self.report("E0510", span, message);
        } else if guard.bound.iter().any(|(it, bound)| {
            *it == local && (bound.starts_with(projection) || projection.starts_with(bound))
        }) {
            match assign {
                true => self.report(
                    "E0506",
                    span,
                    format!("cannot assign to {what} because it is borrowed"),
                ),
                false => self.report(
                    "E0502",
                    span,
                    format!(
                        "cannot borrow {what} as mutable because it is also borrowed as immutable"
                    ),
                ),
            }
        }
    }

    fn index_conflict(
        &mut self,
        protected: &[(LocalId, Vec<PlaceElem>)],
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
            true => format!("cannot assign {what} in indexing expression"),
            false => format!("cannot mutably borrow {what} in indexing expression"),
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

    fn report(&mut self, code: &'static str, span: MirSpan, message: String) {
        use hir_def::hir::{ClosureKind, CoroutineSource, Expr};
        // An `async` closure's future is lowered around its body with no syntax of its own;
        // rustc points at the body.
        let span = match span {
            MirSpan::ExprId(expr)
                if let Expr::Closure {
                    body,
                    closure_kind: ClosureKind::Coroutine { source: CoroutineSource::Closure, .. },
                    ..
                } = self.ctx.store[expr]
                    && let Expr::Block { tail: Some(tail), .. } = self.ctx.store[body] =>
            {
                MirSpan::ExprId(tail)
            }
            _ => span,
        };
        self.errors.push(Some(BorrowckDiagnostic { code, span, message, at_signature: false }));
    }

    fn is_union_literal(&self, place: PlaceRef<'_>, span: MirSpan) -> bool {
        let MirSpan::ExprId(expr) = span else { return false };
        if !matches!(place.projection, [ProjectionElem::Field(_)])
            || !self.union_literals.contains(&expr)
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

    fn describe(&self, place: PlaceRef<'_>) -> Option<String> {
        self.ctx.describe(self.body, place.local, place.projection)
    }

    fn quoted(&self, place: PlaceRef<'_>, otherwise: &str) -> String {
        match self.describe(place) {
            Some(name) => format!("`{name}`"),
            None => otherwise.to_owned(),
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
                        MirSpan::ExprId(expr) => match self.ctx.store[expr] {
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
                    && self.matched.contains(&expr)
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
                    && hir_ty::drop::has_drop_glue(
                        &self.ctx.infcx,
                        self.body.locals[place.local].ty.as_ref(),
                        self.ctx.env,
                    ) == hir_ty::drop::DropGlue::None =>
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
            self.check_for_invalidation_at_unwind();
        }
    }

    /// rustc's `check_for_invalidation_at_exit` at the `resume` its unwind paths end in, which
    /// rust-analyzer's MIR does not have: a terminator that may unwind takes there every loan
    /// in scope, and those whose region outlives a universal one are still in scope at the
    /// `resume`, so each of them is reported, not only the first a `StorageDead` meets.
    fn check_for_invalidation_at_unwind(&mut self) {
        let in_scope: Vec<usize> = self.in_scope.iter().collect();
        for id in in_scope {
            let loan = &self.loans.loans[id];
            if loan.projection.contains(&ProjectionElem::Deref) {
                continue;
            }
            let outlives_universal = self
                .values
                .universals_of(loan.region)
                .any(|it| self.regions.universals[it].1 != UniversalKind::FnBody);
            // The unwind path drops the local the loan is of, a destructor that may see it.
            let borrowed = PlaceRef { local: loan.local, projection: &loan.projection };
            if outlives_universal && self.through_destructor(borrowed) {
                self.report_conflicts_with_destructor(id);
            } else if outlives_universal {
                self.report_does_not_live_long_enough(id);
            }
        }
    }

    /// Whether `span` is a `&raw const` or `&raw mut`, which lowering builds from a borrow.
    fn is_raw_borrow(&self, span: MirSpan) -> bool {
        matches!(span, MirSpan::ExprId(expr) if matches!(
            self.ctx.store[expr],
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
                        "reference to field of packed struct is unaligned".to_owned(),
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
            Rvalue::Aggregate(hir_ty::mir::AggregateKind::Closure(ty), operands) => {
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
            // A binding of a pattern is a new variable each time, where rustc ends the storage
            // of the bindings of a candidate whose guard fails; lowering does not.
            let binds = matches!(span, MirSpan::PatId(_) | MirSpan::BindingId(_));
            if !binds
                && self.ever.contains(place.local.into_raw().into_u32() as usize)
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
        let key = (place.key(), span);
        if self.access_reported.contains(&key) {
            return;
        }
        let mutability_error = self.check_access_permissions(place, span, access, allowed);
        let conflict_error = self.check_access_for_conflict(location, place, span, access, depth);
        if mutability_error || conflict_error {
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
        let in_scope: Vec<usize> = self.in_scope.iter().collect();
        for id in in_scope {
            let loan = &self.loans.loans[id];
            if !places_conflict(
                self.ctx,
                self.body,
                loan,
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
                            self.report_conflicting_borrow(place, span, kind, id)
                        }
                        _ => {
                            let what = self.quoted(place, "value");
                            self.report(
                                "E0503",
                                span,
                                format!("cannot use {what} because it was mutably borrowed"),
                            );
                        }
                    }
                }
                (Access::MutableBorrow { closure_capture, two_phase }, _) => {
                    if two_phase {
                        self.reservation_reported.insert(place.key());
                    }
                    let kind = match (closure_capture, two_phase) {
                        (true, _) => MutBorrowKind::ClosureCapture,
                        (false, true) => MutBorrowKind::TwoPhasedBorrow,
                        (false, false) => MutBorrowKind::Default,
                    };
                    self.report_conflicting_borrow(place, span, BorrowKind::Mut { kind }, id);
                }
                (Access::Activation(activating), _) => {
                    let kind = self.loans.loans[activating].kind;
                    self.report_conflicting_borrow(place, span, kind, id);
                }
                (Access::Mutate, _) => {
                    let what = self.quoted(place, "value");
                    self.report(
                        "E0506",
                        span,
                        format!("cannot assign to {what} because it is borrowed"),
                    );
                }
                (Access::Move, _) => {
                    let what = self.quoted(place, "value");
                    self.report(
                        "E0505",
                        span,
                        format!("cannot move out of {what} because it is borrowed"),
                    );
                }
                (Access::StorageDeadOrDrop, _) => {
                    let loan = &self.loans.loans[id];
                    let borrowed = PlaceRef { local: loan.local, projection: &loan.projection };
                    if self.through_destructor(borrowed) && !borrowed.is_prefix_of(place) {
                        self.report_conflicts_with_destructor(id);
                    } else {
                        self.report_does_not_live_long_enough(id);
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
        let Some(ids) = self.loans.activations.get(&location) else { return };
        for &id in ids {
            let loan = &self.loans.loans[id];
            let projection = loan.projection.clone();
            let place = PlaceRef { local: loan.local, projection: &projection };
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
    fn check_for_invalidation_at_exit(&mut self, _location: Location, _span: MirSpan) {
        let in_scope: Vec<usize> = self.in_scope.iter().collect();
        for id in in_scope {
            if !self.loans.loans[id].projection.contains(&ProjectionElem::Deref) {
                self.report_does_not_live_long_enough(id);
            }
        }
    }

    fn report_conflicting_borrow(
        &mut self,
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
                    format!(
                        "cannot borrow {what} as {} because previous closure requires unique access",
                        mutability(new)
                    ),
                )
            }
            (
                BorrowKind::Mut { kind: ClosureCapture },
                BorrowKind::Mut { kind: ClosureCapture },
            ) => {
                ("E0524", format!("two closures require unique access to {what} at the same time"))
            }
            (BorrowKind::Mut { kind: ClosureCapture }, _) => (
                "E0500",
                format!("closure requires unique access to {what} but it is already borrowed"),
            ),
            (BorrowKind::Mut { .. }, BorrowKind::Mut { .. }) => {
                ("E0499", format!("cannot borrow {what} as mutable more than once at a time"))
            }
            _ => (
                "E0502",
                format!(
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
        self.report(code, span, message);
    }

    /// rustc's `classify_drop_access_kind` being `Destructor`: the place is reached through a
    /// field of a value whose type implements `Drop`.
    fn through_destructor(&self, place: PlaceRef<'_>) -> bool {
        let mut ty = self.ctx.place_ty(self.body, place.local, &[]);
        for elem in place.projection {
            if let ProjectionElem::Field(_) | ProjectionElem::Downcast(_) = elem
                && let TyKind::Adt(def, _) = ty.ty.kind()
                && self.ctx.has_destructor(def.def_id())
            {
                return true;
            }
            ty = self.ctx.project(ty, elem);
        }
        false
    }

    /// rustc's `report_borrow_conflicts_with_destructor`.
    fn report_conflicts_with_destructor(&mut self, id: usize) {
        let loan = &self.loans.loans[id];
        if !self.dead_reported.insert((loan.local, loan.span)) {
            return;
        }
        self.report(
            "E0713",
            loan.span,
            "borrow may still be in use when destructor runs".to_owned(),
        );
    }

    /// rustc's `report_borrowed_value_does_not_live_long_enough`.
    fn report_does_not_live_long_enough(&mut self, id: usize) {
        let loan = &self.loans.loans[id];
        if !self.dead_reported.insert((loan.local, loan.span)) {
            return;
        }
        let projection = loan.projection.clone();
        let place = PlaceRef { local: loan.local, projection: &projection };
        let capture = self.capture_temps.get(&loan.assigned).copied();
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
                        self.report(
                            "E0373",
                            closure,
                            format!("closure may outlive the current function, but it borrows {what}, which is owned by the current function"),
                        );
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
                        let message = match self.describe(place) {
                            Some(_) if place.projection.is_empty() => {
                                "cannot return reference to local variable".to_owned()
                            }
                            _ => "returns a value referencing data owned by the current function"
                                .to_owned(),
                        };
                        self.report("E0515", span, message);
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
                self.report(
                    "E0373",
                    MirSpan::ExprId(closure),
                    format!("closure may outlive the current block, but it borrows {what}, which is owned by the current block"),
                );
            }
            Some(name) => {
                self.report("E0597", loan.span, format!("`{name}` does not live long enough"))
            }
            // At the temporary, as rustc's: a pattern's borrow, `match Some(s()) {
            // Some(ref m) => .. }`, at `Some(s())`, not at `ref m`.
            None => {
                let span = match loan.span {
                    MirSpan::ExprId(_) => loan.span,
                    _ => temporary_span(self.body, loan.local).unwrap_or(loan.span),
                };
                self.report("E0716", span, "temporary value dropped while borrowed".to_owned())
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
        let mut higher_ranked = FxHashSet::default();
        for (longer, &(longer_rv, kind)) in universals.iter().enumerate() {
            if matches!(kind, UniversalKind::Static | UniversalKind::FnBody) {
                continue;
            }
            // As rustc, only the representative of the universal regions that outlive each
            // other is checked against all it outlives; the others only against it.
            let failing: Vec<usize> = match self.representative(longer) {
                Some(representative) => Some(representative)
                    .filter(|&it| !self.regions.knows(longer, it))
                    .into_iter()
                    .collect(),
                None => self
                    .values
                    .universals_of(longer_rv)
                    .filter(|&it| !self.regions.knows(longer, it))
                    .collect(),
            };
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
            let (code, message) = if placeholder {
                ("", "higher-ranked lifetime error".to_owned())
            } else if escapes
                && blamed.category == Category::Return
                && self.regions.closure_env == Some(longer)
                && self.ctx.closure_kind == Some(rustc_type_ir::ClosureKind::FnMut)
                && !self.async_closure()
            {
                ("", "captured variable cannot escape `FnMut` closure body".to_owned())
            } else if escapes
                && matches!(blamed.category, Category::Assignment | Category::CallArgument)
                && (self.closure_variable_names(longer_rv)
                    || self.closure_variable_names(shorter_rv))
            {
                ("E0521", "borrowed data escapes outside of closure".to_owned())
            } else if !self.regions.is_impl_item
                && !in_closure
                && (named_anon(kind, shorter_kind) || named_anon(shorter_kind, kind))
                && !self.regions.unsuggested.contains(&if kind == UniversalKind::AnonParam {
                    longer
                } else {
                    shorter
                })
            {
                ("E0621", "explicit lifetime required in the type of a parameter".to_owned())
            } else if blamed.category == Category::CallArgument
                && shorter_kind == UniversalKind::Static
                && self.parameter_names(longer_rv)
            {
                ("E0521", format!("borrowed data escapes outside of {}", self.owner_descr()))
            } else {
                ("", "lifetime may not live long enough".to_owned())
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
                        matches!(it, hir_def::hir::Expr::Call { callee: c, .. } if *c == callee)
                            .then_some(MirSpan::ExprId(call))
                    })
                    .unwrap_or(span),
                _ => span,
            };
            if placeholder && !higher_ranked.insert(span) {
                continue;
            }
            if blamed.category == Category::Signature {
                self.errors.push(Some(BorrowckDiagnostic {
                    code,
                    span,
                    message,
                    at_signature: true,
                }));
                continue;
            }
            self.report(code, span, message);
        }
        let mut reported = FxHashSet::default();
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
                        TyKind::Alias(_) => "associated type",
                        _ => "parameter type",
                    };
                    let (code, message) = match kind {
                        _ if test.forall => ("", format!("the {what} does not live long enough")),
                        UniversalKind::Static => {
                            ("E0310", format!("the {what} may not live long enough"))
                        }
                        UniversalKind::Named => {
                            ("E0309", format!("the {what} may not live long enough"))
                        }
                        _ => ("E0311", format!("the {what} may not live long enough")),
                    };
                    self.errors.push(Some(BorrowckDiagnostic {
                        code,
                        span: test.span,
                        message,
                        at_signature: false,
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
        let longer_minus: Vec<usize> = if non_local(longer) {
            vec![longer]
        } else {
            let lower: Vec<usize> =
                (0..universals.len()).filter(|&it| non_local(it) && knows(longer, it)).collect();
            lower
                .iter()
                .copied()
                .filter(|&it| {
                    !lower.iter().any(|&other| other != it && knows(other, it) && !knows(it, other))
                })
                .collect()
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
                let outlived: Vec<usize> =
                    shorter_plus.iter().copied().filter(|&it| knows(minus, it)).collect();
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
    fn non_local_upper_bounds(&self, universal: usize) -> Vec<usize> {
        let known = self.regions;
        let universals = &known.universals;
        let non_local =
            |it: usize| matches!(universals[it].1, UniversalKind::Creator | UniversalKind::Static);
        if non_local(universal) {
            return vec![universal];
        }
        let upper: Vec<usize> = (0..universals.len())
            .filter(|&it| non_local(it) && known.knows(it, universal))
            .collect();
        upper
            .iter()
            .copied()
            .filter(|&it| {
                !upper
                    .iter()
                    .any(|&other| other != it && known.knows(it, other) && !known.knows(other, it))
            })
            .collect()
    }

    /// The expression a block's value comes from, where rustc points at a returned value.
    fn tail(&self, span: MirSpan) -> MirSpan {
        let mut span = span;
        while let MirSpan::ExprId(expr) = span
            && let hir_def::hir::Expr::Block { tail: Some(tail), .. } = self.ctx.store[expr]
        {
            span = MirSpan::ExprId(tail);
        }
        span
    }

    /// rustc's `eval_verify_bound` for `ty: lower`, `ty` a type parameter: one of its bounds,
    /// or the function body every parameter outlives, outlives `lower` as region inference
    /// left them.
    fn type_test_holds(&self, ty: Ty<'db>, lower: usize) -> bool {
        if let TyKind::Alias(alias) = ty.kind() {
            return self.alias_test_holds(ty, alias, lower, 0);
        }
        let fn_body = self.regions.universals[1].0;
        let needed: Vec<usize> = self.values.universals_of(lower).collect();
        self.regions.type_bounds_of(ty).into_iter().chain([fn_body]).any(|bound| {
            let have: Vec<usize> = self.values.universals_of(bound).collect();
            needed
                .iter()
                .all(|&shorter| have.iter().any(|&longer| self.regions.knows(longer, shorter)))
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
        let needed: Vec<usize> = self.values.universals_of(lower).collect();
        let outlives = |bound: usize| {
            let have: Vec<usize> = self.values.universals_of(bound).collect();
            needed
                .iter()
                .all(|&shorter| have.iter().any(|&longer| self.regions.knows(longer, shorter)))
        };
        let erase = |ty: Ty<'db>| {
            rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner))
        };
        let erased = erase(ty);
        let regions_of = |ty: Ty<'db>| {
            let mut out = Vec::new();
            rustc_type_ir::fold_regions(interner, ty, |region, _| {
                out.push(match region.kind() {
                    RegionKind::ReStatic => Some(self.regions.static_rv),
                    _ => regions::rv_of(region),
                });
                region
            });
            out
        };
        let knows_all = |longer: usize, shorter: usize| {
            let have: Vec<usize> = self.values.universals_of(longer).collect();
            self.values
                .universals_of(shorter)
                .all(|it| have.iter().any(|&longer| self.regions.knows(longer, it)))
        };
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
        for clause in rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id())
            .iter_instantiated(interner, alias.args)
        {
            if let ClauseKind::TypeOutlives(OutlivesPredicate(bounded, region)) =
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
        let mut components = Vec::new();
        let mut params = Vec::new();
        let captured = match alias.kind {
            rustc_type_ir::AliasTyKind::Opaque { .. } => {
                regions::opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len())
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
                TyKind::Alias(inner) => self.alias_test_holds(it, inner, lower, depth + 1),
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
        let every: Vec<usize>;
        let fields = match self.mut_upvars.get(&expr) {
            Some(it) => it.as_slice(),
            None => {
                every = (0..operands.len()).collect();
                &every
            }
        };
        for &field in fields {
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
        self.used_mut.insert(place.local);
    }

    /// rustc's `lint_unused_mut`: the variables declared `mut` that no mutation needs the `mut`
    /// of. As rustc's `gather_used_muts`, a variable moved into a temporary that needs its
    /// `mut` needs it too, and one never assigned is left to other lints.
    fn unused_mut(&mut self) -> Vec<BorrowckDiagnostic> {
        // rustc lints no body its type check found an error in, and a type rust-analyzer could
        // not find may be such an error, as an array length whose evaluation fails.
        if self.ctx.infer.has_errors {
            return Vec::new();
        }
        let body = self.body;
        let temporaries: FxHashSet<LocalId> = self
            .used_mut
            .iter()
            .copied()
            .filter(|it| !self.ctx.bindings.contains_key(it))
            .collect();
        let mut assigned = FxHashSet::default();
        let mut moved_into_temporary = |operand: &Operand| {
            if let OperandKind::Move(moved) = &operand.kind
                && moved.projection.as_slice().is_empty()
            {
                self.used_mut.insert(moved.local);
            }
        };
        for &block in &self.cfg.order {
            let data = &body.basic_blocks[block];
            for statement in &data.statements {
                if let StatementKind::Assign(place, rvalue) = &statement.kind {
                    assigned.insert(place.local);
                    if temporaries.contains(&place.local) {
                        rvalue.for_each_operand(&mut moved_into_temporary);
                    }
                }
            }
            if let Some(TerminatorKind::Call { destination, args, .. }) =
                data.terminator.as_ref().map(|it| &it.kind)
            {
                assigned.insert(destination.local);
                if temporaries.contains(&destination.local) {
                    args.iter().for_each(&mut moved_into_temporary);
                }
            }
        }
        // An `async fn` or `async` closure moves its arguments into the future it makes, whose
        // body binds them as its own variables.
        let moves_arguments = match self.ctx.closure_expr {
            Some(expr) => matches!(
                self.ctx.store[expr],
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
        let mut unused: Vec<(LocalId, BindingId)> = self
            .ctx
            .bindings
            .iter()
            .map(|(&local, &binding)| (local, binding))
            .filter(|&(local, binding)| {
                let data = &self.ctx.store[binding];
                data.mode == BindingAnnotation::Mutable
                    && !data.name.as_str().starts_with('_')
                    && !self.used_mut.contains(&local)
                    && self.ctx.upvar_of_local(local).is_none()
                    && match body.param_locals.contains(&local) {
                        true => !moves_arguments,
                        false => assigned.contains(&local),
                    }
            })
            .collect();
        unused.sort_by_key(|&(local, _)| local.into_raw().into_u32());
        let mut seen = FxHashSet::default();
        unused.retain(|&(_, binding)| seen.insert(binding));
        unused
            .into_iter()
            .map(|(_, binding)| BorrowckDiagnostic {
                code: "unused_mut",
                span: MirSpan::BindingId(binding),
                message: "variable does not need to be mutable".to_owned(),
                at_signature: false,
            })
            .collect()
    }

    /// rustc's `add_used_mut`: a mutation that needs the `mut` of the variable `local`, which
    /// may have been initialized before, or of what a closure captures.
    fn add_used_mut(&mut self, local: LocalId, root: Root) {
        match root {
            Root::Local(allowed)
                if allowed != Allowed::Yes
                    && self.ever.contains(local.into_raw().into_u32() as usize) =>
            {
                self.used_mut.insert(local);
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
                if base.projection.is_empty() && self.static_muts.contains(&base.local) {
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
                Access::Mutate => {
                    ", as `Fn` closures cannot mutate their captured variables".to_owned()
                }
                _ => ", as it is a captured variable in a `Fn` closure".to_owned(),
            }
        } else {
            let root_place = place.base(root.min(place.projection.len()));
            match root_place.projection.last() {
                Some(ProjectionElem::Deref) => {
                    let base = root_place.base(root_place.projection.len() - 1);
                    let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                    if matches!(ty.kind(), TyKind::RawPtr(..)) {
                        ", which is behind a `*const` pointer".to_owned()
                    } else if self.describe(root_place).is_some() {
                        ", which is behind a `&` reference".to_owned()
                    } else {
                        " as it is behind a `&` reference".to_owned()
                    }
                }
                _ => ", as it is not declared as mutable".to_owned(),
            }
        };
        let message = match access {
            Access::Mutate => format!("cannot {verb} {name}{reason}"),
            _ => format!("cannot {verb} {name} as mutable{reason}"),
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
                if *count == 2
                    && let Some(error) = &mut self.errors[*index]
                {
                    error.span = MirSpan::BindingId(binding);
                }
                return;
            }
            self.mut_errors.insert(place.local, (self.errors.len(), 1));
        }
        self.report(code, span, message);
    }

    fn report_illegal_reassignment(&mut self, place: PlaceRef<'_>, span: MirSpan) {
        let name = self.describe(place).unwrap_or_default();
        let message = if self.body.param_locals.contains(&place.local) {
            format!("cannot assign to immutable argument `{name}`")
        } else {
            format!("cannot assign twice to immutable variable `{name}`")
        };
        self.report("E0384", span, message);
    }

    /// The path of `place`, or the nearest above it, with how much of `place` it covers.
    fn move_path_closest_to(&self, place: PlaceRef<'_>) -> (usize, Mpi) {
        let path = self.moves.find(self.ctx, self.body, place.local, place.projection).path();
        (self.moves.paths[path].projection.len(), path)
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
                let uninit = &self.uninit;
                let moved = self.moves.paths[array].children.iter().find_map(|&child| {
                    match self.moves.paths[child].projection.last() {
                        Some(ProjectionElem::ConstantIndex { offset, .. })
                            if range.contains(offset) =>
                        {
                            self.moves.find_in_descendants(child, &|it| uninit.contains(it))
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
            let uninit = &self.uninit;
            if let Some(moved) = self.moves.find_in_descendants(path, &|it| uninit.contains(it)) {
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
            && self.moves.moves_of[path]
                .iter()
                .any(|&it| self.is_predecessor_of(self.moves.moves[it].location, location))
        {
            return;
        }
        self.report_use_of_moved_or_uninitialized(
            location,
            InitAction::PartialAssignment,
            base,
            span,
            path,
        );
    }

    fn is_predecessor_of(&self, from: Location, to: Location) -> bool {
        if from.block == to.block && from.statement < to.statement {
            return true;
        }
        let mut seen = vec![false; self.body.basic_blocks.len()];
        let mut stack: Vec<_> = self.cfg.successors[index(from.block)].to_vec();
        while let Some(block) = stack.pop() {
            if block == to.block {
                return true;
            }
            if std::mem::replace(&mut seen[index(block)], true) {
                continue;
            }
            stack.extend(self.cfg.successors[index(block)].iter().copied());
        }
        false
    }

    fn check_movable_place(&mut self, location: Location, place: PlaceRef<'_>, span: MirSpan) {
        let mut ty = hir_ty::mir::PlaceTy::from_ty(self.body.locals[place.local].ty.as_ref());
        if ty.ty.references_non_lt_error() {
            return;
        }
        if self.ctx.moves_out_of_guard(place.local, place.projection, span) {
            let moved = self.ctx.place_ty(self.body, place.local, place.projection).ty;
            if !moved.references_non_lt_error() && !self.ctx.is_copy(moved) {
                let kind = MoveErrorKind::Guard;
                self.move_errors.push(MoveError { place: place.key(), location, span, kind });
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
                self.move_errors.push(MoveError { place: place.key(), location, span, kind });
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
        let move_outs = self.moved_indexes(location, path);
        if move_outs.is_empty() {
            // rustc reports only variables: lowering leaves some temporaries unassigned, as
            // the `()` an assignment evaluates to.
            if self.ctx.local_name(used.local).is_none() || !self.uninit_reported.insert(used.local)
            {
                return;
            }
            let name = self.quoted(used, "value");
            let message = match action {
                InitAction::PartialAssignment | InitAction::Assignment => {
                    format!("partially assigned binding {name} isn't fully initialized")
                }
                _ => format!("{} binding {name} isn't initialized", action.as_general_verb()),
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
                            Rvalue::Aggregate(hir_ty::mir::AggregateKind::Closure(_), _)
                        )
                    )
                })
                .map(|it| it.span);
            let span =
                self.capture_temps.get(&self.assigned_local).copied().or(made).unwrap_or(span);
            self.report("E0381", span, message);
            return;
        }
        if let Some((reported, _, _)) = self.move_reported.get(&move_outs) {
            let reported = PlaceRef { local: reported.0, projection: &reported.1 };
            if used.is_prefix_of(reported) {
                return;
            }
        }
        let moved = self.moves.paths[path].projection.len() < used.projection.len()
            || self.moves.paths[path].local != used.local;
        let partially = !moved && self.moves.paths[path].projection.len() > used.projection.len();
        let name = self.quoted(used, "value");
        let what = if partially { "partially moved value" } else { "moved value" };
        let message = format!("{} of {what}: {name}", action.noun());
        let diagnostic = BorrowckDiagnostic { code: "E0382", span, message, at_signature: false };
        self.move_reported.insert(move_outs, (used.key(), location, diagnostic));
    }

    /// The moves out of `path` or a path above it that reach `location` without an init in
    /// between, as rustc's `get_moved_indexes`.
    fn moved_indexes(&self, location: Location, path: Mpi) -> Vec<usize> {
        let mut paths = vec![path];
        let mut up = self.moves.paths[path].parent;
        while let Some(parent) = up {
            paths.push(parent);
            up = self.moves.paths[parent].parent;
        }
        let mut stack = Vec::new();
        let mut back_edges = Vec::new();
        for predecessor in self.cfg.predecessor_locations(self.body, location) {
            if self.dominators.location_dominates(location, predecessor) {
                back_edges.push(predecessor);
            } else {
                stack.push(predecessor);
            }
        }
        let is_argument = self
            .body
            .param_locals
            .iter()
            .any(|&local| paths.contains(&self.moves.local_path(local)));
        let mut visited = FxHashSet::default();
        let mut result = Vec::new();
        let mut reached_start = false;
        let mut visit = |result: &mut Vec<usize>, location: Location| -> bool {
            if !visited.insert(location) {
                return true;
            }
            let is_storage_dead = matches!(
                self.body.basic_blocks[location.block].statements.get(location.statement),
                Some(statement) if matches!(statement.kind, StatementKind::StorageDead(_))
            );
            if !is_storage_dead && let Some(at) = self.moves.moves_at.get(&location) {
                for &move_out in at {
                    if paths.contains(&self.moves.moves[move_out].path) {
                        result.push(move_out);
                        return true;
                    }
                }
            }
            if let Some(at) = self.moves.inits_at.get(&location) {
                for &init in at {
                    let init = self.moves.inits[init];
                    if matches!(init.kind, InitKind::Deep | InitKind::NonPanicPathOnly)
                        && paths.contains(&init.path)
                    {
                        return true;
                    }
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
        if (is_argument || !reached_start) && result.is_empty() {
            while let Some(location) = back_edges.pop() {
                if visit(&mut result, location) {
                    continue;
                }
                back_edges.extend(self.cfg.predecessor_locations(self.body, location));
            }
        }
        result.sort_unstable();
        result.dedup();
        result
    }

    /// The expression a variable's pattern matches on, for each variable a `match`, a
    /// `let` with a pattern or an `if let` binds.
    fn scrutinee_of(&mut self, binding: BindingId) -> Option<Option<ExprId>> {
        let store = self.ctx.store;
        let map = self.scrutinees.get_or_insert_with(|| {
            let mut map = FxHashMap::default();
            let mut add = |pat, scrutinee: ExprId, is_let: bool| {
                let simple = is_let && matches!(store[pat], Pat::Bind { subpat: None, .. });
                store.walk_bindings_in_pat(pat, |binding| {
                    map.insert(binding, if simple { None } else { Some(scrutinee) });
                });
            };
            for (_, expr) in store.exprs() {
                match expr {
                    hir_def::hir::Expr::Match { expr, arms } => {
                        for arm in arms.iter() {
                            add(arm.pat, *expr, false);
                        }
                    }
                    hir_def::hir::Expr::Let { pat, expr } => add(*pat, *expr, false),
                    hir_def::hir::Expr::Block { statements, .. } => {
                        for statement in statements.iter() {
                            if let Statement::Let { pat, initializer: Some(init), .. } = statement {
                                add(*pat, *init, true);
                            }
                        }
                    }
                    _ => (),
                }
            }
            map
        });
        map.get(&binding).copied()
    }

    fn report_move_errors(&mut self) {
        let errors = std::mem::take(&mut self.move_errors);
        let mut reported_groups: FxHashSet<(ExprId, LocalId, Vec<PlaceElem>)> =
            FxHashSet::default();
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
                match self.scrutinee_of(binding) {
                    Some(Some(scrutinee)) => {
                        let len = match error.kind {
                            MoveErrorKind::Borrowed(len)
                            | MoveErrorKind::Destructor(len)
                            | MoveErrorKind::SliceOrArray(len) => len,
                            MoveErrorKind::Guard => 0,
                        };
                        let base = error.place.1[..len.min(error.place.1.len())].to_vec();
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

    fn is_slice_or_array(&self, error: &MoveError, len: usize) -> bool {
        let ty = self.ctx.place_ty(self.body, error.place.0, &error.place.1[..len]).ty;
        matches!(ty.kind(), TyKind::Slice(_) | TyKind::Array(..))
    }

    /// A slice or array type as rustc writes it: `[A; 1]`, not the length's representation.
    fn array_name(&self, ty: Ty<'_>) -> String {
        match ty.kind() {
            TyKind::Array(element, len) => {
                match hir_ty::consteval::try_const_usize(self.ctx.db, len) {
                    Some(len) => format!("[{element:?}; {len}]"),
                    None => format!("[{element:?}; _]"),
                }
            }
            _ => format!("{ty:?}"),
        }
    }

    fn move_error_message(&self, error: &MoveError) -> String {
        let place = PlaceRef { local: error.place.0, projection: &error.place.1 };
        match error.kind {
            MoveErrorKind::Borrowed(len) if self.is_slice_or_array(error, len + 1) => {
                let ty = self.ctx.place_ty(self.body, place.local, &place.projection[..len + 1]).ty;
                let kind = if matches!(ty.kind(), TyKind::Slice(_)) { "slice" } else { "array" };
                format!("cannot move out of type `{}`, a non-copy {kind}", self.array_name(ty))
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
                    Some(name) => format!("cannot move out of `{name}` which is behind {behind}"),
                    None => format!("cannot move out of {behind}"),
                }
            }
            MoveErrorKind::Destructor(len) => {
                let base = place.base(len);
                let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                format!("cannot move out of type `{ty:?}`, which implements the `Drop` trait")
            }
            MoveErrorKind::Guard => {
                format!("cannot move out of {} in pattern guard", self.quoted(place, "value"))
            }
            MoveErrorKind::SliceOrArray(len) => {
                let base = place.base(len);
                let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
                let kind = if matches!(ty.kind(), TyKind::Slice(_)) { "slice" } else { "array" };
                format!("cannot move out of type `{}`, a non-copy {kind}", self.array_name(ty))
            }
        }
    }
}

/// What a `match` arm's guard must leave alone: the places the `match` tests and the
/// references they are behind, which rustc fake-borrows shallowly through every guard, and
/// what the arm binds by value, which a guard sees through shared references, as it sees
/// the arm's bindings.
#[derive(Default)]
struct Guard {
    tested: Vec<(LocalId, Vec<PlaceElem>)>,
    bound: Vec<(LocalId, Vec<PlaceElem>)>,
    bindings: Vec<LocalId>,
}

/// The [`Guard`] of each expression and pattern in a `match` arm's guard.
fn guard_borrows(ctx: &Ctx<'_, '_>, body: &MirBody<'_>) -> FxHashMap<MirSpan, std::rc::Rc<Guard>> {
    use hir_def::hir::Expr;
    let mut tested_by = FxHashMap::default();
    let mut bound_by = FxHashMap::default();
    let mut matches = Vec::new();
    let mut arms_of = Vec::new();
    for (_, expr) in ctx.store.exprs() {
        let Expr::Match { arms, .. } = expr else { continue };
        if arms.iter().all(|arm| arm.guard.is_none()) {
            continue;
        }
        let mut guarded = Vec::new();
        for arm in arms.iter() {
            ctx.store.walk_pats(arm.pat, &mut |pat| {
                tested_by.insert(pat, matches.len());
                if matches!(ctx.store[pat], Pat::Bind { .. }) {
                    bound_by.insert(pat, (matches.len(), arm.guard.map(|_| arms_of.len())));
                }
            });
            if let Some(guard) = arm.guard {
                guarded.push((arms_of.len(), guard));
                arms_of.push((Vec::new(), Vec::new()));
            }
        }
        matches.push((Vec::new(), guarded));
    }
    let mut out = FxHashMap::default();
    if matches.is_empty() {
        return out;
    }
    let add = |set: &mut Vec<(LocalId, Vec<PlaceElem>)>, local, projection: &[PlaceElem]| {
        if !set.iter().any(|(it, other)| *it == local && other == projection) {
            set.push((local, projection.to_vec()));
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
                            add(&mut matches[index].0, place.local, &projection[..len]);
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
            let tested: Vec<&StoredPlace> = match rvalue {
                Rvalue::Discriminant(place) | Rvalue::Len(place) => vec![place],
                Rvalue::CheckedBinaryOp(_, a, b) => [a, b]
                    .into_iter()
                    .filter_map(|operand| match &operand.kind {
                        OperandKind::Copy(place) => Some(place),
                        _ => None,
                    })
                    .collect(),
                _ => continue,
            };
            let set = &mut matches[index].0;
            for place in tested {
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
    for (tested, guarded) in matches {
        for (arm, guard) in guarded {
            let (bound, bindings) = std::mem::take(&mut arms_of[arm]);
            if tested.is_empty() && bound.is_empty() && bindings.is_empty() {
                continue;
            }
            let it = std::rc::Rc::new(Guard { tested: tested.clone(), bound, bindings });
            let mut stack = vec![guard];
            while let Some(expr) = stack.pop() {
                out.insert(MirSpan::ExprId(expr), it.clone());
                let mut pats = Vec::new();
                match &ctx.store[expr] {
                    Expr::Match { arms, .. } => pats.extend(arms.iter().map(|arm| arm.pat)),
                    Expr::Let { pat, .. } => pats.push(*pat),
                    Expr::Block { statements, .. } => {
                        pats.extend(statements.iter().filter_map(|statement| match statement {
                            Statement::Let { pat, .. } => Some(*pat),
                            _ => None,
                        }))
                    }
                    _ => {}
                }
                for pat in pats {
                    ctx.store.walk_pats(pat, &mut |pat| {
                        out.insert(MirSpan::PatId(pat), it.clone());
                    });
                }
                ctx.store.walk_child_exprs(expr, |it| stack.push(it));
            }
        }
    }
    out
}

/// The places rustc fake-borrows shallowly while an index is evaluated, by each expression in
/// that index: in `x[1][i]` with `x: &[&[i32]]`, the bounds check of the slice `x[1]` indexes
/// must hold until `i` is evaluated, so the references it is behind, `x`, are left alone
/// (rustc's `add_fake_borrows_of_base`).
fn index_borrows<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
) -> FxHashMap<MirSpan, Vec<(LocalId, Vec<PlaceElem>)>> {
    use hir_def::hir::Expr;
    fn operand(it: &Operand) -> Option<&StoredPlace> {
        match &it.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => Some(place),
            _ => None,
        }
    }
    let mut evaluated_in = FxHashMap::default();
    let mut places: Vec<&StoredPlace> = Vec::new();
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
    let mut protected: FxHashMap<ExprId, Vec<(LocalId, Vec<PlaceElem>)>> = FxHashMap::default();
    let mut seen = FxHashSet::default();
    for place in places {
        let projection = place.projection.as_slice();
        let indexes: Vec<(usize, LocalId)> = projection
            .iter()
            .enumerate()
            .filter_map(|(at, elem)| match elem {
                ProjectionElem::Index(local) => Some((at, *local)),
                _ => None,
            })
            .collect();
        if indexes.len() < 2 || !seen.insert((place.local, projection)) {
            continue;
        }
        for (n, &(at, _)) in indexes[..indexes.len() - 1].iter().enumerate() {
            if !is_slice(place.local, &projection[..at]) {
                continue;
            }
            let mut fake = Vec::new();
            for len in (0..at).rev() {
                match &projection[len] {
                    ProjectionElem::Deref => fake.push((place.local, projection[..len].to_vec())),
                    ProjectionElem::Index(_) if is_slice(place.local, &projection[..len]) => break,
                    _ => (),
                }
            }
            for &(_, local) in &indexes[n + 1..] {
                if let Some(&expr) = evaluated_in.get(&local)
                    && let Expr::Index { index, .. } = ctx.store[expr]
                {
                    let set = protected.entry(index).or_default();
                    for it in &fake {
                        if !set.contains(it) {
                            set.push(it.clone());
                        }
                    }
                }
            }
        }
    }
    let mut out: FxHashMap<MirSpan, Vec<(LocalId, Vec<PlaceElem>)>> = FxHashMap::default();
    for (index, fake) in protected {
        let mut stack = vec![index];
        while let Some(expr) = stack.pop() {
            let set = out.entry(MirSpan::ExprId(expr)).or_default();
            for it in &fake {
                if !set.contains(it) {
                    set.push(it.clone());
                }
            }
            ctx.store.walk_child_exprs(expr, |it| stack.push(it));
        }
    }
    out
}

fn capture_temps(body: &MirBody<'_>) -> FxHashMap<LocalId, MirSpan> {
    let variables: FxHashSet<LocalId> = body.binding_locals.values().copied().collect();
    let mut temps = FxHashMap::default();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(
                _,
                Rvalue::Aggregate(hir_ty::mir::AggregateKind::Closure(_), operands),
            ) = &statement.kind
            {
                for operand in operands.iter() {
                    if let OperandKind::Move(place) = &operand.kind
                        && place.projection.as_slice().is_empty()
                        && !variables.contains(&place.local)
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

fn static_muts(ctx: &Ctx<'_, '_>, body: &MirBody<'_>) -> FxHashSet<LocalId> {
    let mut locals = FxHashSet::default();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(
                place,
                Rvalue::Use(Operand { kind: OperandKind::Static(id), .. }),
            ) = &statement.kind
                && place.projection.as_slice().is_empty()
                && hir_def::signatures::StaticSignature::of(ctx.db, *id)
                    .flags
                    .contains(hir_def::signatures::StaticFlags::MUTABLE)
            {
                locals.insert(place.local);
            }
        }
    }
    locals
}

fn operand_place(operand: &Operand) -> Option<PlaceRef<'_>> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => Some(PlaceRef::of(place)),
        _ => None,
    }
}
