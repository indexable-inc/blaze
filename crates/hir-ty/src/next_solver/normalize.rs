use rustc_next_trait_solver::{
    normalize::{NormalizationFolder, NormalizationWasAmbiguous},
    placeholder::BoundVarReplacer,
    solve::SolverDelegateEvalExt,
};
use rustc_type_ir::{
    Flags, Interner, IsRigid, TypeFoldable, TypeFolder, TypeSuperFoldable, TypeVisitableExt,
    UniverseIndex,
    inherent::{IntoKind, Term as _},
    solve::{Certainty, NoSolution},
};

use crate::next_solver::{
    AliasTerm, Binder, Const, ConstKind, DbInterner, Goal, ParamEnv, Predicate, ProjectionClause,
    SolverContext, Term, Ty, TyKind,
    fulfill::{FulfillmentCtxt, NextSolverError},
    infer::{
        InferCtxt,
        at::At,
        traits::{Obligation, ObligationCause, PredicateObligation, PredicateObligations},
    },
};

/// rustc's `normalize_with_universes`: every alias in `value` that is not rigid becomes a
/// root `Projection` goal, evaluated at once. The goals that stay ambiguous come back as
/// obligations; a goal with no solution is the error.
fn normalize_with_universes<'db, T>(
    at: At<'_, 'db>,
    value: T,
    universes: Vec<Option<UniverseIndex>>,
) -> Result<(T, PredicateObligations<'db>), PredicateObligation<'db>>
where
    T: TypeFoldable<DbInterner<'db>>,
{
    let infcx = at.infcx;
    let interner = infcx.interner;
    let value = infcx.deeply_resolve_ignoring_regions(value);

    if !interner.renormalize_rigid_aliases() && !value.has_non_rigid_aliases() {
        return Ok((value, PredicateObligations::new()));
    }

    let delegate = <&SolverContext<'db>>::from(infcx);
    let span = at.cause.span();
    let mut stalled_goals = Vec::new(); // alloc: empty unless a normalization is ambiguous
    let value = {
        let mut folder = NormalizationFolder::new(infcx, universes, |alias_term: AliasTerm<'db>| {
            let infer_term = infcx.next_term_var_of_alias_kind(alias_term, span);
            let goal: Goal<'db, Predicate<'db>> = Goal::new(
                interner,
                at.param_env,
                ProjectionClause { projection_term: alias_term, term: infer_term },
            );
            let result = match delegate.evaluate_root_goal(goal, span, None) {
                Ok(result) => result,
                Err(NoSolution) => {
                    return Err(PredicateObligation::new(
                        interner,
                        *at.cause,
                        goal.param_env,
                        goal.predicate,
                    ));
                }
            };
            let normalized = infcx.deeply_resolve_ignoring_regions(infer_term);
            let normalization_was_ambiguous = match result.certainty {
                Certainty::Yes => NormalizationWasAmbiguous::No,
                Certainty::Maybe(_) => {
                    stalled_goals.push(result.goal);
                    NormalizationWasAmbiguous::Yes
                }
            };
            Ok((normalized, normalization_was_ambiguous))
        });
        value.try_fold_with(&mut folder)?
    };
    let obligations = stalled_goals
        .into_iter()
        .map(|goal| Obligation::new(interner, *at.cause, goal.param_env, goal.predicate))
        .collect(); // alloc: empty unless a normalization is ambiguous
    Ok((value, obligations))
}

/// rustc's `solve::normalize`: `value` normalized, ambiguity and errors deferred to
/// fulfillment. Where an alias has no solution, every alias that is not rigid becomes an
/// inference variable with its `Projection` goal, for fulfillment to report.
pub(crate) fn normalize<'db, T>(at: At<'_, 'db>, value: T) -> (T, PredicateObligations<'db>)
where
    T: TypeFoldable<DbInterner<'db>> + Clone,
{
    match normalize_with_universes(at, value.clone(), Vec::new()) {
        Ok(normalized) => normalized,
        Err(_) => {
            let mut replacer = ReplaceAliasWithInfer {
                at,
                obligations: PredicateObligations::new(),
                // alloc: none until a binder is entered
                universes: Vec::new(),
            };
            let value = at.infcx.deeply_resolve_ignoring_regions(value);
            let value = value.fold_with(&mut replacer);
            (value, replacer.obligations)
        }
    }
}

struct ReplaceAliasWithInfer<'me, 'db> {
    at: At<'me, 'db>,
    obligations: PredicateObligations<'db>,
    universes: Vec<Option<UniverseIndex>>,
}

impl<'db> ReplaceAliasWithInfer<'_, 'db> {
    fn term_to_infer(&mut self, alias_term: AliasTerm<'db>) -> Term<'db> {
        let infcx = self.at.infcx;
        let infer_term = infcx.next_term_var_of_alias_kind(alias_term, self.at.cause.span());
        self.obligations.push(Obligation::new(
            infcx.interner,
            *self.at.cause,
            self.at.param_env,
            ProjectionClause { projection_term: alias_term, term: infer_term },
        ));
        infer_term
    }
}

impl<'db> TypeFolder<DbInterner<'db>> for ReplaceAliasWithInfer<'_, 'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.at.infcx.interner
    }

    fn fold_binder<T: TypeFoldable<DbInterner<'db>>>(
        &mut self,
        t: Binder<'db, T>,
    ) -> Binder<'db, T> {
        self.universes.push(None);
        let t = t.super_fold_with(self);
        self.universes.pop();
        t
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if !self.cx().renormalize_rigid_aliases() && !ty.has_non_rigid_aliases() {
            return ty;
        }

        let ty = ty.super_fold_with(self);
        let TyKind::Alias(orig_is_rigid, alias) = ty.kind() else { return ty };
        if !self.cx().renormalize_rigid_aliases() && orig_is_rigid == IsRigid::Yes {
            return ty;
        }

        if ty.has_escaping_bound_vars() {
            let (replaced, ..) =
                BoundVarReplacer::replace_bound_vars(self.at.infcx, &mut self.universes, alias);
            // Keep the higher-ranked alias in the folded value; the fresh term is only
            // used to register its projection obligation.
            let _ = self.term_to_infer(replaced.into());
            ty
        } else {
            self.term_to_infer(alias.into()).expect_type()
        }
    }

    fn fold_const(&mut self, ct: Const<'db>) -> Const<'db> {
        if !self.cx().renormalize_rigid_aliases() && !ct.has_non_rigid_aliases() {
            return ct;
        }

        let ct = ct.super_fold_with(self);
        let ConstKind::Alias(orig_is_rigid, alias_const) = ct.kind() else { return ct };
        if !self.cx().renormalize_rigid_aliases() && orig_is_rigid == IsRigid::Yes {
            return ct;
        }

        if ct.has_escaping_bound_vars() {
            let (replaced, ..) = BoundVarReplacer::replace_bound_vars(
                self.at.infcx,
                &mut self.universes,
                alias_const,
            );
            // Keep the higher-ranked alias in the folded value; the fresh term is only
            // used to register its projection obligation.
            let _ = self.term_to_infer(replaced.into());
            ct
        } else {
            self.term_to_infer(alias_const.into()).expect_const()
        }
    }
}

/// Deeply normalize all aliases in `value`. This does not handle inference and expects
/// its input to be already fully resolved.
pub fn deeply_normalize<'db, T>(at: At<'_, 'db>, value: T) -> Result<T, Vec<NextSolverError<'db>>>
where
    T: TypeFoldable<DbInterner<'db>>,
{
    assert!(!value.has_escaping_bound_vars());
    deeply_normalize_with_skipped_universes(at, value, Vec::new()) // alloc: empty, no binder entered
}

/// Deeply normalize all aliases in `value`. This does not handle inference and expects
/// its input to be already fully resolved.
///
/// Additionally takes a list of universes which represents the binders which have been
/// entered before passing `value` to the function. This is currently needed for
/// `normalize_erasing_regions`, which skips binders as it walks through a type.
pub fn deeply_normalize_with_skipped_universes<'db, T>(
    at: At<'_, 'db>,
    value: T,
    universes: Vec<Option<UniverseIndex>>,
) -> Result<T, Vec<NextSolverError<'db>>>
where
    T: TypeFoldable<DbInterner<'db>>,
{
    let (value, coroutine_goals) =
        deeply_normalize_with_skipped_universes_and_ambiguous_coroutine_goals(
            at, value, universes,
        )?;
    assert!(coroutine_goals.is_empty(), "stalled coroutine goals: {coroutine_goals:?}");

    Ok(value)
}

/// Deeply normalize all aliases in `value`. This does not handle inference and expects
/// its input to be already fully resolved.
///
/// Additionally takes a list of universes which represents the binders which have been
/// entered before passing `value` to the function. This is currently needed for
/// `normalize_erasing_regions`, which skips binders as it walks through a type.
///
/// This returns a set of stalled obligations involving coroutines if the typing mode of
/// the underlying infcx has any stalled coroutine def ids.
pub fn deeply_normalize_with_skipped_universes_and_ambiguous_coroutine_goals<'db, T>(
    at: At<'_, 'db>,
    value: T,
    universes: Vec<Option<UniverseIndex>>,
) -> Result<(T, Vec<Goal<'db, Predicate<'db>>>), Vec<NextSolverError<'db>>>
where
    T: TypeFoldable<DbInterner<'db>>,
{
    let (value, obligations) = normalize_with_universes(at, value, universes)
        .map_err(|obligation| vec![NextSolverError::TrueError(obligation)])?; // alloc: error path

    let mut fulfill_cx = FulfillmentCtxt::new(at.infcx);
    fulfill_cx.register_predicate_obligations(at.infcx, obligations);

    let errors = fulfill_cx.try_evaluate_obligations(at.infcx);
    if !errors.is_empty() {
        return Err(errors);
    }

    let stalled_coroutine_goals = fulfill_cx
        .drain_stalled_obligations_for_coroutines(at.infcx)
        .into_iter()
        .map(|obl| obl.as_goal())
        .collect(); // alloc: empty unless a goal is stalled on a coroutine

    let errors = fulfill_cx.collect_remaining_errors(at.infcx);
    if !errors.is_empty() {
        return Err(errors);
    }

    Ok((value, stalled_coroutine_goals))
}

// Deeply normalize a value and return it
pub(crate) fn deeply_normalize_for_diagnostics<'db, T: TypeFoldable<DbInterner<'db>>>(
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    t: T,
) -> T {
    t.fold_with(&mut DeeplyNormalizeForDiagnosticsFolder {
        at: infcx.at(&ObligationCause::dummy(), param_env),
    })
}

struct DeeplyNormalizeForDiagnosticsFolder<'a, 'tcx> {
    at: At<'a, 'tcx>,
}

impl<'db> TypeFolder<DbInterner<'db>> for DeeplyNormalizeForDiagnosticsFolder<'_, 'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.at.infcx.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        let infcx = self.at.infcx;
        let result: Result<_, Vec<NextSolverError<'db>>> = infcx.commit_if_ok(|_| {
            deeply_normalize_with_skipped_universes_and_ambiguous_coroutine_goals(
                self.at,
                ty,
                vec![None; ty.outer_exclusive_binder().as_usize()], // alloc: diagnostics only
            )
        });
        match result {
            Ok((ty, _)) => ty,
            Err(_) => ty.super_fold_with(self),
        }
    }

    fn fold_const(&mut self, ct: Const<'db>) -> Const<'db> {
        let infcx = self.at.infcx;
        let result: Result<_, Vec<NextSolverError<'db>>> = infcx.commit_if_ok(|_| {
            deeply_normalize_with_skipped_universes_and_ambiguous_coroutine_goals(
                self.at,
                ct,
                vec![None; ct.outer_exclusive_binder().as_usize()], // alloc: diagnostics only
            )
        });
        match result {
            Ok((ct, _)) => ct,
            Err(_) => ct.super_fold_with(self),
        }
    }
}
