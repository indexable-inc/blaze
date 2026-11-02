//! Trait solving using next trait solver.

use std::{cell::OnceCell, fmt, hash::Hash};

use base_db::{Db, Crate};
use hir_def::{
    AdtId, AssocItemId, ExpressionStoreOwnerId, GenericDefId, HasModule, ImplId, Lookup, TraitId,
    expr_store::ExpressionStore,
    hir::generics::WherePredicate,
    lang_item::{LangItemEnum, LangItems},
    nameres::DefMap,
    resolver::Resolver,
    signatures::{
        ConstFlags, ConstSignature, EnumFlags, EnumSignature, FnFlags, FunctionSignature,
        StructFlags, StructSignature, TraitFlags, TraitSignature, TypeAliasFlags,
        TypeAliasSignature, UnionSignature,
    },
};
use hir_expand::name::Name;
use intern::sym;
use rustc_type_ir::{
    AliasTy, AliasTyKind, TypeVisitableExt, TypingMode,
    inherent::{BoundExistentialPredicates, IntoKind, Ty as _},
};

use crate::{
    LifetimeElisionKind, LifetimeLoweringMode, Span, TyLoweringContext,
    db::HirDatabase,
    generics::Generics,
    lower::LoweringMode,
    next_solver::{
        DbInterner, GenericArgs, ParamEnv, StoredClauses, Ty, TyKind,
        infer::{
            DbInternerInferExt, InferCtxt,
            select::EvaluationResult,
            traits::{Obligation, ObligationCause},
        },
        obligation_ctxt::ObligationCtxt,
    },
};

/// Type for `hir`, because commonly we want both param env and a crate in an exported API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParamEnvAndCrate<'db> {
    pub param_env: ParamEnv<'db>,
    pub krate: Crate,
}

impl<'db> ParamEnvAndCrate<'db> {
    #[inline]
    pub fn store(self) -> StoredParamEnvAndCrate {
        StoredParamEnvAndCrate { param_env: self.param_env.clauses.store(), krate: self.krate }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StoredParamEnvAndCrate {
    param_env: StoredClauses,
    pub krate: Crate,
}

impl StoredParamEnvAndCrate {
    #[inline]
    pub fn param_env<'db>(&self, _db: &'db Db) -> ParamEnv<'db> {
        ParamEnv { clauses: self.param_env.as_ref() }
    }

    #[inline]
    pub fn as_ref<'db>(&self, db: &'db Db) -> ParamEnvAndCrate<'db> {
        ParamEnvAndCrate { param_env: self.param_env(db), krate: self.krate }
    }
}

/// This should be used in `hir` only.
pub fn structurally_normalize_ty<'db>(
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    env: ParamEnv<'db>,
) -> Ty<'db> {
    let TyKind::Alias(..) = ty.kind() else { return ty };
    let mut ocx = ObligationCtxt::new(infcx);
    let ty = ocx.structurally_normalize_ty(&ObligationCause::dummy(), env, ty).unwrap_or(ty);
    ty.replace_infer_with_error(infcx.interner)
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, blaze_query::Value)]
pub enum FnTrait {
    // Warning: Order is important. If something implements `x` it should also implement
    // `y` if `y <= x`.
    FnOnce,
    FnMut,
    Fn,

    AsyncFnOnce,
    AsyncFnMut,
    AsyncFn,
}

impl FnTrait {
    pub fn method_name(self) -> Name {
        match self {
            FnTrait::FnOnce => Name::new_symbol_root(sym::call_once),
            FnTrait::FnMut => Name::new_symbol_root(sym::call_mut),
            FnTrait::Fn => Name::new_symbol_root(sym::call),
            FnTrait::AsyncFnOnce => Name::new_symbol_root(sym::async_call_once),
            FnTrait::AsyncFnMut => Name::new_symbol_root(sym::async_call_mut),
            FnTrait::AsyncFn => Name::new_symbol_root(sym::async_call),
        }
    }

    pub fn get_id(self, lang_items: &LangItems) -> Option<TraitId> {
        match self {
            FnTrait::FnOnce => lang_items.FnOnce,
            FnTrait::FnMut => lang_items.FnMut,
            FnTrait::Fn => lang_items.Fn,
            FnTrait::AsyncFnOnce => lang_items.AsyncFnOnce,
            FnTrait::AsyncFnMut => lang_items.AsyncFnMut,
            FnTrait::AsyncFn => lang_items.AsyncFn,
        }
    }

    pub fn lang_item(self) -> LangItemEnum {
        match self {
            FnTrait::FnOnce => LangItemEnum::FnOnce,
            FnTrait::FnMut => LangItemEnum::FnMut,
            FnTrait::Fn => LangItemEnum::Fn,
            FnTrait::AsyncFnOnce => LangItemEnum::AsyncFnOnce,
            FnTrait::AsyncFnMut => LangItemEnum::AsyncFnMut,
            FnTrait::AsyncFn => LangItemEnum::AsyncFn,
        }
    }

    /// The trait's name.
    pub const fn as_str(self) -> &'static str {
        match self {
            FnTrait::FnOnce => "FnOnce",
            FnTrait::FnMut => "FnMut",
            FnTrait::Fn => "Fn",
            FnTrait::AsyncFnOnce => "AsyncFnOnce",
            FnTrait::AsyncFnMut => "AsyncFnMut",
            FnTrait::AsyncFn => "AsyncFn",
        }
    }
}

impl fmt::Display for FnTrait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// This should not be used in `hir-ty`, only in `hir`.
pub fn implements_trait_unique<'db>(
    ty: Ty<'db>,
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    trait_: TraitId,
) -> bool {
    implements_trait_unique_with_infcx(db, env, trait_, |infcx| {
        infcx.fill_rest_fresh_args(Span::Dummy, trait_.into(), [ty.into()])
    })
}

/// This should not be used in `hir-ty`, only in `hir`.
pub fn implements_trait_unique_with_args<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    trait_: TraitId,
    args: GenericArgs<'db>,
) -> bool {
    implements_trait_unique_with_infcx(db, env, trait_, |_| args)
}

pub fn implements_trait_unique_with_infcx<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    trait_: TraitId,
    create_args: impl FnOnce(&InferCtxt<'db>) -> GenericArgs<'db>,
) -> bool {
    let interner = DbInterner::new_with(db, env.krate);
    // FIXME(next-solver): I believe this should be `PostAnalysis`.
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());

    let args = create_args(&infcx);
    trait_ref_holds(&infcx, env, trait_, args)
}

/// For each of `traits` in order, `each` of it and what [`implements_trait_unique`] answers
/// for `ty`, from one interner and one inference context in the same typing mode. Each check
/// runs in its own probe, which rolls back the universe, every variable table, region
/// constraints and obligations and opaque types, so no check sees what another left.
pub fn implements_traits_unique<'db>(
    ty: Ty<'db>,
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    traits: &[TraitId],
    mut each: impl FnMut(TraitId, bool),
) {
    let interner = DbInterner::new_with(db, env.krate);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    for &trait_ in traits {
        let holds = infcx.probe(|_| {
            let args = infcx.fill_rest_fresh_args(Span::Dummy, trait_.into(), [ty.into()]);
            trait_ref_holds(&infcx, env, trait_, args)
        });
        each(trait_, holds);
    }
}

fn trait_ref_holds<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnvAndCrate<'db>,
    trait_: TraitId,
    args: GenericArgs<'db>,
) -> bool {
    let interner = infcx.interner;
    let trait_ref = rustc_type_ir::TraitRef::new_from_args(interner, trait_.into(), args);
    if trait_ref.self_ty().is_ty_error() {
        return false;
    }

    let obligation = Obligation::new(interner, ObligationCause::dummy(), env.param_env, trait_ref);
    infcx.predicate_must_hold_modulo_regions(&obligation)
}

/// Whether two types unify once `create_tys` has replaced their generic parameters with
/// inference variables: `[u8]` and an impl's `[T]` do, `[u8]` and `[MaybeUninit<T>]` do not.
pub fn could_unify_with_infcx<'db>(
    db: &'db Db,
    krate: Crate,
    create_tys: impl FnOnce(&InferCtxt<'db>) -> (Ty<'db>, Ty<'db>),
) -> bool {
    let interner = DbInterner::new_with(db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let (a, b) = create_tys(&infcx);
    infcx.at(&ObligationCause::dummy(), ParamEnv::empty()).eq(a, b).is_ok()
}

/// Whether the trait impl `impl_` may apply to the type `create_ty` makes, as rustdoc's
/// `synthesize_blanket_impls` asks: the impl's parameters are inference variables, its self
/// type is equated with the type, and its where-clauses and trait ref may each hold.
pub fn impl_may_apply<'db>(
    db: &'db Db,
    krate: Crate,
    impl_: ImplId,
    create_ty: impl FnOnce(&InferCtxt<'db>) -> Ty<'db>,
) -> bool {
    let interner = DbInterner::new_with(db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let ty = create_ty(&infcx);
    let Some(trait_ref) = db.impl_trait(impl_) else { return false };
    let args = infcx.fresh_args_for_item(Span::Dummy, impl_.into());
    let trait_ref = trait_ref.instantiate(interner, args).skip_norm_wip();
    if infcx.at(&ObligationCause::dummy(), ParamEnv::empty()).eq(trait_ref.self_ty(), ty).is_err() {
        return false;
    }
    let may_hold = |obligation| infcx.predicate_may_hold(&obligation);
    let clauses = crate::lower::GenericPredicates::query_all(db, impl_.into())
        .iter_instantiated(interner, args.as_slice())
        .map(crate::next_solver::Unnormalized::skip_norm_wip);
    crate::next_solver::util::clauses_as_obligations(
        clauses,
        ObligationCause::dummy(),
        ParamEnv::empty(),
    )
    .all(may_hold)
        && may_hold(Obligation::new(
            interner,
            ObligationCause::dummy(),
            ParamEnv::empty(),
            trait_ref,
        ))
}

/// Whether the trait's item `item` cannot be used through `impl_`: one of its own where-clauses
/// that names only the trait's parameters (`Self::Item: Ord` of `Iterator::cmp`) cannot hold
/// there. rustc's `is_impossible_associated_item` (`compiler/rustc_trait_selection/src/traits/mod.rs`
/// at rust 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0), which rustdoc asks before it lists an item
/// the impl leaves as the trait provides it. `krate` is the crate asking, rustc's local crate:
/// a clause it could not know the answer to holds as far as it can tell.
pub fn is_impossible_associated_item(
    db: &Db,
    krate: Crate,
    impl_: ImplId,
    item: GenericDefId,
) -> bool {
    use std::ops::ControlFlow;

    use rustc_type_ir::{TypeSuperVisitable, TypeVisitable, TypeVisitor};

    use crate::next_solver::{Const, ConstKind, EarlyBinder, Region, RegionKind};

    /// Breaks at a parameter of the item's own, numbered after the trait's.
    struct OwnParam(u32);
    impl<'db> TypeVisitor<DbInterner<'db>> for OwnParam {
        type Result = ControlFlow<()>;
        fn visit_ty(&mut self, ty: Ty<'db>) -> ControlFlow<()> {
            match ty.kind() {
                TyKind::Param(param) if param.index >= self.0 => ControlFlow::Break(()),
                _ => ty.super_visit_with(self),
            }
        }
        fn visit_region(&mut self, region: Region<'db>) -> ControlFlow<()> {
            match region.kind() {
                RegionKind::ReEarlyParam(param) if param.index >= self.0 => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            }
        }
        fn visit_const(&mut self, konst: Const<'db>) -> ControlFlow<()> {
            match konst.kind() {
                ConstKind::Param(param) if param.index >= self.0 => ControlFlow::Break(()),
                _ => konst.super_visit_with(self),
            }
        }
    }

    let Some(trait_ref) = db.impl_trait(impl_) else { return false };
    // Solved for `krate`, rustc's local crate: coherence answers a trait goal only a crate
    // upstream of it could implement (`io::Error: Ord`) as ambiguous, since that crate may add
    // the impl.
    let interner = DbInterner::new_with(db, krate);
    // As coherence asks: the impl's parameters are inference variables, so a clause that holds
    // for some of their values (`T: Ord` of an `IntoIter<T>`) is ambiguous, not an error.
    let infcx = interner.infer_ctxt().build(TypingMode::Coherence);
    let fresh = infcx.fresh_args_for_item(Span::Dummy, impl_.into());
    let args = trait_ref.instantiate(interner, fresh).skip_norm_wip().args;
    let parent = crate::generics::generics(db, item).len_parent() as u32;
    let mut ocx = ObligationCtxt::new(&infcx);
    for clause in crate::lower::GenericPredicates::query_own_explicit(db, item)
        .skip_binder()
        .filter(|clause| clause.visit_with(&mut OwnParam(parent)).is_continue())
    {
        let clause = EarlyBinder::bind(interner, clause).instantiate(interner, args).skip_norm_wip();
        let obligation =
            Obligation::new(interner, ObligationCause::dummy(), ParamEnv::empty(), clause);
        ocx.register_obligation(obligation);
    }
    !ocx.try_evaluate_obligations().is_empty()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WherePredicateEvaluation {
    Holds,
    NotProven,
    HasErrors,
    NoObligations,
}

/// This should not be used in `hir-ty`, only in `hir`.
/// This is exposed to allow the IDE to evaluate arbitrary predicates.
pub fn where_predicate_must_hold<'db>(
    db: &'db Db,
    resolver: &Resolver<'db>,
    store: &ExpressionStore,
    def: ExpressionStoreOwnerId,
    generic_def: GenericDefId,
    env: ParamEnvAndCrate<'db>,
    predicate: &WherePredicate,
) -> WherePredicateEvaluation {
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        let generics = OnceCell::<Generics<'db>>::new();
        let mut ctx = TyLoweringContext::new(
            db,
            resolver,
            store,
            def,
            generic_def,
            &generics,
            LifetimeElisionKind::Infer,
            LifetimeLoweringMode::Bound,
            frame,
        )
        .with_interning_mode(LoweringMode::Ide);
        let mut clauses = Vec::new_in(frame);
        clauses.extend(ctx.lower_where_predicate(predicate, false).map(|(clause, _)| clause));

        if !ctx.diagnostics.is_empty()
            || clauses.iter().any(|clause| clause.as_predicate().references_error())
        {
            return WherePredicateEvaluation::HasErrors;
        }

        if clauses.is_empty() {
            return if ctx.unsized_types.is_empty() {
                WherePredicateEvaluation::HasErrors
            } else {
                WherePredicateEvaluation::NoObligations
            };
        }

        let result = infcx.probe(|snapshot| {
            let mut ocx = ObligationCtxt::new(&infcx);
            for clause in clauses {
                let obligation = Obligation::new(
                    interner,
                    ObligationCause::dummy(),
                    env.param_env,
                    clause.as_predicate(),
                );
                ocx.register_obligation(obligation);
            }

            let mut result = EvaluationResult::EvaluatedToOk;
            for error in ocx.evaluate_obligations_error_on_ambiguity() {
                if error.is_true_error() {
                    return EvaluationResult::EvaluatedToErr;
                }
                result = result.max(EvaluationResult::EvaluatedToAmbig);
            }
            if infcx.opaque_types_added_in_snapshot(snapshot) {
                result.max(EvaluationResult::EvaluatedToOkModuloOpaqueTypes)
            } else if infcx.region_constraints_added_in_snapshot(snapshot) {
                result.max(EvaluationResult::EvaluatedToOkModuloRegions)
            } else {
                result
            }
        });

        if result.must_apply_modulo_regions() {
            WherePredicateEvaluation::Holds
        } else {
            WherePredicateEvaluation::NotProven
        }
    })
}

/// Whether the inherent impl `impl_id` may be written in the crate of `def_map`. The answer
/// depends on the impl, that crate and whether it is `#![rustc_coherence_is_core]`, and is
/// kept per impl.
pub fn is_inherent_impl_coherent(db: &Db, def_map: &DefMap, impl_id: ImplId) -> bool {
    inherent_impl_coherent(db, impl_id, def_map.krate(), def_map.is_rustc_coherence_is_core())
}

#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
fn inherent_impl_coherent(
    db: &Db,
    impl_id: ImplId,
    krate: Crate,
    rustc_coherence_is_core: bool,
) -> bool {
    let mut self_ty = db.impl_self_ty(impl_id).instantiate_identity().skip_norm_wip();
    // rustc's `InherentCollect::check_item`: a pattern type is taken as its base.
    while let TyKind::Pat(base, _) = self_ty.kind() {
        self_ty = base;
    }
    let self_ty = self_ty.kind();
    let impl_allowed = match self_ty {
        TyKind::Tuple(_)
        | TyKind::FnDef(_, _)
        | TyKind::FnPtr(..)
        | TyKind::UnsafeBinder(_)
        | TyKind::Array(_, _)
        | TyKind::Never
        | TyKind::RawPtr(_, _)
        | TyKind::Ref(_, _, _)
        | TyKind::Slice(_)
        | TyKind::Str
        | TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_) => rustc_coherence_is_core,

        TyKind::Adt(adt_def, _) => adt_def.def_id().module(db).krate(db) == krate,
        TyKind::Dynamic(it, _) => {
            it.principal_def_id().is_some_and(|trait_id| trait_id.0.module(db).krate(db) == krate)
        }
        // rustc's `InherentNominal` (E0118): no type to hang the impl on, in any crate.
        TyKind::Param(_)
        | TyKind::Alias(
            _,
            AliasTy {
                kind:
                    AliasTyKind::Projection { .. }
                    | AliasTyKind::Inherent { .. }
                    | AliasTyKind::Opaque { .. },
                ..
            },
        ) => false,

        _ => true,
    };
    impl_allowed || {
        let rustc_has_incoherent_inherent_impls = match self_ty {
            TyKind::Tuple(_)
            | TyKind::FnDef(_, _)
            | TyKind::FnPtr(..)
            | TyKind::UnsafeBinder(_)
            | TyKind::Array(_, _)
            | TyKind::Never
            | TyKind::RawPtr(_, _)
            | TyKind::Ref(_, _, _)
            | TyKind::Slice(_)
            | TyKind::Str
            | TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_) => true,

            TyKind::Adt(adt_def, _) => match adt_def.def_id() {
                hir_def::AdtId::StructId(id) => StructSignature::of(db, id)
                    .flags
                    .contains(StructFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS),
                hir_def::AdtId::UnionId(id) => UnionSignature::of(db, id)
                    .flags
                    .contains(StructFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS),
                hir_def::AdtId::EnumId(it) => EnumSignature::of(db, it)
                    .flags
                    .contains(EnumFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS),
            },
            TyKind::Dynamic(it, _) => it.principal_def_id().is_some_and(|trait_id| {
                TraitSignature::of(db, trait_id.0)
                    .flags
                    .contains(TraitFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS)
            }),

            _ => false,
        };
        let items = impl_id.impl_items(db);
        rustc_has_incoherent_inherent_impls
            && !items.items.is_empty()
            && items.items.iter().all(|&(_, assoc)| match assoc {
                AssocItemId::FunctionId(it) => FunctionSignature::of(db, it)
                    .flags
                    .contains(FnFlags::RUSTC_ALLOW_INCOHERENT_IMPL),
                AssocItemId::ConstId(it) => ConstSignature::of(db, it)
                    .flags
                    .contains(ConstFlags::RUSTC_ALLOW_INCOHERENT_IMPL),
                AssocItemId::TypeAliasId(it) => TypeAliasSignature::of(db, it)
                    .flags
                    .contains(TypeAliasFlags::RUSTC_ALLOW_INCOHERENT_IMPL),
            })
    }
}

/// Checks whether the impl satisfies the orphan rules.
///
/// Given `impl<P1..=Pn> Trait<T1..=Tn> for T0`, an `impl` is valid only if at least one of the following is true:
/// - Trait is a local trait
/// - All of
///   - At least one of the types `T0..=Tn` must be a local type. Let `Ti` be the first such type.
///   - No uncovered type parameters `P1..=Pn` may appear in `T0..Ti` (excluding `Ti`)
pub fn check_orphan_rules<'db>(db: &'db Db, impl_: ImplId) -> bool {
    orphan_rules_hold(db, impl_)
}

#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
fn orphan_rules_hold(db: &Db, impl_: ImplId) -> bool {
    check_orphan_rules_uncached(db, impl_)
}

fn check_orphan_rules_uncached<'db>(db: &'db Db, impl_: ImplId) -> bool {
    let Some(impl_trait) = db.impl_trait(impl_) else {
        // not a trait impl
        return true;
    };

    let local_crate = impl_.lookup(db).container.krate(db);
    let is_local = |tgt_crate| tgt_crate == local_crate;

    let trait_ref = impl_trait.instantiate_identity().skip_norm_wip();
    let trait_id = trait_ref.def_id.0;
    if is_local(trait_id.module(db).krate(db)) {
        // trait to be implemented is local
        return true;
    }

    // Normalizing folds only aliases, so a header without one is its own normal form.
    let trait_ref = if trait_ref.has_aliases() {
        let interner = DbInterner::new_with(db, local_crate);
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        let param_env = db.trait_environment(impl_.into());
        infcx
            .at(&ObligationCause::dummy(), param_env)
            .deeply_normalize(trait_ref)
            .unwrap_or(trait_ref)
    } else {
        trait_ref
    };

    let unwrap_fundamental = |mut ty: Ty<'db>| {
        // Unwrap all layers of fundamental types with a loop.
        loop {
            match ty.kind() {
                TyKind::Ref(_, referenced, _) => ty = referenced,
                TyKind::Adt(adt_def, subs) => {
                    let AdtId::StructId(s) = adt_def.def_id() else {
                        break ty;
                    };
                    let struct_signature = StructSignature::of(db, s);
                    if struct_signature.flags.contains(StructFlags::FUNDAMENTAL) {
                        let next = subs.types().next();
                        match next {
                            Some(it) => ty = it,
                            None => break ty,
                        }
                    } else {
                        break ty;
                    }
                }
                _ => break ty,
            }
        }
    };
    //   - At least one of the types `T0..=Tn` must be a local type. Let `Ti` be the first such type.

    // FIXME: param coverage
    //   - No uncovered type parameters `P1..=Pn` may appear in `T0..Ti` (excluding `Ti`)
    let is_not_orphan = trait_ref.args.types().any(|ty| match unwrap_fundamental(ty).kind() {
        TyKind::Adt(adt_def, _) => is_local(adt_def.def_id().module(db).krate(db)),
        TyKind::Error(_) => true,
        TyKind::Dynamic(it, _) => {
            it.principal_def_id().is_some_and(|trait_id| is_local(trait_id.0.module(db).krate(db)))
        }
        _ => false,
    });
    #[allow(clippy::let_and_return, reason = "the name clarifies the meaning of the boolean")]
    is_not_orphan
}

#[cfg(test)]
mod fn_trait_tests {
    use hir_def::lang_item::LangItemEnum;

    use super::FnTrait;

    /// `hir`'s own `FnTrait`, before it became this one: its `Display` and `lang_item`.
    fn hir_copy(it: FnTrait) -> (&'static str, LangItemEnum) {
        match it {
            FnTrait::FnOnce => ("FnOnce", LangItemEnum::FnOnce),
            FnTrait::FnMut => ("FnMut", LangItemEnum::FnMut),
            FnTrait::Fn => ("Fn", LangItemEnum::Fn),
            FnTrait::AsyncFnOnce => ("AsyncFnOnce", LangItemEnum::AsyncFnOnce),
            FnTrait::AsyncFnMut => ("AsyncFnMut", LangItemEnum::AsyncFnMut),
            FnTrait::AsyncFn => ("AsyncFn", LangItemEnum::AsyncFn),
        }
    }

    #[test]
    fn names_and_lang_items_are_hirs() {
        for it in [
            FnTrait::FnOnce,
            FnTrait::FnMut,
            FnTrait::Fn,
            FnTrait::AsyncFnOnce,
            FnTrait::AsyncFnMut,
            FnTrait::AsyncFn,
        ] {
            let (name, lang) = hir_copy(it);
            assert_eq!(it.to_string(), name);
            assert_eq!(it.as_str(), name);
            assert_eq!(it.lang_item(), lang);
        }
    }
}
