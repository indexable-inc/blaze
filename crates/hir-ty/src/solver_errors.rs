//! Handling of trait solver errors and converting them to errors `hir` can pass to `ide-diagnostics`.
//!
//! Note that we also have [`crate::next_solver::infer::errors`], which takes the raw [`NextSolverError`],
//! and converts it into [`FulfillmentError`] that contains more details.
//!
//! Every [`FulfillmentErrorCode`] gives a diagnostic: a definite error gives a [`SolverDiagnostic`]
//! (rustc's `report_fulfillment_error`), an ambiguity a `TypeMustBeKnown` (rustc's
//! `report_ambiguity_errors` and `maybe_report_ambiguity`, [`ambiguity_diagnostics`]), and a code
//! the next solver never makes an [`UnreportedSolverError`] naming it.
//!
//! [`NextSolverError`]: crate::next_solver::fulfill::NextSolverError

use hir_def::{GenericParamId, TraitId};
use intern::Symbol;
use macros::{TypeFoldable, TypeVisitable};
use rustc_type_ir::{
    BoundConstness, ClausePolarity, ClosureKind, ImplPolarity, InferTy, Interner, IsRigid,
    TypeFoldable as _, TypeVisitableExt,
    elaborate::elaborate,
    inherent::{GenericArg as _, GenericArgs as _, GenericsOf as _, IntoKind, SliceLike, Term as _},
    lang_items::SolverTraitLangItem,
    solve::TyOrConstInferVar,
};

use crate::{
    InferenceDiagnostic, Span,
    next_solver::{
        AliasTermKind, AnyImplId, ClauseKind, DbInterner, GenericArg, ParamEnv, PolyProjectionClause,
        PolyTraitClause, Predicate, PredicateKind, ProjectionClause, StoredGenericArg,
        StoredGenericArgs,
        StoredTraitRef, StoredTy, Term, TraitClause, TyKind, Unnormalized,
        infer::{
            BoundRegionConversionTime, InferCtxt,
            errors::{FulfillmentError, FulfillmentErrorCode},
            resolve::ReplaceInferWithError,
            select::{ImplSource, SelectionError},
            traits::{Obligation, ObligationCause, ObligationCauseCode, PredicateObligation},
        },
        obligation_ctxt::ObligationCtxt,
        util::clauses_as_obligations,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, TypeVisitable, TypeFoldable)]
pub struct SolverDiagnostic {
    pub span: Span,
    /// Why the goal was required (rustc's obligation cause code): the bound of a callee for one
    /// of its where clauses, a block's tail, and the rest.
    #[type_visitable(ignore)]
    pub cause: ObligationCauseCode,
    pub kind: SolverDiagnosticKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, TypeVisitable, TypeFoldable)]
pub enum SolverDiagnosticKind {
    TraitUnimplemented {
        trait_predicate: StoredTraitPredicate,
        parent_trait_predicates: Vec<StoredTraitPredicate>,
        /// For each of `parent_trait_predicates`, the impl that would prove it, whose where
        /// clause required the goal nearer the error, with that clause's index in the impl's
        /// [`crate::GenericPredicates::query_all`] (rustc's `ImplDerived` and its
        /// `impl_def_predicate_index`); `None` where that goal came from no impl.
        #[type_visitable(ignore)]
        parent_impls: Vec<Option<(AnyImplId, u32)>>,
    },
    /// A closure does not implement the `Fn*` or `AsyncFn*` trait asked of it because of its
    /// closure kind (E0525).
    ClosureKindMismatch {
        #[type_visitable(ignore)]
        expected: ClosureKind,
        #[type_visitable(ignore)]
        found: ClosureKind,
        #[type_visitable(ignore)]
        is_async: bool,
        /// The closure's type, `TyKind::Closure` or `TyKind::CoroutineClosure`: rustc points
        /// at the closure.
        closure: StoredTy,
    },
    /// A goal other than a trait's that does not hold: a `[const] Trait` bound
    /// (`report_host_effect_error`), a trait that is not dyn-compatible (E0038), a type that is
    /// not well-formed, or one rustc's `report_selection_error` calls a bug.
    GoalUnmet {
        goal: StoredGoal,
        /// For a `[const] Trait` or `const Trait` goal of a `const` trait, the impl that meets the
        /// trait goal when that impl is not `const` (`report_host_effect_error`).
        #[type_visitable(ignore)]
        non_const_impl: Option<AnyImplId>,
    },
    /// E0275: evaluating the goal overflowed (`report_overflow_no_abort`).
    Overflow {
        goal: StoredGoal,
        #[type_visitable(ignore)]
        suggest_increasing_limit: bool,
    },
    /// E0271: "type mismatch resolving `goal`", a projection goal (`report_projection_error`).
    ProjectionMismatch {
        goal: StoredGoal,
        /// What the goal's alias normalizes to where that is not the goal's term: rustc's
        /// "expected `term`, found `normalized`".
        normalized: Option<StoredGenericArg>,
        /// The user-written impl selection picks for the projection's trait ref, whose
        /// associated item rustc labels "expected this to be `term`" (when local and final).
        #[type_visitable(ignore)]
        impl_: Option<AnyImplId>,
    },
    /// E0308: a subtyping or coercion goal of two types that differ (`report_mismatched_types`).
    TypesDiffer { expected: StoredTy, found: StoredTy },
    /// "the constant `ct` is not of type `expected_ty`", which is `ct_ty`.
    ConstArgHasWrongType { ct: StoredGenericArg, ct_ty: StoredTy, expected_ty: StoredTy },
    /// "higher-ranked lifetime bound could not be satisfied" (`-Zassumptions-on-binders`).
    HigherRankedOutlives,
    /// A code only the old solver makes, named so the error says what blaze does not report.
    Unreported {
        #[type_visitable(ignore)]
        code: UnreportedSolverError,
    },
}

/// The fulfillment error codes the next solver never gives, which rustc reports from the old
/// solver's selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnreportedSolverError {
    Cycle,
    SignatureMismatch,
    TraitDynIncompatible,
    NotConstEvaluatable,
    SelectionOverflow,
    ConstEquate,
}

/// Which "type annotations needed" error an undecided type gives: rustc's
/// `TypeAnnotationNeeded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeAnnotationNeeded {
    /// E0282: the type itself is undecided.
    E0282,
    /// E0283: a trait bound on it could be met in more than one way.
    E0283,
    /// E0284: another goal on it could be.
    E0284,
}

/// A goal as error reporting prints it, its inference variables made `{unknown}`. A trait is
/// its id and its arguments, the self type first; an alias is the type or constant it names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, TypeVisitable, TypeFoldable)]
pub enum StoredGoal {
    /// `args[0]: Trait<args[1..]>`, or `!Trait` when negative.
    Trait {
        #[type_visitable(ignore)]
        trait_: TraitId,
        args: StoredGenericArgs,
        #[type_visitable(ignore)]
        polarity: ClausePolarity,
    },
    /// `args[0]: [const] Trait<args[1..]>` (`maybe_const`) or `const Trait`.
    HostEffect {
        #[type_visitable(ignore)]
        trait_: TraitId,
        args: StoredGenericArgs,
        #[type_visitable(ignore)]
        maybe_const: bool,
    },
    /// `alias == term`.
    Projection { alias: StoredGenericArg, term: StoredGenericArg },
    /// What `alias` normalizes to, `term` an inference variable.
    NormalizesTo { alias: StoredGenericArg, term: StoredGenericArg },
    WellFormed { term: StoredGenericArg },
    /// `a <: b`.
    Subtype { a: StoredTy, b: StoredTy },
    /// `a` coerces to `b`.
    Coerce { a: StoredTy, b: StoredTy },
    ConstEvaluatable { ct: StoredGenericArg },
    ConstArgHasType { ct: StoredGenericArg, ty: StoredTy },
    ConstEquate { a: StoredGenericArg, b: StoredGenericArg },
    DynCompatible {
        #[type_visitable(ignore)]
        trait_: TraitId,
    },
    /// `#[unstable_feature_bound(feature)]` of an impl: rustc gives the feature's error.
    UnstableFeature {
        #[type_visitable(ignore)]
        feature: Symbol,
    },
    /// `a: b`: a region or type outlives the region `b`.
    Outlives { a: StoredGenericArg, b: StoredGenericArg },
    /// rustc's `PredicateKind::Ambiguous`.
    Ambiguous,
}

impl StoredGoal {
    /// `predicate` with its bound variables as they are and its inference variables made
    /// errors, as hir prints an undecided type.
    pub fn new<'db>(interner: DbInterner<'db>, predicate: Predicate<'db>) -> Self {
        let predicate = predicate.fold_with(&mut ReplaceInferWithError::new(interner));
        let arg = |arg: GenericArg<'db>| arg.store();
        match predicate.kind().skip_binder() {
            PredicateKind::Clause(ClauseKind::Trait(data)) => StoredGoal::Trait {
                trait_: data.trait_ref.def_id.0,
                args: data.trait_ref.args.store(),
                polarity: data.polarity,
            },
            PredicateKind::Clause(ClauseKind::HostEffect(data)) => StoredGoal::HostEffect {
                trait_: data.trait_ref.def_id.0,
                args: data.trait_ref.args.store(),
                maybe_const: matches!(data.constness, BoundConstness::Maybe),
            },
            PredicateKind::Clause(ClauseKind::Projection(data)) => StoredGoal::Projection {
                alias: arg(data.projection_term.to_term(interner, IsRigid::No).into()),
                term: arg(data.term.into()),
            },
            PredicateKind::NormalizesTo(data) => StoredGoal::NormalizesTo {
                alias: arg(data.alias.to_term(interner, IsRigid::No).into()),
                term: arg(data.term.into()),
            },
            PredicateKind::Clause(ClauseKind::WellFormed(term)) => {
                StoredGoal::WellFormed { term: arg(term.into()) }
            }
            PredicateKind::Subtype(data) => StoredGoal::Subtype { a: data.a.store(), b: data.b.store() },
            PredicateKind::Coerce(data) => StoredGoal::Coerce { a: data.a.store(), b: data.b.store() },
            PredicateKind::Clause(ClauseKind::ConstEvaluatable(ct)) => {
                StoredGoal::ConstEvaluatable { ct: arg(ct.into()) }
            }
            PredicateKind::Clause(ClauseKind::ConstArgHasType(ct, ty)) => {
                StoredGoal::ConstArgHasType { ct: arg(ct.into()), ty: ty.store() }
            }
            PredicateKind::ConstEquate(a, b) => StoredGoal::ConstEquate { a: arg(a.into()), b: arg(b.into()) },
            PredicateKind::DynCompatible(trait_) => StoredGoal::DynCompatible { trait_: trait_.0 },
            PredicateKind::Clause(ClauseKind::UnstableFeature(feature)) => {
                StoredGoal::UnstableFeature { feature: feature.0 }
            }
            PredicateKind::Clause(ClauseKind::RegionOutlives(outlives)) => {
                StoredGoal::Outlives { a: arg(outlives.0.into()), b: arg(outlives.1.into()) }
            }
            PredicateKind::Clause(ClauseKind::TypeOutlives(outlives)) => {
                StoredGoal::Outlives { a: arg(outlives.0.into()), b: arg(outlives.1.into()) }
            }
            PredicateKind::Ambiguous => StoredGoal::Ambiguous,
        }
    }
}

/// An `impl` or where clause that could meet an ambiguous trait goal: rustc's
/// `CandidateSource`, as `annotate_source_of_ambiguity` lists them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, TypeVisitable, TypeFoldable)]
pub enum AmbiguityCandidate {
    Impl(#[type_visitable(ignore)] AnyImplId),
    /// A where clause of the body's owner, `args[0]: trait_<args[1..]>`.
    ParamEnv {
        #[type_visitable(ignore)]
        trait_: TraitId,
        args: StoredGenericArgs,
    },
}

/// A goal of an ambiguity error and the candidates rustc lists for it: those that could apply
/// when there are two to nine of them and an argument of the trait is known, else none.
#[derive(Debug, Clone, PartialEq, Eq, Hash, TypeVisitable, TypeFoldable)]
pub struct AmbiguityNote {
    pub goal: StoredGoal,
    pub candidates: Vec<AmbiguityCandidate>,
}

/// What an ambiguity error (E0282, E0283, E0284 from a pending goal) reports: the goal, and
/// the goals ambiguous for the same inference variable at the same place, which rustc merges
/// into it as notes ("the type must also implement `Tr`", "cannot satisfy `..`").
#[derive(Debug, Clone, PartialEq, Eq, Hash, TypeVisitable, TypeFoldable)]
pub struct Ambiguity {
    pub primary: AmbiguityNote,
    pub related: Vec<AmbiguityNote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, TypeVisitable, TypeFoldable)]
pub struct StoredTraitPredicate {
    pub trait_ref: StoredTraitRef,
    pub polarity: ClausePolarity,
}

impl StoredTraitPredicate {
    #[inline]
    pub fn get<'db>(&'db self, interner: DbInterner<'db>) -> TraitClause<'db> {
        TraitClause { polarity: self.polarity, trait_ref: self.trait_ref.get(interner) }
    }
}

impl SolverDiagnostic {
    /// The diagnostic of a definite error. An ambiguity (`Ambiguity { overflow: None }`) gives
    /// none: [`ambiguity_diagnostics`] reports those, merged.
    pub fn from_fulfillment_error<'db>(
        infcx: &InferCtxt<'db>,
        error: &FulfillmentError<'db>,
    ) -> Option<Self> {
        let span = error.obligation.cause.span();
        if span.is_dummy() {
            return None;
        }

        let interner = infcx.interner;
        let goal = || {
            StoredGoal::new(interner, infcx.deeply_resolve_ignoring_regions(error.obligation.predicate))
        };
        let unreported = |code| SolverDiagnosticKind::Unreported { code };
        let kind = match &error.code {
            FulfillmentErrorCode::Select(SelectionError::Unimplemented) => {
                match error.obligation.predicate.kind().skip_binder() {
                    PredicateKind::Clause(ClauseKind::Trait(trait_pred)) => {
                        let poly_trait_pred = error.obligation.predicate.kind().rebind(trait_pred);
                        match closure_kind_mismatch(infcx, error, poly_trait_pred) {
                            Some(kind) => kind,
                            None => handle_trait_unimplemented(error, trait_pred)?,
                        }
                    }
                    // rustc's `span_delayed_bug`: another error explains it.
                    PredicateKind::Clause(ClauseKind::TypeOutlives(_)) => return None,
                    // A type that is not well-formed because of an error is already reported.
                    PredicateKind::Clause(ClauseKind::WellFormed(term))
                        if infcx.deeply_resolve_ignoring_regions(term).references_error() =>
                    {
                        return None;
                    }
                    _ => SolverDiagnosticKind::GoalUnmet {
                        goal: goal(),
                        non_const_impl: non_const_impl(infcx, &error.obligation),
                    },
                }
            }
            FulfillmentErrorCode::Select(SelectionError::ConstArgHasWrongType {
                ct,
                ct_ty,
                expected_ty,
            }) => SolverDiagnosticKind::ConstArgHasWrongType {
                ct: GenericArg::from(*ct).store(),
                ct_ty: ct_ty.store(),
                expected_ty: expected_ty.store(),
            },
            FulfillmentErrorCode::Select(SelectionError::SignatureMismatch(_)) => {
                unreported(UnreportedSolverError::SignatureMismatch)
            }
            FulfillmentErrorCode::Select(SelectionError::TraitDynIncompatible(_)) => {
                unreported(UnreportedSolverError::TraitDynIncompatible)
            }
            FulfillmentErrorCode::Select(SelectionError::NotConstEvaluatable(_)) => {
                unreported(UnreportedSolverError::NotConstEvaluatable)
            }
            FulfillmentErrorCode::Select(SelectionError::Overflow(_)) => {
                unreported(UnreportedSolverError::SelectionOverflow)
            }
            FulfillmentErrorCode::Project(_) => SolverDiagnosticKind::ProjectionMismatch {
                goal: goal(),
                normalized: projection_normalized(infcx, &error.obligation),
                impl_: projection_impl(infcx, &error.obligation),
            },
            FulfillmentErrorCode::Outlives => SolverDiagnosticKind::HigherRankedOutlives,
            FulfillmentErrorCode::Subtype(expected_found, _) => SolverDiagnosticKind::TypesDiffer {
                expected: infcx.deeply_resolve_ignoring_regions(expected_found.expected).store(),
                found: infcx.deeply_resolve_ignoring_regions(expected_found.found).store(),
            },
            FulfillmentErrorCode::ConstEquate(..) => unreported(UnreportedSolverError::ConstEquate),
            FulfillmentErrorCode::Cycle(_) => unreported(UnreportedSolverError::Cycle),
            &FulfillmentErrorCode::Ambiguity { overflow: Some(suggest_increasing_limit) } => {
                SolverDiagnosticKind::Overflow { goal: goal(), suggest_increasing_limit }
            }
            FulfillmentErrorCode::Ambiguity { overflow: None } => return None,
        };
        Some(SolverDiagnostic { span, cause: error.obligation.cause.code(), kind })
    }
}

/// What the alias of a projection goal normalizes to, when that differs from the goal's term:
/// rustc's `report_projection_error`, which normalizes the alias, takes the nested goals it can,
/// and keeps the normalized term only when equating it with the goal's term fails.
fn projection_normalized<'db>(
    infcx: &InferCtxt<'db>,
    obligation: &PredicateObligation<'db>,
) -> Option<StoredGenericArg> {
    let interner = infcx.interner;
    let kind = obligation.predicate.kind();
    let PredicateKind::Clause(ClauseKind::Projection(data)) = kind.skip_binder() else {
        return None;
    };
    infcx.probe(|_| {
        let data = infcx.instantiate_binder_with_fresh_vars(
            obligation.cause.span(),
            BoundRegionConversionTime::HigherRankedType,
            kind.rebind(data),
        );
        let mut ocx = ObligationCtxt::new(infcx);
        let (cause, param_env) = (&obligation.cause, obligation.param_env);
        let alias = data.projection_term.to_term(interner, IsRigid::No);
        let normalized = ocx.structurally_normalize_term(cause, param_env, alias).ok()?;
        let _ = ocx.try_evaluate_obligations();
        ocx.eq(cause, param_env, data.term, normalized).is_err().then(|| {
            let normalized = infcx.deeply_resolve_ignoring_regions(normalized);
            GenericArg::from(normalized).fold_with(&mut ReplaceInferWithError::new(interner)).store()
        })
    })
}

/// The user-written impl selection picks for a projection goal's trait ref: rustc's
/// `report_projection_error`, which selects it with the goal's bound variables as placeholders
/// to label that impl's associated item. `None` for an alias that is not a trait's associated
/// item, and for a selection that is ambiguous, fails or is not a user-written impl.
fn projection_impl<'db>(
    infcx: &InferCtxt<'db>,
    obligation: &PredicateObligation<'db>,
) -> Option<AnyImplId> {
    let interner = infcx.interner;
    let kind = infcx.deeply_resolve_ignoring_regions(obligation.predicate).kind();
    let PredicateKind::Clause(ClauseKind::Projection(data)) = kind.skip_binder() else {
        return None;
    };
    if !matches!(
        data.projection_term.kind(interner),
        AliasTermKind::ProjectionTy { .. } | AliasTermKind::ProjectionConst { .. }
    ) {
        return None;
    }
    infcx.probe(|_| {
        let trait_ref = infcx
            .enter_forall_and_leak_universe(kind.rebind(data.projection_term.trait_ref(interner)));
        let obligation =
            Obligation::new(interner, ObligationCause::dummy(), obligation.param_env, trait_ref);
        match infcx.select(&obligation) {
            Ok(Some(ImplSource::UserDefined(impl_))) => Some(impl_.impl_def_id),
            _ => None,
        }
    })
}

/// The impl selection picks for a `[const] Trait` or `const Trait` goal's trait predicate, when
/// the trait is `const` and the impl is not: rustc's `report_host_effect_error`, which then says
/// "trait `Trait` is implemented but not `const`" or suggests making the impl `const`.
fn non_const_impl<'db>(
    infcx: &InferCtxt<'db>,
    obligation: &PredicateObligation<'db>,
) -> Option<AnyImplId> {
    let interner = infcx.interner;
    let kind = infcx.deeply_resolve_ignoring_regions(obligation.predicate).kind();
    let PredicateKind::Clause(ClauseKind::HostEffect(data)) = kind.skip_binder() else {
        return None;
    };
    if !crate::effects::trait_is_const(interner.db(), data.trait_ref.def_id.0) {
        return None;
    }
    infcx.probe(|_| {
        let trait_ref = infcx.instantiate_binder_with_fresh_vars(
            obligation.cause.span(),
            BoundRegionConversionTime::HigherRankedType,
            kind.rebind(data.trait_ref),
        );
        let obligation =
            Obligation::new(interner, ObligationCause::dummy(), obligation.param_env, trait_ref);
        match infcx.select(&obligation) {
            Ok(Some(ImplSource::UserDefined(impl_)))
                if !crate::effects::impl_is_const(interner.db(), impl_.impl_def_id) =>
            {
                Some(impl_.impl_def_id)
            }
            _ => None,
        }
    })
}

/// rustc's `emit_specialized_closure_kind_error`: the closure kind of a closure that does not
/// implement a `Fn*` or `AsyncFn*` trait, when that is why.
fn closure_kind_mismatch<'db>(
    infcx: &InferCtxt<'db>,
    error: &FulfillmentError<'db>,
    trait_pred: PolyTraitClause<'db>,
) -> Option<SolverDiagnosticKind> {
    let interner = infcx.interner;
    let mut trait_pred = infcx.deeply_resolve_ignoring_regions(trait_pred);
    // An `AsyncFnKindHelper` goal comes from the `AsyncFn*` goal it is required by.
    if interner.is_trait_lang_item(trait_pred.def_id(), SolverTraitLangItem::AsyncFnKindHelper)
        && let Some(parent) =
            error.parent_trait_obligations.last().and_then(|parent| parent.as_trait_clause())
    {
        trait_pred = infcx.deeply_resolve_ignoring_regions(parent);
    }

    let original_self_ty = trait_pred.self_ty().skip_binder();
    let self_ty = match original_self_ty.kind() {
        TyKind::Ref(_, inner, _) if matches!(inner.kind(), TyKind::Closure(..)) => inner,
        _ => original_self_ty,
    };

    let (expected, is_async) = match interner.as_trait_lang_item(trait_pred.def_id())? {
        SolverTraitLangItem::Fn => (ClosureKind::Fn, false),
        SolverTraitLangItem::FnMut => (ClosureKind::FnMut, false),
        SolverTraitLangItem::FnOnce => (ClosureKind::FnOnce, false),
        SolverTraitLangItem::AsyncFn => (ClosureKind::Fn, true),
        SolverTraitLangItem::AsyncFnMut => (ClosureKind::FnMut, true),
        SolverTraitLangItem::AsyncFnOnce => (ClosureKind::FnOnce, true),
        _ => return None,
    };

    let found_args = match self_ty.kind() {
        TyKind::Closure(_, args) => {
            args.as_closure().sig().map_bound(|sig| sig.inputs().as_slice()[0])
        }
        TyKind::CoroutineClosure(_, args) => args
            .as_coroutine_closure()
            .coroutine_closure_sig()
            .map_bound(|sig| sig.tupled_inputs_ty),
        _ => return None,
    };
    let expected_args = trait_pred.map_bound(|trait_pred| trait_pred.trait_ref.args.type_at(1));
    // With arguments of other types, the closure is not what was asked for at all.
    if infcx.enter_forall(found_args, |found_args| {
        infcx.enter_forall(expected_args, |expected_args| {
            !infcx.can_eq(error.obligation.param_env, expected_args, found_args)
        })
    }) {
        return None;
    }

    let found = infcx.closure_kind(self_ty)?;
    (!found.extends(expected)).then(|| SolverDiagnosticKind::ClosureKindMismatch {
        expected,
        found,
        is_async,
        closure: self_ty.store(),
    })
}

fn handle_trait_unimplemented<'db>(
    error: &FulfillmentError<'db>,
    trait_pred: TraitClause<'db>,
) -> Option<SolverDiagnosticKind> {
    let trait_predicate = StoredTraitPredicate {
        trait_ref: StoredTraitRef::new(trait_pred.trait_ref),
        polarity: trait_pred.polarity,
    };

    let (mut parent_trait_predicates, mut parent_impls): (Vec<_>, Vec<_>) = error
        .parent_trait_obligations
        .iter()
        .zip(&error.parent_impls)
        .filter_map(|(predicate, &parent_impl)| Some((predicate.as_trait_clause()?, parent_impl)))
        .map(|(trait_predicate, parent_impl)| {
            let trait_predicate = trait_predicate.skip_binder();
            let predicate = StoredTraitPredicate {
                trait_ref: StoredTraitRef::new(trait_predicate.trait_ref),
                polarity: trait_predicate.polarity,
            };
            (predicate, parent_impl)
        })
        .unzip(); // alloc: error path, the diagnostic's own lists
    parent_trait_predicates.reverse();
    parent_impls.reverse();

    Some(SolverDiagnosticKind::TraitUnimplemented {
        trait_predicate,
        parent_trait_predicates,
        parent_impls,
    })
}

/// rustc's `ErrorSortKey`: sizedness and well-formedness goals last, so that the error names
/// the goal that says most.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum ErrorSortKey {
    OtherKind,
    SizedTrait,
    MetaSizedTrait,
    PointeeSizedTrait,
    Coerce,
    WellFormed,
}

fn sizedness<'db>(interner: DbInterner<'db>, predicate: Predicate<'db>) -> Option<SolverTraitLangItem> {
    let trait_ = match predicate.kind().skip_binder() {
        PredicateKind::Clause(ClauseKind::Trait(data)) => data.trait_ref.def_id,
        PredicateKind::Clause(ClauseKind::HostEffect(data)) => data.trait_ref.def_id,
        _ => return None,
    };
    [SolverTraitLangItem::Sized, SolverTraitLangItem::MetaSized, SolverTraitLangItem::PointeeSized]
        .into_iter()
        .find(|&item| interner.is_trait_lang_item(trait_, item))
}

fn sort_key<'db>(interner: DbInterner<'db>, error: &FulfillmentError<'db>) -> ErrorSortKey {
    match sizedness(interner, error.obligation.predicate) {
        Some(SolverTraitLangItem::Sized) => return ErrorSortKey::SizedTrait,
        Some(SolverTraitLangItem::MetaSized) => return ErrorSortKey::MetaSizedTrait,
        Some(_) => return ErrorSortKey::PointeeSizedTrait,
        None => {}
    }
    match error.obligation.predicate.kind().skip_binder() {
        PredicateKind::Coerce(_) => ErrorSortKey::Coerce,
        PredicateKind::Clause(ClauseKind::WellFormed(_)) => ErrorSortKey::WellFormed,
        _ => ErrorSortKey::OtherKind,
    }
}

/// rustc's `ambiguity_term`: the term an ambiguity error blames, the first still holding an
/// inference variable.
/// The origin of the first variable in `term` made for a generic parameter (rustc's
/// `TypeVariableOrigin`, which `extract_inference_diagnostics_data` names when `term` is that
/// variable and `FindInferSourceVisitor` turns into the turbofish suggestion when it is inside):
/// where it was made and the parameter, a type parameter other than a trait's `Self` or a const
/// parameter.
pub(crate) fn variable_origin<'db>(
    infcx: &InferCtxt<'db>,
    term: Term<'db>,
) -> Option<(Span, GenericParamId)> {
    GenericArg::from(term).walk().find_map(|arg| {
        named::variable_param(infcx, TyOrConstInferVar::maybe_from_generic_arg::<DbInterner<'db>>(arg)?)
    })
}

fn ambiguity_term<'db>(predicate: Predicate<'db>) -> Option<Term<'db>> {
    match predicate.kind().skip_binder() {
        PredicateKind::Clause(ClauseKind::Trait(data)) => data
            .trait_ref
            .args
            .iter()
            .filter_map(|arg| arg.as_term())
            .find(|term| term.has_non_region_infer()),
        PredicateKind::Clause(ClauseKind::Projection(data)) => data
            .projection_term
            .args
            .iter()
            .filter_map(|arg| arg.as_term())
            .chain([data.term])
            .find(|term| term.has_non_region_infer()),
        PredicateKind::Clause(ClauseKind::WellFormed(term)) => Some(term),
        PredicateKind::Clause(ClauseKind::ConstEvaluatable(ct)) => {
            GenericArg::from(ct).walk().filter_map(|arg| arg.as_term()).find(|term| term.is_infer())
        }
        PredicateKind::Clause(ClauseKind::ConstArgHasType(ct, _)) => Some(ct.into()),
        PredicateKind::Subtype(data) => Some(data.a.into()),
        PredicateKind::NormalizesTo(data) if data.term.is_infer() => Some(data.term),
        _ => None,
    }
}

/// rustc's `can_match_trait`.
fn can_match_trait<'db>(
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    goal: TraitClause<'db>,
    assumption: PolyTraitClause<'db>,
) -> bool {
    if goal.polarity != assumption.skip_binder().polarity {
        return false;
    }
    let assumption = infcx.instantiate_binder_with_fresh_vars(
        Span::Dummy,
        BoundRegionConversionTime::HigherRankedType,
        assumption,
    );
    infcx.can_eq(param_env, goal.trait_ref, assumption.trait_ref)
}

/// rustc's `can_match_projection`, in an empty environment as rustc's.
fn can_match_projection<'db>(
    infcx: &InferCtxt<'db>,
    goal: ProjectionClause<'db>,
    assumption: PolyProjectionClause<'db>,
) -> bool {
    let assumption = infcx.instantiate_binder_with_fresh_vars(
        Span::Dummy,
        BoundRegionConversionTime::HigherRankedType,
        assumption,
    );
    let param_env = ParamEnv::empty();
    infcx.can_eq(param_env, goal.projection_term, assumption.projection_term)
        && infcx.can_eq(param_env, goal.term, assumption.term)
}

/// rustc's `error_implies`: whether `cond` holding means `error` does, `error` being a goal
/// that `cond` elaborates to.
fn error_implies<'db>(
    infcx: &InferCtxt<'db>,
    cond: &PredicateObligation<'db>,
    error: &PredicateObligation<'db>,
) -> bool {
    if cond.param_env != error.param_env {
        return false;
    }
    if cond.predicate == error.predicate {
        return true;
    }
    let interner = infcx.interner;
    let param_env = error.param_env;
    if let Some(error) = error.predicate.as_trait_clause() {
        infcx.enter_forall(error, |error| {
            elaborate(interner, [cond.predicate])
                .filter_map(|implied| implied.as_trait_clause())
                .any(|implied| can_match_trait(infcx, param_env, error, implied))
        })
    } else if let Some(error) = error.predicate.as_projection_clause() {
        infcx.enter_forall(error, |error| {
            elaborate(interner, [cond.predicate])
                .filter_map(|implied| implied.as_projection_clause())
                .any(|implied| can_match_projection(infcx, error, implied))
        })
    } else {
        false
    }
}

/// rustc's `compute_applicable_impls_for_diagnostics`: the `impl`s whose header and where
/// clauses could meet `trait_pred`, then the where clauses of the body's owner that could.
fn applicable_candidates<'db>(
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    trait_pred: PolyTraitClause<'db>,
) -> Vec<AmbiguityCandidate> {
    let interner = infcx.interner;
    let polarity = trait_pred.skip_binder().polarity;
    let impl_may_apply = |impl_: AnyImplId| {
        infcx.probe(|_| {
            infcx.enter_forall(trait_pred, |placeholder| {
                let mut ocx = ObligationCtxt::new(infcx);
                let impl_args = infcx.fresh_args_for_item(Span::Dummy, impl_.into());
                let impl_trait_ref =
                    interner.impl_trait_ref(impl_).instantiate(interner, impl_args).skip_norm_wip();
                if ocx
                    .eq(&ObligationCause::dummy(), param_env, placeholder.trait_ref, impl_trait_ref)
                    .is_err()
                {
                    return false;
                }
                match (interner.impl_polarity(impl_), polarity) {
                    (ImplPolarity::Positive, ClausePolarity::Positive)
                    | (ImplPolarity::Negative, ClausePolarity::Negative) => {}
                    _ => return false,
                }
                ocx.register_obligations(clauses_as_obligations(
                    interner
                        .clauses_of(impl_.into())
                        .iter_instantiated(interner, impl_args.as_slice())
                        .map(Unnormalized::skip_norm_wip),
                    ObligationCause::dummy(),
                    param_env,
                ));
                ocx.try_evaluate_obligations().is_empty()
            })
        })
    };
    let param_env_may_apply = |clause: PolyTraitClause<'db>| {
        infcx.probe(|_| {
            infcx.enter_forall(trait_pred, |placeholder| {
                let mut ocx = ObligationCtxt::new(infcx);
                let clause = infcx.instantiate_binder_with_fresh_vars(
                    Span::Dummy,
                    BoundRegionConversionTime::HigherRankedType,
                    clause,
                );
                if ocx
                    .eq(&ObligationCause::dummy(), param_env, placeholder.trait_ref, clause.trait_ref)
                    .is_err()
                {
                    return false;
                }
                ocx.try_evaluate_obligations().is_empty()
            })
        })
    };

    let mut candidates = Vec::new(); // alloc: error path
    interner.for_each_relevant_impl(trait_pred.skip_binder().trait_ref, |impl_| {
        if impl_may_apply(impl_) {
            candidates.push(AmbiguityCandidate::Impl(impl_));
        }
    });
    for clause in param_env.clauses.iter() {
        let kind = clause.kind();
        if let ClauseKind::Trait(data) = kind.skip_binder()
            && data.trait_ref.def_id == trait_pred.skip_binder().trait_ref.def_id
            && param_env_may_apply(kind.rebind(data))
        {
            candidates.push(AmbiguityCandidate::ParamEnv {
                trait_: data.trait_ref.def_id.0,
                args: data.trait_ref.args.store(),
            });
        }
    }
    candidates
}

/// rustc's `applicable_impls_to_mention`: the candidates worth listing, or `None` when the
/// error should describe the bound another way.
fn applicable_impls_to_mention<'db>(
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    trait_pred: PolyTraitClause<'db>,
) -> Option<Vec<AmbiguityCandidate>> {
    let interner = infcx.interner;
    let mut candidates = applicable_candidates(infcx, param_env, trait_pred);
    let has_known_type = trait_pred.skip_binder().trait_ref.args.iter().filter_map(|arg| arg.as_type()).any(|ty| {
        !matches!(ty.kind(), TyKind::Infer(InferTy::TyVar(_) | InferTy::IntVar(_) | InferTy::FloatVar(_)))
    });
    let is_concrete = |candidate: &AmbiguityCandidate| match *candidate {
        AmbiguityCandidate::Impl(impl_) => interner.generics_of(impl_.into()).count() == 0,
        AmbiguityCandidate::ParamEnv { .. } => true,
    };
    // Of many candidates, only those without parameters are likely what was meant.
    if candidates.len() > 5 && !candidates.iter().all(is_concrete) {
        candidates.retain(is_concrete);
    }
    (candidates.len() > 1 && candidates.len() < 10 && has_known_type).then_some(candidates)
}

/// rustc's `report_fulfillment_errors` for the ambiguities left when inference is done:
/// sorted so the goal that says most comes first, a goal another at its place implies
/// dropped, and the goals blaming the same inference variable at the same place merged into
/// one error. `tainted`: the body has an error already, so most ambiguities say nothing more.
/// The errors come in the order the goals were registered.
pub(crate) fn ambiguity_diagnostics<'db>(
    infcx: &InferCtxt<'db>,
    mut errors: Vec<FulfillmentError<'db>>,
    mut tainted: bool,
) -> Vec<InferenceDiagnostic> {
    let interner = infcx.interner;
    errors.retain(|error| matches!(error.code, FulfillmentErrorCode::Ambiguity { overflow: None }));
    errors.sort_by_key(|error| sort_key(interner, error));

    let mut is_suppressed = vec![false; errors.len()]; // alloc: error path
    for index in 0..errors.len() {
        let span = errors[index].obligation.cause.span();
        for other in 0..errors.len() {
            if is_suppressed[other] || errors[other].obligation.cause.span() != span {
                continue;
            }
            let (error, cond) = (&errors[index].obligation, &errors[other].obligation);
            if error_implies(infcx, cond, error) && !(other >= index && error_implies(infcx, error, cond)) {
                is_suppressed[index] = true;
                break;
            }
        }
    }

    let infer_var = |error: &FulfillmentError<'db>| {
        ambiguity_term(infcx.deeply_resolve_ignoring_regions(error.obligation.predicate))
            .and_then(|term| {
                GenericArg::from(term).walk().find_map(TyOrConstInferVar::maybe_from_generic_arg::<DbInterner<'db>>)
            })
            .map(|var| match var {
                TyOrConstInferVar::Ty(vid) => TyOrConstInferVar::Ty(infcx.sub_unification_table_root_var(vid)),
                other => other,
            })
    };
    let infer_vars: Vec<_> = errors.iter().map(infer_var).collect(); // alloc: error path
    // Only goals a note can state merge: a trait's other than sizedness, and a projection's.
    let merges = |other: usize, span: Span| {
        let error = &errors[other].obligation;
        error.cause.span() == span
            && match error.predicate.kind().skip_binder() {
                PredicateKind::Clause(ClauseKind::Trait(_)) => sizedness(interner, error.predicate).is_none(),
                PredicateKind::Clause(ClauseKind::Projection(_)) => true,
                _ => false,
            }
    };

    let mut merged = vec![false; errors.len()]; // alloc: error path
    let mut reported_as_primary = vec![false; errors.len()]; // alloc: error path
    let mut diagnostics = Vec::new(); // alloc: error path
    for (index, error) in errors.iter().enumerate() {
        if is_suppressed[index] || error.obligation.predicate.references_error() {
            continue;
        }
        if !merged[index] {
            let span = error.obligation.cause.span();
            let group: Vec<usize> = match infer_vars[index] {
                // alloc: error path
                Some(var) => (0..errors.len()).filter(|&other| other != index && infer_vars[other] == Some(var)).collect(),
                None => Vec::new(), // alloc: none, empty
            };
            let related: Vec<&FulfillmentError<'db>> = group
                .iter()
                .copied()
                .filter(|&other| {
                    merges(other, span)
                        && !is_suppressed[other]
                        && !errors[other].obligation.predicate.references_error()
                        && !reported_as_primary[other]
                })
                .map(|other| &errors[other])
                .collect(); // alloc: error path
            diagnostics.extend(report_ambiguity(infcx, error, &related, tainted));
            for &other in &group {
                if merges(other, span) {
                    merged[other] = true;
                }
            }
            reported_as_primary[index] = true;
        }
        tainted = true;
    }
    diagnostics
}

/// rustc's `maybe_report_ambiguity`: the error of one ambiguous goal, with the goals merged
/// into it, or none when an earlier error explains it.
fn report_ambiguity<'db>(
    infcx: &InferCtxt<'db>,
    error: &FulfillmentError<'db>,
    related: &[&FulfillmentError<'db>],
    tainted: bool,
) -> Option<InferenceDiagnostic> {
    let interner = infcx.interner;
    let obligation = &error.obligation;
    let span = obligation.cause.span();
    if span.is_dummy() {
        return None;
    }
    let predicate = infcx.deeply_resolve_ignoring_regions(obligation.predicate);
    let term = ambiguity_term(predicate);
    let diagnostic = |kind, top_term: Option<Term<'db>>, candidates, related| {
        let origin = top_term.and_then(|term| variable_origin(infcx, term));
        // alloc: error path, a reported diagnostic's term
        let named = top_term.map(|term| Box::new(NamedTerm::new(infcx, term)));
        let top_term = top_term.map(|term| {
            GenericArg::from(term).fold_with(&mut ReplaceInferWithError::new(interner)).store()
        });
        InferenceDiagnostic::TypeMustBeKnown {
            at_point: span,
            top_term,
            kind,
            // alloc: error path
            ambiguity: Some(Box::new(Ambiguity {
                primary: AmbiguityNote { goal: StoredGoal::new(interner, predicate), candidates },
                related,
            })),
            origin,
            named,
            cause: obligation.cause.code(),
        }
    };
    let (kind, top_term, candidates) = match predicate.kind().skip_binder() {
        PredicateKind::Clause(ClauseKind::Trait(data)) => {
            if predicate.references_error() {
                return None;
            }
            // Of an undecided type that must be sized, the type is what is undecided.
            if matches!(
                sizedness(interner, predicate),
                Some(SolverTraitLangItem::Sized | SolverTraitLangItem::MetaSized)
            ) {
                if tainted {
                    return None;
                }
                let self_ty = data.trait_ref.self_ty();
                // alloc: none, empty
                return Some(diagnostic(TypeAnnotationNeeded::E0282, Some(self_ty.into()), Vec::new(), Vec::new()));
            }
            let candidates = applicable_impls_to_mention(infcx, obligation.param_env, predicate.kind().rebind(data));
            match candidates {
                // Two where clauses or impls equal but for regions: not worth a second error.
                Some(_) if tainted && term.is_none() => return None,
                Some(candidates) => (TypeAnnotationNeeded::E0283, term, candidates),
                None if tainted => return None,
                None => (TypeAnnotationNeeded::E0283, term, Vec::new()), // alloc: none, empty
            }
        }
        PredicateKind::Clause(ClauseKind::WellFormed(_)) | PredicateKind::Subtype(_) => {
            if predicate.references_error() || tainted {
                return None;
            }
            (TypeAnnotationNeeded::E0282, term, Vec::new()) // alloc: none, empty
        }
        PredicateKind::Clause(ClauseKind::Projection(_) | ClauseKind::ConstEvaluatable(_)) => {
            if predicate.references_error() || tainted {
                return None;
            }
            (TypeAnnotationNeeded::E0284, term, Vec::new()) // alloc: none, empty
        }
        PredicateKind::Clause(ClauseKind::ConstArgHasType(..)) => {
            (TypeAnnotationNeeded::E0284, term, Vec::new()) // alloc: none, empty
        }
        // "cannot normalize `alias`", and every other goal's "cannot satisfy `goal`".
        _ => {
            if tainted {
                return None;
            }
            (TypeAnnotationNeeded::E0284, None, Vec::new()) // alloc: none, empty
        }
    };

    // The merged goals, but those rustc's notes leave out: bookkeeping goals and goals that
    // print as one already noted.
    let mut mentioned = vec![StoredGoal::new(interner, predicate)]; // alloc: error path
    let mut notes = Vec::new(); // alloc: error path
    for related in related {
        let related_pred = infcx.deeply_resolve_ignoring_regions(related.obligation.predicate);
        let goal = StoredGoal::new(interner, related_pred);
        if mentioned.contains(&goal) {
            continue;
        }
        mentioned.push(goal.clone()); // clone: error path
        let candidates = match related_pred.kind().skip_binder() {
            PredicateKind::Clause(ClauseKind::Trait(data)) if sizedness(interner, related_pred).is_none() => {
                if matches!(data.self_ty().kind(), TyKind::Infer(_)) || tainted {
                    Vec::new() // alloc: none, empty
                } else {
                    applicable_impls_to_mention(infcx, related.obligation.param_env, related_pred.kind().rebind(data))
                        .unwrap_or_default()
                }
            }
            PredicateKind::Clause(ClauseKind::Projection(_)) => Vec::new(), // alloc: none, empty
            _ => continue,
        };
        notes.push(AmbiguityNote { goal, candidates });
    }
    Some(diagnostic(kind, top_term, candidates, notes))
}

mod named;
#[cfg(test)]
mod tests;

pub use named::NamedTerm;
