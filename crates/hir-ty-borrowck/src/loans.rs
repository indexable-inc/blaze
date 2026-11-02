//! Loans, as rustc's `borrow_set.rs`, the `Borrows` analysis of `dataflow.rs` and
//! `places_conflict.rs`: each borrow is a loan of a place, in scope from where it is made to
//! where its region stops holding, or the reference to it is overwritten, or the place it
//! borrows goes away.
//!
//! The borrows, the writes and the reads that activate a two-phase borrow are the decode's
//! ([`Decoded`]), in reverse postorder and statement order, the order the loans are numbered
//! in.

use hir_def::{
    DefWithBodyId,
    hir::{Array, Expr},
};
use rustc_type_ir::inherent::IntoKind;

use hir_ty::{
    InferBodyId,
    mir::{BasicBlockId, BorrowKind, LocalId, MirBody, MirSpan, MutBorrowKind, PlaceElem, ProjectionElem},
    next_solver::TyKind,
};

use super::{
    Ctx,
    bitset::{BitSet, Transfer, bits},
    cfg::{Cfg, Location, block, index},
    dataflow::forward,
    decode::{Decoded, NONE, WriteKind, local, raw},
    inference::RegionValues,
    intervals::IntervalSet,
    mem::{self, FMap, FSet, FVec, Frame},
    places::PlaceId,
    regions::{Regions, Rv},
    relation::{Facts, Relation},
    rows::{Dense, Edge, Effects, Rows, Store},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Activation {
    NotTwoPhase,
    NotActivated,
    At(Location),
}

pub(super) struct Loan {
    pub(super) local: LocalId,
    /// The range of [`Loans::projections`] the projection of the place borrowed is, and of
    /// [`Loans::prefixes`] the places its prefixes are.
    projection: (u32, u32),
    pub(super) kind: BorrowKind,
    pub(super) region: Rv,
    pub(super) location: Location,
    pub(super) span: MirSpan,
    /// The local the reference is assigned to.
    pub(super) assigned: LocalId,
    pub(super) activation: Activation,
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

pub(super) struct Loans<'f> {
    pub(super) loans: FVec<'f, Loan>,
    /// The projections of every loan's place, one after another.
    projections: FVec<'f, PlaceElem>,
    /// Beside each element of [`Self::projections`], the place it projects: the loan's place
    /// up to that element, whose type the conflicts read.
    prefixes: FVec<'f, PlaceId>,
    /// Each list in the order the loans were made, or for activations, found.
    index: Relation<'f>,
    scopes: Relation<'f>,
    /// What each point kills, found once with [`places_conflict`] for the entry sets and the
    /// walk, which replay it.
    kills: Relation<'f>,
    /// The borrows of a promoted copy, which make no loan and read nothing.
    pub(super) promoted: FSet<'f, Location>,
}

/// How much of a place an access touches, as rustc's `AccessDepth`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Depth {
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
pub(super) enum Bias {
    Overlap,
    NoOverlap,
}

/// Whether a borrow of `local.projection`, whose prefixes are the places `prefixes` (root
/// first), is not a loan rustc tracks: one through a shared reference or a raw pointer, which
/// nothing the body does can invalidate.
fn ignored(
    ctx: &Ctx<'_, '_>,
    local: LocalId,
    projection: &[PlaceElem],
    prefixes: &[PlaceId],
    statics: &FSet<'_, LocalId>,
) -> bool {
    if statics.contains(&local) {
        return true;
    }
    let Some(last) = projection.iter().rposition(|it| *it == ProjectionElem::Deref) else {
        return false;
    };
    let places = ctx.places.borrow();
    for (index, elem) in projection[..=last].iter().enumerate() {
        if *elem == ProjectionElem::Deref {
            match places.ty(prefixes[index]).ty.kind() {
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
    let consts = std::cell::RefCell::new(mem::map(ctx.frame));
    let cx = super::qualifs::ValueCx {
        db: ctx.db,
        infcx: &ctx.infcx,
        env: ctx.env,
        owner: body.owner,
        store: ctx.store,
        infer: ctx.infer,
        consts: &consts,
    };
    cx.value(expr, 0).is_empty()
}

/// The locals a `static` is read through.
pub(super) fn static_locals<'f>(frame: &'f Frame, decoded: &Decoded<'_>) -> FSet<'f, LocalId> {
    let mut found = mem::set(frame);
    for constant in decoded.constants.iter().filter(|it| it.is_static) {
        found.insert(constant.local);
    }
    found
}

/// The temporaries a `const` or `static` item's value borrows where rustc extends their
/// lifetime, into the value itself: the operand of a `&` the value is, or that is an element,
/// a field, a tuple struct's or variant's constructor argument, a block's value or a cast
/// operand of such. Likewise a `const { .. }` block's value.
fn extended_temporaries<'a>(
    ctx: &Ctx<'a, '_>,
    body: &MirBody<'_>,
    decoded: &Decoded<'_>,
) -> FSet<'a, LocalId> {
    let mut refs = mem::set(ctx.frame);
    let root = match body.owner {
        InferBodyId::AnonConstId(konst) if ctx.inline_const => Some(konst.loc(ctx.db).expr),
        _ => ctx.store.expr_roots().next_back(),
    };
    let mut stack = mem::collect(ctx.frame, root);
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
    let mut found = mem::set(ctx.frame);
    for borrow in decoded.borrows() {
        if let MirSpan::ExprId(expr) = borrow.span
            && refs.contains(&expr)
            && borrow.whole
            && !ctx.bindings.contains_key(&borrow.local)
        {
            found.insert(borrow.local);
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
fn const_promoted<'a>(
    ctx: &Ctx<'a, '_>,
    body: &MirBody<'_>,
    decoded: &Decoded<'_>,
) -> FSet<'a, LocalId> {
    let certain = certain_blocks(ctx.frame, body, decoded);
    let calls = |expr: hir_def::hir::ExprId| {
        let mut stack = Vec::new_in(ctx.frame);
        stack.push(expr);
        while let Some(expr) = stack.pop() {
            if is_call(ctx, expr) {
                return true;
            }
            ctx.store.walk_child_exprs(expr, |it| stack.push(it));
        }
        false
    };
    let mut found = mem::set(ctx.frame);
    for borrow in decoded.borrows() {
        if let MirSpan::ExprId(expr) = borrow.span
            && let Expr::Ref { expr: inner, mutability, .. } = ctx.store.expr(expr)
            && (mutability.is_shared()
                || matches!(ctx.store.expr(inner), Expr::Array(Array::ElementList { elements }) if elements.is_empty()))
            && borrow.whole
            && const_promotable(ctx, body, inner, 0)
            && (certain.contains(&borrow.location.block) || !calls(inner))
        {
            let ty = body.locals[borrow.local].ty.as_ref();
            if hir_ty::drop::is_freeze(&ctx.infcx, ty, ctx.env)
                && ctx.drop_glue(ty) == hir_ty::drop::DropGlue::None
            {
                found.insert(borrow.local);
            }
        }
    }
    found
}

/// The blocks a body certainly runs if it returns, rustc's `promotion_safe_blocks`: from the
/// start on through gotos, calls, drops and asserts.
fn certain_blocks<'f>(
    frame: &'f Frame,
    body: &MirBody<'_>,
    decoded: &Decoded<'_>,
) -> FSet<'f, BasicBlockId> {
    let mut found = mem::set(frame);
    let mut at = body.start_block;
    while found.insert(at) {
        match decoded.certain[index(at)] {
            NONE => break,
            next => at = block(next as usize),
        }
    }
    found
}

/// The borrows rustc's `promote_consts` makes of a promoted copy: a shared borrow, through
/// any projection but a dereference, of a temporary assigned once from constants and
/// aggregates of them, of a type with no drop glue or interior mutability. The borrow
/// neither reads the temporary nor holds it, so the `ref a` of `let ref a @ b = U;` does
/// not borrow what `b` moves out.
fn promoted_borrows<'a>(
    ctx: &Ctx<'a, '_>,
    body: &MirBody<'_>,
    decoded: &Decoded<'_>,
) -> FSet<'a, Location> {
    let temp = |local: LocalId| {
        local != super::return_slot()
            && !body.param_locals.contains(&local)
            && !ctx.bindings.contains_key(&local)
    };
    fn promotable(
        decoded: &Decoded<'_>,
        temp: &impl Fn(LocalId) -> bool,
        local: LocalId,
        depth: usize,
    ) -> bool {
        depth < 16
            && temp(local)
            && decoded.temp_of(local).is_some_and(|locals| {
                locals.iter().all(|&it| promotable(decoded, temp, it, depth + 1))
            })
    }
    let mut found = mem::set(ctx.frame);
    for borrow in decoded.borrows() {
        if borrow.kind == BorrowKind::Shared
            && !borrow.through_deref
            && promotable(decoded, &temp, borrow.local, 0)
        {
            let ty = body.locals[borrow.local].ty.as_ref();
            if hir_ty::drop::is_freeze(&ctx.infcx, ty, ctx.env)
                && ctx.drop_glue(ty) == hir_ty::drop::DropGlue::None
            {
                found.insert(borrow.location);
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
    let promotable = |it: hir_def::hir::ExprId| const_promotable(ctx, body, it, depth + 1);
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
        Expr::Array(Array::ElementList { elements }) => elements.iter().copied().all(promotable),
        Expr::Array(Array::Repeat { initializer, .. }) => {
            const_promotable(ctx, body, initializer, depth + 1)
        }
        Expr::Tuple { exprs } => exprs.iter().copied().all(promotable),
        Expr::RecordLit { fields, spread: hir_def::hir::RecordSpread::None, .. } => {
            fields.iter().map(|it| it.expr).all(promotable)
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
            callable && !dropped && args.iter().copied().all(promotable)
        }
        // `NAME.as_str()` of a `const NAME`, as tracing's `fieldset!` names its fields.
        Expr::MethodCall { receiver, args, .. } => {
            let callable = ctx.infer.method_resolution(expr).is_some_and(|(func, _)| {
                hir_def::signatures::FunctionSignature::of(ctx.db, func).is_const()
            });
            let dropped = std::iter::once(&receiver)
                .chain(args.iter())
                .any(|&it| ctx.drop_glue(ctx.infer.expr_ty(it)) != hir_ty::drop::DropGlue::None);
            callable
                && !dropped
                && std::iter::once(receiver).chain(args.iter().copied()).all(promotable)
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

impl<'a> Loans<'a> {
    pub(super) fn gather<'db>(
        ctx: &Ctx<'a, 'db>,
        body: &MirBody<'db>,
        cfg: &Cfg<'_>,
        decoded: &Decoded<'_>,
        regions: &Regions<'_, '_>,
        unique: impl Fn(LocalId, LocalId, &[PlaceElem]) -> bool,
    ) -> Self {
        let frame = ctx.frame;
        let mut statics = static_locals(frame, decoded);
        // A promoted constant is `'static`, as a `static` is; so is what a `const` or `static`
        // item's value borrows, which becomes part of that value whatever it calls.
        let item = matches!(
            body.owner,
            InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_))
        );
        // rustc promotes no call, not even of a `const fn` in a `const` block.
        let promotes = |&(local, expr): &(LocalId, hir_def::hir::ExprId)| {
            item || (!is_call(ctx, expr)
                && (ctx.is_plain(body.locals[local].ty.as_ref())
                    || plain_value(ctx, body, expr)))
        };
        let mut promoted: FVec<'_, &(LocalId, hir_def::hir::ExprId)> = Vec::new_in(frame);
        let mut rejected = mem::set(frame);
        for it in body.promoted_locals.iter() {
            match promotes(it) {
                true => promoted.push(it),
                false => {
                    rejected.insert(it.1);
                }
            }
        }
        // Nor what borrows one it does not promote, `&&TEST_DROP`.
        let mut stack: FVec<'_, hir_def::hir::ExprId> = Vec::new_in(frame);
        let mut borrows_rejected = |expr: hir_def::hir::ExprId| {
            stack.clear();
            stack.push(expr);
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
                .iter()
                .filter(|&&&(_, expr)| rejected.is_empty() || !borrows_rejected(expr))
                .map(|&&(local, _)| local),
        );
        if item {
            statics.extend(extended_temporaries(ctx, body, decoded));
            statics.extend(const_promoted(ctx, body, decoded));
        } else if ctx.inline_const {
            statics.extend(extended_temporaries(ctx, body, decoded));
        }
        // A function item is a constant of a type with no data, promoted where it is borrowed:
        // `&Trait::method`, which inference may leave without a type when it cannot tell `Self`.
        for constant in decoded.constants.iter() {
            if matches!(
                body.locals[constant.local].ty.as_ref().kind(),
                TyKind::FnDef(..) | TyKind::Error(_)
            ) {
                statics.insert(constant.local);
            }
        }
        let borrows = decoded.reachable_borrows();
        let mut loans = Loans {
            loans: Vec::with_capacity_in(borrows.len(), frame),
            projections: Vec::new_in(frame),
            prefixes: Vec::new_in(frame),
            index: Relation::default(),
            scopes: Relation::default(),
            kills: Relation::default(),
            promoted: promoted_borrows(ctx, body, decoded),
        };
        let mut facts = Facts::with_capacity(frame, borrows.len() * 2);
        let mut two_phase: FMap<'_, LocalId, usize> = mem::map(frame);
        let mut chain: FVec<'_, PlaceId> = Vec::new_in(frame);
        for borrow in borrows {
            let location = borrow.location;
            let projection = decoded.projection(borrow.place);
            ctx.places.borrow().chain(borrow.place, &mut chain);
            let prefixes = &chain[..projection.len()];
            if ignored(ctx, borrow.local, projection, prefixes, &statics)
                || loans.promoted.contains(&location)
            {
                continue;
            }
            let Some(&region) = regions.loan_regions.get(&location) else { continue };
            let mut kind = borrow.kind;
            // rustc makes the mutable borrow a closure captures of a variable without
            // `mut` a unique one.
            if let BorrowKind::Mut { kind: MutBorrowKind::Default } = kind
                && unique(borrow.assigned, borrow.local, projection)
            {
                kind = BorrowKind::Mut { kind: MutBorrowKind::ClosureCapture };
            }
            let two = matches!(kind, BorrowKind::Mut { kind: MutBorrowKind::TwoPhasedBorrow })
                && borrow.assigned_whole;
            let id = loans.loans.len();
            if two {
                two_phase.insert(borrow.assigned, id);
            }
            let start = loans.projections.len() as u32;
            loans.projections.extend_from_slice(projection);
            loans.prefixes.extend_from_slice(prefixes);
            loans.loans.push(Loan {
                local: borrow.local,
                projection: (start, loans.projections.len() as u32),
                kind,
                region,
                location,
                span: borrow.span,
                assigned: borrow.assigned,
                activation: if two {
                    Activation::NotActivated
                } else {
                    Activation::NotTwoPhase
                },
            });
            facts.push(MADE_AT, cfg.point(location), id);
            facts.push(OF_LOCAL, raw(borrow.local), id);
        }
        if !two_phase.is_empty() {
            // A two-phase borrow is activated by the first use of the reference.
            for &block in cfg.order.iter() {
                let start = cfg.block_start(block);
                for point in start..=cfg.block_end(body, block) {
                    let location = Location { block, statement: point - start };
                    for &used in decoded.two_phase.of(point) {
                        if let Some(&id) = two_phase.get(&local(used))
                            && loans.loans[id].activation == Activation::NotActivated
                            && loans.loans[id].location != location
                        {
                            loans.loans[id].activation = Activation::At(location);
                            facts.push(ACTIVATED_AT, point, id);
                        }
                    }
                }
            }
        }
        loans.index = facts.freeze(&[cfg.points, body.locals.len(), cfg.points]);
        loans.kills = loans.find_kills(ctx, body, cfg, decoded);
        loans
    }

    /// The loans each reachable point kills, in the order it kills them.
    fn find_kills<'db>(
        &self,
        ctx: &Ctx<'a, 'db>,
        body: &MirBody<'db>,
        cfg: &Cfg<'_>,
        decoded: &Decoded<'_>,
    ) -> Relation<'a> {
        let mut facts = Facts::new(ctx.frame);
        if self.loans.is_empty() {
            return facts.freeze(&[cfg.points]);
        }
        for &block in cfg.order.iter() {
            for point in cfg.block_start(block)..=cfg.block_end(body, block) {
                self.killed_by(ctx, body, decoded, point, &mut |id| {
                    facts.push(KILLED_AT, point, id)
                });
            }
        }
        facts.freeze(&[cfg.points])
    }

    /// Calls `kill` with each loan the statement or terminator at `point` ends: a write
    /// (an assignment, an `asm!` output, a call's destination, a `DropAndReplace`) or the end
    /// of a local's storage.
    fn killed_by<'db>(
        &self,
        ctx: &Ctx<'a, 'db>,
        body: &MirBody<'db>,
        decoded: &Decoded<'_>,
        point: usize,
        kill: &mut impl FnMut(usize),
    ) {
        for written in decoded.writes.of(point) {
            if written.kind != WriteKind::Yield {
                let place = written.place;
                let projection = decoded.projection(place);
                self.kill_on_place(ctx, body, decoded.local(place), projection, kill);
            }
        }
        let dead = decoded.dead[point];
        if dead != NONE {
            for &id in self.of_local(local(dead)) {
                kill(id as usize);
            }
        }
    }

    /// The projection of the place `id` borrows.
    pub(super) fn projection(&self, id: usize) -> &[PlaceElem] {
        let (start, end) = self.loans[id].projection;
        &self.projections[start as usize..end as usize]
    }

    /// The loan made at `point`, if one is.
    fn made_at(&self, point: usize) -> Option<usize> {
        self.index.get(MADE_AT, point).first().map(|&it| it as usize)
    }

    /// The loans of a place of `local`, in the order they were made.
    pub(super) fn of_local(&self, local: LocalId) -> &'a [u32] {
        self.index.get(OF_LOCAL, raw(local))
    }

    /// The two-phase loans first used at `point`.
    pub(super) fn activated_at(&self, point: usize) -> &'a [u32] {
        self.index.get(ACTIVATED_AT, point)
    }

    /// Where each loan goes out of scope: the first points past it its region does not hold
    /// at, as rustc's `OutOfScopePrecomputer`.
    pub(super) fn compute_scopes(
        &mut self,
        ctx: &Ctx<'a, '_>,
        body: &MirBody<'_>,
        cfg: &Cfg<'_>,
        values: &RegionValues<'_>,
    ) {
        let mut facts = Facts::new(ctx.frame);
        let mut walk = ScopeWalk::new(ctx.frame, body.basic_blocks.len());
        for id in 0..self.loans.len() {
            let points = values.points_of(self.loans[id].region);
            walk.out_of_scope(
                body,
                cfg,
                self.loans[id].location,
                |start, end| points.first_absent_in(start, end),
                |point| facts.push(ENDS_AT, point, id),
            );
        }
        self.scopes = facts.freeze(&[cfg.points]);
    }

    /// Polonius alpha's scopes: a loan goes out of scope at the first point after it is made
    /// that it is not live at, as rustc's `PoloniusOutOfScopePrecomputer`.
    pub(super) fn compute_polonius_scopes(
        &mut self,
        ctx: &Ctx<'a, '_>,
        body: &MirBody<'_>,
        cfg: &Cfg<'_>,
        live: &[IntervalSet<'_>],
    ) {
        let mut facts = Facts::new(ctx.frame);
        let mut walk = ScopeWalk::new(ctx.frame, body.basic_blocks.len());
        for id in 0..self.loans.len() {
            let issued = cfg.point(self.loans[id].location);
            // The loan is in scope where it is made, live there or not.
            walk.out_of_scope(
                body,
                cfg,
                self.loans[id].location,
                |start, end| match live[id].first_absent_in(start, end) {
                    Some(point) if point == issued => {
                        (point < end).then(|| live[id].first_absent_in(point + 1, end)).flatten()
                    }
                    other => other,
                },
                |point| facts.push(ENDS_AT, point, id),
            );
        }
        self.scopes = facts.freeze(&[cfg.points]);
    }

    /// The loans in scope at the start of each block, a row per block: rustc's `Borrows`.
    pub(super) fn entry_sets(&self, ctx: &Ctx<'a, '_>, body: &MirBody<'_>, cfg: &Cfg<'_>) -> Rows<'a> {
        let frame = ctx.frame;
        let blocks = body.basic_blocks.len();
        let count = self.loans.len();
        if count == 0 {
            return Rows::Dense(Dense::new(frame, blocks, 0));
        }
        let mut transfer = Effects::new(frame, blocks, count);
        for &block in cfg.order.iter() {
            let start = cfg.block_start(block);
            let mut row = transfer.row(index(block));
            for point in start..=cfg.block_end(body, block) {
                let location = Location { block, statement: point - start };
                self.before(cfg, location, &mut row);
                self.after(cfg, location, &mut row);
            }
        }
        let boundary = bits(frame, count);
        forward(frame, cfg, &transfer, body.start_block, boundary.words(), |_, _| Edge::Same)
    }

    /// The loans that go out of scope as `location` starts.
    pub(super) fn before(&self, cfg: &Cfg<'_>, location: Location, state: &mut impl Transfer) {
        for &id in self.scopes.get(ENDS_AT, cfg.point(location)) {
            state.kill(id as usize);
        }
    }

    /// What the statement or terminator at `location` does to the loans in scope: the kills
    /// [`Self::find_kills`] found, then the loan it makes.
    pub(super) fn after(&self, cfg: &Cfg<'_>, location: Location, state: &mut impl Transfer) {
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
        ctx: &Ctx<'a, 'db>,
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

/// What [`ScopeWalk::out_of_scope`] keeps from one loan to the next: the blocks a walk
/// entered, as a set and as a list to clear it by.
struct ScopeWalk<'f> {
    entered: BitSet<'f>,
    entered_list: FVec<'f, BasicBlockId>,
    work: FVec<'f, BasicBlockId>,
}

impl<'f> ScopeWalk<'f> {
    fn new(frame: &'f Frame, blocks: usize) -> Self {
        ScopeWalk {
            entered: bits(frame, blocks),
            entered_list: Vec::new_in(frame),
            work: Vec::new_in(frame),
        }
    }

    /// Gives `end_at` the first points reachable from just past `issued` that are not in
    /// scope, given `first_absent(start, end)`, the first point of `start..=end` not in
    /// scope. It walks whole blocks: a block's points are consecutive, so one lookup finds
    /// where a loan ends in it.
    fn out_of_scope(
        &mut self,
        body: &MirBody<'_>,
        cfg: &Cfg<'_>,
        issued: Location,
        mut first_absent: impl FnMut(usize, usize) -> Option<usize>,
        mut end_at: impl FnMut(usize),
    ) {
        for block in self.entered_list.drain(..) {
            self.entered.remove(index(block));
        }
        self.work.clear();
        let work = &mut self.work;
        let last = |block: BasicBlockId| cfg.block_end(body, block);
        // The rest of the issuing block, if the loan is not made by its terminator.
        let issued_point = cfg.point(issued);
        let rest = issued.statement < body.basic_blocks[issued.block].statements.len();
        if rest {
            match first_absent(issued_point + 1, last(issued.block)) {
                Some(point) => end_at(point),
                None => work.extend(cfg.successors(issued.block).iter().copied()),
            }
        } else {
            work.extend(cfg.successors(issued.block).iter().copied());
        }
        while let Some(block) = work.pop() {
            if !self.entered.insert(index(block)) {
                continue;
            }
            self.entered_list.push(block);
            let start = cfg.block_start(block);
            // Coming back around to the issuing block, the points past the loan were seen.
            let (end, through) = if rest && block == issued.block {
                (issued_point, false)
            } else {
                (last(block), true)
            };
            match first_absent(start, end) {
                Some(point) => end_at(point),
                None if through => work.extend(cfg.successors(block).iter().copied()),
                None => (),
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Overlap {
    Arbitrary,
    EqualOrDisjoint,
    Disjoint,
}

/// Whether an access of `depth` to the place `local.projection` touches what loan `id`
/// borrows, as rustc's `borrow_conflicts_with_place`.
pub(super) fn places_conflict<'db>(
    ctx: &Ctx<'_, 'db>,
    _body: &MirBody<'db>,
    loans: &Loans<'_>,
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
    let prefixes = &loans.prefixes[loan.projection.0 as usize..loan.projection.1 as usize];
    for (index, &elem) in borrow.iter().enumerate() {
        // Each prefix's type, as `multi_projection_ty` folds them.
        let base_ty = ctx.places.borrow().ty(prefixes[index]);
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
