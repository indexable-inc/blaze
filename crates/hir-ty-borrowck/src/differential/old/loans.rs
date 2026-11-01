//! Loans, as rustc's `borrow_set.rs`, the `Borrows` analysis of `dataflow.rs` and
//! `places_conflict.rs`: each borrow is a loan of a place, in scope from where it is made to
//! where its region stops holding, or the reference to it is overwritten, or the place it
//! borrows goes away.

use blaze_hash::{HashMap, HashSet};
use hir_def::{
    DefWithBodyId,
    hir::{Array, Expr},
};
use rustc_type_ir::inherent::IntoKind;
use smallvec::SmallVec;

use hir_ty::{
    InferBodyId,
    mir::{
        BasicBlockId, BorrowKind, LocalId, MirBody, MirSpan, MutBorrowKind, OperandKind, PlaceElem,
        ProjectionElem, Rvalue, StatementKind, TerminatorKind,
    },
    next_solver::TyKind,
};

use super::{
    Ctx,
    bitset::{BitMatrix, BitSet, Transfer},
    cfg::{Cfg, Location, index},
    dataflow::{GenKill, forward},
    regions::{RegionValues, Regions, Rv},
    relation::{Facts, Relation},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Activation {
    NotTwoPhase,
    NotActivated,
    At(Location),
}

pub(crate) struct Loan {
    pub(crate) local: LocalId,
    /// The range of [`Loans::projections`] the projection of the place borrowed is.
    projection: (u32, u32),
    pub(crate) kind: BorrowKind,
    pub(crate) region: Rv,
    pub(crate) location: Location,
    pub(crate) span: MirSpan,
    /// The local the reference is assigned to.
    pub(crate) assigned: LocalId,
    pub(crate) activation: Activation,
}

// One per borrow, read when a conflict is found; the sets in scope are bit indices into
// `Loans::loans`, so the dataflow never touches it.
const _: () = assert!(size_of::<Loan>() <= 88);

/// The kinds of the facts of [`Loans::index`]: the loan made at a point, the loans of a
/// local, the two-phase loans activated at a point.
const MADE_AT: u8 = 0;
const OF_LOCAL: u8 = 1;
const ACTIVATED_AT: u8 = 2;
/// The kind of the facts of [`Loans::scopes`]: the loans that go out of scope as a point
/// starts.
const ENDS_AT: u8 = 0;
/// The kind of the facts of [`Loans::kills`]: the loans the statement or terminator at a point
/// ends, by overwriting what holds them or ending the storage of what they borrow.
const KILLED_AT: u8 = 0;

pub(crate) struct Loans {
    pub(crate) loans: Vec<Loan>,
    /// The projections of every loan's place, one after another.
    projections: Vec<PlaceElem>,
    /// Each list in the order the loans were made, or for activations, found.
    index: Relation,
    scopes: Relation,
    /// What each point kills, found once with [`places_conflict`] for the entry sets and the
    /// walk, which replay it.
    kills: Relation,
    /// The borrows of a promoted copy, which make no loan and read nothing.
    pub(crate) promoted: HashSet<Location>,
}

/// How much of a place an access touches, as rustc's `AccessDepth`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Depth {
    /// The place, not what is behind references in it, as an assignment.
    Shallow,
    /// The length of an array, or what a fake borrow sees.
    ShallowArtificial,
    Deep,
    /// A drop, which does not reach behind a reference.
    Drop,
}

/// How to take two places it is not known whether they overlap, as two indexes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bias {
    Overlap,
    NoOverlap,
}

/// Whether a borrow of `place` is not a loan rustc tracks: one through a shared reference or
/// a raw pointer, which nothing the body does can invalidate.
fn ignored<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    local: LocalId,
    projection: &[PlaceElem],
    statics: &HashSet<LocalId>,
) -> bool {
    if statics.contains(&local) {
        return true;
    }
    let Some(last) = projection.iter().rposition(|it| *it == ProjectionElem::Deref) else {
        return false;
    };
    let mut base = ctx.place_ty(body, local, &[]);
    for (index, elem) in projection[..=last].iter().enumerate() {
        if index > 0 {
            base = ctx.project(base, &projection[index - 1]);
        }
        if *elem == ProjectionElem::Deref {
            match base.ty.kind() {
                TyKind::Ref(_, _, rustc_ast_ir::Mutability::Not) | TyKind::RawPtr(..) => {
                    return true;
                }
                _ => (),
            }
        }
    }
    false
}

/// Whether `expr` calls a function, which rustc does not promote, not even a `const fn` in a
/// `const` block; a tuple struct's or variant's constructor is not one, nor is a
/// `#[rustc_promotable]` function such as `size_of`.
fn is_call(ctx: &Ctx<'_, '_>, expr: hir_def::hir::ExprId) -> bool {
    match ctx.store.expr(expr) {
        Expr::MethodCall { .. } => true,
        Expr::Call { callee, .. } => !matches!(
            ctx.infer.expr_ty(callee).kind(),
            TyKind::FnDef(def, _) if match def.0 {
                hir_def::CallableDefId::StructId(_) | hir_def::CallableDefId::EnumVariantId(_) => true,
                hir_def::CallableDefId::FunctionId(func) => hir_def::attrs::AttrFlags::query(ctx.db, func.into())
                    .contains(hir_def::attrs::AttrFlags::RUSTC_PROMOTABLE),
            }
        ),
        _ => false,
    }
}

/// Whether the value `expr` makes has nothing to drop and no cell in it, which rustc asks of a
/// value it promotes rather than of its type: `&[MarkupElement::Emphasis]` or
/// `&FormatElement::Line(LineMode::Hard)` is promoted, though another variant holds a `Cow` or
/// a `Box`, and `&[]` of any element type, `[Cell<u8>]` too.
fn plain_value<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, expr: hir_def::hir::ExprId) -> bool {
    let cx = super::qualifs::ValueCx {
        db: ctx.db,
        infcx: &ctx.infcx,
        env: ctx.env,
        owner: body.owner,
        store: ctx.store,
        infer: ctx.infer,
        consts: &std::cell::RefCell::new(crate::mem::map(ctx.frame)),
    };
    cx.value(expr, 0).is_empty()
}

/// The locals a `static` is read through.
pub(crate) fn static_locals(body: &MirBody<'_>) -> HashSet<LocalId> {
    let mut found = HashSet::default();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(place, Rvalue::Use(operand)) = &statement.kind
                && let OperandKind::Static(_) = operand.kind
                && place.projection.is_empty()
            {
                found.insert(place.local);
            }
        }
    }
    found
}

/// The temporaries a `const` or `static` item's value borrows where rustc extends their
/// lifetime, into the value itself: the operand of a `&` the value is, or that is an element,
/// a field, a tuple struct's or variant's constructor argument, a block's value or a cast
/// operand of such. Likewise a `const { .. }` block's value.
fn extended_temporaries(ctx: &Ctx<'_, '_>, body: &MirBody<'_>) -> HashSet<LocalId> {
    let mut refs = HashSet::default();
    let root = match body.owner {
        InferBodyId::AnonConstId(konst) if ctx.inline_const => Some(konst.loc(ctx.db).expr),
        _ => ctx.store.expr_roots().next_back(),
    };
    let mut stack: Vec<_> = root.into_iter().collect();
    while let Some(expr) = stack.pop() {
        match ctx.store.expr(expr) {
            Expr::Const(inner) => stack.push(inner),
            Expr::Ref { expr: inner, .. } => {
                refs.insert(expr);
                stack.push(inner);
            }
            Expr::Array(Array::ElementList { elements }) => stack.extend(elements.iter().copied()),
            Expr::Tuple { exprs } => stack.extend(exprs.iter().copied()),
            Expr::RecordLit { fields, .. } => stack.extend(fields.iter().map(|it| it.expr)),
            Expr::Call { callee, args }
                if ctx.infer.variant_resolution_for_expr(callee).is_some()
                    || matches!(ctx.store.expr(callee), Expr::Path(path) if path.is_self_type())
                        && matches!(
                            ctx.infer.expr_ty(callee).kind(),
                            TyKind::FnDef(def, _)
                                if !matches!(def.0, hir_def::CallableDefId::FunctionId(_))
                        ) =>
            {
                stack.extend(args.iter().copied())
            }
            Expr::Block { tail: Some(tail), .. } => stack.push(tail),
            Expr::Cast { expr, .. } => stack.push(expr),
            _ => (),
        }
    }
    let mut found = HashSet::default();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(_, Rvalue::Ref(_, place)) = &statement.kind
                && let MirSpan::ExprId(expr) = statement.span
                && refs.contains(&expr)
                && place.projection.is_empty()
                && !ctx.bindings.contains_key(&place.local)
            {
                found.insert(place.local);
            }
        }
    }
    found
}

/// The temporaries a `const` or `static` item's value borrows that rustc promotes there: in
/// a const context a call of a `const fn` is promotable too, as are the literals, constants
/// and aggregates its arguments are made of, of a type with no drop glue and no interior
/// mutability (`Ms::new(&[M::new("a")])`). A borrow is promoted only if it is shared, or of
/// an empty array, and a call only where the item's value certainly runs it, not in a branch
/// that may be dead code.
fn const_promoted(ctx: &Ctx<'_, '_>, body: &MirBody<'_>) -> HashSet<LocalId> {
    let certain = certain_blocks(body);
    let calls = |expr: hir_def::hir::ExprId| {
        let mut stack = vec![expr];
        while let Some(expr) = stack.pop() {
            if is_call(ctx, expr) {
                return true;
            }
            ctx.store.walk_child_exprs(expr, |it| stack.push(it));
        }
        false
    };
    let mut found = HashSet::default();
    for (id, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(_, Rvalue::Ref(_, place)) = &statement.kind
                && let MirSpan::ExprId(expr) = statement.span
                && let Expr::Ref { expr: inner, mutability, .. } = ctx.store.expr(expr)
                && (mutability.is_shared()
                    || matches!(ctx.store.expr(inner), Expr::Array(Array::ElementList { elements }) if elements.is_empty()))
                && place.projection.is_empty()
                && const_promotable(ctx, body, inner, 0)
                && (certain.contains(&id) || !calls(inner))
            {
                let ty = body.locals[place.local].ty.as_ref();
                if hir_ty::drop::is_freeze(&ctx.infcx, ty, ctx.env)
                    && ctx.drop_glue(ty) == hir_ty::drop::DropGlue::None
                {
                    found.insert(place.local);
                }
            }
        }
    }
    found
}

/// The blocks a body certainly runs if it returns, rustc's `promotion_safe_blocks`: from the
/// start on through gotos, calls, drops and asserts.
fn certain_blocks(body: &MirBody<'_>) -> HashSet<hir_ty_mir::mir::BasicBlockId> {
    let mut found = HashSet::default();
    let mut block = body.start_block;
    while found.insert(block) {
        block = match body.basic_blocks[block].terminator.as_ref().map(|it| &it.kind) {
            Some(
                TerminatorKind::Goto { target }
                | TerminatorKind::Call { target: Some(target), .. }
                | TerminatorKind::Drop { target, .. }
                | TerminatorKind::Assert { target, .. },
            ) => *target,
            _ => break,
        };
    }
    found
}

/// The borrows rustc's `promote_consts` makes of a promoted copy: a shared borrow, through
/// any projection but a dereference, of a temporary assigned once from constants and
/// aggregates of them, of a type with no drop glue or interior mutability. The borrow
/// neither reads the temporary nor holds it, so the `ref a` of `let ref a @ b = U;` does
/// not borrow what `b` moves out.
fn promoted_borrows(ctx: &Ctx<'_, '_>, body: &MirBody<'_>) -> HashSet<Location> {
    fn define<'b>(
        defs: &mut HashMap<LocalId, Option<&'b Rvalue>>,
        local: LocalId,
        rvalue: Option<&'b Rvalue>,
    ) {
        defs.entry(local).and_modify(|it| *it = None).or_insert(rvalue);
    }
    let mut defs = HashMap::default();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            match &statement.kind {
                StatementKind::Assign(place, rvalue) => {
                    define(&mut defs, place.local, place.projection.is_empty().then_some(rvalue));
                    // A mutable borrow of a part of the temporary is a use rustc's
                    // `TempState` takes for a mutation, `&mut t.0` of `let ref a @ (ref mut
                    // b, _) = (1, 2)`, so `a` borrows the temporary itself.
                    if let Rvalue::Ref(BorrowKind::Mut { .. }, borrowed) = rvalue
                        && !borrowed.projection.as_slice().is_empty()
                    {
                        define(&mut defs, borrowed.local, None);
                    }
                }
                StatementKind::Deinit(place) => define(&mut defs, place.local, None),
                _ => (),
            }
        }
        if let Some(terminator) = &block.terminator
            && let TerminatorKind::Call { destination, .. } = &terminator.kind
        {
            define(&mut defs, destination.local, None);
        }
    }
    let temp = |local: LocalId| {
        local != super::return_slot()
            && !body.param_locals.contains(&local)
            && !ctx.bindings.contains_key(&local)
    };
    fn promotable(
        defs: &HashMap<LocalId, Option<&Rvalue>>,
        temp: &impl Fn(LocalId) -> bool,
        local: LocalId,
        depth: usize,
    ) -> bool {
        let operand = |operand: &hir_ty_mir::mir::Operand| match &operand.kind {
            OperandKind::Constant { .. } | OperandKind::Allocation { .. } => true,
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                place.projection.is_empty() && promotable(defs, temp, place.local, depth + 1)
            }
            OperandKind::Static(_) => false,
        };
        depth < 16
            && temp(local)
            && match defs.get(&local) {
                Some(Some(Rvalue::Use(it))) => operand(it),
                // A closure that captures nothing too, `unwrap_or(&|_| true)`.
                Some(Some(Rvalue::Aggregate(kind, operands))) => {
                    !matches!(kind, hir_ty_mir::mir::AggregateKind::Union(..))
                        && operands.iter().all(operand)
                }
                _ => false,
            }
    }
    let mut found = HashSet::default();
    for (block, data) in body.basic_blocks.iter() {
        for (statement, it) in data.statements.iter().enumerate() {
            if let StatementKind::Assign(_, Rvalue::Ref(BorrowKind::Shared, place)) = &it.kind
                && !place.projection.as_slice().contains(&ProjectionElem::Deref)
                && promotable(&defs, &temp, place.local, 0)
            {
                let ty = body.locals[place.local].ty.as_ref();
                if hir_ty::drop::is_freeze(&ctx.infcx, ty, ctx.env)
                    && ctx.drop_glue(ty) == hir_ty::drop::DropGlue::None
                {
                    found.insert(Location { block, statement });
                }
            }
        }
    }
    found
}

fn const_promotable(
    ctx: &Ctx<'_, '_>,
    body: &MirBody<'_>,
    expr: hir_def::hir::ExprId,
    depth: usize,
) -> bool {
    use hir_def::resolver::{HasResolver, ValueNs};
    if depth > 16 {
        return false;
    }
    let all = |exprs: Vec<hir_def::hir::ExprId>| {
        exprs.into_iter().all(|it| const_promotable(ctx, body, it, depth + 1))
    };
    match ctx.store.expr(expr) {
        Expr::Literal(_) => true,
        Expr::Path(path) => {
            let owner = body.owner;
            let mut resolver = owner.resolver(ctx.db);
            let _guard =
                resolver.update_to_inner_scope(ctx.db, owner.expression_store_owner(ctx.db), expr);
            let hygiene = ctx.store.expr_path_hygiene(expr);
            match resolver.resolve_path_in_value_ns_fully(ctx.db, path, hygiene) {
                None | Some(ValueNs::LocalBinding(_)) => false,
                // rustc's `promote_consts` lets a static be borrowed or read in a static's
                // value, but not a thread-local or an extern one, which CTFE cannot read.
                Some(ValueNs::StaticId(it)) => {
                    matches!(body.owner, InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(_)))
                        && !hir_def::signatures::StaticSignature::of(ctx.db, it)
                            .flags
                            .contains(hir_def::signatures::StaticFlags::EXTERN)
                        && !hir_def::attrs::AttrFlags::query(ctx.db, it.into())
                            .contains(hir_def::attrs::AttrFlags::THREAD_LOCAL)
                }
                Some(_) => true,
            }
        }
        Expr::Array(Array::ElementList { elements }) => all(elements.to_vec()),
        Expr::Array(Array::Repeat { initializer, .. }) => {
            const_promotable(ctx, body, initializer, depth + 1)
        }
        Expr::Tuple { exprs } => all(exprs.to_vec()),
        Expr::RecordLit { fields, spread: hir_def::hir::RecordSpread::None, .. } => {
            all(fields.iter().map(|it| it.expr).collect())
        }
        Expr::Ref { expr, .. } | Expr::Cast { expr, .. } => {
            const_promotable(ctx, body, expr, depth + 1)
        }
        Expr::Call { callee, args } => {
            let callable = match ctx.infer.expr_ty(callee).kind() {
                TyKind::FnDef(def, _) => match def.0 {
                    hir_def::CallableDefId::StructId(_)
                    | hir_def::CallableDefId::EnumVariantId(_) => true,
                    hir_def::CallableDefId::FunctionId(func) => {
                        hir_def::signatures::FunctionSignature::of(ctx.db, func).is_const()
                    }
                },
                _ => false,
            };
            // Not with an argument that needs dropping, `ManuallyDrop::new(String::new())`.
            let dropped = args
                .iter()
                .any(|&it| ctx.drop_glue(ctx.infer.expr_ty(it)) != hir_ty::drop::DropGlue::None);
            callable && !dropped && all(args.to_vec())
        }
        // `NAME.as_str()` of a `const NAME`, as tracing's `fieldset!` names its fields.
        Expr::MethodCall { receiver, args, .. } => {
            let callable = ctx.infer.method_resolution(expr).is_some_and(|(func, _)| {
                hir_def::signatures::FunctionSignature::of(ctx.db, func).is_const()
            });
            let dropped = std::iter::once(&receiver)
                .chain(args.iter())
                .any(|&it| ctx.drop_glue(ctx.infer.expr_ty(it)) != hir_ty::drop::DropGlue::None);
            callable && !dropped && all(std::iter::once(receiver).chain(args.to_vec()).collect())
        }
        // A block of items and a tail is its tail in MIR, where rustc promotes.
        Expr::Block { statements, tail: Some(tail), .. }
            if statements.iter().all(|it| matches!(it, hir_def::hir::Statement::Item(_))) =>
        {
            const_promotable(ctx, body, tail, depth + 1)
        }
        _ => false,
    }
}

impl Loans {
    pub(crate) fn gather<'db>(
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        cfg: &Cfg<'_>,
        regions: &Regions<'_, '_>,
        unique: impl Fn(LocalId, LocalId, &[PlaceElem]) -> bool,
    ) -> Self {
        let mut statics = static_locals(body);
        // A promoted constant is `'static`, as a `static` is; so is what a `const` or `static`
        // item's value borrows, which becomes part of that value whatever it calls.
        let item = matches!(
            body.owner,
            InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_))
        );
        let (promoted, rejected): (Vec<_>, Vec<_>) =
            body.promoted_locals.iter().partition(|&&(local, expr)| {
                // rustc promotes no call, not even of a `const fn` in a `const` block.
                item || (!is_call(ctx, expr)
                    && (ctx.is_plain(body.locals[local].ty.as_ref())
                        || plain_value(ctx, body, expr)))
            });
        // Nor what borrows one it does not promote, `&&TEST_DROP`.
        let rejected: HashSet<hir_def::hir::ExprId> =
            rejected.into_iter().map(|&(_, expr)| expr).collect();
        let borrows_rejected = |expr: hir_def::hir::ExprId| {
            let mut stack = vec![expr];
            while let Some(expr) = stack.pop() {
                if rejected.contains(&expr) {
                    return true;
                }
                ctx.store.walk_child_exprs(expr, |it| stack.push(it));
            }
            false
        };
        statics.extend(
            promoted
                .into_iter()
                .filter(|&&(_, expr)| rejected.is_empty() || !borrows_rejected(expr))
                .map(|&(local, _)| local),
        );
        if item {
            statics.extend(extended_temporaries(ctx, body));
            statics.extend(const_promoted(ctx, body));
        } else if ctx.inline_const {
            statics.extend(extended_temporaries(ctx, body));
        }
        // A function item is a constant of a type with no data, promoted where it is borrowed:
        // `&Trait::method`, which inference may leave without a type when it cannot tell `Self`.
        for (_, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                if let StatementKind::Assign(place, Rvalue::Use(operand)) = &statement.kind
                    && !matches!(operand.kind, OperandKind::Copy(_) | OperandKind::Move(_))
                    && place.projection.is_empty()
                    && matches!(
                        body.locals[place.local].ty.as_ref().kind(),
                        TyKind::FnDef(..) | TyKind::Error(_)
                    )
                {
                    statics.insert(place.local);
                }
            }
        }
        let mut loans = Loans {
            loans: Vec::new(),
            projections: Vec::new(),
            index: Relation::default(),
            scopes: Relation::default(),
            kills: Relation::default(),
            promoted: promoted_borrows(ctx, body),
        };
        let mut facts = Facts::default();
        let mut two_phase: HashMap<LocalId, usize> = HashMap::default();
        for &block in &cfg.order {
            for (statement, it) in body.basic_blocks[block].statements.iter().enumerate() {
                let StatementKind::Assign(place, Rvalue::Ref(kind, borrowed)) = &it.kind else {
                    continue;
                };
                let location = Location { block, statement };
                let projection = borrowed.projection.as_slice();
                if ignored(ctx, body, borrowed.local, projection, &statics)
                    || loans.promoted.contains(&location)
                {
                    continue;
                }
                let Some(&region) = regions.loan_regions.get(&location) else { continue };
                let mut kind = *kind;
                // rustc makes the mutable borrow a closure captures of a variable without
                // `mut` a unique one.
                if let BorrowKind::Mut { kind: MutBorrowKind::Default } = kind
                    && unique(place.local, borrowed.local, projection)
                {
                    kind = BorrowKind::Mut { kind: MutBorrowKind::ClosureCapture };
                }
                let two = matches!(kind, BorrowKind::Mut { kind: MutBorrowKind::TwoPhasedBorrow })
                    && place.projection.is_empty();
                let id = loans.loans.len();
                if two {
                    two_phase.insert(place.local, id);
                }
                let start = loans.projections.len() as u32;
                loans.projections.extend_from_slice(projection);
                loans.loans.push(Loan {
                    local: borrowed.local,
                    projection: (start, loans.projections.len() as u32),
                    kind,
                    region,
                    location,
                    span: it.span,
                    assigned: place.local,
                    activation: if two {
                        Activation::NotActivated
                    } else {
                        Activation::NotTwoPhase
                    },
                });
                facts.push(MADE_AT, cfg.point(location), id);
                facts.push(OF_LOCAL, borrowed.local.into_raw().into_u32() as usize, id);
            }
        }
        if !two_phase.is_empty() {
            // A two-phase borrow is activated by the first use of the reference.
            for &block in &cfg.order {
                let data = &body.basic_blocks[block];
                for statement in 0..=data.statements.len() {
                    let location = Location { block, statement };
                    let mut used = Vec::new();
                    if let Some(it) = data.statements.get(statement) {
                        match &it.kind {
                            StatementKind::Assign(place, rvalue) => {
                                rvalue.for_each_operand(&mut |op| {
                                    if let OperandKind::Copy(p) | OperandKind::Move(p) = &op.kind {
                                        used.push(p.local);
                                    }
                                });
                                if let Rvalue::Ref(_, p)
                                | Rvalue::CopyForDeref(p)
                                | Rvalue::Len(p)
                                | Rvalue::Discriminant(p) = rvalue
                                {
                                    used.push(p.local);
                                }
                                if !place.projection.is_empty() {
                                    used.push(place.local);
                                }
                            }
                            _ => continue,
                        }
                    } else if let Some(terminator) = &data.terminator {
                        terminator.kind.for_each_operand(&mut |op| {
                            if let OperandKind::Copy(p) | OperandKind::Move(p) = &op.kind {
                                used.push(p.local);
                            }
                        });
                    }
                    for local in used {
                        if let Some(&id) = two_phase.get(&local)
                            && loans.loans[id].activation == Activation::NotActivated
                            && loans.loans[id].location != location
                        {
                            loans.loans[id].activation = Activation::At(location);
                            facts.push(ACTIVATED_AT, cfg.point(location), id);
                        }
                    }
                }
            }
        }
        loans.index = facts.freeze(&[cfg.points, body.locals.len(), cfg.points]);
        loans.kills = loans.find_kills(ctx, body, cfg);
        loans
    }

    /// The loans each reachable point kills, in the order it kills them.
    fn find_kills<'db>(&self, ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, cfg: &Cfg<'_>) -> Relation {
        let mut facts = Facts::default();
        if self.loans.is_empty() {
            return facts.freeze(&[cfg.points]);
        }
        for &block in &cfg.order {
            let data = &body.basic_blocks[block];
            for statement in 0..=data.statements.len() {
                let point = cfg.point(Location { block, statement });
                self.killed_by(ctx, body, block, statement, &mut |id| {
                    facts.push(KILLED_AT, point, id)
                });
            }
        }
        facts.freeze(&[cfg.points])
    }

    /// Calls `kill` with each loan the statement or terminator at `statement` of `block`
    /// ends.
    fn killed_by<'db>(
        &self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        block: BasicBlockId,
        statement: usize,
        kill: &mut impl FnMut(usize),
    ) {
        let data = &body.basic_blocks[block];
        if let Some(statement) = data.statements.get(statement) {
            match &statement.kind {
                StatementKind::Assign(place, _) => {
                    self.kill_on_place(ctx, body, place.local, place.projection.as_slice(), kill)
                }
                StatementKind::StorageDead(local) => {
                    for &id in self.of_local(*local) {
                        kill(id as usize);
                    }
                }
                StatementKind::InlineAsm(asm) => {
                    for operand in &asm.operands {
                        if let hir_ty_mir::mir::AsmOperand::Reg { output: Some(output), .. } =
                            operand
                        {
                            self.kill_on_place(
                                ctx,
                                body,
                                output.local,
                                output.projection.as_slice(),
                                kill,
                            );
                        }
                    }
                }
                _ => (),
            }
        } else if let Some(terminator) = &data.terminator {
            match &terminator.kind {
                TerminatorKind::Call { destination: place, .. }
                | TerminatorKind::DropAndReplace { place, .. } => {
                    self.kill_on_place(ctx, body, place.local, place.projection.as_slice(), kill)
                }
                _ => (),
            }
        }
    }

    /// The projection of the place `id` borrows.
    pub(crate) fn projection(&self, id: usize) -> &[PlaceElem] {
        let (start, end) = self.loans[id].projection;
        &self.projections[start as usize..end as usize]
    }

    /// The loan made at `point`, if one is.
    fn made_at(&self, point: usize) -> Option<usize> {
        self.index.get(MADE_AT, point).first().map(|&it| it as usize)
    }

    /// The loans of a place of `local`, in the order they were made.
    pub(crate) fn of_local(&self, local: LocalId) -> &[u32] {
        self.index.get(OF_LOCAL, local.into_raw().into_u32() as usize)
    }

    /// The two-phase loans first used at `point`.
    pub(crate) fn activated_at(&self, point: usize) -> &[u32] {
        self.index.get(ACTIVATED_AT, point)
    }

    /// Where each loan goes out of scope: the first points past it its region does not hold
    /// at, as rustc's `OutOfScopePrecomputer`.
    pub(crate) fn compute_scopes(
        &mut self,
        body: &MirBody<'_>,
        cfg: &Cfg<'_>,
        values: &RegionValues<'_>,
    ) {
        let mut facts = Facts::default();
        for id in 0..self.loans.len() {
            let points = values.points_of(self.loans[id].region);
            let ends = out_of_scope(body, cfg, self.loans[id].location, |start, end| {
                points.first_absent_in(start, end)
            });
            for point in ends {
                facts.push(ENDS_AT, point, id);
            }
        }
        self.scopes = facts.freeze(&[cfg.points]);
    }

    /// Polonius alpha's scopes: a loan goes out of scope at the first point after it is made
    /// that it is not live at, as rustc's `PoloniusOutOfScopePrecomputer`.
    pub(crate) fn compute_polonius_scopes(
        &mut self,
        body: &MirBody<'_>,
        cfg: &Cfg<'_>,
        live: &[crate::intervals::IntervalSet<'_>],
    ) {
        let mut facts = Facts::default();
        for id in 0..self.loans.len() {
            let issued = cfg.point(self.loans[id].location);
            // The loan is in scope where it is made, live there or not.
            let ends = out_of_scope(body, cfg, self.loans[id].location, |start, end| {
                match live[id].first_absent_in(start, end) {
                    Some(point) if point == issued => {
                        (point < end).then(|| live[id].first_absent_in(point + 1, end)).flatten()
                    }
                    other => other,
                }
            });
            for point in ends {
                facts.push(ENDS_AT, point, id);
            }
        }
        self.scopes = facts.freeze(&[cfg.points]);
    }

    /// The loans in scope at the start of each block, a row per block: rustc's `Borrows`.
    pub(crate) fn entry_sets(&self, body: &MirBody<'_>, cfg: &Cfg<'_>) -> BitMatrix {
        let blocks = body.basic_blocks.len();
        let count = self.loans.len();
        if count == 0 {
            return BitMatrix::new(blocks, 0);
        }
        let mut transfer = GenKill::new(blocks, count);
        for &block in &cfg.order {
            let data = &body.basic_blocks[block];
            let mut row = transfer.row(index(block));
            for statement in 0..=data.statements.len() {
                let location = Location { block, statement };
                self.before(cfg, location, &mut row);
                self.after(cfg, location, &mut row);
            }
        }
        forward(cfg, &transfer, body.start_block, &BitSet::new(count), |_, _, _| ())
    }

    /// The loans that go out of scope as `location` starts.
    pub(crate) fn before(&self, cfg: &Cfg<'_>, location: Location, state: &mut impl Transfer) {
        for &id in self.scopes.get(ENDS_AT, cfg.point(location)) {
            state.kill(id as usize);
        }
    }

    /// What the statement or terminator at `location` does to the loans in scope: the kills
    /// [`Self::find_kills`] found, then the loan it makes.
    pub(crate) fn after(&self, cfg: &Cfg<'_>, location: Location, state: &mut impl Transfer) {
        let point = cfg.point(location);
        for &id in self.kills.get(KILLED_AT, point) {
            state.kill(id as usize);
        }
        if let Some(id) = self.made_at(point) {
            state.gen_(id);
        }
    }

    /// rustc's `kill_borrows_on_place`: overwriting a local ends every loan of what it holds.
    fn kill_on_place<'db>(
        &self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        local: LocalId,
        projection: &[PlaceElem],
        kill: &mut impl FnMut(usize),
    ) {
        for &id in self.of_local(local) {
            let id = id as usize;
            if projection.is_empty()
                || places_conflict(
                    ctx,
                    body,
                    self,
                    id,
                    local,
                    projection,
                    Depth::Deep,
                    Bias::NoOverlap,
                )
            {
                kill(id);
            }
        }
    }
}

/// The first points reachable from just past `issued` that are not in scope, given
/// `first_absent(start, end)`, the first point of `start..=end` not in scope. It walks whole
/// blocks: a block's points are consecutive, so one lookup finds where a loan ends in it.
fn out_of_scope(
    body: &MirBody<'_>,
    cfg: &Cfg<'_>,
    issued: Location,
    mut first_absent: impl FnMut(usize, usize) -> Option<usize>,
) -> SmallVec<[usize; 2]> {
    let mut out = SmallVec::new();
    let last = |block: BasicBlockId| {
        cfg.point(Location { block, statement: body.basic_blocks[block].statements.len() })
    };
    let mut entered = BitSet::new(body.basic_blocks.len());
    let mut work: Vec<BasicBlockId> = Vec::new();
    // The rest of the issuing block, if the loan is not made by its terminator.
    let issued_point = cfg.point(issued);
    let rest = issued.statement < body.basic_blocks[issued.block].statements.len();
    if rest {
        match first_absent(issued_point + 1, last(issued.block)) {
            Some(point) => out.push(point),
            None => work.extend(cfg.successors(issued.block).iter().copied()),
        }
    } else {
        work.extend(cfg.successors(issued.block).iter().copied());
    }
    while let Some(block) = work.pop() {
        if !entered.insert(index(block)) {
            continue;
        }
        let start = cfg.point(Location { block, statement: 0 });
        // Coming back around to the issuing block, the points past the loan were seen.
        let (end, through) =
            if rest && block == issued.block { (issued_point, false) } else { (last(block), true) };
        match first_absent(start, end) {
            Some(point) => out.push(point),
            None if through => work.extend(cfg.successors(block).iter().copied()),
            None => (),
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Overlap {
    Arbitrary,
    EqualOrDisjoint,
    Disjoint,
}

/// Whether an access of `depth` to the place `local.projection` touches what loan `id`
/// borrows, as rustc's `borrow_conflicts_with_place`.
pub(crate) fn places_conflict<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    loans: &Loans,
    id: usize,
    local: LocalId,
    projection: &[PlaceElem],
    depth: Depth,
    bias: Bias,
) -> bool {
    let loan = &loans.loans[id];
    if loan.local != local {
        return false;
    }
    let borrow = loans.projection(id);
    // Each prefix's type from the one before, as `multi_projection_ty` folds them.
    let mut base_ty = ctx.place_ty(body, local, &[]);
    for (index, &elem) in borrow.iter().enumerate() {
        if index > 0 {
            base_ty = ctx.project(base_ty, &borrow[index - 1]);
        }
        if let Some(&access) = projection.get(index) {
            match projection_conflict(ctx, base_ty, elem, access, bias) {
                Overlap::Arbitrary => return true,
                Overlap::EqualOrDisjoint => (),
                Overlap::Disjoint => return false,
            }
        } else {
            // The loan is of a part of the place accessed.
            match (elem, base_ty.ty.kind(), depth) {
                (_, _, Depth::ShallowArtificial) => return false,
                (ProjectionElem::Deref, _, Depth::Shallow) => return false,
                (
                    ProjectionElem::Deref,
                    TyKind::Ref(_, _, rustc_ast_ir::Mutability::Mut),
                    Depth::Drop,
                ) => {
                    return false;
                }
                (ProjectionElem::Deref, TyKind::Ref(_, _, rustc_ast_ir::Mutability::Not), _) => {
                    return false;
                }
                (ProjectionElem::Field(_), TyKind::Adt(def, _), Depth::Drop)
                    if ctx.has_destructor(def.def_id()) =>
                {
                    return true;
                }
                _ => (),
            }
        }
    }
    // The access is of the place borrowed or of a part of it. A fake borrow of a place
    // does not conflict with an access of a part of it.
    !(loan.kind == BorrowKind::Shallow && borrow.len() < projection.len())
}

fn projection_conflict<'db>(
    ctx: &Ctx<'_, 'db>,
    base: hir_ty_mir::mir::PlaceTy<'_>,
    borrow: PlaceElem,
    access: PlaceElem,
    bias: Bias,
) -> Overlap {
    let unknown = match bias {
        Bias::Overlap => Overlap::EqualOrDisjoint,
        Bias::NoOverlap => Overlap::Disjoint,
    };
    match (borrow, access) {
        (ProjectionElem::Deref, ProjectionElem::Deref) => Overlap::EqualOrDisjoint,
        (ProjectionElem::Field(a), ProjectionElem::Field(b)) => {
            if a == b {
                Overlap::EqualOrDisjoint
            } else {
                match base.ty.kind() {
                    TyKind::Adt(def, _) if def.is_union() => Overlap::Arbitrary,
                    _ => Overlap::Disjoint,
                }
            }
        }
        (ProjectionElem::Downcast(a), ProjectionElem::Downcast(b)) => {
            if a == b {
                Overlap::EqualOrDisjoint
            } else {
                Overlap::Disjoint
            }
        }
        (ProjectionElem::Index(..), ProjectionElem::Index(..))
        | (
            ProjectionElem::Index(..),
            ProjectionElem::ConstantIndex { .. } | ProjectionElem::Subslice { .. },
        )
        | (
            ProjectionElem::ConstantIndex { .. } | ProjectionElem::Subslice { .. },
            ProjectionElem::Index(..),
        ) => unknown,
        (
            ProjectionElem::ConstantIndex { offset: a, min_length: la, from_end: fa },
            ProjectionElem::ConstantIndex { offset: b, min_length: lb, from_end: fb },
        ) => {
            if fa == fb {
                if a == b { Overlap::EqualOrDisjoint } else { Overlap::Disjoint }
            } else if let Some(len) = array_len(ctx, base) {
                let (a, b) = (if fa { len - a } else { a }, if fb { len - b } else { b });
                if a == b { Overlap::EqualOrDisjoint } else { Overlap::Disjoint }
            } else {
                // Both patterns matched, so the slice has at least the longer minimum; an offset
                // from the end is then at least `min_length - offset` from the start.
                let (from_begin, from_end) = if fa { (b, a) } else { (a, b) };
                if from_begin >= la.max(lb) - from_end {
                    Overlap::EqualOrDisjoint
                } else {
                    Overlap::Disjoint
                }
            }
        }
        (
            ProjectionElem::ConstantIndex { offset, from_end, .. },
            ProjectionElem::Subslice { from, to },
        )
        | (
            ProjectionElem::Subslice { from, to },
            ProjectionElem::ConstantIndex { offset, from_end, .. },
        ) => {
            let overlaps = match (array_len(ctx, base), from_end) {
                (Some(len), _) => {
                    let index = if from_end { len.wrapping_sub(offset) } else { offset };
                    index >= from && index < len.saturating_sub(to)
                }
                (None, false) => offset >= from,
                (None, true) => offset > to,
            };
            if overlaps { Overlap::EqualOrDisjoint } else { Overlap::Disjoint }
        }
        (
            ProjectionElem::Subslice { from: fa, to: ta },
            ProjectionElem::Subslice { from: fb, to: tb },
        ) => match array_len(ctx, base) {
            Some(len) if fa.max(fb) >= len.saturating_sub(ta.max(tb)) => Overlap::Disjoint,
            _ => Overlap::EqualOrDisjoint,
        },
        _ => Overlap::Arbitrary,
    }
}

fn array_len(ctx: &Ctx<'_, '_>, base: hir_ty_mir::mir::PlaceTy<'_>) -> Option<u64> {
    match base.ty.kind() {
        TyKind::Array(_, len) => {
            hir_ty::consteval::try_const_usize(ctx.db, len).map(|it| it as u64)
        }
        _ => None,
    }
}
