//! The queries hir-ty declares and a crate above it computes: MIR (hir-ty-mir).
//!
//! hir-ty, hir-ty-borrowck and blaze-items read MIR, whose code is in hir-ty-mir, above them.
//! The memos are declared here, so a hit is a static call from every crate; only a miss calls
//! through the table, as only a miss calls `execute` in rustc. The crate with the code
//! registers its table with `inventory::submit!`, which the linker keeps in every binary that
//! links that crate: nothing installs anything, and a binary with no table, or two, panics on
//! the first miss, naming what to link. A crate's own unit tests, which link a test build and a
//! library copy, use the test build ([`Mir::own_tests`]).
//!
//! The MIR queries are one per [`MirOwner`] kind and [`MirStage`], each monomorphized or not:
//! each keeps its own memo under its own key, and every one computes through `compute` or
//! `compute_monomorphized`, which take the stage before from its memo ([`mir_at`]).

use std::sync::OnceLock;

use base_db::Db;
use rustc_type_ir::{
    Interner as _, RegionKind, TypeFlags, TypeFoldable, TypeFolder, TypeSuperFoldable,
    TypeVisitableExt as _, inherent::IntoKind as _,
};

use crate::{
    InferBodyId,
    db::{InternedClosureId, InternedCoroutineId, InternedOpaqueTyId},
    mir::{InlinedBody, MirBody, MirEvalError, MirLowerError, MirOptions, MirOwner, MirStage},
    next_solver::{
        Allocation, DbInterner, GenericArgs, Region, StoredGenericArgs, StoredTy, Ty,
        default_types,
    },
    traits::{ParamEnvAndCrate, StoredParamEnvAndCrate},
};

type Built<'db> = Result<MirBody<'db>, MirLowerError<'db>>;

/// A body as its query keeps it.
pub type Kept<'db> = Result<&'db MirBody<'db>, &'db MirLowerError<'db>>;

/// hir-ty-mir's code for the MIR queries: lowering, and the step from one stage to the next.
pub struct Mir {
    /// `owner`'s body built from its HIR.
    pub build: for<'db> fn(&'db Db, MirOwner<'db>) -> Built<'db>,
    /// A built body after the runtime passes.
    pub run_passes: for<'db> fn(&'db Db, &MirBody<'db>) -> Built<'db>,
    /// A runtime body optimized and checked as the options ask.
    pub optimize:
        for<'db> fn(&'db Db, &MirBody<'db>, MirOptions, Option<InlinedBody<'db>>) -> Built<'db>,
    /// A body with the arguments in place of its generic parameters, in the environment.
    pub monomorphize: for<'db> fn(
        &'db Db,
        &MirBody<'db>,
        &StoredGenericArgs,
        &StoredParamEnvAndCrate,
    ) -> Built<'db>,

    /// The types an `async` block holds across a yield (rustc's `mir_coroutine_witnesses`).
    pub coroutine_witnesses: for<'db> fn(&'db Db, InternedCoroutineId<'db>) -> Box<[StoredTy]>,

    /// The value running `body` returns.
    pub interpret_mir: for<'db> fn(
        &'db Db,
        &'db MirBody<'db>,
        Option<ParamEnvAndCrate<'db>>,
    ) -> Result<Allocation<'db>, MirEvalError<'db>>,
    pub opaque_of_impl: for<'db> fn(
        &'db Db,
        ParamEnvAndCrate<'db>,
        InternedOpaqueTyId<'db>,
        GenericArgs<'db>,
    ) -> Option<Ty<'db>>,
    /// `cfg!(test)` where the table is built. A crate's own unit tests link it twice, their
    /// test build and the library copy a dev-dependency links; the test build is the one tested.
    pub own_tests: bool,
}

inventory::collect!(Mir);

/// The linked MIR table.
pub fn mir() -> &'static Mir {
    static ONE: OnceLock<&'static Mir> = OnceLock::new();
    ONE.get_or_init(|| {
        let mut tested = inventory::iter::<Mir>().filter(|it| it.own_tests);
        let Some(first) = tested.next() else { return one::<Mir>("hir-ty-mir") };
        assert!(tested.next().is_none(), "two test builds of `{}` are linked", std::any::type_name::<Mir>());
        let libraries = inventory::iter::<Mir>().filter(|it| !it.own_tests).count();
        assert!(libraries <= 1, "two `{}` are linked beside a test build", std::any::type_name::<Mir>());
        first
    })
}

fn one<T: inventory::Collect>(by: &str) -> &'static T {
    let mut all = inventory::iter::<T>();
    let first = all.next().unwrap_or_else(|| {
        panic!("no `{}` is linked: link {by} (`use hir as _;`)", std::any::type_name::<T>())
    });
    assert!(all.next().is_none(), "two `{}` are linked", std::any::type_name::<T>());
    first
}

/// How many of the lowered bodies the query engine keeps the value of once a view is over; 0 keeps
/// every one.
pub fn set_mir_capacity(db: &mut Db, capacity: usize) {
    mir_body_query::set_lru_capacity(db, capacity);
    mir_body_for_closure_query::set_lru_capacity(db, capacity);
    mir_body_for_coroutine_query::set_lru_capacity(db, capacity);
    mir_body_for_coroutine_by_move_query::set_lru_capacity(db, capacity);
}

/// How many of the runtime bodies (and their monomorphized copies) the query engine keeps the
/// value of once a view is over; 0 keeps every one. A body let go is lowered again when it is next asked
/// for, and what depends on it cannot be backdated then.
pub fn set_runtime_mir_capacity(db: &mut Db, capacity: usize) {
    runtime_mir_body_query::set_lru_capacity(db, capacity);
    runtime_mir_body_for_closure_query::set_lru_capacity(db, capacity);
    runtime_mir_body_for_coroutine_query::set_lru_capacity(db, capacity);
    runtime_mir_body_for_coroutine_by_move_query::set_lru_capacity(db, capacity);
    monomorphized_runtime_mir_body_query::set_lru_capacity(db, capacity);
    monomorphized_runtime_mir_body_for_closure_query::set_lru_capacity(db, capacity);
    monomorphized_runtime_mir_body_for_coroutine_query::set_lru_capacity(db, capacity);
    monomorphized_runtime_mir_body_for_coroutine_by_move_query::set_lru_capacity(db, capacity);
}

/// `owner`'s body at `stage`, from the memo of the query that keeps it.
pub fn mir_at<'db>(db: &'db Db, owner: MirOwner<'db>, stage: MirStage) -> Kept<'db> {
    use MirOwner as O;
    match (stage, owner) {
        (MirStage::Built, O::Body(def)) => mir_body_query(db, def),
        (MirStage::Built, O::Closure(it)) => mir_body_for_closure_query(db, it),
        (MirStage::Built, O::Coroutine(it)) => mir_body_for_coroutine_query(db, it),
        (MirStage::Built, O::CoroutineByMove(it)) => mir_body_for_coroutine_by_move_query(db, it),
        (MirStage::Runtime, O::Body(def)) => runtime_mir_body_query(db, def),
        (MirStage::Runtime, O::Closure(it)) => runtime_mir_body_for_closure_query(db, it),
        (MirStage::Runtime, O::Coroutine(it)) => runtime_mir_body_for_coroutine_query(db, it),
        (MirStage::Runtime, O::CoroutineByMove(it)) => {
            runtime_mir_body_for_coroutine_by_move_query(db, it)
        }
        (MirStage::Optimized(options), O::Body(def)) => optimized_mir_body_query(db, def, options),
        (MirStage::Optimized(options), O::Closure(it)) => {
            optimized_mir_body_for_closure_query(db, it, options)
        }
        (MirStage::Optimized(options), O::Coroutine(it)) => {
            optimized_mir_body_for_coroutine_query(db, it, options)
        }
        (MirStage::Optimized(options), O::CoroutineByMove(it)) => {
            optimized_mir_body_for_coroutine_by_move_query(db, it, options)
        }
    }
}

/// `owner`'s body at `stage` with `subst` in place of its generic parameters, from the memo of
/// the query that keeps it.
pub fn monomorphized_mir_at<'db>(
    db: &'db Db,
    owner: MirOwner<'db>,
    stage: MirStage,
    subst: StoredGenericArgs,
    env: StoredParamEnvAndCrate,
) -> Kept<'db> {
    use MirOwner as O;
    match (stage, owner) {
        (MirStage::Built, O::Body(def)) => monomorphized_mir_body_query(db, def, subst, env),
        (MirStage::Built, O::Closure(it)) => {
            monomorphized_mir_body_for_closure_query(db, it, subst, env)
        }
        (MirStage::Built, O::Coroutine(it)) => {
            monomorphized_mir_body_for_coroutine_query(db, it, subst, env)
        }
        (MirStage::Built, O::CoroutineByMove(it)) => {
            monomorphized_mir_body_for_coroutine_by_move_query(db, it, subst, env)
        }
        (MirStage::Runtime, O::Body(def)) => {
            monomorphized_runtime_mir_body_query(db, def, subst, env)
        }
        (MirStage::Runtime, O::Closure(it)) => {
            monomorphized_runtime_mir_body_for_closure_query(db, it, subst, env)
        }
        (MirStage::Runtime, O::Coroutine(it)) => {
            monomorphized_runtime_mir_body_for_coroutine_query(db, it, subst, env)
        }
        (MirStage::Runtime, O::CoroutineByMove(it)) => {
            monomorphized_runtime_mir_body_for_coroutine_by_move_query(db, it, subst, env)
        }
        (MirStage::Optimized(options), O::Body(def)) => {
            monomorphized_optimized_mir_body_query(db, def, options, subst, env)
        }
        (MirStage::Optimized(options), O::Closure(it)) => {
            monomorphized_optimized_mir_body_for_closure_query(db, it, options, subst, env)
        }
        (MirStage::Optimized(options), O::Coroutine(it)) => {
            monomorphized_optimized_mir_body_for_coroutine_query(db, it, options, subst, env)
        }
        (MirStage::Optimized(options), O::CoroutineByMove(it)) => {
            monomorphized_optimized_mir_body_for_coroutine_by_move_query(
                db, it, options, subst, env,
            )
        }
    }
}

/// What the query of `owner` at `stage` computes: the body built, or the stage before from
/// its memo taken one step on.
fn compute<'db>(db: &'db Db, owner: MirOwner<'db>, stage: MirStage) -> Built<'db> {
    let mir = mir();
    match stage {
        MirStage::Built => crate::body_store::built_mir(db, owner, || (mir.build)(db, owner)),
        MirStage::Runtime => {
            let built = mir_at(db, owner, MirStage::Built).map_err(Clone::clone)?;
            (mir.run_passes)(db, built)
        }
        MirStage::Optimized(options) => {
            let runtime = mir_at(db, owner, MirStage::Runtime).map_err(Clone::clone)?;
            (mir.optimize)(db, runtime, options, owner.caller())
        }
    }
}

/// What the monomorphized query of `owner` at `stage` computes: the generic body from its memo,
/// instantiated.
fn compute_monomorphized<'db>(
    db: &'db Db,
    owner: MirOwner<'db>,
    stage: MirStage,
    subst: StoredGenericArgs,
    env: StoredParamEnvAndCrate,
) -> Built<'db> {
    let generic = mir_at(db, owner, stage).map_err(Clone::clone)?;
    (mir().monomorphize)(db, generic, &subst, &env)
}

// A body that needs itself to be built is an error, not a fixpoint: one per arity of the keys.

fn mir_loop1<'db, A>(_: &'db Db, _: blaze_query::db::Id, _: A) -> Built<'db> {
    Err(MirLowerError::Loop)
}

fn mir_loop2<'db, A, B>(_: &'db Db, _: blaze_query::db::Id, _: A, _: B) -> Built<'db> {
    Err(MirLowerError::Loop)
}

fn mir_loop3<'db, A, B, C>(_: &'db Db, _: blaze_query::db::Id, _: A, _: B, _: C) -> Built<'db> {
    Err(MirLowerError::Loop)
}

fn mir_loop4<'db, A, B, C, D>(_: &'db Db, _: blaze_query::db::Id, _: A, _: B, _: C, _: D) -> Built<'db> {
    Err(MirLowerError::Loop)
}

// Built MIR.

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0, mem_tag = Mir)]
pub fn mir_body_query<'db>(db: &'db Db, def: InferBodyId<'db>) -> Built<'db> {
    compute(db, MirOwner::Body(def), MirStage::Built)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0, mem_tag = Mir)]
pub fn mir_body_for_closure_query<'db>(db: &'db Db, closure: InternedClosureId<'db>) -> Built<'db> {
    compute(db, MirOwner::Closure(closure), MirStage::Built)
}

/// The body of an `async` block, as the `Future::poll` of the block's type.
#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0, mem_tag = Mir)]
pub fn mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Built<'db> {
    compute(db, MirOwner::Coroutine(coroutine), MirStage::Built)
}

/// The body of the `async` block of an `async` closure that is called by value though it could
/// be called again: the future that makes owns what the closure captured, where the block
/// itself borrows from the closure.
#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0, mem_tag = Mir)]
pub fn mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Built<'db> {
    compute(db, MirOwner::CoroutineByMove(coroutine), MirStage::Built)
}

// Built MIR, monomorphized.

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0, mem_tag = Mir)]
pub fn monomorphized_mir_body_query<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    compute_monomorphized(db, MirOwner::Body(owner), MirStage::Built, subst, trait_env)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0, mem_tag = Mir)]
pub fn monomorphized_mir_body_for_closure_query<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    compute_monomorphized(db, MirOwner::Closure(closure), MirStage::Built, subst, trait_env)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0, mem_tag = Mir)]
pub fn monomorphized_mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    compute_monomorphized(db, MirOwner::Coroutine(coroutine), MirStage::Built, subst, trait_env)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0, mem_tag = Mir)]
pub fn monomorphized_mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    let owner = MirOwner::CoroutineByMove(coroutine);
    compute_monomorphized(db, owner, MirStage::Built, subst, trait_env)
}

// Runtime MIR.

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0, mem_tag = Mir)]
pub fn runtime_mir_body_query<'db>(db: &'db Db, def: InferBodyId<'db>) -> Built<'db> {
    compute(db, MirOwner::Body(def), MirStage::Runtime)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0, mem_tag = Mir)]
pub fn runtime_mir_body_for_closure_query<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
) -> Built<'db> {
    compute(db, MirOwner::Closure(closure), MirStage::Runtime)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0, mem_tag = Mir)]
pub fn runtime_mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Built<'db> {
    compute(db, MirOwner::Coroutine(coroutine), MirStage::Runtime)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0, mem_tag = Mir)]
pub fn runtime_mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Built<'db> {
    compute(db, MirOwner::CoroutineByMove(coroutine), MirStage::Runtime)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0, mem_tag = Mir)]
pub fn monomorphized_runtime_mir_body_query<'db>(
    db: &'db Db,
    def: InferBodyId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    compute_monomorphized(db, MirOwner::Body(def), MirStage::Runtime, subst, trait_env)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0, mem_tag = Mir)]
pub fn monomorphized_runtime_mir_body_for_closure_query<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    compute_monomorphized(db, MirOwner::Closure(closure), MirStage::Runtime, subst, trait_env)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0, mem_tag = Mir)]
pub fn monomorphized_runtime_mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    let owner = MirOwner::Coroutine(coroutine);
    compute_monomorphized(db, owner, MirStage::Runtime, subst, trait_env)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0, mem_tag = Mir)]
pub fn monomorphized_runtime_mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    let owner = MirOwner::CoroutineByMove(coroutine);
    compute_monomorphized(db, owner, MirStage::Runtime, subst, trait_env)
}

// Optimized MIR.

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop2, lru = 0, mem_tag = Mir)]
pub fn optimized_mir_body_query<'db>(
    db: &'db Db,
    def: InferBodyId<'db>,
    options: MirOptions,
) -> Built<'db> {
    compute(db, MirOwner::Body(def), MirStage::Optimized(options))
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop2, lru = 0, mem_tag = Mir)]
pub fn optimized_mir_body_for_closure_query<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
    options: MirOptions,
) -> Built<'db> {
    compute(db, MirOwner::Closure(closure), MirStage::Optimized(options))
}

/// The body of an `async` block's `poll` with `options`: of the passes, only the UB checks run
/// on a coroutine body.
#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop2, lru = 0, mem_tag = Mir)]
pub fn optimized_mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    options: MirOptions,
) -> Built<'db> {
    compute(db, MirOwner::Coroutine(coroutine), MirStage::Optimized(options))
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop2, lru = 0, mem_tag = Mir)]
pub fn optimized_mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    options: MirOptions,
) -> Built<'db> {
    compute(db, MirOwner::CoroutineByMove(coroutine), MirStage::Optimized(options))
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop4, lru = 0, mem_tag = Mir)]
pub fn monomorphized_optimized_mir_body_query<'db>(
    db: &'db Db,
    def: InferBodyId<'db>,
    options: MirOptions,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    let stage = MirStage::Optimized(options);
    compute_monomorphized(db, MirOwner::Body(def), stage, subst, trait_env)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop4, lru = 0, mem_tag = Mir)]
pub fn monomorphized_optimized_mir_body_for_closure_query<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
    options: MirOptions,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    let stage = MirStage::Optimized(options);
    compute_monomorphized(db, MirOwner::Closure(closure), stage, subst, trait_env)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop4, lru = 0, mem_tag = Mir)]
pub fn monomorphized_optimized_mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    options: MirOptions,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    let stage = MirStage::Optimized(options);
    compute_monomorphized(db, MirOwner::Coroutine(coroutine), stage, subst, trait_env)
}

#[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop4, lru = 0, mem_tag = Mir)]
pub fn monomorphized_optimized_mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    options: MirOptions,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    let stage = MirStage::Optimized(options);
    compute_monomorphized(db, MirOwner::CoroutineByMove(coroutine), stage, subst, trait_env)
}

// What a coroutine holds across a yield.

/// The types an `async` block holds across a yield, by which its future implements an auto
/// trait: rustc's `mir_coroutine_witnesses` (`CoroutineLayout::field_tys` without what
/// `ignore_for_traits` marks), generic in the parameters of the coroutine's parent, with regions
/// erased. No cycle result: a goal on a coroutine of the body being inferred waits until that
/// body is checked, as rustc's stalled coroutine goals, so a cycle here is a bug to stop on.
#[blaze_query::tracked(returns(deref), mem_tag = Mir)]
pub fn coroutine_witnesses_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Box<[StoredTy]> {
    let mut witnesses = (mir().coroutine_witnesses)(db, coroutine);
    let interner = DbInterner::new_no_crate(db);
    for witness in witnesses.iter_mut() {
        *witness = erase_regions(interner, witness.as_ref()).store();
    }
    witnesses
}

/// rustc's `erase_regions`: every free region `'erased`, and every binder's variables
/// anonymized. MIR locals keep typeck's regions (`'static`, early-bound parameters, errors).
pub(crate) fn erase_regions<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Ty<'db> {
    return ty.fold_with(&mut EraseRegions { interner });

    struct EraseRegions<'db> {
        interner: DbInterner<'db>,
    }

    impl<'db> TypeFolder<DbInterner<'db>> for EraseRegions<'db> {
        fn cx(&self) -> DbInterner<'db> {
            self.interner
        }

        fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
            if ty.has_type_flags(TypeFlags::HAS_BINDER_VARS | TypeFlags::HAS_FREE_REGIONS) {
                ty.super_fold_with(self)
            } else {
                ty
            }
        }

        fn fold_binder<T: TypeFoldable<DbInterner<'db>>>(
            &mut self,
            t: rustc_type_ir::Binder<DbInterner<'db>, T>,
        ) -> rustc_type_ir::Binder<DbInterner<'db>, T> {
            self.interner.anonymize_bound_vars(t).super_fold_with(self)
        }

        fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
            match region.kind() {
                RegionKind::ReBound(..) => region,
                _ => default_types().regions.erased,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Unlinked;
    inventory::collect!(Unlinked);

    struct Once(u32);
    inventory::collect!(Once);
    inventory::submit! { Once(7) }

    #[derive(Debug)]
    struct Twice;
    inventory::collect!(Twice);
    inventory::submit! { Twice }
    inventory::submit! { Twice }

    #[test]
    fn a_table_nobody_registered_names_what_to_link() {
        let panic = std::panic::catch_unwind(|| one::<Unlinked>("the-crate")).unwrap_err();
        let message = panic.downcast_ref::<String>().unwrap();
        assert!(message.contains("link the-crate"), "{message}");
        assert!(message.contains("Unlinked"), "{message}");
    }

    #[test]
    fn a_table_registered_once_is_found() {
        assert_eq!(one::<Once>("the-crate").0, 7);
    }

    #[test]
    fn a_witness_keeps_no_free_region_and_its_bound_regions() {
        use rustc_type_ir::{BoundVar, INNERMOST, Mutability, inherent::Ty as _};

        use crate::next_solver::{BoundRegionKind, BoundVarKinds, BoundVariableKind};

        let db = Db::for_tests();
        let interner = DbInterner::new_no_crate(&db);
        let types = &default_types().types;
        let regions = &default_types().regions;
        let anon = BoundVariableKind::Region(BoundRegionKind::Anon);
        let shared = |region, ty| Ty::new_ref(interner, region, ty, Mutability::Not);
        // `for<'a> fn(&'a u8, &'r u16)`.
        let fn_ptr = |r| {
            let bound = Region::new_anon_bound(interner, INNERMOST, BoundVar::from_u32(0));
            let sig = interner
                .mk_fn_sig_safe_rust_abi([shared(bound, types.u8), shared(r, types.u16)], types.unit);
            Ty::new_fn_ptr(
                interner,
                rustc_type_ir::Binder::bind_with_vars(sig, BoundVarKinds::new_from_slice(&[anon])),
            )
        };

        for (ty, erased) in [
            (
                shared(regions.statik, shared(regions.error, types.u32)),
                shared(regions.erased, shared(regions.erased, types.u32)),
            ),
            (fn_ptr(regions.statik), fn_ptr(regions.erased)),
            (types.u32, types.u32),
        ] {
            assert_eq!(erase_regions(interner, ty), erased, "{ty:?}");
        }
    }

    #[test]
    fn a_table_registered_twice_is_refused() {
        let panic = std::panic::catch_unwind(|| one::<Twice>("the-crate")).unwrap_err();
        let message = panic.downcast_ref::<String>().unwrap();
        assert!(message.contains("two"), "{message}");
    }
}
