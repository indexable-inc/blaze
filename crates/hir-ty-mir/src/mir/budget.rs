//! What `tests/mir/alloc_budget.rs` measures: the pipelines' steps one at a time, outside
//! the query database, so a test can count what one step allocates.

use base_db::Db;
use blaze_mem::{Frame, Tag, frame};
use hir_def::{HasModule, hir::ExprId, resolver::HasResolver};

use crate::{
    InferBodyId,
    db::HirDatabase,
    next_solver::{
        DbInterner, StoredGenericArgs, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};

use super::{InlinedBody, MirBody, MirLowerError, MirOptions, transform::Ctx};

/// A pass of the pipelines, by rustc's name for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    CleanupPostBorrowck,
    SimplifyCfg,
    RemoveStorageMarkers,
    RemoveUnneededDrops,
    UnreachablePropagation,
    InstSimplify,
    LowerSliceLenCalls,
    SimplifyConstCondition,
    SimplifyLocals,
    SingleUseConsts,
    DeadStoreElimination,
    CopyProp,
    CriticalCallEdges,
    RemoveNoopLandingPads,
    CheckAlignment,
    CheckNull,
    CheckEnums,
    AddAsserts,
    ElaborateDrops,
    /// Drop elaboration's analyses, without its rewrite: live blocks, move paths, maybe-init
    /// and maybe-uninit, dead unwinds.
    DropAnalysis,
}

/// The optimizations, each of which leaves a body it runs on a second time as it was.
pub const OPTIMIZATIONS: &[Pass] = &[
    Pass::CleanupPostBorrowck,
    Pass::SimplifyCfg,
    Pass::RemoveStorageMarkers,
    Pass::RemoveUnneededDrops,
    Pass::UnreachablePropagation,
    Pass::InstSimplify,
    Pass::LowerSliceLenCalls,
    Pass::SimplifyConstCondition,
    Pass::SimplifyLocals,
    Pass::SingleUseConsts,
    Pass::DeadStoreElimination,
    Pass::CopyProp,
    Pass::CriticalCallEdges,
    Pass::RemoveNoopLandingPads,
];

/// The context the passes of a body share, made once per body as the pipelines make it.
pub struct Passes<'f, 'db>(Ctx<'f, 'db>);

/// Runs `f` with the passes' context for `owner`'s body, in a frame of its own.
pub fn with_passes<'db, R>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    f: impl FnOnce(&Passes<'_, 'db>) -> R,
) -> R {
    frame::task(Tag::Mir, |frame: &Frame| f(&Passes(Ctx::new(db, owner, frame))))
}

impl<'db> Passes<'_, 'db> {
    /// `pass` on `body`, in a frame of its own, as the pipelines run it.
    pub fn run(&self, body: &mut MirBody<'db>, pass: Pass) -> Result<(), MirLowerError<'db>> {
        frame::task(Tag::Mir, |frame| super::optimize::run_one(&self.0, body, pass, frame))
    }
}

/// The `InferCtxt` that `Ctx::new`, `Filler::new` and `MirLowerCtx::new` build for `owner`.
pub fn new_infer_ctxt<'db>(db: &'db Db, owner: InferBodyId<'db>) -> InferCtxt<'db> {
    DbInterner::new_with(db, owner.krate(db)).infer_ctxt().build(TypingMode::non_body_analysis())
}

/// What `mir_body_query` builds for `def`, without the query database.
pub fn lower<'db>(db: &'db Db, def: InferBodyId<'db>) -> Result<MirBody<'db>, MirLowerError<'db>> {
    super::lower::lower_body(db, def)
}

/// What lowering `def` builds that this crate does not own: the `Resolver` `MirLowerCtx::new`
/// makes, moved to the scopes of `tail` as a path there moves it, the `InferCtxt`, and the
/// function's signature with its late-bound regions freed, as `lower_body_to_mir` asks for it.
pub fn lowering_setup<'db>(db: &'db Db, def: InferBodyId<'db>, tail: ExprId) {
    let mut resolver = def.resolver(db);
    let guard = resolver.update_to_inner_scope(db, def.expression_store_owner(db), tail);
    resolver.reset_to_guard(guard);
    let _ = new_infer_ctxt(db, def);
    if let Some(func) = def.as_function() {
        let interner = DbInterner::new_with(db, def.krate(db));
        let _ = interner.liberate_late_bound_regions(
            func.into(),
            db.callable_item_signature(func.into()).instantiate_identity().skip_norm_wip(),
        );
    }
}

/// `run_passes`, as its queries call it.
pub fn run_passes<'db>(
    db: &'db Db,
    built: &MirBody<'db>,
) -> Result<MirBody<'db>, MirLowerError<'db>> {
    super::transform::run_passes(db, built)
}

/// `optimize`, as its queries call it: `caller` is the function the body is optimized as
/// (`MirOwner::caller`), `None` for a body on its own.
pub fn optimize<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    options: MirOptions,
    caller: Option<InlinedBody<'db>>,
) -> Result<MirBody<'db>, MirLowerError<'db>> {
    super::optimize::optimize(db, body, options, caller)
}

/// `monomorphized`, as its queries call it.
pub fn monomorphize<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    subst: &StoredGenericArgs,
    env: &StoredParamEnvAndCrate,
) -> Result<MirBody<'db>, MirLowerError<'db>> {
    super::monomorphization::monomorphized(db, body, subst, env)
}
