//! The home of `HirDatabase`, the type inference-related queries as methods of the one
//! database, `base_db::Db`.

use arrayvec::ArrayVec;
use base_db::{Crate, Db, target::TargetLoadError};
use blaze_hashcons::Interned;
use either::Either;
use hir_def::{
    AdtId, BuiltinDeriveImplId, CallableDefId, ConstId, ConstParamId, EnumVariantId,
    ExpressionStoreOwnerId, FunctionId, GenericDefId, HasModule, ImplId, ModuleId,
    StaticId, TraitId, TypeAliasId, VariantId,
    builtin_derive::BuiltinDeriveImplMethod,
    expr_store::ExpressionStore,
    hir::{ClosureKind, ExprId},
    layout::TargetDataLayout,
    resolver::{HasResolver, Resolver},
    signatures::{ConstSignature, StaticSignature},
};
use span::Edition;
use stdx::impl_from;

use crate::{
    GenericDefaultsRef, GenericPredicates, ImplTraitId, InferBodyId, TyDefId,
    TyLoweringDiagnostic, TyLoweringResult, ValueTyDefId,
    consteval::ConstEvalError,
    dyn_compatibility::DynCompatibilityViolation,
    layout::{InternedLayout, LayoutError},
    lower::{
        ConstParamTypes, FieldTypes, GenericDefaults, Lowered, SelfAndAssocBounds,
        TrackedStructToken, WithDefinedOpaques,
    },
    mir::{MirBody, MirEvalError, MirLowerError, MirOptions, MirOwner, MirStage},
    next_solver::{
        Allocation, Clause, EarlyBinder, GenericArgs, ParamEnv, PolyFnSig, StoredEarlyBinder,
        StoredGenericArgs, StoredPolyFnSig, StoredTraitRef, StoredTy, TraitRef, Ty, VariancesOf,
    },
    providers,
    traits::{ParamEnvAndCrate, StoredParamEnvAndCrate},
};

/// What hir-ty's inference, lowering, method resolution, layout, const eval and trait
/// solving allocate (docs/v2/plan/lanes/mem/db.md, contract 3).
pub(crate) const INFER_TAG: blaze_mem::Tag = blaze_mem::Tag::Infer;
/// What hir-ty's MIR queries allocate.
pub(crate) const MIR_TAG: blaze_mem::Tag = blaze_mem::Tag::Mir;

mod sealed {
    pub trait Sealed {}
    impl Sealed for base_db::Db {}
}

/// hir-ty's queries as methods of the one database, [`Db`]: `db.ty(def)` is a static call to a
/// body the caller can inline. It has one impl, and it is not dyn-compatible, so no caller
/// can take a `dyn` database and pay a vtable call on every hit.
pub trait HirDatabase: sealed::Sealed {
    #[doc(hidden)]
    const NOT_DYN: () = ();

    // region:mir

    fn mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    /// The body as the interpreter and the code generator run it: after drop elaboration and
    /// the other passes to `MirPhase::Runtime`. The IDE asks for [`HirDatabase::mir_body`].
    fn runtime_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn runtime_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn runtime_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn runtime_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    /// The types an `async` block holds across a yield, by which its future implements an
    /// auto trait: rustc's `mir_coroutine_witnesses` (`CoroutineLayout::field_tys` without
    /// what `ignore_for_traits` marks). Generic in the parameters of the coroutine's parent,
    /// with regions erased.
    fn coroutine_witnesses<'db>(&'db self, def: InternedCoroutineId<'db>) -> &'db [StoredTy];

    /// `owner`'s body at `stage`, from the memo of the query named for them.
    fn mir<'db>(
        &'db self,
        owner: MirOwner<'db>,
        stage: MirStage,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    /// `owner`'s body at `stage` with `subst` in place of its generic parameters, in `env`,
    /// from the memo of the monomorphized query named for them.
    fn monomorphized_mir<'db>(
        &'db self,
        owner: MirOwner<'db>,
        stage: MirStage,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    /// The runtime body after rustc's MIR optimizations as `options` says
    /// ([`MirOptions`]): its level, its UB checks and whether lifetime markers are kept.
    fn optimized_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn optimized_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    /// The runtime body of a coroutine with the UB checks `options` asks for inserted; a
    /// coroutine is not optimized further.
    fn optimized_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn optimized_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_optimized_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_optimized_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_optimized_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_optimized_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_runtime_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_runtime_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_runtime_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_runtime_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn const_eval<'db>(
        &'db self,
        def: ConstId,
        subst: GenericArgs<'db>,
        trait_env: Option<ParamEnvAndCrate<'db>>,
    ) -> Result<Allocation<'db>, ConstEvalError<'db>>;

    fn anon_const_eval<'db>(
        &'db self,
        def: AnonConstId<'db>,
        subst: GenericArgs<'db>,
        trait_env: Option<ParamEnvAndCrate<'db>>,
    ) -> Result<Allocation<'db>, ConstEvalError<'db>>;

    fn const_eval_static<'db>(
        &'db self,
        def: StaticId,
    ) -> Result<Allocation<'db>, ConstEvalError<'db>>;

    fn const_eval_discriminant<'db>(
        &'db self,
        def: EnumVariantId,
    ) -> Result<i128, ConstEvalError<'db>>;

    /// The method of the impl a call of the trait method `func` runs, or `func` itself when
    /// none is found: what an IDE reads, of code that may not type-check.
    fn lookup_impl_method<'db>(
        &'db self,
        env: ParamEnvAndCrate<'db>,
        func: FunctionId,
        fn_subst: GenericArgs<'db>,
    ) -> (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>);

    /// As [`HirDatabase::lookup_impl_method`], with why no impl's method is found: what
    /// codegen reads, to stop where a concrete call has none
    /// ([`NoImplMethod::is_expected`](crate::method_resolution::NoImplMethod::is_expected)).
    fn lookup_impl_method_or_why<'db>(
        &'db self,
        env: ParamEnvAndCrate<'db>,
        func: FunctionId,
        fn_subst: GenericArgs<'db>,
    ) -> Result<
        (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>),
        crate::method_resolution::NoImplMethod,
    >;

    /// What the interpreter (`hir_ty_mir::interpret_mir`) gives running `body`: the value it
    /// returns.
    fn interpret_mir<'db>(
        &'db self,
        body: &'db MirBody<'db>,
        trait_env: Option<ParamEnvAndCrate<'db>>,
    ) -> Result<Allocation<'db>, MirEvalError<'db>>;

    /// The opaque type of the impl's method that `opaque`, the `impl Trait` a trait's method
    /// returns, is with `args`, once they name the impl (`hir_ty_mir::opaque_of_impl`).
    fn opaque_of_impl<'db>(
        &'db self,
        env: ParamEnvAndCrate<'db>,
        opaque: InternedOpaqueTyId<'db>,
        args: GenericArgs<'db>,
    ) -> Option<Ty<'db>>;

    // endregion:mir

    fn layout_of_adt(
        &self,
        def: AdtId,
        args: StoredGenericArgs,
        trait_env: StoredParamEnvAndCrate,
    ) -> Result<Interned<'static, InternedLayout>, LayoutError>;

    fn layout_of_ty(
        &self,
        ty: StoredTy,
        env: StoredParamEnvAndCrate,
    ) -> Result<Interned<'static, InternedLayout>, LayoutError>;

    fn target_data_layout(&self, krate: Crate) -> Result<&TargetDataLayout, TargetLoadError>;

    fn dyn_compatibility_of_trait(&self, trait_: TraitId) -> Option<DynCompatibilityViolation>;

    fn ty<'db>(&'db self, def: TyDefId) -> EarlyBinder<'db, Ty<'db>>;

    fn type_for_type_alias_with_diagnostics<'db>(
        &'db self,
        def: TypeAliasId,
    ) -> &'db TyLoweringResult<'db, WithDefinedOpaques<StoredEarlyBinder<StoredTy>>>;

    /// Returns the type of the value of the given constant, or `None` if the `ValueTyDefId` is
    /// a `StructId` or `EnumVariantId` with a record constructor.
    fn value_ty<'db>(&'db self, def: ValueTyDefId) -> Option<EarlyBinder<'db, Ty<'db>>>;

    fn type_for_const<'db>(&'db self, def: ConstId) -> EarlyBinder<'db, Ty<'db>>;

    fn type_for_const_with_diagnostics<'db>(
        &'db self,
        def: ConstId,
    ) -> &'db TyLoweringResult<'db, StoredEarlyBinder<StoredTy>>;

    fn type_for_static<'db>(&'db self, def: StaticId) -> EarlyBinder<'db, Ty<'db>>;

    fn type_for_static_with_diagnostics<'db>(
        &'db self,
        def: StaticId,
    ) -> &'db TyLoweringResult<'db, StoredEarlyBinder<StoredTy>>;

    fn impl_self_ty_with_diagnostics<'db>(
        &'db self,
        def: ImplId,
    ) -> Lowered<'db, StoredEarlyBinder<StoredTy>>;

    fn impl_self_ty<'db>(&'db self, def: ImplId) -> EarlyBinder<'db, Ty<'db>>;

    fn const_param_types_with_diagnostics<'db>(
        &'db self,
        def: GenericDefId,
    ) -> &'db TyLoweringResult<'db, ConstParamTypes>;

    fn const_param_types(&self, def: GenericDefId) -> &ConstParamTypes;

    fn const_param_ty<'db>(&'db self, def: ConstParamId) -> Ty<'db>;

    fn impl_trait_with_diagnostics<'db>(
        &'db self,
        def: ImplId,
    ) -> Option<Lowered<'db, StoredEarlyBinder<StoredTraitRef>>>;

    fn impl_trait<'db>(&'db self, def: ImplId) -> Option<EarlyBinder<'db, TraitRef<'db>>>;

    fn field_types_with_diagnostics<'db>(&'db self, var: VariantId)
    -> Lowered<'db, FieldTypes>;

    fn field_types(&self, var: VariantId) -> &FieldTypes;

    fn callable_item_signature<'db>(
        &'db self,
        def: CallableDefId,
    ) -> EarlyBinder<'db, PolyFnSig<'db>>;

    fn fn_sig_for_fn_with_diagnostics<'db>(
        &'db self,
        def: FunctionId,
    ) -> &'db TyLoweringResult<'db, WithDefinedOpaques<StoredEarlyBinder<StoredPolyFnSig>>>;

    fn trait_environment<'db>(&'db self, def: GenericDefId) -> ParamEnv<'db>;

    fn generic_defaults_with_diagnostics<'db>(
        &'db self,
        def: GenericDefId,
    ) -> &'db TyLoweringResult<'db, GenericDefaults>;

    /// This returns an empty list if no parameter has default.
    ///
    /// The binders of the returned defaults are only up to (not including) this parameter.
    fn generic_defaults(&self, def: GenericDefId) -> GenericDefaultsRef<'_>;

    fn type_alias_bounds_with_diagnostics<'db>(
        &'db self,
        type_alias: TypeAliasId,
    ) -> &'db TyLoweringResult<'db, SelfAndAssocBounds>;

    fn type_alias_bounds<'db>(
        &'db self,
        type_alias: TypeAliasId,
    ) -> EarlyBinder<'db, &'db [Clause<'db>]>;

    fn type_alias_self_bounds<'db>(
        &'db self,
        type_alias: TypeAliasId,
    ) -> EarlyBinder<'db, &'db [Clause<'db>]>;

    fn variances_of<'db>(&'db self, def: GenericDefId) -> VariancesOf<'db>;

    /// The path of `ty` as `std::any::type_name` prints it, for the MIR intrinsic
    /// ([`crate::display`]). `None` when it has no source path.
    fn type_name<'db>(&'db self, ty: Ty<'db>, module: ModuleId) -> Option<String>;
}

impl HirDatabase for Db {
    #[inline]
    fn mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::mir_body_query(self, def).map_err(Clone::clone)
    }

    #[inline]
    fn mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::mir_body_for_closure_query(self, def).map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_mir_body_query(self, def, subst, env).map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_mir_body_for_closure_query(self, def, subst, env)
            .map_err(Clone::clone)
    }

    #[inline]
    fn mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::mir_body_for_coroutine_query(self, def).map_err(Clone::clone)
    }

    #[inline]
    fn mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::mir_body_for_coroutine_by_move_query(self, def).map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_mir_body_for_coroutine_by_move_query(self, def, subst, env)
            .map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_mir_body_for_coroutine_query(self, def, subst, env)
            .map_err(Clone::clone)
    }

    #[inline]
    fn runtime_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::runtime_mir_body_query(self, def).map_err(Clone::clone)
    }

    #[inline]
    fn runtime_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::runtime_mir_body_for_closure_query(self, def).map_err(Clone::clone)
    }

    #[inline]
    fn runtime_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::runtime_mir_body_for_coroutine_query(self, def).map_err(Clone::clone)
    }

    #[inline]
    fn runtime_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::runtime_mir_body_for_coroutine_by_move_query(self, def).map_err(Clone::clone)
    }

    #[inline]
    fn coroutine_witnesses<'db>(&'db self, def: InternedCoroutineId<'db>) -> &'db [StoredTy] {
        providers::coroutine_witnesses_query(self, def)
    }

    #[inline]
    fn mir<'db>(
        &'db self,
        owner: MirOwner<'db>,
        stage: MirStage,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::mir_at(self, owner, stage).map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_mir<'db>(
        &'db self,
        owner: MirOwner<'db>,
        stage: MirStage,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_mir_at(self, owner, stage, subst, env).map_err(Clone::clone)
    }

    #[inline]
    fn optimized_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::optimized_mir_body_query(self, def, options).map_err(Clone::clone)
    }

    #[inline]
    fn optimized_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::optimized_mir_body_for_closure_query(self, def, options).map_err(Clone::clone)
    }

    #[inline]
    fn optimized_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::optimized_mir_body_for_coroutine_query(self, def, options).map_err(Clone::clone)
    }

    #[inline]
    fn optimized_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::optimized_mir_body_for_coroutine_by_move_query(self, def, options)
            .map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_optimized_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_optimized_mir_body_query(self, def, options, subst, env)
            .map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_optimized_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_optimized_mir_body_for_closure_query(
            self, def, options, subst, env,
        )
        .map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_optimized_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_optimized_mir_body_for_coroutine_query(
            self, def, options, subst, env,
        )
        .map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_optimized_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_optimized_mir_body_for_coroutine_by_move_query(
            self, def, options, subst, env,
        )
        .map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_runtime_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_runtime_mir_body_query(self, def, subst, env)
            .map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_runtime_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_runtime_mir_body_for_closure_query(self, def, subst, env)
            .map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_runtime_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_runtime_mir_body_for_coroutine_query(self, def, subst, env)
            .map_err(Clone::clone)
    }

    #[inline]
    fn monomorphized_runtime_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
        providers::monomorphized_runtime_mir_body_for_coroutine_by_move_query(
            self, def, subst, env,
        )
        .map_err(Clone::clone)
    }

    #[inline]
    fn const_eval<'db>(
        &'db self,
        def: ConstId,
        subst: GenericArgs<'db>,
        trait_env: Option<ParamEnvAndCrate<'db>>,
    ) -> Result<Allocation<'db>, ConstEvalError<'db>> {
        crate::consteval::const_eval(self, def, subst, trait_env)
    }

    #[inline]
    fn anon_const_eval<'db>(
        &'db self,
        def: AnonConstId<'db>,
        subst: GenericArgs<'db>,
        trait_env: Option<ParamEnvAndCrate<'db>>,
    ) -> Result<Allocation<'db>, ConstEvalError<'db>> {
        crate::consteval::anon_const_eval(self, def, subst, trait_env)
    }

    #[inline]
    fn const_eval_static<'db>(
        &'db self,
        def: StaticId,
    ) -> Result<Allocation<'db>, ConstEvalError<'db>> {
        crate::consteval::const_eval_static(self, def)
    }

    #[inline]
    fn const_eval_discriminant<'db>(
        &'db self,
        def: EnumVariantId,
    ) -> Result<i128, ConstEvalError<'db>> {
        crate::consteval::const_eval_discriminant_variant(self, def)
    }

    fn lookup_impl_method<'db>(
        &'db self,
        env: ParamEnvAndCrate<'db>,
        func: FunctionId,
        fn_subst: GenericArgs<'db>,
    ) -> (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>)
    {
        self.lookup_impl_method_or_why(env, func, fn_subst)
            .unwrap_or((Either::Left(func), fn_subst))
    }

    fn lookup_impl_method_or_why<'db>(
        &'db self,
        env: ParamEnvAndCrate<'db>,
        func: FunctionId,
        fn_subst: GenericArgs<'db>,
    ) -> Result<
        (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>),
        crate::method_resolution::NoImplMethod,
    > {
        if !matches!(func.loc(self).container, hir_def::ItemContainerId::TraitId(_)) {
            return Ok((Either::Left(func), fn_subst));
        }
        crate::method_resolution::lookup_impl_method_stored(
            self,
            env.store(),
            func,
            fn_subst.store(),
        )
        .map(|(found, args)| (found, args.as_ref()))
    }

    #[inline]
    fn interpret_mir<'db>(
        &'db self,
        body: &'db MirBody<'db>,
        trait_env: Option<ParamEnvAndCrate<'db>>,
    ) -> Result<Allocation<'db>, MirEvalError<'db>> {
        (providers::mir().interpret_mir)(self, body, trait_env)
    }

    #[inline]
    fn opaque_of_impl<'db>(
        &'db self,
        env: ParamEnvAndCrate<'db>,
        opaque: InternedOpaqueTyId<'db>,
        args: GenericArgs<'db>,
    ) -> Option<Ty<'db>> {
        (providers::mir().opaque_of_impl)(self, env, opaque, args)
    }

    #[inline]
    fn layout_of_adt(
        &self,
        def: AdtId,
        args: StoredGenericArgs,
        trait_env: StoredParamEnvAndCrate,
    ) -> Result<Interned<'static, InternedLayout>, LayoutError> {
        crate::layout::layout_of_adt_query(self, def, args, trait_env)
    }

    #[inline]
    fn layout_of_ty(
        &self,
        ty: StoredTy,
        env: StoredParamEnvAndCrate,
    ) -> Result<Interned<'static, InternedLayout>, LayoutError> {
        crate::layout::layout_of_ty_query(self, ty, env)
    }

    #[inline]
    fn target_data_layout(&self, krate: Crate) -> Result<&TargetDataLayout, TargetLoadError> {
        // clone: error path, an `Arc<str>` message
        crate::layout::target_data_layout_query(self, krate).map_err(|err| err.clone())
    }

    #[inline]
    fn dyn_compatibility_of_trait(&self, trait_: TraitId) -> Option<DynCompatibilityViolation> {
        crate::dyn_compatibility::dyn_compatibility_of_trait_query(self, trait_)
    }

    #[inline]
    fn ty<'db>(&'db self, def: TyDefId) -> EarlyBinder<'db, Ty<'db>> {
        crate::lower::ty_query(self, def)
    }

    #[inline]
    fn type_for_type_alias_with_diagnostics<'db>(
        &'db self,
        def: TypeAliasId,
    ) -> &'db TyLoweringResult<'db, WithDefinedOpaques<StoredEarlyBinder<StoredTy>>> {
        crate::lower::type_for_type_alias_with_diagnostics(self, def)
    }

    #[inline]
    fn value_ty<'db>(&'db self, def: ValueTyDefId) -> Option<EarlyBinder<'db, Ty<'db>>> {
        crate::lower::value_ty(self, def)
    }

    #[inline]
    fn type_for_const<'db>(&'db self, def: ConstId) -> EarlyBinder<'db, Ty<'db>> {
        crate::lower::type_for_const(self, def)
    }

    #[inline]
    fn type_for_const_with_diagnostics<'db>(
        &'db self,
        def: ConstId,
    ) -> &'db TyLoweringResult<'db, StoredEarlyBinder<StoredTy>> {
        crate::lower::type_for_const_with_diagnostics(self, def)
    }

    #[inline]
    fn type_for_static<'db>(&'db self, def: StaticId) -> EarlyBinder<'db, Ty<'db>> {
        crate::lower::type_for_static(self, def)
    }

    #[inline]
    fn type_for_static_with_diagnostics<'db>(
        &'db self,
        def: StaticId,
    ) -> &'db TyLoweringResult<'db, StoredEarlyBinder<StoredTy>> {
        crate::lower::type_for_static_with_diagnostics(self, def)
    }

    #[inline]
    fn impl_self_ty_with_diagnostics<'db>(
        &'db self,
        def: ImplId,
    ) -> Lowered<'db, StoredEarlyBinder<StoredTy>> {
        crate::lower::impl_self_ty_with_diagnostics(self, def)
    }

    #[inline]
    fn impl_self_ty<'db>(&'db self, def: ImplId) -> EarlyBinder<'db, Ty<'db>> {
        crate::lower::impl_self_ty_query(self, def)
    }

    #[inline]
    fn const_param_types_with_diagnostics<'db>(
        &'db self,
        def: GenericDefId,
    ) -> &'db TyLoweringResult<'db, ConstParamTypes> {
        crate::lower::const_param_types_with_diagnostics(self, def)
    }

    #[inline]
    fn const_param_types(&self, def: GenericDefId) -> &ConstParamTypes {
        crate::lower::const_param_types(self, def)
    }

    #[inline]
    fn const_param_ty<'db>(&'db self, def: ConstParamId) -> Ty<'db> {
        crate::lower::const_param_ty(self, def)
    }

    #[inline]
    fn impl_trait_with_diagnostics<'db>(
        &'db self,
        def: ImplId,
    ) -> Option<Lowered<'db, StoredEarlyBinder<StoredTraitRef>>> {
        crate::lower::impl_trait_with_diagnostics(self, def)
    }

    #[inline]
    fn impl_trait<'db>(&'db self, def: ImplId) -> Option<EarlyBinder<'db, TraitRef<'db>>> {
        crate::lower::impl_trait_query(self, def)
    }

    #[inline]
    fn field_types_with_diagnostics<'db>(&'db self, var: VariantId) -> Lowered<'db, FieldTypes> {
        crate::lower::field_types_with_diagnostics(self, var)
    }

    #[inline]
    fn field_types(&self, var: VariantId) -> &FieldTypes {
        crate::lower::field_types_query(self, var)
    }

    #[inline]
    fn callable_item_signature<'db>(
        &'db self,
        def: CallableDefId,
    ) -> EarlyBinder<'db, PolyFnSig<'db>> {
        crate::lower::callable_item_signature(self, def)
    }

    #[inline]
    fn fn_sig_for_fn_with_diagnostics<'db>(
        &'db self,
        def: FunctionId,
    ) -> &'db TyLoweringResult<'db, WithDefinedOpaques<StoredEarlyBinder<StoredPolyFnSig>>> {
        crate::lower::fn_sig_for_fn(self, def)
    }

    #[inline]
    fn trait_environment<'db>(&'db self, def: GenericDefId) -> ParamEnv<'db> {
        crate::lower::trait_environment(self, def)
    }

    #[inline]
    fn generic_defaults_with_diagnostics<'db>(
        &'db self,
        def: GenericDefId,
    ) -> &'db TyLoweringResult<'db, GenericDefaults> {
        crate::lower::generic_defaults_with_diagnostics(self, def)
    }

    #[inline]
    fn generic_defaults(&self, def: GenericDefId) -> GenericDefaultsRef<'_> {
        crate::lower::generic_defaults(self, def)
    }

    #[inline]
    fn type_alias_bounds_with_diagnostics<'db>(
        &'db self,
        type_alias: TypeAliasId,
    ) -> &'db TyLoweringResult<'db, SelfAndAssocBounds> {
        crate::lower::type_alias_bounds_with_diagnostics(self, type_alias)
    }

    #[inline]
    fn type_alias_bounds<'db>(
        &'db self,
        type_alias: TypeAliasId,
    ) -> EarlyBinder<'db, &'db [Clause<'db>]> {
        crate::lower::type_alias_bounds(self, type_alias)
    }

    #[inline]
    fn type_alias_self_bounds<'db>(
        &'db self,
        type_alias: TypeAliasId,
    ) -> EarlyBinder<'db, &'db [Clause<'db>]> {
        crate::lower::type_alias_self_bounds(self, type_alias)
    }

    #[inline]
    fn variances_of<'db>(&'db self, def: GenericDefId) -> VariancesOf<'db> {
        crate::variance::variances_of(self, def)
    }

    #[inline]
    fn type_name<'db>(&'db self, ty: Ty<'db>, module: ModuleId) -> Option<String> {
        crate::display::type_name(self, ty, module)
    }
}

#[blaze_query::interned(debug)]
#[derive(PartialOrd, Ord)]
pub struct InternedOpaqueTyId {
    #[returns(copy)]
    pub loc: ImplTraitId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct InternedClosure<'db> {
    pub owner: InferBodyId<'db>,
    pub expr: ExprId,
    pub kind: ClosureKind,
}

/// What a closure is interned by: its place among the closures of its body. Its expression
/// moves with every edit of the body before it, and a closure that moved would be another
/// type, with everything generic over it instantiated again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct ClosureKey<'db> {
    owner: InferBodyId<'db>,
    nth: u32,
    kind: ClosureKind,
}

impl<'db> ClosureKey<'db> {
    fn new(db: &'db Db, loc: InternedClosure<'db>) -> Self {
        let closures = closure_exprs(db, loc.owner.expression_store_owner(db), ());
        let nth = closures.partition_point(|&it| it < loc.expr) as u32;
        ClosureKey { owner: loc.owner, nth, kind: loc.kind }
    }

    /// Where the closure is among the closures of its body.
    pub fn nth(db: &'db Db, loc: InternedClosure<'db>) -> u32 {
        Self::new(db, loc).nth
    }

    fn loc(self, db: &'db Db) -> InternedClosure<'db> {
        let closures = closure_exprs(db, self.owner.expression_store_owner(db), ());
        // A key from before an edit that took closures out of the body names none of them.
        // Whatever asks about it is not asked for its answer.
        let expr = closures.get(self.nth as usize).or(closures.last()).copied();
        let expr = expr.unwrap_or_else(|| ExprId::from_raw(la_arena::RawIdx::from_u32(0)));
        InternedClosure { owner: self.owner, expr, kind: self.kind }
    }
}

/// The closures, `async` blocks and coroutines of a body, in the order of their expressions.
// The unit makes this a function of several arguments, whose key `closure_exprs::arguments`
// gives back (blaze-item-hash reads it).
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
pub fn closure_exprs(db: &Db, owner: ExpressionStoreOwnerId, _: ()) -> Box<[ExprId]> {
    ExpressionStore::of(db, owner)
        .exprs()
        .filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Closure { .. }))
        .map(|(id, _)| id)
        .collect() // alloc: the memo's stored list, once per body
}

#[blaze_query::interned(constructor = new_impl, debug)]
#[derive(PartialOrd, Ord)]
pub struct InternedClosureId<'db> {
    #[returns(copy)]
    key: ClosureKey<'db>,
}

impl<'db> InternedClosureId<'db> {
    pub fn loc(self, db: &'db Db) -> InternedClosure<'db> {
        self.key(db).loc(db)
    }

    #[inline]
    pub fn new(db: &'db Db, loc: InternedClosure<'db>) -> Self {
        if cfg!(debug_assertions) {
            let store = ExpressionStore::of(db, loc.owner.expression_store_owner(db));
            let expr = store.expr(loc.expr);
            assert!(
                matches!(
                    expr,
                    hir_def::hir::Expr::Closure {
                        closure_kind: hir_def::hir::ClosureKind::Closure
                            | hir_def::hir::ClosureKind::CoroutineClosure(_),
                        ..
                    }
                ),
                "expected a closure, found {expr:?}"
            );
        }

        Self::new_impl(db, ClosureKey::new(db, loc))
    }
}

#[blaze_query::interned(constructor = new_impl, debug)]
#[derive(PartialOrd, Ord)]
pub struct InternedCoroutineId<'db> {
    #[returns(copy)]
    key: ClosureKey<'db>,
}

impl<'db> InternedCoroutineId<'db> {
    pub fn loc(self, db: &'db Db) -> InternedClosure<'db> {
        self.key(db).loc(db)
    }

    #[inline]
    pub fn new(db: &'db Db, loc: InternedClosure<'db>) -> Self {
        if cfg!(debug_assertions) {
            let store = ExpressionStore::of(db, loc.owner.expression_store_owner(db));
            let expr = store.expr(loc.expr);
            assert!(
                matches!(
                    expr,
                    hir_def::hir::Expr::Closure {
                        closure_kind: hir_def::hir::ClosureKind::OldCoroutine(_)
                            | hir_def::hir::ClosureKind::Coroutine { .. },
                        ..
                    }
                ),
                "expected a coroutine, found {expr:?}"
            );
        }

        Self::new_impl(db, ClosureKey::new(db, loc))
    }
}

#[blaze_query::interned(constructor = new_impl, debug)]
#[derive(PartialOrd, Ord)]
pub struct InternedCoroutineClosureId<'db> {
    #[returns(copy)]
    key: ClosureKey<'db>,
}

impl<'db> InternedCoroutineClosureId<'db> {
    pub fn loc(self, db: &'db Db) -> InternedClosure<'db> {
        self.key(db).loc(db)
    }

    #[inline]
    pub fn new(db: &'db Db, loc: InternedClosure<'db>) -> Self {
        if cfg!(debug_assertions) {
            let store = ExpressionStore::of(db, loc.owner.expression_store_owner(db));
            let expr = store.expr(loc.expr);
            assert!(
                matches!(
                    expr,
                    hir_def::hir::Expr::Closure {
                        closure_kind: hir_def::hir::ClosureKind::CoroutineClosure(_),
                        ..
                    }
                ),
                "expected a coroutine closure, found {expr:?}"
            );
        }

        Self::new_impl(db, ClosureKey::new(db, loc))
    }

    /// The closure this is when called: its body returns the future of its `async` block.
    pub fn as_closure(self, db: &'db Db) -> InternedClosureId<'db> {
        InternedClosureId::new(db, self.loc(db))
    }
}

/// An anonymous const expression that appears in a type position (e.g., array lengths,
/// const generic arguments like `{ N + 1 }`, or const param defaults). Unlike named constants,
/// these don't have their own `Body` — their expressions live in the parent's signature `ExpressionStore`.
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct AnonConstLoc {
    /// The owner store containing this expression.
    pub owner: ExpressionStoreOwnerId,
    /// The ExprId within the owner's ExpressionStore that is the root
    /// of this anonymous const expression.
    pub expr: ExprId,
    pub ty: StoredEarlyBinder<StoredTy>,
    /// Whether to allow using generic params from the owner.
    /// true for array repeats, false for everything else.
    pub allow_using_generic_params: bool,
}

#[blaze_query::interned(debug, constructor = new_)]
#[derive(PartialOrd, Ord)]
pub struct AnonConstId {
    #[returns(ref)]
    pub loc: AnonConstLoc,
}

impl<'db> AnonConstId<'db> {
    pub fn new(db: &'db Db, loc: AnonConstLoc, token: TrackedStructToken) -> Self {
        _ = token;
        AnonConstId::new_(db, loc)
    }
}

impl HasModule for AnonConstId<'_> {
    fn module(&self, db: &Db) -> ModuleId {
        self.loc(db).owner.module(db)
    }
}

impl HasResolver for AnonConstId<'_> {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        self.loc(db).owner.resolver(db)
    }
}

pub fn signature_anon_consts_and_diagnostics<'db>(
    db: &'db Db,
    def: GenericDefId,
) -> ArrayVec<(&'db [AnonConstId<'db>], &'db [TyLoweringDiagnostic]), 5> {
    let mut result = ArrayVec::new();

    // Queries common to all generic defs:
    push(&mut result, db.generic_defaults_with_diagnostics(def).as_lowered());
    push(&mut result, GenericPredicates::query_with_diagnostics(db, def));
    push(&mut result, db.const_param_types_with_diagnostics(def).as_lowered());

    match def {
        GenericDefId::ImplId(id) => {
            push(&mut result, db.impl_self_ty_with_diagnostics(id));
            if let Some(trait_ref) = db.impl_trait_with_diagnostics(id) {
                push(&mut result, trait_ref);
            }
        }
        GenericDefId::TypeAliasId(id) => {
            push(&mut result, db.type_for_type_alias_with_diagnostics(id).as_lowered());
            push(&mut result, db.type_alias_bounds_with_diagnostics(id).as_lowered());
        }
        GenericDefId::FunctionId(id) => {
            push(&mut result, db.fn_sig_for_fn_with_diagnostics(id).as_lowered())
        }
        GenericDefId::ConstId(def) => {
            push(&mut result, db.type_for_const_with_diagnostics(def).as_lowered())
        }
        GenericDefId::StaticId(def) => {
            push(&mut result, db.type_for_static_with_diagnostics(def).as_lowered())
        }
        GenericDefId::TraitId(_) | GenericDefId::AdtId(_) => {}
    }

    return result;

    fn push<'db, T: ?Sized>(
        result: &mut ArrayVec<(&'db [AnonConstId<'db>], &'db [TyLoweringDiagnostic]), 5>,
        item: crate::lower::Lowered<'db, T>,
    ) {
        result.push((item.defined_anon_consts(), item.diagnostics()));
    }
}

impl<'db> AnonConstId<'db> {
    /// A `const { .. }` block of a body, which MIR lowering makes a constant of its own. The
    /// body inferred it.
    pub fn is_inline_const(self, db: &'db Db) -> bool {
        let loc = self.loc(db);
        matches!(loc.owner, ExpressionStoreOwnerId::Body(_))
            && matches!(
                hir_def::expr_store::ExpressionStore::of(db, loc.owner).expr(loc.expr),
                hir_def::hir::Expr::Const(_)
            )
    }

    pub fn all_from_signature(
        db: &'db Db,
        def: GenericDefId,
    ) -> impl Iterator<Item = AnonConstId<'db>> {
        signature_anon_consts_and_diagnostics(db, def)
            .into_iter()
            .flat_map(|(anon_consts, _)| anon_consts)
            .copied()
    }
}

/// A constant, which might appears as a const item, an anonymous const block in expressions
/// or patterns, or as a constant in types with const generics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Supertype)]
pub enum GeneralConstId<'db> {
    ConstId(ConstId),
    StaticId(StaticId),
    AnonConstId(AnonConstId<'db>),
}

impl_from!(impl<'db> ConstId, StaticId, AnonConstId<'db> for GeneralConstId<'db>);

impl<'db> GeneralConstId<'db> {
    pub fn generic_def(self, db: &'db Db) -> Option<GenericDefId> {
        match self {
            GeneralConstId::ConstId(it) => Some(it.into()),
            GeneralConstId::StaticId(it) => Some(it.into()),
            GeneralConstId::AnonConstId(it) => Some(it.loc(db).owner.generic_def(db)),
        }
    }

    pub fn name(self, db: &'db Db) -> String {
        match self {
            GeneralConstId::StaticId(it) => {
                // alloc: error text, every caller formats a diagnostic or a difference
                StaticSignature::of(db, it).name.display(db, Edition::CURRENT).to_string()
            }
            GeneralConstId::ConstId(const_id) => {
                ConstSignature::of(db, const_id).name.as_ref().map_or_else(
                    // alloc: error text
                    || "_".to_owned(),
                    // alloc: error text
                    |name| name.display(db, Edition::CURRENT).to_string(),
                )
            }
            // alloc: error text
            GeneralConstId::AnonConstId(_) => "{const}".to_owned(),
        }
    }
}

/// The kind numbers of hir-ty's queries and interned ids, for blaze-query and the
/// why-did-this-rerun explainer: hir-ty's part of blaze-query's `FRONT` range. A number is
/// never given to another query, nor reused once its query is gone.
pub mod kinds {
    blaze_query::query_kinds! {
        range: 0x2400..0x2600, owner: "hir-ty";

        // Bodies.
        INFER = 0x2400, "infer_body_query";
        INFER_ANON_CONST = 0x2401, "infer_anon_const_body_query";
        CLOSURE_EXPRS = 0x2402, "closure_exprs";
        COROUTINE_OBLIGATIONS = 0x2403, "coroutine_obligation_errors";

        // Const eval.
        CONST_EVAL = 0x2410, "const_eval_query";
        ANON_CONST_EVAL = 0x2411, "anon_const_eval_query";
        CONST_EVAL_STATIC = 0x2412, "const_eval_static_query";
        CONST_EVAL_DISCRIMINANT = 0x2413, "const_eval_discriminant_variant";
        ANON_CONST_VALTREE_ERROR = 0x2414, "anon_const_valtree_error";
        EXPLICIT_DISCRIMINANT = 0x2415, "explicit_discriminant";
        EXPLICIT_DISCRIMINANT_INDICES = 0x2416, "explicit_discriminant_indices";
        ENUM_DISCRIMINANTS = 0x2417, "enum_discriminants";

        // Signatures.
        IMPL_TRAIT = 0x2420, "impl_trait_of_impl";
        TYPE_FOR_CONST = 0x2421, "type_for_const_with_diagnostics";
        TYPE_FOR_STATIC = 0x2422, "type_for_static_with_diagnostics";
        TYPE_FOR_TYPE_ALIAS = 0x2423, "type_for_type_alias_with_diagnostics";
        IMPL_SELF_TY = 0x2424, "impl_self_ty_of_impl";
        CONST_PARAM_TYPES = 0x2425, "const_param_types_with_diagnostics";
        FIELD_TYPES = 0x2426, "field_types_of_variant";
        SUPERTRAITS_INFO = 0x2427, "supertraits_info";
        ASSOC_TYPE_SHORTHAND = 0x2428, "resolve_type_param_assoc_type_shorthand";
        TYPE_ALIAS_BOUNDS = 0x2429, "type_alias_bounds_with_diagnostics";
        GENERIC_PREDICATES = 0x242a, "generic_predicates_of";
        TRAIT_ENVIRONMENT = 0x242b, "trait_environment_query";
        CONST_CONDITIONS = 0x242c, "const_conditions_query";
        GENERIC_DEFAULTS = 0x242d, "generic_defaults_with_diagnostics";
        FN_SIG_FOR_FN = 0x242e, "fn_sig_for_fn";
        ELIDED_LIFETIMES = 0x242f, "elided_lifetimes";
        FN_SIG_FOR_STRUCT_CONSTRUCTOR = 0x2430, "fn_sig_for_struct_constructor";
        FN_SIG_FOR_ENUM_VARIANT_CONSTRUCTOR = 0x2431, "fn_sig_for_enum_variant_constructor";
        BUILTIN_DERIVE_PREDICATES = 0x2432, "predicates";
        CONST_PARAM_ENV = 0x2433, "const_param_env_query";
        ITEM_BOUNDS = 0x2434, "ItemBounds::of";
        IMPL_SUPER_OUTLIVES = 0x2435, "impl_super_outlives";
        CRATE_FIELD_TYPES = 0x2436, "crate_field_types";
        CRATE_IMPL_SELF_TYS = 0x2437, "crate_impl_self_tys";
        CRATE_IMPL_TRAITS = 0x2438, "crate_impl_traits";
        CRATE_IMPL_PREDICATES = 0x2439, "crate_impl_predicates";
        CONST_PARAM_TY_ERRORS = 0x243a, "const_param_ty_errors";
        INHERENT_ASSOC_TYPES = 0x243b, "inherent_assoc_types";
        FN_SIG_WITH_ELIDED_BOUND = 0x243c, "fn_sig_with_elided_bound";

        // Variance, dyn compatibility, representability.
        VARIANCES_OF = 0x2440, "variances_of_query";
        CTOR_VARIANCES = 0x2441, "ctor_variances_query";
        DYN_COMPATIBILITY_OF_TRAIT = 0x2442, "dyn_compatibility_of_trait_query";
        GENERICS_REQUIRE_SIZED_SELF = 0x2443, "generics_require_sized_self";
        REPRESENTABILITY = 0x2444, "representability";
        PARAMS_IN_REPR = 0x2445, "params_in_repr";
        OPAQUE_VARIANCES = 0x2446, "opaque_variances_query";

        // Layout and drop.
        LAYOUT_OF_ADT = 0x2450, "layout_of_adt_query";
        LAYOUT_OF_TY = 0x2451, "layout_of_ty_query";
        TARGET_DATA_LAYOUT = 0x2452, "target_data_layout_query";
        STRUCT_FLAGS = 0x2453, "struct_flags";
        VARIANT_FIELD_IDS = 0x2454, "variant_field_ids";
        ENUM_VARIANT_IDS = 0x2455, "enum_variant_ids";
        DESTRUCTOR = 0x2456, "destructor";
        STRUCT_TAIL_MAY_BE_UNSIZED = 0x2457, "struct_tail_may_be_unsized";
        CRATE_TARGET_ARCH = 0x2458, "crate_target_arch";
        DECLARED_TARGET_FEATURES = 0x2459, "declared_target_features";
        CRATE_TARGET_FEATURES = 0x245a, "crate_target_features";

        // Method resolution, impls, coherence, specialization.
        LOOKUP_IMPL_METHOD = 0x2460, "lookup_impl_method_stored";
        IMPL_VALUE_ITEM = 0x2461, "impl_value_item";
        TRAIT_VALUE_ITEM = 0x2462, "trait_value_item";
        LOADED_CRATES = 0x2463, "loaded_crates";
        IMPLS_IN_BODIES = 0x2464, "impls_in_bodies";
        MACROS_WRITING_IMPLS = 0x2465, "macros_writing_impls";
        MACRO_DEFINITIONS = 0x2466, "macro_definitions";
        CRATES_WITH_INCOHERENT_IMPLS = 0x2467, "crates_containing_incoherent_inherent_impls";
        INHERENT_IMPLS_FOR_CRATE = 0x2468, "InherentImpls::for_crate";
        INHERENT_IMPLS_IN_CRATE_FOR_SELF_TY = 0x2469, "InherentImpls::in_crate_for_self_ty";
        INHERENT_IMPLS_FOR_BLOCK = 0x246a, "InherentImpls::for_block";
        INHERENT_IMPLS_IN_BLOCK_FOR_SELF_TY = 0x246b, "InherentImpls::in_block_for_self_ty";
        TRAIT_IMPLS_FOR_CRATE = 0x246c, "TraitImpls::for_crate";
        TRAIT_IMPLS_FOR_BLOCK = 0x246d, "TraitImpls::for_block";
        TRAIT_IMPLS_FOR_CRATE_AND_DEPS = 0x246e, "TraitImpls::for_crate_and_deps";
        TRAIT_IMPLS_IN_CRATE_AND_DEPS_FOR_TRAIT_AND_SELF_TY = 0x246f,
            "TraitImpls::in_crate_and_deps_for_trait_and_self_ty";
        TRAIT_IMPLS_BLANKET_IN_CRATE_AND_DEPS = 0x2470, "TraitImpls::blanket_in_crate_and_deps";
        TRAIT_IMPLS_IN_CRATE_FOR_TRAIT_AND_SELF_TY = 0x2471,
            "TraitImpls::in_crate_for_trait_and_self_ty";
        TRAIT_IMPLS_IN_BLOCK_FOR_TRAIT_AND_SELF_TY = 0x2472,
            "TraitImpls::in_block_for_trait_and_self_ty";
        TRAIT_IMPLS_IN_CRATE_FOR_TRAIT = 0x2473, "TraitImpls::in_crate_for_trait";
        TRAIT_IMPLS_IN_CRATE_AND_DEPS_FOR_TRAIT = 0x2474, "TraitImpls::in_crate_and_deps_for_trait";
        INHERENT_IMPL_COHERENT = 0x2475, "inherent_impl_coherent";
        ORPHAN_RULES_HOLD = 0x2476, "orphan_rules_hold";
        SPECIALIZES = 0x2477, "specializes_query";
        IMPL_HAS_UNCONSTRAINED_PARAMS = 0x2478, "impl_has_unconstrained_params";
        TRAIT_VALUE_ITEMS = 0x2479, "trait_value_items";
        IN_CRATE_TRAIT_VALUE_ITEMS = 0x247a, "in_crate_trait_value_items";
        CRATES_THAT_NAME = 0x247b, "crates_that_name";
        MAY_SPECIALIZE = 0x247c, "may_specialize";
        RELEVANT_IMPLS = 0x247d, "relevant_impls";
        MODULE_TRAIT_SET = 0x247e, "module_trait_set";
        MODULE_ITEMS_NAMED = 0x247f, "module_items_named";

        // Opaque types.
        IMPL_TYPE_ALIASES = 0x2480, "impl_type_aliases";
        TRAIT_TYPE_ALIASES = 0x2481, "trait_type_aliases";
        RPIT_HIDDEN_TYPES = 0x2482, "rpit_hidden_types";
        TAIT_HIDDEN_TYPES = 0x2483, "tait_hidden_types";
        DEFINING_FUNCTIONS = 0x2484, "defining_functions";
        DEFINE_OPAQUE_FUNCTIONS = 0x2485, "define_opaque_functions";
        DEFINE_OPAQUE_AT = 0x2486, "define_opaque_at";

        // MIR, declared in `providers` and computed by hir-ty-mir.
        MIR_BODY = 0x2490, "mir_body_query";
        MIR_BODY_FOR_CLOSURE = 0x2491, "mir_body_for_closure_query";
        MIR_BODY_FOR_COROUTINE = 0x2492, "mir_body_for_coroutine_query";
        MIR_BODY_FOR_COROUTINE_BY_MOVE = 0x2493, "mir_body_for_coroutine_by_move_query";
        MONOMORPHIZED_MIR_BODY = 0x2494, "monomorphized_mir_body_query";
        MONOMORPHIZED_MIR_BODY_FOR_CLOSURE = 0x2495, "monomorphized_mir_body_for_closure_query";
        MONOMORPHIZED_MIR_BODY_FOR_COROUTINE = 0x2496,
            "monomorphized_mir_body_for_coroutine_query";
        MONOMORPHIZED_MIR_BODY_FOR_COROUTINE_BY_MOVE = 0x2497,
            "monomorphized_mir_body_for_coroutine_by_move_query";
        RUNTIME_MIR_BODY = 0x2498, "runtime_mir_body_query";
        RUNTIME_MIR_BODY_FOR_CLOSURE = 0x2499, "runtime_mir_body_for_closure_query";
        RUNTIME_MIR_BODY_FOR_COROUTINE = 0x249a, "runtime_mir_body_for_coroutine_query";
        RUNTIME_MIR_BODY_FOR_COROUTINE_BY_MOVE = 0x249b,
            "runtime_mir_body_for_coroutine_by_move_query";
        MONOMORPHIZED_RUNTIME_MIR_BODY = 0x249c, "monomorphized_runtime_mir_body_query";
        MONOMORPHIZED_RUNTIME_MIR_BODY_FOR_CLOSURE = 0x249d,
            "monomorphized_runtime_mir_body_for_closure_query";
        MONOMORPHIZED_RUNTIME_MIR_BODY_FOR_COROUTINE = 0x249e,
            "monomorphized_runtime_mir_body_for_coroutine_query";
        MONOMORPHIZED_RUNTIME_MIR_BODY_FOR_COROUTINE_BY_MOVE = 0x249f,
            "monomorphized_runtime_mir_body_for_coroutine_by_move_query";
        OPTIMIZED_MIR_BODY = 0x24a0, "optimized_mir_body_query";
        OPTIMIZED_MIR_BODY_FOR_CLOSURE = 0x24a1, "optimized_mir_body_for_closure_query";
        OPTIMIZED_MIR_BODY_FOR_COROUTINE = 0x24a2, "optimized_mir_body_for_coroutine_query";
        OPTIMIZED_MIR_BODY_FOR_COROUTINE_BY_MOVE = 0x24a3,
            "optimized_mir_body_for_coroutine_by_move_query";
        MONOMORPHIZED_OPTIMIZED_MIR_BODY = 0x24a4, "monomorphized_optimized_mir_body_query";
        MONOMORPHIZED_OPTIMIZED_MIR_BODY_FOR_CLOSURE = 0x24a5,
            "monomorphized_optimized_mir_body_for_closure_query";
        MONOMORPHIZED_OPTIMIZED_MIR_BODY_FOR_COROUTINE = 0x24a6,
            "monomorphized_optimized_mir_body_for_coroutine_query";
        MONOMORPHIZED_OPTIMIZED_MIR_BODY_FOR_COROUTINE_BY_MOVE = 0x24a7,
            "monomorphized_optimized_mir_body_for_coroutine_by_move_query";
        COROUTINE_WITNESSES = 0x24a8, "coroutine_witnesses_query";

        // Per-module impl tables.
        INHERENT_IMPLS_FOR_MODULE = 0x24b0, "InherentImpls::for_module";
        TRAIT_IMPLS_FOR_MODULE = 0x24b4, "TraitImpls::for_module";
        TRAIT_IMPLS_FOR_CRATE_AND_TRAIT = 0x24b8, "TraitImpls::for_crate_and_trait";

        // Maintained impl tables and trait candidates (`method_resolution/delta`).
        TRAIT_SLICE = 0x24d0, "trait_slice";
        INHERENT_SLICE = 0x24d1, "inherent_slice";
        CANDIDATE_SLICE = 0x24d2, "candidate_slice";
        TRAIT_BODIES = 0x24d3, "trait_bodies";
        INHERENT_BODIES = 0x24d6, "inherent_bodies";
        BODY_IMPL_SET_HASH = 0x24da, "body_impl_set_hash";
        IMPL_ROW_HASH = 0x24db, "impl_row_hash";
        BLANKET_HASH = 0x24dc, "blanket_hash";
        BODY_PART = 0x24dd, "body_part";
        TRAIT_SLICE_HASH = 0x24de, "trait_slice_hash";
        INHERENT_SLICE_HASH = 0x24df, "inherent_slice_hash";
        CANDIDATE_SLICE_HASH = 0x24e0, "candidate_slice_hash";

        // Imports.
        USED_IMPORTS_WITH_TRAITS = 0x24c0, "used_imports_with_traits";
        USES_FN_CACHE = 0x24c1, "uses_fn_cache";

        // Bodies served from the check cache.
        SERVED_PART = 0x24f0, "served_part";
        SERVED_ITEMS = 0x24f1, "served_items";
        LOCATION_LITERALS = 0x24f2, "location_literals";
        INFERENCE_VALUE = 0x24f3, "inference_value";
        MIR_VALUE = 0x24f4, "mir_value";

        // Goals answered from the crate's own impls (`next_solver/global_goals`).
        AUTO_TRAIT_DEPTH = 0x2500, "auto_trait_depth";
        COPY_CLONE_IMPL = 0x2501, "copy_clone_impl";

        // Inputs.
        FROZEN_IMPLS = 0x25e0, "FrozenImpls";
        TRAIT_IMPLS_SLICE_INPUT = 0x25e1, "TraitImplsSlice";
        INHERENT_SLICE_INPUT = 0x25e2, "InherentSlice";
        CANDIDATE_SLICE_INPUT = 0x25e3, "CandidateSlice";
        SLICE_DIRECTORY = 0x25e4, "SliceDirectory";
        IMPL_SLICES = 0x25e5, "ImplSlices";
        IMPL_IDENTITIES = 0x25e6, "ImplIdentities";
        SERVED_BODIES = 0x25e7, "ServedBodies";
        KEPT_CONSTS = 0x25e8, "KeptConsts";

        // Interned ids.
        INTERNED_OPAQUE_TY = 0x25f0, "InternedOpaqueTyId";
        INTERNED_CLOSURE = 0x25f1, "InternedClosureId";
        INTERNED_COROUTINE = 0x25f2, "InternedCoroutineId";
        INTERNED_COROUTINE_CLOSURE = 0x25f3, "InternedCoroutineClosureId";
        ANON_CONST = 0x25f4, "AnonConstId";
        IMPL_PAIR = 0x25f5, "ImplPair";
        RELEVANT_IMPLS_KEY = 0x25f6, "RelevantImplsKey";
    }

    #[cfg(test)]
    mod tests {
        use std::path::Path;

        use blaze_hash::HashSet;

        use super::*;

        #[test]
        fn kinds_are_distinct_and_hir_tys() {
            assert!(blaze_query::kinds::FRONT.start <= RANGE.start);
            assert!(RANGE.end <= blaze_query::kinds::FRONT.end);
            let mut numbers = HashSet::default();
            let mut names = HashSet::default();
            for &(kind, name) in ALL {
                assert!(RANGE.contains(&kind.0), "{name}: {:#x} is not hir-ty's", kind.0);
                assert!(numbers.insert(kind), "{name}: {:#x} is given twice", kind.0);
                assert!(names.insert(name), "{name} has two kinds");
                assert_eq!(of(name), Some(kind));
                assert_eq!(super::name(kind), Some(name));
            }
            assert_eq!(of("no_such_query"), None);
            assert_eq!(super::name(QueryKind(RANGE.end - 1)), None);
        }

        /// The queries and interned ids declared in the source of hir-ty, as [`ALL`] names them,
        /// outside test modules: a test's query is in no schema.
        fn declared() -> Vec<String> {
            let mut found = Vec::new();
            let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
            while let Some(dir) = dirs.pop() {
                for entry in std::fs::read_dir(&dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.file_stem().is_some_and(|it| it == "tests") {
                        continue;
                    }
                    if path.is_dir() {
                        dirs.push(path);
                    } else if path.extension().is_some_and(|it| it == "rs") {
                        scan(&std::fs::read_to_string(&path).unwrap(), &mut found);
                    }
                }
            }
            found
        }

        /// What follows each query attribute of `text`: a function, a method of a tracked
        /// `impl`, or an interned or input struct.
        fn scan(text: &str, found: &mut Vec<String>) {
            let attribute = concat!("#[", "blaze_query::");
            let mut in_impl: Option<String> = None;
            let mut lines = text.lines();
            while let Some(line) = lines.next() {
                if line.starts_with('}') {
                    in_impl = None;
                }
                let Some(rest) = line.trim_start().strip_prefix(attribute) else { continue };
                let keyword = match () {
                    _ if rest.starts_with("tracked") => "fn",
                    _ if rest.starts_with("interned") || rest.starts_with("input") => "struct",
                    _ => continue,
                };
                for next in lines.by_ref() {
                    let next = next.trim_start();
                    if next.starts_with("//") || next.starts_with("#[") {
                        continue;
                    }
                    if let Some(ty) = next.strip_prefix("impl") {
                        in_impl = Some(ident(ty));
                        break;
                    }
                    let mut words = next.split_whitespace();
                    if words.any(|it| it == keyword) {
                        let name = ident(words.next().unwrap_or_default());
                        found.push(match (keyword, &in_impl) {
                            ("fn", Some(ty)) => format!("{ty}::{name}"),
                            _ => name,
                        });
                        break;
                    }
                }
            }
        }

        /// The name `text` starts with, after the generic parameters of an `impl`.
        fn ident(text: &str) -> String {
            let text = text.trim_start();
            let text = match text.strip_prefix('<') {
                Some(rest) => &rest[rest.find('>').map_or(0, |at| at + 1)..],
                None => text,
            };
            // alloc: test code
            text.trim_start().chars().take_while(|&c| c.is_alphanumeric() || c == '_').collect()
        }

        #[test]
        fn every_query_has_a_kind() {
            let declared = declared();
            for name in &declared {
                assert!(of(name).is_some(), "`{name}` has no kind in `hir_ty::db::kinds`");
            }
            for &(_, name) in ALL {
                assert!(declared.iter().any(|it| it == name), "`{name}` is no query of hir-ty");
            }
        }
    }
}
