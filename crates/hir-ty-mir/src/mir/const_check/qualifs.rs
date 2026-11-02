//! The flow-sensitive half of rustc's const checker (`check_consts/{qualifs,resolver}.rs`, the
//! qualif checks of `check.rs` and `post_drop_elaboration.rs`): which values of a const context
//! hold something with a destructor, a destructor that is not const, or interior mutability, and
//! what of that rustc refuses: a value dropped in the body (E0493), a mutable borrow (E0764) or a
//! shared borrow of a cell (E0492) that may outlive the evaluation, and a read of a
//! `#[thread_local]` static (E0625).
//!
//! A qualif is in a local by what was assigned to it, not by its type: `None` of an
//! `Option<String>` holds nothing to drop, and a local moved out of holds nothing either. As
//! rustc, what is not a local's value (a call's result, an argument, what is behind a pointer)
//! has a qualif when some value of its type may.

use std::collections::VecDeque;

use base_db::Db;
use blaze_ids::bits::{BitMatrix, words_for};
use blaze_mem::{Frame, Tag, frame};
use hir_def::{
    AdtId, DefWithBodyId, HasModule, StaticId,
    attrs::AttrFlags,
    expr_store::{Body, ExpressionStore},
    hir::{Expr, type_ref::Rawness},
    signatures::{StaticFlags, StaticSignature},
    unstable_features::UnstableFeatures,
};
use intern::Symbol;
use rustc_type_ir::{BoundConstness, TypeVisitableExt, inherent::IntoKind};

use hir_ty::{
    InferBodyId,
    db::HirDatabase,
    drop::{DropGlue, has_drop_glue_in, is_freeze},
    effects::const_param_env,
    next_solver::{
        AliasConstKind, AnonConstIdWrapper, Binder, ConstKind, DbInterner, FreeConstAliasId,
        InherentAssocConstId, ParamEnv, TraitRef, Ty, TyKind, TypingMode,
        infer::{
            DbInternerInferExt, InferCtxt,
            traits::{Obligation, ObligationCause},
        },
    },
};

use crate::mir::{
    AggregateKind, AsmOperand, BasicBlockId, BorrowKind, LocalId, MirBody, MirSpan, Operand,
    OperandKind, PlaceElem, PlaceTy, ProjectionElem, Rvalue, Statement, StatementKind,
    StoredPlace, Terminator, TerminatorKind,
    framed::{Bits, FrameMap, bits_in, block_index, local_index, map_in},
    return_slot,
};

use super::{ConstCheckKind, ConstContext, reachable as reachable_blocks, stability};

/// What rustc's qualif-based checks refuse in `body`, a const context of kind `context`:
/// each a kind and the span rustc points at, in the order rustc visits them.
pub(super) fn check_qualifs<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    body: &MirBody<'db>,
    context: ConstContext,
    reachable: &[BasicBlockId],
    frame: &Frame,
    report: &mut impl FnMut(ConstCheckKind, MirSpan),
) {
    let precise = precise_live_drops(db, owner, context);
    // rustc's `ConstContext::Static(Mutability::Mut)`, where any mutable borrow is allowed.
    let mutable_static = matches!(
        owner,
        InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(it))
            if StaticSignature::of(db, it).flags.contains(StaticFlags::MUTABLE)
    );
    let live = blocks_in(body, reachable, frame);
    let mut checker = Checker::new(db, body, context, frame);
    let mut drops = false;
    for (block, data) in body.basic_blocks.iter() {
        if !live.contains(block_index(block)) || data.is_cleanup {
            continue;
        }
        for (index, statement) in data.statements.iter().enumerate() {
            checker.check_statement(block, index, statement, mutable_static, report);
        }
        let Some(terminator) = &data.terminator else { continue };
        let mut thread_local = false;
        terminator.kind.for_each_operand(&mut |it| thread_local |= reads_thread_local(db, it));
        if thread_local {
            report(ConstCheckKind::ThreadLocalAccess, terminator.span);
        }
        let (TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. }) =
            &terminator.kind
        else {
            continue;
        };
        if precise {
            let ty = checker.cx.place_ty(place);
            drops |= checker.cx.in_any_value_of_ty(Qualif::NeedsDrop, ty);
            continue;
        }
        let index = data.statements.len();
        checker.check_drop(owner, block, index, place, terminator.span, report);
    }
    // rustc's `check_live_drops`, over the body after drop elaboration: a drop elaboration
    // finds dead is gone, and one of a value moved out of in part drops the rest. Elaboration
    // only takes drops away or splits one into its fields' drops, so a body with no drop of
    // something to drop has none after it either.
    if !drops {
        return;
    }
    // A body that builds but does not get to runtime MIR has that error reported by what asks
    // for its runtime MIR, evaluation and code generation.
    let Ok(runtime) = db.runtime_mir_body(owner) else { return };
    let live = blocks_in(runtime, &reachable_blocks(runtime, frame), frame);
    let mut checker = Checker::new(db, runtime, context, frame);
    for (block, data) in runtime.basic_blocks.iter() {
        if !live.contains(block_index(block)) || data.is_cleanup {
            continue;
        }
        if let Some(Terminator { kind: TerminatorKind::Drop { place, .. }, span }) = &data.terminator
        {
            checker.check_drop(owner, block, data.statements.len(), place, *span, report);
        }
    }
}

/// `blocks` as a set.
fn blocks_in<'f>(body: &MirBody<'_>, blocks: &[BasicBlockId], frame: &'f Frame) -> Bits<'f> {
    let mut set: Bits<'f> = bits_in(body.basic_blocks.len(), frame);
    for &block in blocks {
        set.insert(block_index(block));
    }
    set
}

/// rustc's `post_drop_elaboration::checking_enabled`: whether live drops are checked after
/// drop elaboration (`const_precise_live_drops`). A `const fn` of a `staged_api` crate that is
/// safe to expose on stable is checked before it, unless it allows the feature with
/// `#[rustc_allow_const_fn_unstable(const_precise_live_drops)]`.
fn precise_live_drops(db: &Db, owner: InferBodyId<'_>, context: ConstContext) -> bool {
    let krate = owner.module(db).krate(db);
    let features = UnstableFeatures::query(db, krate);
    let gate = Symbol::intern("const_precise_live_drops");
    // rustc's `enforce_recursive_const_stability`.
    if let Some(func) = owner.as_function()
        && context == ConstContext::ConstFn
        && features.staged_api
        && stability::safe_to_expose(db, stability::Item::Fn(func), krate)
    {
        return stability::allows_const_fn_unstable(db, func, &gate);
    }
    features.is_enabled(&gate)
}

/// Whether `operand` reads a `#[thread_local]` static: rustc's `Rvalue::ThreadLocalRef`.
fn reads_thread_local(db: &Db, operand: &Operand) -> bool {
    matches!(
        operand.kind,
        OperandKind::Static(it) if is_thread_local(db, it)
    )
}

fn is_thread_local(db: &Db, static_: StaticId) -> bool {
    AttrFlags::query(db, static_.into()).contains(AttrFlags::THREAD_LOCAL)
}

/// One of rustc's three `Qualif`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Qualif {
    /// A value with an `UnsafeCell` in it, which is not `Freeze`.
    HasMutInterior = 0,
    /// A value with a destructor to run.
    NeedsDrop = 1,
    /// A value with a destructor to run that is not `[const] Destruct`.
    NeedsNonConstDrop = 2,
}

impl Qualif {
    /// rustc's `IS_CLEARED_ON_MOVE`: a local moved out of holds nothing to drop, but what is
    /// borrowed of a cell may still be reached through the borrow.
    fn is_cleared_on_move(self) -> bool {
        self != Qualif::HasMutInterior
    }

    fn of(self, qualifs: ConstQualifs) -> bool {
        match self {
            Qualif::HasMutInterior => qualifs.has_mut_interior,
            Qualif::NeedsDrop => qualifs.needs_drop,
            Qualif::NeedsNonConstDrop => qualifs.needs_non_const_drop,
        }
    }
}

/// rustc's `ConstQualifs` of a constant (`mir_const_qualif`): which qualifs the value its body
/// returns has. A const context reading the constant takes these, not its type's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, blaze_query::Value)]
struct ConstQualifs {
    has_mut_interior: bool,
    needs_drop: bool,
    needs_non_const_drop: bool,
}

impl ConstQualifs {
    /// What leaves the value to its type.
    const BY_TYPE: ConstQualifs =
        ConstQualifs { has_mut_interior: true, needs_drop: true, needs_non_const_drop: true };
}

#[blaze_query::tracked(cycle_result = const_qualifs_cycle_result, returns(copy))]
fn const_qualifs<'db>(db: &'db Db, def: InferBodyId<'db>) -> ConstQualifs {
    // A constant whose MIR does not build is refused where it is lowered.
    let Ok(body) = db.mir_body(def) else { return ConstQualifs::BY_TYPE };
    frame::task(Tag::Mir, |frame| {
        Checker::new(db, body, ConstContext::Const, frame).qualifs_in_return_place()
    })
}

fn const_qualifs_cycle_result<'db>(
    _db: &'db Db,
    _: blaze_query::db::Id,
    _def: InferBodyId<'db>,
) -> ConstQualifs {
    ConstQualifs::BY_TYPE
}

/// The qualifs of the constant `item` when rustc peeks at its value: not of a trait's
/// associated constant, whose value the impl picks.
fn qualifs_of_item<'db>(db: &'db Db, item: AliasConstKind<'db>) -> Option<ConstQualifs> {
    match item {
        AliasConstKind::Projection { .. } => None,
        AliasConstKind::InherentSelf { def_id: InherentAssocConstId(it) }
        | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(it) }
        | AliasConstKind::Free { def_id: FreeConstAliasId(it) } => {
            Some(const_qualifs(db, it.into()))
        }
        AliasConstKind::Anon { def_id: AnonConstIdWrapper(it) } => {
            Some(const_qualifs(db, it.into()))
        }
    }
}

/// What the qualifs ask of the types of a body.
struct Cx<'a, 'f, 'db> {
    db: &'db Db,
    body: &'a MirBody<'db>,
    context: ConstContext,
    infcx: InferCtxt<'db>,
    env: ParamEnv<'db>,
    frame: &'f Frame,
    /// Per type, for qualif `q`: bit `2q` that it is known, bit `2q + 1` that it holds.
    types: FrameMap<'f, Ty<'db>, u8>,
    store: Option<&'db ExpressionStore>,
}

impl<'a, 'f, 'db> Cx<'a, 'f, 'db> {
    fn new(db: &'db Db, body: &'a MirBody<'db>, context: ConstContext, frame: &'f Frame) -> Self {
        let owner = body.owner;
        let env = const_param_env(db, owner.generic_def(db));
        let infcx =
            DbInterner::new_with(db, env.krate).infer_ctxt().build(TypingMode::non_body_analysis());
        Cx {
            db,
            body,
            context,
            infcx,
            env: env.param_env,
            frame,
            types: map_in(frame),
            store: None,
        }
    }

    fn local_ty(&self, local: LocalId) -> Ty<'db> {
        self.body.locals[local].ty.as_ref()
    }

    fn projected_ty(&self, local: LocalId, elems: &[PlaceElem]) -> Ty<'db> {
        PlaceTy::from_ty(self.local_ty(local)).multi_projection_ty(&self.infcx, self.env, elems).ty
    }

    fn place_ty(&self, place: &StoredPlace) -> Ty<'db> {
        self.projected_ty(place.local, place.projection.as_slice())
    }

    /// rustc's `Qualif::in_any_value_of_ty`.
    fn in_any_value_of_ty(&mut self, q: Qualif, ty: Ty<'db>) -> bool {
        let known = 1u8 << (2 * q as u8);
        let holds = known << 1;
        let cached = self.types.get(&ty).copied().unwrap_or(0);
        if cached & known != 0 {
            return cached & holds != 0;
        }
        let value = match q {
            Qualif::HasMutInterior => !is_freeze(&self.infcx, ty, self.env),
            Qualif::NeedsDrop => has_drop_glue_in(self.frame, &self.infcx, ty, self.env) != DropGlue::None,
            Qualif::NeedsNonConstDrop => {
                self.in_any_value_of_ty(Qualif::NeedsDrop, ty) && !self.is_const_destruct(ty)
            }
        };
        let cached = self.types.get(&ty).copied().unwrap_or(0);
        self.types.insert(ty, cached | known | if value { holds } else { 0 });
        value
    }

    /// Whether `ty: [const] Destruct` holds, `const` in a constant or static and conditionally
    /// in a `const fn`. What the solver can't decide does not hold.
    fn is_const_destruct(&self, ty: Ty<'db>) -> bool {
        let interner = self.infcx.interner;
        let Some(destruct) = interner.lang_items().Destruct else { return false };
        let constness = match self.context {
            ConstContext::ConstFn => BoundConstness::Maybe,
            ConstContext::Const | ConstContext::Static => BoundConstness::Const,
        };
        let clause = Binder::dummy(TraitRef::new(interner, destruct.into(), [ty]))
            .to_host_effect_clause(interner, constness);
        let obligation =
            Obligation::new(interner, ObligationCause::dummy(), self.env, clause.as_predicate());
        self.infcx.predicate_must_hold_modulo_regions(&obligation)
    }

    /// rustc's `Qualif::is_structural_in_adt_value`: whether a value of `adt` has the qualif
    /// only by what it is made of. An `UnsafeCell` has interior mutability whatever it holds,
    /// and a type with a destructor needs that run.
    fn is_structural(&self, q: Qualif, adt: AdtId) -> bool {
        match q {
            Qualif::HasMutInterior => self
                .infcx
                .interner
                .lang_items()
                .UnsafeCell
                .is_none_or(|cell| AdtId::StructId(cell) != adt),
            Qualif::NeedsDrop | Qualif::NeedsNonConstDrop => {
                hir_ty::drop::destructor(self.db, adt).is_none()
            }
        }
    }

    /// Whether the borrow written at `span` is `&raw const` or `&raw mut`: lowering makes
    /// both a `Ref` at the expression.
    fn is_raw(&mut self, span: MirSpan) -> bool {
        let MirSpan::ExprId(expr) = span else { return false };
        let (db, owner) = (self.db, self.body.owner);
        let store =
            *self.store.get_or_insert_with(|| ExpressionStore::of(db, owner.expression_store_owner(db)));
        matches!(store.expr(expr), Expr::Ref { rawness: Rawness::RawPtr, .. })
    }
}

fn is_indirect(elems: &[PlaceElem]) -> bool {
    elems.iter().any(|it| matches!(it, ProjectionElem::Deref))
}

/// rustc's `qualifs::in_place`: a projection keeps the qualif of what it projects from, unless
/// its type can't have it; behind a pointer it is the type's.
fn in_place<'a, 'f, 'db, F>(
    cx: &mut Cx<'a, 'f, 'db>,
    q: Qualif,
    in_local: &mut F,
    local: LocalId,
    elems: &[PlaceElem],
) -> bool
where
    F: FnMut(&mut Cx<'a, 'f, 'db>, LocalId) -> bool,
{
    for at in (0..elems.len()).rev() {
        if let ProjectionElem::Index(index) = elems[at]
            && in_local(cx, index)
        {
            return true;
        }
        let ty = cx.projected_ty(local, &elems[..=at]);
        if !cx.in_any_value_of_ty(q, ty) {
            return false;
        }
        if matches!(elems[at], ProjectionElem::Deref) {
            return true;
        }
    }
    in_local(cx, local)
}

/// rustc's `qualifs::in_operand`: a constant's is what its value has, when rustc peeks at it,
/// and any value of its type may otherwise.
fn in_operand<'a, 'f, 'db, F>(
    cx: &mut Cx<'a, 'f, 'db>,
    q: Qualif,
    in_local: &mut F,
    operand: &Operand,
) -> bool
where
    F: FnMut(&mut Cx<'a, 'f, 'db>, LocalId) -> bool,
{
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => {
            in_place(cx, q, in_local, place.local, place.projection.as_slice())
        }
        OperandKind::Constant { konst, ty } => {
            if let ConstKind::Alias(_, alias) = konst.as_ref().kind()
                && let Some(qualifs) = qualifs_of_item(cx.db, alias.kind)
                && !q.of(qualifs)
            {
                return false;
            }
            cx.in_any_value_of_ty(q, ty.as_ref())
        }
        OperandKind::Allocation { allocation } => cx.in_any_value_of_ty(q, allocation.as_ref().ty),
        // A `&'static` to the static, which is `Freeze` and has nothing to drop.
        OperandKind::Static(_) => false,
    }
}

/// rustc's `qualifs::in_rvalue`, for `rvalue` assigned to `dest`.
fn in_rvalue<'a, 'f, 'db, F>(
    cx: &mut Cx<'a, 'f, 'db>,
    q: Qualif,
    in_local: &mut F,
    rvalue: &Rvalue,
    dest: &StoredPlace,
) -> bool
where
    F: FnMut(&mut Cx<'a, 'f, 'db>, LocalId) -> bool,
{
    match rvalue {
        Rvalue::Use(operand)
        | Rvalue::Repeat(operand, _)
        | Rvalue::UnaryOp(_, operand)
        | Rvalue::Cast(_, operand, _) => in_operand(cx, q, in_local, operand),
        Rvalue::BinaryOp(_, lhs, rhs) | Rvalue::CheckedBinaryOp(_, lhs, rhs) => {
            in_operand(cx, q, in_local, lhs) || in_operand(cx, q, in_local, rhs)
        }
        Rvalue::Discriminant(place) | Rvalue::CopyForDeref(place) => {
            in_place(cx, q, in_local, place.local, place.projection.as_slice())
        }
        // A `usize`.
        Rvalue::Len(_) => false,
        Rvalue::Ref(_, place) => {
            let elems = place.projection.as_slice();
            // A reborrow is as a copy of the reference.
            if let [base @ .., ProjectionElem::Deref] = elems
                && matches!(cx.projected_ty(place.local, base).kind(), TyKind::Ref(..))
            {
                return in_place(cx, q, in_local, place.local, base);
            }
            in_place(cx, q, in_local, place.local, elems)
        }
        Rvalue::Aggregate(kind, operands) => {
            let by_type = match kind {
                AggregateKind::Adt(variant, _) => !cx.is_structural(q, variant.adt_id(cx.db)),
                AggregateKind::Union(..) => true,
                AggregateKind::Array(_) | AggregateKind::Tuple(_) | AggregateKind::Closure(_) => {
                    false
                }
            };
            if by_type {
                let ty = cx.place_ty(dest);
                return cx.in_any_value_of_ty(q, ty);
            }
            operands.iter().any(|operand| in_operand(cx, q, in_local, operand))
        }
        Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
            match *never {}
        }
    }
}

/// The blocks dataflow goes through: what control reaches from the start along the edges taken
/// when nothing unwinds, and a `FalseEdge`'s imaginary one, without the blocks lowering builds
/// that rustc builds no way into.
struct Graph<'f> {
    /// In the order first met.
    order: Vec<BasicBlockId, &'f Frame>,
    reached: Bits<'f>,
}

impl<'f> Graph<'f> {
    fn new(body: &MirBody<'_>, frame: &'f Frame) -> Graph<'f> {
        let blocks = body.basic_blocks.len();
        let mut unreached: Bits<'f> = bits_in(blocks, frame);
        for &block in &body.unreached {
            unreached.insert(block_index(block));
        }
        let mut reached: Bits<'f> = bits_in(blocks, frame);
        let mut order = Vec::with_capacity_in(blocks, frame);
        let mut stack = Vec::with_capacity_in(blocks, frame);
        stack.push(body.start_block);
        while let Some(block) = stack.pop() {
            let at = block_index(block);
            if reached.contains(at) || unreached.contains(at) {
                continue;
            }
            reached.insert(at);
            order.push(block);
            if let Some(terminator) = &body.basic_blocks[block].terminator {
                flow_successors(&terminator.kind, |next| stack.push(next));
            }
        }
        Graph { order, reached }
    }
}

fn flow_successors(kind: &TerminatorKind, mut f: impl FnMut(BasicBlockId)) {
    for next in kind.successors() {
        f(next);
    }
    if let TerminatorKind::FalseEdge { imaginary_target, .. } = kind {
        f(*imaginary_target);
    }
}

type Matrix<'f> = BitMatrix<usize, &'f mut [u64]>;

fn matrix_in<'f>(rows: usize, columns: usize, frame: &'f Frame) -> Matrix<'f> {
    BitMatrix::from_words(rows, columns, frame.alloc_slice_fill_with(rows * words_for(columns), |_| 0))
}

/// A forward dataflow to its fixpoint, joining by union: `entry` has each block's state on
/// entry, the start block's set, and `apply` takes a state through a block.
fn fixpoint<'f>(
    body: &MirBody<'_>,
    graph: &Graph<'f>,
    entry: &mut Matrix<'f>,
    state: &mut Bits<'f>,
    frame: &'f Frame,
    mut apply: impl FnMut(&mut Bits<'f>, BasicBlockId),
) {
    let mut queued: Bits<'f> = bits_in(body.basic_blocks.len(), frame);
    let mut queue = VecDeque::with_capacity_in(graph.order.len(), frame);
    for &block in graph.order.iter() {
        queued.insert(block_index(block));
        queue.push_back(block);
    }
    while let Some(block) = queue.pop_front() {
        queued.remove(block_index(block));
        state.copy_from(&entry.row(block_index(block)));
        apply(state, block);
        let Some(terminator) = &body.basic_blocks[block].terminator else { continue };
        let out: &Bits<'f> = state;
        flow_successors(&terminator.kind, |next| {
            let at = block_index(next);
            if graph.reached.contains(at) && entry.union_row_with(at, out) && !queued.contains(at) {
                queued.insert(at);
                queue.push_back(next);
            }
        });
    }
}

/// rustc's `FlowSensitiveAnalysis` of one qualif, with a cursor into it. A state has the
/// locals the qualif is in, then from `borrowed` on the locals borrowed in a way that may
/// change them, which a move then does not clear.
struct Flow<'f> {
    q: Qualif,
    entry: Matrix<'f>,
    state: Bits<'f>,
    borrowed: usize,
    /// Where `state` is: before statement `index` of `block`.
    block: Option<BasicBlockId>,
    index: usize,
}

impl<'f> Flow<'f> {
    fn compute<'db>(cx: &mut Cx<'_, 'f, 'db>, graph: &Graph<'f>, q: Qualif) -> Flow<'f> {
        let (body, frame) = (cx.body, cx.frame);
        let borrowed = words_for(body.locals.len()) * 64;
        let mut entry = matrix_in(body.basic_blocks.len(), 2 * borrowed, frame);
        let start = block_index(body.start_block);
        for &param in &body.param_locals {
            let ty = cx.local_ty(param);
            if cx.in_any_value_of_ty(q, ty) {
                entry.insert(start, local_index(param));
            }
        }
        let mut state: Bits<'f> = bits_in(2 * borrowed, frame);
        fixpoint(body, graph, &mut entry, &mut state, frame, |state, block| {
            let data = &body.basic_blocks[block];
            for statement in &data.statements {
                statement_effect(cx, q, state, borrowed, statement);
            }
            if let Some(terminator) = &data.terminator {
                terminator_effect(cx, q, state, borrowed, terminator);
            }
        });
        Flow { q, entry, state, borrowed, block: None, index: 0 }
    }

    /// Moves the cursor to before statement `index` of `block` (its terminator at the
    /// statements' count).
    fn seek(&mut self, cx: &mut Cx<'_, 'f, '_>, block: BasicBlockId, index: usize) {
        if self.block != Some(block) || self.index > index {
            self.state.copy_from(&self.entry.row(block_index(block)));
            self.block = Some(block);
            self.index = 0;
        }
        let body = cx.body;
        let statements = &body.basic_blocks[block].statements;
        while self.index < index {
            statement_effect(cx, self.q, &mut self.state, self.borrowed, &statements[self.index]);
            self.index += 1;
        }
    }
}

/// rustc's `TransferFunction::assign_qualif_direct`: an assignment to a place that is not
/// behind a pointer qualifies its local, never unqualifies it; to a field of a union that may
/// have the qualif it qualifies it whatever is assigned.
fn assign_direct(
    cx: &mut Cx<'_, '_, '_>,
    q: Qualif,
    state: &mut Bits<'_>,
    place: &StoredPlace,
    value: bool,
) {
    let elems = place.projection.as_slice();
    let value = value
        || (0..elems.len()).any(|at| {
            let base = cx.projected_ty(place.local, &elems[..at]);
            base.is_union() && cx.in_any_value_of_ty(q, base)
        });
    if value {
        state.insert(local_index(place.local));
    }
}

fn clear_moved(state: &mut Bits<'_>, borrowed: usize, operand: &Operand) {
    if let OperandKind::Move(place) = &operand.kind
        && place.projection.is_empty()
        && !state.contains(borrowed + local_index(place.local))
    {
        state.remove(local_index(place.local));
    }
}

fn statement_effect(
    cx: &mut Cx<'_, '_, '_>,
    q: Qualif,
    state: &mut Bits<'_>,
    borrowed: usize,
    statement: &Statement,
) {
    match &statement.kind {
        StatementKind::Assign(place, rvalue) => {
            let value = in_rvalue(cx, q, &mut |_, local| state.contains(local_index(local)), rvalue, place);
            if !is_indirect(place.projection.as_slice()) {
                assign_direct(cx, q, state, place, value);
            }
            if q.is_cleared_on_move() {
                rvalue.for_each_operand(&mut |operand| clear_moved(state, borrowed, operand));
            }
            let Rvalue::Ref(kind, target) = rvalue else { return };
            let elems = target.projection.as_slice();
            if is_indirect(elems) {
                return;
            }
            let ty = cx.projected_ty(target.local, elems);
            // rustc's `ref_allows_mutation` and `address_of_allows_mutation`.
            let allows_mutation = match kind {
                BorrowKind::Mut { .. } => true,
                BorrowKind::Shared | BorrowKind::Shallow => {
                    cx.is_raw(statement.span) || cx.in_any_value_of_ty(Qualif::HasMutInterior, ty)
                }
            };
            if allows_mutation && cx.in_any_value_of_ty(q, ty) {
                state.insert(local_index(target.local));
                state.insert(borrowed + local_index(target.local));
            }
        }
        &StatementKind::StorageDead(local) => {
            state.remove(local_index(local));
            state.remove(borrowed + local_index(local));
        }
        // rustc's `InlineAsm` terminator: what it reads is moved, and its outputs are as a
        // call's result.
        StatementKind::InlineAsm(asm) => {
            for operand in &asm.operands {
                if let AsmOperand::Reg { input: Some(input), .. } = operand
                    && q.is_cleared_on_move()
                {
                    clear_moved(state, borrowed, input);
                }
            }
            for operand in &asm.operands {
                if let AsmOperand::Reg { output: Some(output), .. } = operand {
                    assign_result(cx, q, state, output);
                }
            }
        }
        StatementKind::FakeRead(_)
        | StatementKind::Deinit(_)
        | StatementKind::StorageLive(_)
        | StatementKind::Nop => {}
    }
}

fn terminator_effect(
    cx: &mut Cx<'_, '_, '_>,
    q: Qualif,
    state: &mut Bits<'_>,
    borrowed: usize,
    terminator: &Terminator,
) {
    if let TerminatorKind::DropAndReplace { place, value, .. } = &terminator.kind {
        let value = in_operand(cx, q, &mut |_, local| state.contains(local_index(local)), value);
        if !is_indirect(place.projection.as_slice()) {
            assign_direct(cx, q, state, place, value);
        }
    }
    if q.is_cleared_on_move() {
        terminator.kind.for_each_operand(&mut |operand| clear_moved(state, borrowed, operand));
    }
    match &terminator.kind {
        TerminatorKind::Call { destination: place, .. }
        | TerminatorKind::Yield { resume_arg: place, .. } => assign_result(cx, q, state, place),
        _ => {}
    }
}

/// rustc's `apply_call_return_effect`: what a call returns may be any value of its type.
fn assign_result(cx: &mut Cx<'_, '_, '_>, q: Qualif, state: &mut Bits<'_>, place: &StoredPlace) {
    if is_indirect(place.projection.as_slice()) {
        return;
    }
    let ty = cx.place_ty(place);
    let value = cx.in_any_value_of_ty(q, ty);
    assign_direct(cx, q, state, place, value);
}

/// The locals rustc's `local_is_transient` finds: dead at every `Return`, by `MaybeStorageLive`
/// from the locals with no storage markers and the parameters.
fn transient_locals<'f>(body: &MirBody<'_>, graph: &Graph<'f>, frame: &'f Frame) -> Bits<'f> {
    let locals = body.locals.len();
    let mut always: Bits<'f> = bits_in(locals, frame);
    always.insert_range(0..locals);
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::StorageLive(local) | StatementKind::StorageDead(local) =
                statement.kind
            {
                always.remove(local_index(local));
            }
        }
    }
    let mut entry = matrix_in(body.basic_blocks.len(), locals, frame);
    let start = block_index(body.start_block);
    entry.union_row_with(start, &always);
    for &param in &body.param_locals {
        entry.insert(start, local_index(param));
    }
    let mut state: Bits<'f> = bits_in(locals, frame);
    fixpoint(body, graph, &mut entry, &mut state, frame, |state, block| {
        storage_effect(body, state, block)
    });
    let mut transient = always;
    transient.insert_range(0..locals);
    for &block in graph.order.iter() {
        let returns = matches!(
            body.basic_blocks[block].terminator.as_ref().map(|it| &it.kind),
            Some(TerminatorKind::Return)
        );
        if returns {
            state.copy_from(&entry.row(block_index(block)));
            storage_effect(body, &mut state, block);
            transient.subtract(&state);
        }
    }
    transient
}

fn storage_effect(body: &MirBody<'_>, state: &mut Bits<'_>, block: BasicBlockId) {
    for statement in &body.basic_blocks[block].statements {
        match statement.kind {
            StatementKind::StorageLive(local) => {
                state.insert(local_index(local));
            }
            StatementKind::StorageDead(local) => {
                state.remove(local_index(local));
            }
            _ => {}
        }
    }
}

/// rustc's `Checker`, for its qualif checks: the qualifs' dataflow, each run when first asked
/// of a local whose type may have it, and the transient locals.
struct Checker<'a, 'f, 'db> {
    cx: Cx<'a, 'f, 'db>,
    graph: Graph<'f>,
    flows: [Option<Flow<'f>>; 3],
    transient: Option<Bits<'f>>,
}

impl<'a, 'f, 'db> Checker<'a, 'f, 'db> {
    fn new(
        db: &'db Db,
        body: &'a MirBody<'db>,
        context: ConstContext,
        frame: &'f Frame,
    ) -> Self {
        Checker {
            cx: Cx::new(db, body, context, frame),
            graph: Graph::new(body, frame),
            flows: [None, None, None],
            transient: None,
        }
    }

    /// rustc's `Qualifs::in_local`: whether `q` holds of `local` before statement `index` of
    /// `block`.
    fn in_local(&mut self, q: Qualif, local: LocalId, block: BasicBlockId, index: usize) -> bool {
        local_qualif(&mut self.cx, &self.graph, &mut self.flows, q, local, block, index)
    }

    /// rustc's `Qualifs::in_return_place`, at the first `Return`.
    fn qualifs_in_return_place(&mut self) -> ConstQualifs {
        let body = self.cx.body;
        let returns = body.basic_blocks.iter().find(|(_, block)| {
            matches!(block.terminator.as_ref().map(|it| &it.kind), Some(TerminatorKind::Return))
        });
        let Some((block, data)) = returns else {
            let ty = self.cx.local_ty(return_slot());
            return ConstQualifs {
                has_mut_interior: self.cx.in_any_value_of_ty(Qualif::HasMutInterior, ty),
                needs_drop: self.cx.in_any_value_of_ty(Qualif::NeedsDrop, ty),
                needs_non_const_drop: self.cx.in_any_value_of_ty(Qualif::NeedsNonConstDrop, ty),
            };
        };
        let index = data.statements.len();
        ConstQualifs {
            has_mut_interior: self.in_local(Qualif::HasMutInterior, return_slot(), block, index),
            needs_drop: self.in_local(Qualif::NeedsDrop, return_slot(), block, index),
            needs_non_const_drop: self.in_local(
                Qualif::NeedsNonConstDrop,
                return_slot(),
                block,
                index,
            ),
        }
    }

    /// rustc's `place_may_escape`: a borrow in a constant or static of a place that may still
    /// be there when the evaluation ends. A `const fn`'s borrows all end with the call or
    /// borrow from its arguments.
    fn place_may_escape(&mut self, local: LocalId, elems: &[PlaceElem]) -> bool {
        if self.cx.context == ConstContext::ConstFn || is_indirect(elems) {
            return false;
        }
        let (cx, graph) = (&self.cx, &self.graph);
        let transient =
            self.transient.get_or_insert_with(|| transient_locals(cx.body, graph, cx.frame));
        !transient.contains(local_index(local))
    }

    /// The checks of rustc's `visit_rvalue` at a statement: `ThreadLocalAccess`,
    /// `EscapingMutBorrow` and `EscapingCellBorrow`.
    fn check_statement(
        &mut self,
        block: BasicBlockId,
        index: usize,
        statement: &Statement,
        mutable_static: bool,
        report: &mut impl FnMut(ConstCheckKind, MirSpan),
    ) {
        let db = self.cx.db;
        let mut thread_local = false;
        statement.kind.for_each_operand(&mut |it| thread_local |= reads_thread_local(db, it));
        if thread_local {
            report(ConstCheckKind::ThreadLocalAccess, statement.span);
        }
        let StatementKind::Assign(_, Rvalue::Ref(kind, place)) = &statement.kind else { return };
        let elems = place.projection.as_slice();
        match kind {
            BorrowKind::Mut { .. } => {
                if !mutable_static && self.place_may_escape(place.local, elems) {
                    report(ConstCheckKind::EscapingMutBorrow, statement.span);
                }
            }
            BorrowKind::Shared | BorrowKind::Shallow => {
                let (cx, graph, flows) = (&mut self.cx, &self.graph, &mut self.flows);
                let q = Qualif::HasMutInterior;
                let interior = in_place(
                    cx,
                    q,
                    &mut |cx, local| local_qualif(cx, graph, flows, q, local, block, index),
                    place.local,
                    elems,
                );
                if interior && self.place_may_escape(place.local, elems) {
                    report(ConstCheckKind::EscapingCellBorrow, statement.span);
                }
            }
        }
    }

    /// rustc's `check_drop_terminator`: a drop at statement `index` of `block` of a value that
    /// needs it.
    fn check_drop(
        &mut self,
        owner: InferBodyId<'_>,
        block: BasicBlockId,
        index: usize,
        place: &StoredPlace,
        dropped_at: MirSpan,
        report: &mut impl FnMut(ConstCheckKind, MirSpan),
    ) {
        let ty = self.cx.place_ty(place);
        let local = place.projection.is_empty().then_some(place.local);
        let needs_drop = match local {
            Some(local) => self.in_local(Qualif::NeedsDrop, local, block, index),
            None => self.cx.in_any_value_of_ty(Qualif::NeedsDrop, ty),
        };
        if !needs_drop {
            return;
        }
        let (span, needs_non_const_drop) = match local {
            Some(local) => (
                local_span(self.cx.db, owner, self.cx.body, local, dropped_at),
                self.in_local(Qualif::NeedsNonConstDrop, local, block, index),
            ),
            None => (dropped_at, self.cx.in_any_value_of_ty(Qualif::NeedsNonConstDrop, ty)),
        };
        report(ConstCheckKind::LiveDrop { ty: ty.store(), dropped_at, needs_non_const_drop }, span);
    }
}

/// [`Checker::in_local`] over the parts of a checker, for a check that has its `Cx` lent out.
/// A local whose type can't have the qualif has it nowhere; peeking into an opaque type may
/// ask for this body's own qualifs, so a local of one is always looked at.
fn local_qualif<'f>(
    cx: &mut Cx<'_, 'f, '_>,
    graph: &Graph<'f>,
    flows: &mut [Option<Flow<'f>>; 3],
    q: Qualif,
    local: LocalId,
    block: BasicBlockId,
    index: usize,
) -> bool {
    let ty = cx.local_ty(local);
    if !ty.has_opaque_types() && !cx.in_any_value_of_ty(q, ty) {
        return false;
    }
    let flow = flows[q as usize].get_or_insert_with(|| Flow::compute(cx, graph, q));
    flow.seek(cx, block, index);
    flow.state.contains(local_index(local))
}

/// Where rustc points at a local, its `LocalDecl`'s span: the pattern of a parameter or a
/// binding, `self`, or the expression a temporary holds; `otherwise` for one with none.
fn local_span(
    db: &Db,
    owner: InferBodyId<'_>,
    body: &MirBody<'_>,
    local: LocalId,
    otherwise: MirSpan,
) -> MirSpan {
    // An anonymous constant has no parameters.
    if let InferBodyId::DefWithBodyId(def) = owner
        && let Some(at) = body.param_locals.iter().position(|&it| it == local)
    {
        let store = Body::of(db, def);
        // The `self` parameter's local comes first.
        let has_self = store.self_param.is_some();
        if has_self && at == 0 {
            return MirSpan::SelfParam;
        }
        if let Some(param) = store.params.get(at - usize::from(has_self)) {
            return MirSpan::PatId(param.user_written);
        }
    }
    if let Some((binding, _)) = body.binding_locals.iter().find(|&(_, &it)| it == local) {
        let (_, map) = ExpressionStore::with_source_map(db, owner.expression_store_owner(db));
        return match map.patterns_for_binding(binding).first() {
            Some(&pat) => MirSpan::PatId(pat),
            None => MirSpan::BindingId(binding),
        };
    }
    body.basic_blocks
        .iter()
        .flat_map(|(_, block)| &block.statements)
        .find(|it| matches!(it.kind, StatementKind::StorageLive(it) if it == local))
        .map_or(otherwise, |it| it.span)
}
