//! Fulfill loop for next-solver.

use std::mem;

use rustc_next_trait_solver::solve::{
    GoalEvaluation, GoalStalledOn, StalledOnCoroutines, fast_path::compute_goal_fast_path,
};
use rustc_type_ir::{
    InferCtxtLike, Interner as _,
    solve::{Certainty, FastPathDepth},
};

use crate::next_solver::{
    DbInterner, ParamEnv, Predicate, SolverContext, TypingMode, global_goals,
    infer::{
        InferCtxt,
        traits::{Obligation, ObligationCause, PredicateObligation, PredicateObligations},
    },
    recycle,
};

pub(crate) mod watch;

type StalledOn<'db> = Option<GoalStalledOn<DbInterner<'db>>>;

/// Obligations in the order they are evaluated in, a column for each part of one and one for
/// what it is stalled on. The order is rustc's: it decides which errors are reported and which
/// inference variables are constrained first.
#[derive(Debug, Default)]
pub(crate) struct Pending<'db> {
    cause: Vec<ObligationCause>,
    param_env: Vec<ParamEnv<'db>>,
    predicate: Vec<Predicate<'db>>,
    recursion_depth: Vec<u32>,
    stalled_on: Vec<StalledOn<'db>>,
    watches: watch::Watches,
}

const _: () = assert!(
    size_of::<Pending<'static>>() == 5 * size_of::<Vec<u8>>() + size_of::<watch::Watches>()
);

impl<'db> Pending<'db> {
    fn len(&self) -> usize {
        self.predicate.len()
    }

    fn is_empty(&self) -> bool {
        self.predicate.is_empty()
    }

    fn push(&mut self, obligation: PredicateObligation<'db>, stalled_on: StalledOn<'db>) {
        let Obligation { cause, param_env, predicate, recursion_depth } = obligation;
        self.cause.push(cause);
        self.param_env.push(param_env);
        self.predicate.push(predicate);
        self.recursion_depth.push(recursion_depth);
        self.stalled_on.push(stalled_on);
    }

    fn obligation(&self, at: usize) -> PredicateObligation<'db> {
        Obligation {
            cause: self.cause[at],
            param_env: self.param_env[at],
            predicate: self.predicate[at],
            recursion_depth: self.recursion_depth[at],
        }
    }

    fn obligations(&self) -> impl Iterator<Item = PredicateObligation<'db>> + '_ {
        (0..self.len()).map(|at| self.obligation(at))
    }

    pub(crate) fn clear(&mut self) {
        self.truncate(0);
    }

    /// Bytes its columns hold room for.
    pub(crate) fn held(&self) -> usize {
        self.cause.capacity() * size_of::<ObligationCause>()
            + self.param_env.capacity() * size_of::<ParamEnv<'db>>()
            + self.predicate.capacity() * size_of::<Predicate<'db>>()
            + self.recursion_depth.capacity() * size_of::<u32>()
            + self.stalled_on.capacity() * size_of::<StalledOn<'db>>()
            + self.watches.held()
    }

    #[cfg(test)]
    pub(crate) fn reserve_for_tests(&mut self, rows: usize) {
        self.cause.reserve_exact(rows);
        self.param_env.reserve_exact(rows);
        self.predicate.reserve_exact(rows);
        self.recursion_depth.reserve_exact(rows);
        self.stalled_on.reserve_exact(rows);
    }

    fn truncate(&mut self, len: usize) {
        self.cause.truncate(len);
        self.param_env.truncate(len);
        self.predicate.truncate(len);
        self.recursion_depth.truncate(len);
        self.stalled_on.truncate(len);
        self.watches.forget();
    }

    /// rustc's `retain_mut`: keeps, in order, the rows `keep` is true of, compacting every
    /// column in place. `keep` may change a row's obligation and what it is stalled on, and
    /// what it leaves is kept.
    fn retain(
        &mut self,
        mut keep: impl FnMut(&mut PredicateObligation<'db>, &mut StalledOn<'db>) -> bool,
    ) {
        let mut kept = 0;
        for at in 0..self.len() {
            let mut obligation = self.obligation(at);
            if !keep(&mut obligation, &mut self.stalled_on[at]) {
                continue;
            }
            let Obligation { cause, param_env, predicate, recursion_depth } = obligation;
            self.cause[kept] = cause;
            self.param_env[kept] = param_env;
            self.predicate[kept] = predicate;
            self.recursion_depth[kept] = recursion_depth;
            // `kept <= at`, and the rows before `at` are all decided.
            self.stalled_on.swap(kept, at);
            kept += 1;
        }
        self.truncate(kept);
    }
}

/// A trait engine using the new trait solver.
///
/// This is mostly identical to how `evaluate_all` works inside of the
/// solver, except that the requirements are slightly different.
///
/// Unlike `evaluate_all` it is possible to add new obligations later on
/// and we also have to track diagnostics information by using `Obligation`
/// instead of `Goal`.
///
/// It is also likely that we want to use slightly different datastructures
/// here as this will have to deal with far more root goals than `evaluate_all`.
#[derive(Debug)]
pub struct FulfillmentCtxt<'db> {
    pending: Pending<'db>,

    /// The snapshot in which this context was created. Using the context
    /// outside of this snapshot leads to subtle bugs if the snapshot
    /// gets rolled back. Because of this we explicitly check that we only
    /// use the context in exactly this snapshot.
    usable_in_snapshot: usize,

    /// `InferCtxt::state_changes` when a round last evaluated every pending obligation and
    /// changed nothing, with nothing registered since. Each obligation then gives the answer it
    /// gave in that round, so another round would change nothing either.
    fixpoint: Option<u64>,
}

impl<'db> FulfillmentCtxt<'db> {
    pub fn new(infcx: &InferCtxt<'db>) -> FulfillmentCtxt<'db> {
        FulfillmentCtxt {
            pending: recycle::pending(),
            usable_in_snapshot: infcx.num_open_snapshots(),
            fixpoint: None,
        }
    }
}

impl Drop for FulfillmentCtxt<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            recycle::give_pending(mem::take(&mut self.pending));
        }
    }
}

impl<'db> FulfillmentCtxt<'db> {
    /// A goal the fast path proves is not kept; one it finds stalled is kept with exactly
    /// what it is stalled on, as rustc does.
    #[tracing::instrument(level = "trace", skip(self, infcx))]
    pub(crate) fn register_predicate_obligation(
        &mut self,
        infcx: &InferCtxt<'db>,
        obligation: PredicateObligation<'db>,
    ) {
        assert_eq!(self.usable_in_snapshot, infcx.num_open_snapshots());
        self.fixpoint = None;
        let delegate = <&SolverContext<'db>>::from(infcx);
        let depth = FastPathDepth::Any(Some(infcx.interner.recursion_limit()));
        let goal = obligation.as_goal();
        match compute_goal_fast_path(delegate, goal, obligation.cause.span(), depth) {
            Some((GoalEvaluation { certainty: Certainty::Yes, .. }, _)) => {}
            Some((GoalEvaluation { certainty: Certainty::Maybe(_), stalled_on, .. }, _)) => {
                self.pending.push(obligation, stalled_on)
            }
            None if global_goals::holds_at_registration(delegate, obligation.as_goal()) => {}
            None => self.pending.push(obligation, None),
        }
    }

    pub(crate) fn register_predicate_obligations(
        &mut self,
        infcx: &InferCtxt<'db>,
        obligations: impl IntoIterator<Item = PredicateObligation<'db>>,
    ) {
        for obligation in obligations {
            self.register_predicate_obligation(infcx, obligation);
        }
    }

    pub(crate) fn collect_remaining_errors(
        &mut self,
        _infcx: &InferCtxt<'db>,
    ) -> Vec<NextSolverError<'db>> {
        if self.pending.is_empty() {
            return Vec::new(); // alloc: empty
        }
        let errors = self.pending.obligations().map(NextSolverError::Ambiguity).collect(); // alloc: error path
        self.pending.clear();
        errors
    }

    pub(crate) fn evaluate_obligations_error_on_ambiguity(
        &mut self,
        infcx: &InferCtxt<'db>,
    ) -> Vec<NextSolverError<'db>> {
        let errors = self.try_evaluate_obligations(infcx);
        if !errors.is_empty() {
            return errors;
        }

        self.collect_remaining_errors(infcx)
    }

    /// [`Self::evaluate_obligations_error_on_ambiguity`] handing each error to `on_error`.
    pub(crate) fn evaluate_obligations_error_on_ambiguity_with(
        &mut self,
        infcx: &InferCtxt<'db>,
        on_error: &mut impl FnMut(NextSolverError<'db>),
    ) {
        if self.try_evaluate_obligations_with(infcx, on_error) {
            return;
        }
        self.pending.obligations().map(NextSolverError::Ambiguity).for_each(&mut *on_error);
        self.pending.clear();
    }

    pub(crate) fn pending_obligations(&self) -> PredicateObligations<'db> {
        self.pending.obligations().collect() // alloc: the caller takes the pending set, once
    }

    /// The pending obligations the solver found stalled on a coroutine the typeck root
    /// defines, taken out.
    pub(crate) fn drain_stalled_obligations_for_coroutines(
        &mut self,
        infcx: &InferCtxt<'db>,
    ) -> PredicateObligations<'db> {
        let stalled_coroutines = match infcx.typing_mode_raw().assert_not_erased() {
            TypingMode::Typeck { defining_opaque_types_and_generators } => {
                defining_opaque_types_and_generators
            }
            TypingMode::Coherence
            | TypingMode::PostTypeckThroughBorrowck { defining_opaque_types: _ }
            | TypingMode::PostBorrowck { defined_opaque_types: _ }
            | TypingMode::Reflection
            | TypingMode::PostAnalysis
            | TypingMode::Codegen => return Default::default(),
        };

        if stalled_coroutines.as_slice().is_empty() {
            return Default::default();
        }

        let mut stalled = PredicateObligations::new();
        self.pending.retain(|obligation, stalled_on| {
            let on_coroutines = stalled_on.as_ref().is_some_and(|it| {
                matches!(it.stalled_maybe_info.stalled_on_coroutines, StalledOnCoroutines::Yes)
            });
            if on_coroutines {
                stalled.push(obligation.clone()); // clone: four `Copy` fields, row dropped next
            }
            !on_coroutines
        });
        stalled
    }
}

#[derive(Debug, Clone)]
pub enum NextSolverError<'db> {
    TrueError(PredicateObligation<'db>),
    Ambiguity(PredicateObligation<'db>),
    /// An obligation that made inference progress more often than the recursion limit:
    /// rustc's fatal fulfillment overflow.
    Overflow(PredicateObligation<'db>),
}

impl NextSolverError<'_> {
    #[inline]
    pub fn is_true_error(&self) -> bool {
        matches!(self, NextSolverError::TrueError(_))
    }
}
