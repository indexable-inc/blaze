//! Defining `SolverContext` for next-trait-solver.

use std::{collections::hash_map::Entry, fmt::Debug, mem, ops::ControlFlow};

use blaze_hash::HashMap;
use hir_def::{
    AssocItemId,
    signatures::{ConstSignature, TypeAliasSignature},
};
use rustc_next_trait_solver::{delegate::SolverDelegate, solve::SearchStorage};
use rustc_type_ir::{
    AliasTyKind, CanonicalizerState, ClausePolarity, GenericArgKind, InferCtxtLike, InferTy,
    Interner, TypeFlags, TypeSuperVisitable, TypeVisitable, TypeVisitableExt, TypeVisitor,
    data_structures::SsoHashSet,
    inherent::{GenericArg as _, GenericArgs as _, IntoKind, Term as _, Ty as _},
    lang_items::SolverTraitLangItem,
    solve::{
        ComputeGoalFastPathOutcome, FastPathDepth, FetchEligibleAssocItemResponse, GoalStalledOn,
        GoalStalledOnOpaques, MaybeInfo, NoSolution, StalledVars, SubRoots, SucceededInErased,
        TyOrConstInferVar, VisibleForLeakCheck,
    },
    transmute::Answer,
};
use smallvec::smallvec;
use tracing::debug;

use crate::{
    ParamEnvAndCrate, Span,
    consteval::ConstEvalError,
    db::HirDatabase,
    lower::GenericPredicates,
    mir::MirLowerError,
    next_solver::{
        AliasConst, AliasConstKind, AliasTy, AllocationData, AnyImplId, CanonicalVarKind, Clause,
        ClauseKind, CoercePredicate, ConstExt, ConstKind, ErrorGuaranteed, GenericArgs,
        ImplOrTraitAssocTermId, OpaqueTyIdWrapper, ParamEnv, Predicate, PredicateKind,
        RegionConstraint, SubtypePredicate, TermId, TraitAssocTermId, Ty, TyKind, TypingMode,
        Unnormalized, fold::fold_tys, inspect::ProofTreeVisitor, recycle,
        util::sizedness_fast_path,
    },
};

use super::{
    Const, DbInterner, GenericArg, Region,
    global_goals::shape,
    infer::{DbInternerInferExt, InferCtxt, canonical::instantiate::CanonicalExt},
    inspect::{InspectConfig, InspectGoal},
};

pub type Goal<'db, P> = rustc_type_ir::solve::Goal<DbInterner<'db>, P>;

#[repr(transparent)]
pub(crate) struct SolverContext<'db>(pub(crate) InferCtxt<'db>);

impl<'a, 'db> From<&'a InferCtxt<'db>> for &'a SolverContext<'db> {
    fn from(infcx: &'a InferCtxt<'db>) -> Self {
        // SAFETY: `SolverContext` is `repr(transparent)` over `InferCtxt<'db>`, so the two
        // references have one layout and validity; the borrow `'a` and `'db` are kept as they
        // are. rustc's `SolverDelegate` is the same cast.
        unsafe { std::mem::transmute::<&'a InferCtxt<'db>, &'a SolverContext<'db>>(infcx) }
    }
}

impl<'db> std::ops::Deref for SolverContext<'db> {
    type Target = InferCtxt<'db>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A root goal that overflowed the recursion limit and then held with twice that limit:
/// rustc's `recursion_depth_exceeding_limit` future-compatibility warning, which it attaches
/// to the whole crate. The solver keeps the answer of the second evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextSolverOverflow<'db> {
    pub goal: Predicate<'db>,
    pub span: Span,
    /// The goals the first evaluation overflowed through, below `goal`: at each one the
    /// nested goal that needed the most depth, at most 16 deep (rustc's "which requires").
    pub chain: Vec<Predicate<'db>>,
}

impl<'db> SolverContext<'db> {
    fn known_no_opaque_types_in_storage(&self) -> bool {
        self.inner.borrow_mut().opaque_types().is_empty()
            // In erased mode, no opaques in the storage is not enough to answer here.
            && !self.typing_mode_raw().is_erased_not_coherence()
    }

    /// The fast-path answers that cost no search-graph depth.
    fn depth_free_fast_path(
        &self,
        goal: Goal<'db, Predicate<'db>>,
        span: Span,
    ) -> ComputeGoalFastPathOutcome<DbInterner<'db>> {
        use ComputeGoalFastPathOutcome as Outcome;
        let interner = self.interner;

        let pred = goal.predicate.kind();
        match pred.skip_binder() {
            PredicateKind::Clause(ClauseKind::Trait(trait_pred)) => {
                let trait_pred = pred.rebind(trait_pred);
                let self_ty = self.shallow_resolve(trait_pred.self_ty().skip_binder());
                if let Some(vid) = self_ty.ty_vid()
                    // We don't do this fast path when opaques are defined since we may
                    // eventually use opaques to incompletely guide inference via ty var
                    // self types.
                    // FIXME: Properly consider opaques here.
                    && self.known_no_opaque_types_in_storage()
                {
                    goal_stalled_on_args_or_nonempty_opaques(smallvec![TyOrConstInferVar::Ty(vid)])
                } else if trait_pred.polarity() == ClausePolarity::Positive {
                    match interner.as_trait_lang_item(trait_pred.def_id()) {
                        Some(SolverTraitLangItem::Sized | SolverTraitLangItem::MetaSized) => {
                            let predicate = self.deeply_resolve_ignoring_regions(goal.predicate);
                            if sizedness_fast_path(interner, predicate, goal.param_env) {
                                Outcome::TriviallyHolds
                            } else {
                                Outcome::NoFastPath
                            }
                        }
                        Some(SolverTraitLangItem::Copy | SolverTraitLangItem::Clone) => {
                            let self_ty = self
                                .deeply_resolve_ignoring_regions(trait_pred.self_ty().skip_binder());
                            // Unlike `Sized` traits, which always prefer the built-in impl,
                            // `Copy`/`Clone` may be shadowed by a param-env candidate which
                            // could force a lifetime error or guide inference. While that's
                            // not generally desirable, it is observable, so for now let's
                            // ignore this fast path for types that have regions or infer.
                            if !self_ty
                                .has_type_flags(TypeFlags::HAS_FREE_REGIONS | TypeFlags::HAS_INFER)
                                && self_ty.is_trivially_pure_clone_copy()
                            {
                                Outcome::TriviallyHolds
                            } else {
                                Outcome::NoFastPath
                            }
                        }
                        _ => Outcome::NoFastPath,
                    }
                } else {
                    Outcome::NoFastPath
                }
            }
            PredicateKind::DynCompatible(def_id) if interner.trait_is_dyn_compatible(def_id) => {
                Outcome::TriviallyHolds
            }
            PredicateKind::Clause(ClauseKind::RegionOutlives(outlives)) => {
                if outlives.has_escaping_bound_vars() {
                    return Outcome::NoFastPath;
                }
                // Region constraints of nested goals are never visible to the leak check;
                // this one skips the nested goal boundary, so it says so itself.
                InferCtxtLike::sub_regions(
                    &self.0,
                    outlives.1,
                    outlives.0,
                    VisibleForLeakCheck::No,
                    span,
                );
                Outcome::TriviallyHolds
            }
            PredicateKind::Clause(ClauseKind::TypeOutlives(outlives)) => {
                if outlives.has_escaping_bound_vars() {
                    return Outcome::NoFastPath;
                }
                let ty = self.deeply_resolve_ignoring_regions(outlives.0);
                if ty.has_non_rigid_aliases() {
                    return Outcome::NoFastPath;
                }
                if ty.has_non_region_infer() {
                    let mut infers = CollectNonRegionInfer::default();
                    ty.visit_with(&mut infers);
                    return goal_stalled_on_args(infers.infers);
                }
                InferCtxtLike::register_ty_outlives(&self.0, outlives.0, outlives.1, span);
                Outcome::TriviallyHolds
            }
            PredicateKind::Subtype(SubtypePredicate { a, b, .. })
            | PredicateKind::Coerce(CoercePredicate { a, b }) => {
                if a.has_escaping_bound_vars() || b.has_escaping_bound_vars() {
                    return Outcome::NoFastPath;
                }
                match (self.shallow_resolve(a).kind(), self.shallow_resolve(b).kind()) {
                    (
                        TyKind::Infer(InferTy::TyVar(a_vid)),
                        TyKind::Infer(InferTy::TyVar(b_vid)),
                    ) => {
                        self.sub_unify_ty_vids_raw(a_vid, b_vid);
                        goal_stalled_on_args(smallvec![
                            TyOrConstInferVar::Ty(a_vid),
                            TyOrConstInferVar::Ty(b_vid),
                        ])
                    }
                    _ => Outcome::NoFastPath,
                }
            }
            PredicateKind::Clause(ClauseKind::ConstArgHasType(ct, _)) => {
                if ct.has_escaping_bound_vars() {
                    return Outcome::NoFastPath;
                }
                match self.shallow_resolve_const(ct).ct_vid() {
                    Some(vid) => goal_stalled_on_args(smallvec![TyOrConstInferVar::Const(vid)]),
                    None => Outcome::NoFastPath,
                }
            }
            PredicateKind::Clause(ClauseKind::WellFormed(arg)) => {
                if arg.has_escaping_bound_vars() {
                    return Outcome::NoFastPath;
                }
                let arg = self.shallow_resolve_term(arg);
                if arg.is_trivially_wf(interner) {
                    Outcome::TriviallyHolds
                } else if arg.is_infer() {
                    goal_stalled_on_args(smallvec![
                        TyOrConstInferVar::maybe_from_term::<DbInterner<'db>>(arg)
                            .expect("an inference variable"),
                    ])
                } else {
                    Outcome::NoFastPath
                }
            }
            _ => Outcome::NoFastPath,
        }
    }
}

/// A fast-path answer: the goal is stalled on `stalled_vars` alone.
fn goal_stalled_on_args<'db>(
    stalled_vars: StalledVars,
) -> ComputeGoalFastPathOutcome<DbInterner<'db>> {
    ComputeGoalFastPathOutcome::TriviallyStalled {
        stalled_on: GoalStalledOn {
            stalled_vars,
            sub_roots: SubRoots::new(),
            stalled_maybe_info: MaybeInfo::AMBIGUOUS,
            opaques: GoalStalledOnOpaques::No,
        },
    }
}

/// A fast-path answer: the goal is stalled on `stalled_vars` or on the opaque type storage
/// becoming non-empty. Only for a context that is not in erased mode, which
/// [`SolverContext::known_no_opaque_types_in_storage`] checks.
fn goal_stalled_on_args_or_nonempty_opaques<'db>(
    stalled_vars: StalledVars,
) -> ComputeGoalFastPathOutcome<DbInterner<'db>> {
    ComputeGoalFastPathOutcome::TriviallyStalled {
        stalled_on: GoalStalledOn {
            stalled_vars,
            sub_roots: SubRoots::new(),
            stalled_maybe_info: MaybeInfo::AMBIGUOUS,
            opaques: GoalStalledOnOpaques::Yes {
                num_opaques_in_storage: 0,
                previously_succeeded_in_erased: SucceededInErased::No,
            },
        },
    }
}

/// Every type and const inference variable in a type, in visiting order.
#[derive(Default)]
struct CollectNonRegionInfer<'db> {
    infers: StalledVars,
    visited: SsoHashSet<Ty<'db>>,
}

impl<'db> CollectNonRegionInfer<'db> {
    fn push(&mut self, arg: GenericArg<'db>) {
        // alloc: past two variables `StalledVars` spills.
        self.infers.push(
            TyOrConstInferVar::maybe_from_generic_arg::<DbInterner<'db>>(arg)
                .expect("collected an inference variable"),
        );
    }
}

impl<'db> TypeVisitor<DbInterner<'db>> for CollectNonRegionInfer<'db> {
    type Result = ();

    fn visit_ty(&mut self, ty: Ty<'db>) {
        if self.visited.contains(&ty) {
            return;
        }
        match ty.kind() {
            TyKind::Infer(_) => self.push(ty.into()),
            _ => ty.super_visit_with(self),
        }
        self.visited.insert(ty);
    }

    fn visit_const(&mut self, ct: Const<'db>) {
        match ct.kind() {
            ConstKind::Infer(_) => self.push(ct.into()),
            _ => ct.super_visit_with(self),
        }
    }
}

impl<'db> SolverDelegate for SolverContext<'db> {
    type Interner = DbInterner<'db>;
    type Infcx = InferCtxt<'db>;

    fn cx(&self) -> Self::Interner {
        self.0.interner
    }

    fn build_with_canonical<V>(
        cx: Self::Interner,
        canonical: &rustc_type_ir::CanonicalQueryInput<Self::Interner, V>,
    ) -> (Self, V, rustc_type_ir::CanonicalVarValues<Self::Interner>)
    where
        V: rustc_type_ir::TypeFoldable<Self::Interner>,
    {
        let (infcx, value, vars) = cx.infer_ctxt().build_with_canonical(Span::Dummy, canonical);
        (SolverContext(infcx), value, vars)
    }

    fn compute_goal_fast_path(
        &self,
        goal: Goal<'db, Predicate<'db>>,
        span: Span,
        depth: FastPathDepth,
    ) -> ComputeGoalFastPathOutcome<Self::Interner> {
        use ComputeGoalFastPathOutcome as Outcome;
        if self.interner.assumptions_on_binders() {
            return Outcome::NoFastPath;
        }
        if depth.depth_free() {
            match self.depth_free_fast_path(goal, span) {
                Outcome::NoFastPath => {}
                outcome => return outcome,
            }
        }
        match depth.available().and_then(|available| shape::holds_within(self, goal, available)) {
            Some(required_depth) => Outcome::HoldsWithin { required_depth },
            None => Outcome::NoFastPath,
        }
    }

    fn fresh_var_for_kind(
        &self,
        arg: GenericArg<'db>,
        span: Span,
        universe: rustc_type_ir::UniverseIndex,
    ) -> GenericArg<'db> {
        match arg.kind() {
            GenericArgKind::Lifetime(_) => self.next_region_var_in_universe(universe, span).into(),
            GenericArgKind::Type(_) => self.next_ty_var_in_universe(universe, span).into(),
            GenericArgKind::Const(_) => self.next_const_var_in_universe(universe, span).into(),
        }
    }

    fn leak_check(
        &self,
        max_input_universe: rustc_type_ir::UniverseIndex,
    ) -> Result<(), NoSolution> {
        InferCtxt::leak_check(&self.0, max_input_universe, None).map_err(|_| NoSolution)
    }

    /// rustc's `try_evaluate_const`: `Ok(None)` for a constant whose arguments still name a
    /// generic parameter, an inference variable or a placeholder, an error constant when
    /// evaluating fails, and `Err` only when normalizing the constant's type does.
    fn evaluate_const<E: Debug>(
        &self,
        param_env: ParamEnv<'db>,
        alias_const: AliasConst<'db>,
        normalize_ty: impl FnOnce(Unnormalized<'db, Ty<'db>>) -> Result<Ty<'db>, E>,
    ) -> Result<Option<Const<'db>>, E> {
        let interner = self.interner;
        let db = interner.db;
        let alias_const = self.deeply_resolve_ignoring_regions(alias_const);
        let evaluated = match alias_const.kind {
            AliasConstKind::Projection { def_id: super::TraitAssocConstId(konst) }
            | AliasConstKind::InherentImpl { def_id: super::InherentAssocConstId(konst) }
            | AliasConstKind::Free { def_id: super::FreeConstAliasId(konst) } => {
                if has_generics_or_infers(alias_const.args) {
                    return Ok(None);
                }
                db.const_eval(konst, alias_const.args, None)
            }
            AliasConstKind::Anon { def_id } => {
                let anon = def_id.0;
                let loc = anon.loc(db);
                if loc.allow_using_generic_params {
                    // An array repeat count may name the owner's generic parameters without
                    // depending on them; rustc evaluates it with identity arguments in its
                    // own environment, whatever arguments it is used with.
                    let owner = loc.owner.generic_def(db);
                    let args = GenericArgs::identity_for_item(interner, owner.into());
                    db.anon_const_eval(anon, args, None)
                } else {
                    if has_generics_or_infers(alias_const.args) {
                        return Ok(None);
                    }
                    db.anon_const_eval(anon, alias_const.args, None)
                }
            }
            AliasConstKind::InherentSelf { def_id } => panic!(
                "{def_id:?}: an inherent associated constant in `Self` form reached const \
                 evaluation; the solver only evaluates its impl form"
            ),
        };
        let allocation = match evaluated {
            Ok(allocation) => allocation,
            Err(ConstEvalError::MirLowerError(MirLowerError::GenericArgNotProvided(..))) => {
                return Ok(None);
            }
            Err(_) => return Ok(Some(Const::new_error(interner, ErrorGuaranteed))),
        };
        let ty = normalize_ty(alias_const.type_of(interner))?;
        let env = ParamEnvAndCrate { param_env, krate: interner.expect_crate() };
        let konst = if ty == allocation.ty {
            Const::new_from_allocation(interner, &allocation, env)
        } else {
            let allocation = AllocationData {
                ty,
                memory: allocation.memory.clone(), // clone: only when normalizing changed the type
                memory_map: allocation.memory_map.clone(), // clone: only when normalizing changed the type
            };
            Const::new_from_allocation(interner, &allocation, env)
        };
        // rustc's `try_evaluate_const`: a constant with no value tree is an error here; it is
        // reported for what the constant is used as.
        Ok(Some(konst.unwrap_or_else(|_| Const::new_error(interner, ErrorGuaranteed))))
    }

    /// What `arg` needs to be well formed: the bounds of every struct, enum and union in it
    /// on its arguments, and the same of the types in it that are not known yet. A type under
    /// a `for<..>` it names is passed over, as rustc does. `None` is a type of which nothing
    /// is known.
    fn well_formed_goals(
        &self,
        param_env: ParamEnv<'db>,
        arg: <Self::Interner as rustc_type_ir::Interner>::Term,
    ) -> Option<Vec<Goal<'db, Predicate<'db>>>> {
        let interner = self.interner;
        let Some(ty) = arg.as_type() else { return Some(Vec::new()) }; // alloc: empty
        let ty = self.deeply_resolve_ignoring_regions(ty);
        if ty.is_ty_var() {
            return None;
        }
        let mut goals = Vec::new(); // alloc: `SolverDelegate::well_formed_goals` returns a `Vec`
        let free = |part: &Ty<'db>| !part.has_escaping_bound_vars();
        for part in ty.walk().filter_map(|it| it.as_type()).filter(free) {
            match part.kind() {
                TyKind::Infer(InferTy::TyVar(_)) => {
                    goals.push(Goal::new(interner, param_env, ClauseKind::WellFormed(part.into())));
                }
                TyKind::Adt(def, args) => {
                    let bounds = GenericPredicates::query_all(interner.db, def.def_id().into());
                    goals.extend(
                        bounds
                            .iter_instantiated(interner, args.as_slice())
                            .map(|bound| Goal::new(interner, param_env, bound.skip_norm_wip())),
                    );
                }
                _ => {}
            }
        }
        Some(goals)
    }

    /// rustc's: what `make_query_region_constraints` gives, each constraint once, visible to
    /// the leak check if any of its copies is.
    fn make_deduplicated_region_constraints(
        &self,
    ) -> Vec<(RegionConstraint<'db>, VisibleForLeakCheck)> {
        let mut constraints = Vec::new(); // alloc: the trait returns a `Vec`; none when empty
        self.make_deduplicated_region_constraints_into(&mut constraints);
        constraints
    }

    /// The same, appended to `into` (the solver's pooled list). Up to `SCANNED` distinct
    /// constraints are deduplicated by a scan of those appended, so a response allocates
    /// nothing once `into` has room; past that a map is built once.
    fn make_deduplicated_region_constraints_into(
        &self,
        into: &mut Vec<(RegionConstraint<'db>, VisibleForLeakCheck)>,
    ) {
        if !self.interner.regions() {
            return;
        }
        let base = into.len();
        let mut seen: Option<HashMap<RegionConstraint<'db>, usize>> = None;
        self.0.for_each_query_region_constraint(|constraint, visible| {
            if says_nothing(constraint) {
                return;
            }
            let found = match &mut seen {
                Some(seen) => match seen.entry(constraint) {
                    Entry::Occupied(at) => Some(*at.get()),
                    Entry::Vacant(at) => {
                        at.insert(into.len());
                        None
                    }
                },
                None => into[base..].iter().position(|it| it.0 == constraint).map(|at| base + at),
            };
            match found {
                Some(at) => {
                    let previous = &mut into[at].1;
                    *previous = previous.or(visible);
                }
                None => {
                    into.push((constraint, visible));
                    if seen.is_none() && into.len() - base > SCANNED {
                        seen = Some(
                            into[base..]
                                .iter()
                                .enumerate()
                                .map(|(at, it)| (it.0, base + at))
                                // alloc: once per response past `SCANNED` distinct constraints.
                                .collect(),
                        );
                    }
                }
            }
        });
    }

    fn instantiate_canonical<V>(
        &self,
        canonical: rustc_type_ir::Canonical<Self::Interner, V>,
        values: rustc_type_ir::CanonicalVarValues<Self::Interner>,
    ) -> V
    where
        V: rustc_type_ir::TypeFoldable<Self::Interner>,
    {
        canonical.instantiate(self.cx(), &values)
    }

    fn instantiate_canonical_var(
        &self,
        kind: CanonicalVarKind<'db>,
        span: Span,
        var_values: &[GenericArg<'db>],
        universe_map: impl Fn(rustc_type_ir::UniverseIndex) -> rustc_type_ir::UniverseIndex,
    ) -> GenericArg<'db> {
        self.0.instantiate_canonical_var(span, kind, var_values, universe_map)
    }

    fn add_item_bounds_for_hidden_type(
        &self,
        opaque_id: OpaqueTyIdWrapper<'_>,
        args: GenericArgs<'db>,
        param_env: ParamEnv<'db>,
        hidden_ty: Ty<'db>,
        goals: &mut Vec<Goal<'db, Predicate<'db>>>,
    ) {
        let interner = self.interner;
        // Require that the hidden type is well-formed. We have to
        // make sure we wf-check the hidden type to fix #114728.
        //
        // However, we don't check that all types are well-formed.
        // We only do so for types provided by the user or if they are
        // "used", e.g. for method selection.
        //
        // This means we never check the wf requirements of the hidden
        // type during MIR borrowck, causing us to infer the wrong
        // lifetime for its member constraints which then results in
        // unexpected region errors.
        goals.push(Goal::new(interner, param_env, ClauseKind::WellFormed(hidden_ty.into())));

        let replace_opaques_in = |clause: Clause<'db>| {
            fold_tys(interner, clause, |ty| match ty.kind() {
                // Replace all other mentions of the same opaque type with the hidden type,
                // as the bounds must hold on the hidden type after all.
                TyKind::Alias(
                    _,
                    AliasTy { kind: AliasTyKind::Opaque { def_id: def_id2 }, args: args2, .. },
                ) if opaque_id == def_id2 && args == args2 => hidden_ty,
                _ => ty,
            })
        };

        let item_bounds = opaque_id.0.predicates(interner.db);
        for predicate in item_bounds.iter_instantiated_copied(interner, args.as_slice()) {
            let predicate = replace_opaques_in(predicate.skip_norm_wip());

            // Require that the predicate holds for the concrete type.
            debug!(?predicate);
            goals.push(Goal::new(interner, param_env, predicate));
        }
    }

    fn fetch_eligible_assoc_item(
        &self,
        _goal_trait_ref: rustc_type_ir::TraitRef<Self::Interner>,
        trait_assoc_def_id: TraitAssocTermId,
        impl_id: AnyImplId,
    ) -> FetchEligibleAssocItemResponse<Self::Interner> {
        let AnyImplId::ImplId(impl_id) = impl_id else {
            // Builtin derive traits don't have type/consts assoc items.
            return FetchEligibleAssocItemResponse::Err(ErrorGuaranteed);
        };
        let db = self.0.interner.db;
        let impl_items = impl_id.impl_items(db);
        let id = match trait_assoc_def_id.0 {
            TermId::TypeAliasId(trait_assoc_id) => {
                let trait_assoc_data = TypeAliasSignature::of(db, trait_assoc_id);
                let type_of = |impl_id: hir_def::ImplId| {
                    impl_id.impl_items(db).items.iter().find_map(|(name, id)| match *id {
                        AssocItemId::TypeAliasId(id) if *name == trait_assoc_data.name => Some(id),
                        _ => None,
                    })
                };
                if type_of(impl_id).is_some()
                    && crate::specialization::impl_has_unconstrained_params(db, impl_id)
                {
                    return FetchEligibleAssocItemResponse::Err(ErrorGuaranteed);
                }
                type_of(impl_id)
                    // A specializing impl has the types of the impl it specializes.
                    .or_else(|| {
                        let ancestor =
                            crate::specialization::specialized_ancestor(db, impl_id, |it| {
                                type_of(it).is_some()
                            })?;
                        type_of(ancestor)
                    })
                    // rustc's `assoc_def`: the trait's item, with or without a default; the
                    // solver asks `has_item_definition` about one without.
                    .or(Some(trait_assoc_id))
                    .map(|def| ImplOrTraitAssocTermId(TermId::TypeAliasId(def)))
            }
            TermId::ConstId(trait_assoc_id) => {
                let trait_assoc_data = ConstSignature::of(self.0.interner.db, trait_assoc_id);
                let trait_assoc_name = trait_assoc_data
                    .name
                    .as_ref()
                    .expect("unnamed consts should not get passed to the solver");
                let in_impl =
                    impl_items.items.iter().find_map(|(impl_assoc_name, impl_assoc_id)| {
                        if let AssocItemId::ConstId(impl_assoc_id) = *impl_assoc_id
                            && impl_assoc_name == trait_assoc_name
                        {
                            Some(impl_assoc_id)
                        } else {
                            None
                        }
                    });
                if in_impl.is_some()
                    && crate::specialization::impl_has_unconstrained_params(db, impl_id)
                {
                    return FetchEligibleAssocItemResponse::Err(ErrorGuaranteed);
                }
                in_impl
                    .or(Some(trait_assoc_id))
                    .map(|def| ImplOrTraitAssocTermId(TermId::ConstId(def)))
            }
        };
        match id {
            Some(id) => FetchEligibleAssocItemResponse::Found(id),
            None => match self.typing_mode_raw() {
                TypingMode::ErasedNotCoherence(_) => {
                    FetchEligibleAssocItemResponse::NotFoundBecauseErased
                }
                typing_mode => {
                    FetchEligibleAssocItemResponse::NotFound(typing_mode.assert_not_erased())
                }
            },
        }
    }

    /// rustc answers with `rustc_transmute`'s layout analysis, which blaze has not ported:
    /// a `TransmuteFrom` goal stops here rather than holding unproven.
    fn is_transmutable(
        &self,
        src: Ty<'db>,
        dst: Ty<'db>,
        assume: Const<'db>,
    ) -> Result<Answer<Region<'db>, Ty<'db>>, NoSolution> {
        panic!(
            "`TransmuteFrom<{src:?}, {assume:?}>` for `{dst:?}`: blaze has no port of \
             rustc_transmute, so `#![feature(transmutability)]` goals cannot be proven"
        )
    }

    fn obtain_canonicalizer_state(&self) -> CanonicalizerState<Self::Interner> {
        mem::take(&mut self.canonicalizer_state.borrow_mut())
    }

    fn release_canonicalizer_state(&self, mut state: CanonicalizerState<Self::Interner>) {
        // Cleared, not freed: the next canonicalizer reuses its allocations.
        state.clear();
        *self.canonicalizer_state.borrow_mut() = state;
    }

    fn obtain_search_storage(&self) -> SearchStorage<Self::Interner> {
        recycle::search_storage()
    }

    fn release_search_storage(&self, storage: SearchStorage<Self::Interner>) {
        recycle::give_search_storage(storage);
    }

    fn emit_next_solver_overflow_fcw(&self, goal: Goal<'db, Predicate<'db>>, span: Span) {
        let goal = self.deeply_resolve_ignoring_regions(goal);
        let mut chain = OverflowedGoalChain {
            span,
            predicates: Vec::new(), // alloc: overflow report, error path
            recursion_limit: self.interner.recursion_limit().min(16),
        };
        let _ = self
            .with_disabled_next_solver_overflow_fcw(|| self.visit_proof_tree(goal, &mut chain));
        self.push_next_solver_overflow(NextSolverOverflow {
            goal: goal.predicate,
            span,
            // The first is `goal` itself.
            chain: chain.predicates.into_iter().skip(1).collect(), // alloc: overflow report, error path
        });
    }
}

/// Whether a constant's arguments still name a generic parameter, an inference variable or a
/// placeholder: evaluating it then waits, as rustc's `EvaluateConstErr::HasGenericsOrInfers`.
fn has_generics_or_infers(args: GenericArgs<'_>) -> bool {
    args.has_non_region_param() || args.has_non_region_infer() || args.has_non_region_placeholders()
}

/// rustc's chain of the goals an overflow went through: from each goal with one applicable
/// candidate, the nested goal that needed the most depth.
struct OverflowedGoalChain<'db> {
    span: Span,
    predicates: Vec<Predicate<'db>>,
    recursion_limit: usize,
}

impl<'db> ProofTreeVisitor<'db> for OverflowedGoalChain<'db> {
    type Result = ControlFlow<()>;

    fn span(&self) -> Span {
        self.span
    }

    fn config(&self) -> InspectConfig {
        InspectConfig { max_depth: self.recursion_limit }
    }

    fn visit_goal(&mut self, goal: &InspectGoal<'_, 'db>) -> Self::Result {
        self.predicates.push(goal.goal().predicate);
        if let Some(candidate) = goal.unique_applicable_candidate() {
            goal.infcx().probe(|_| {
                match candidate
                    .instantiate_nested_goals(self.span)
                    .into_iter()
                    .max_by_key(|nested| nested.required_depth())
                {
                    Some(deepest) => deepest.visit_with(self),
                    None => ControlFlow::Continue(()),
                }
            })?;
        }
        ControlFlow::Continue(())
    }

    fn on_recursion_limit(&mut self) -> Self::Result {
        ControlFlow::Break(())
    }
}

/// Whether what proving a goal needs of an error region, a lifetime rust-analyzer could not
/// lower, says nothing a caller asking for regions reads: it has no region of the body to be,
/// and only one between it and a placeholder marks that placeholder unknown. Answered, such a
/// need would make every coinductive cycle through the type it is in (an auto trait over a
/// recursive type) differ from its first provisional answer and run again, and the reruns of
/// nested cycles multiply.
/// Distinct region constraints a response deduplicates by scanning before it builds a map: a
/// scan of 16 rows is cheaper than hashing, and most responses have fewer.
const SCANNED: usize = 16;

fn says_nothing(constraint: RegionConstraint<'_>) -> bool {
    let unknown = |error: Region<'_>, other: Region<'_>| error.is_error() && !other.is_placeholder();
    match constraint {
        RegionConstraint::Outlives(outlives) => match outlives.0.as_region() {
            Some(longer) => unknown(longer, outlives.1) || unknown(outlives.1, longer),
            None => outlives.1.is_error(),
        },
        RegionConstraint::Eq(eq) => unknown(eq.0, eq.1) || unknown(eq.1, eq.0),
    }
}
