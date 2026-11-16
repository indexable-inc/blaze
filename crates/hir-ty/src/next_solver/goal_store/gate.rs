//! The goal store's gate (docs/v2/plan/design/typeck-store.md): which root goals the store
//! is asked about and offered, before the solver canonicalizes them.
//!
//! A root goal whose predicate and param env, resolved through the unification table, hold no
//! inference variable, placeholder, local region, error or escaping bound variable, asked in a
//! typing mode that defines no opaque type, with no opaque type in the inference context, has
//! one answer per crate and regions flag: the solver reads nothing else of the inference
//! context for it. Such a goal is named inside the task by a [`GoalKey`], one `u64` packed from
//! the rows of its predicate and param env and its typing mode: a row is this process's
//! interning order, so a `GoalKey` never leaves the task; the store names the goal by its
//! stable form (`super::stable`).
//!
//! Only answers that leave the inference context as they found it are offered: `Ok(Yes)` with
//! no change, or `Err(NoSolution)`, with an equal [`Mark`] before and after, so no overflow
//! note, region constraint, variable or universe was added. Such an answer replayed has the
//! effect the evaluation had. An evaluation recorded with no read list (`interner::recorded`
//! gives none when the snapshot keeps no read lists) is not offered.
//!
//! The in-memory table of these answers per (crate, regions flag, revision) and the shape
//! table of auto-trait, `Copy` and `Clone` goals were deleted on 2026-11-15 with the daemon's
//! revisions: within one run the solver's own global cache (`interner::tls_cache`) answers a
//! goal asked again.

use std::num::NonZeroU32;

use rustc_next_trait_solver::solve::{
    GoalEvaluation, GoalStalledOn, HasChanged, SolverDelegateEvalExt,
};
use rustc_type_ir::{
    Interner, TypeFlags, TypeVisitableExt,
    solve::{Certainty, NoSolution},
};

use crate::{
    Span,
    next_solver::{
        Clauses, DbInterner, Goal, ParamEnv, Predicate, SolverContext, SolverDefIds, TypingMode,
        goal_store,
        infer::InferCtxt,
        interner::recorded,
    },
};

/// A goal with these flags reads the inference context, or is not one goal for every body.
const NOT_GLOBAL: TypeFlags = TypeFlags::HAS_INFER
    .union(TypeFlags::HAS_PLACEHOLDER)
    .union(TypeFlags::HAS_FREE_LOCAL_REGIONS)
    .union(TypeFlags::HAS_ERROR);

const ROW_BITS: u32 = 30;
const ROW_MASK: u64 = (1 << ROW_BITS) - 1;
const MODE_SHIFT: u32 = 2 * ROW_BITS;
const MODE_MASK: u64 = 0b111;

/// A global root goal: the predicate's row in bits 0..30, the param env's clause-list row in
/// bits 30..60, the typing mode's tag in bits 60..63. Rows are this process's interning order:
/// a key is used inside the task that made it and never written.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct GoalKey(u64);

impl GoalKey {
    #[inline]
    pub(crate) fn of<'db>(
        predicate: Predicate<'db>,
        param_env: ParamEnv<'db>,
        typing_mode: TypingMode<'db>,
    ) -> Option<GoalKey> {
        if predicate.has_type_flags(NOT_GLOBAL)
            || predicate.has_escaping_bound_vars()
            || param_env.clauses.has_type_flags(NOT_GLOBAL)
            || param_env.clauses.has_escaping_bound_vars()
        {
            return None;
        }
        let mode = mode_tag(typing_mode)?;
        let predicate = u64::from(predicate.row().get());
        let env = u64::from(param_env.clauses.row().get());
        debug_assert!(predicate <= ROW_MASK, "predicate rows end at `table::LAST_ROW`");
        if env > ROW_MASK {
            return None;
        }
        Some(GoalKey(predicate | env << ROW_BITS | mode << MODE_SHIFT))
    }

    #[inline]
    pub(crate) fn bits(self) -> u64 {
        self.0
    }

    /// The typing mode's tag: 0 coherence, 1 typeck, 2 through borrowck, 3 post borrowck,
    /// 4 reflection, 5 post analysis, 6 codegen.
    #[inline]
    pub(crate) fn mode_tag(self) -> u8 {
        ((self.0 >> MODE_SHIFT) & MODE_MASK) as u8
    }

    pub(crate) fn predicate<'db>(self) -> Predicate<'db> {
        let row = NonZeroU32::new((self.0 & ROW_MASK) as u32).expect("predicate rows start at 1");
        Predicate::at(row)
    }

    pub(crate) fn param_env<'db>(self) -> ParamEnv<'db> {
        let row = NonZeroU32::new(((self.0 >> ROW_BITS) & ROW_MASK) as u32)
            .expect("clause-list rows start at 1");
        ParamEnv { clauses: Clauses::at(row) }
    }

    pub(crate) fn typing_mode<'db>(self) -> TypingMode<'db> {
        let empty = SolverDefIds::default;
        match self.mode_tag() {
            0 => TypingMode::Coherence,
            1 => TypingMode::Typeck { defining_opaque_types_and_generators: empty() },
            2 => TypingMode::PostTypeckThroughBorrowck { defining_opaque_types: empty() },
            3 => TypingMode::PostBorrowck { defined_opaque_types: empty() },
            4 => TypingMode::Reflection,
            5 => TypingMode::PostAnalysis,
            6 => TypingMode::Codegen,
            tag => unreachable!("`GoalKey::of` writes no typing-mode tag {tag}"),
        }
    }
}

/// The tag of a typing mode in which a global goal has one answer. A mode that names opaque
/// types to define is keyed only with none: with some, the answer depends on the body. The
/// erased mode is not keyed: it stands for every non-coherence mode at once and its answer
/// may be retried in another.
#[inline]
fn mode_tag(mode: TypingMode<'_>) -> Option<u64> {
    match mode {
        TypingMode::Coherence => Some(0),
        TypingMode::Typeck { defining_opaque_types_and_generators: defining } => {
            defining.is_empty().then_some(1)
        }
        TypingMode::PostTypeckThroughBorrowck { defining_opaque_types: defining } => {
            defining.is_empty().then_some(2)
        }
        TypingMode::PostBorrowck { defined_opaque_types: defined } => {
            defined.is_empty().then_some(3)
        }
        TypingMode::Reflection => Some(4),
        TypingMode::PostAnalysis => Some(5),
        TypingMode::Codegen => Some(6),
        TypingMode::ErasedNotCoherence(_) => None,
    }
}

/// The answer of a global goal that left the inference context as it found it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Answer {
    Holds,
    Fails,
}

impl Answer {
    fn of<'db>(
        goal: Goal<'db, Predicate<'db>>,
        result: &Result<GoalEvaluation<DbInterner<'db>>, NoSolution>,
    ) -> Option<Answer> {
        match result {
            Ok(GoalEvaluation {
                goal: evaluated,
                certainty: Certainty::Yes,
                has_changed: HasChanged::No,
                stalled_on: None,
            }) if *evaluated == goal => Some(Answer::Holds),
            Ok(_) => None,
            Err(NoSolution) => Some(Answer::Fails),
        }
    }

    fn evaluation<'db>(
        self,
        goal: Goal<'db, Predicate<'db>>,
    ) -> Result<GoalEvaluation<DbInterner<'db>>, NoSolution> {
        match self {
            Answer::Holds => Ok(GoalEvaluation {
                goal,
                certainty: Certainty::Yes,
                has_changed: HasChanged::No,
                stalled_on: None,
            }),
            Answer::Fails => Err(NoSolution),
        }
    }
}

/// The key of a root goal whose answer the store may give, and the goal as the solver sees
/// it: resolved through the unification table as `evaluate_goal_raw` resolves it first, so a
/// goal whose variables were all unified with global types (a method probe's where clauses,
/// once its receiver is known) is the global goal it stands for. The evaluation's goal is
/// that resolved one, hit or miss.
#[inline]
fn root_key<'db>(
    infcx: &InferCtxt<'db>,
    goal: Goal<'db, Predicate<'db>>,
) -> Option<(GoalKey, Goal<'db, Predicate<'db>>)> {
    if infcx.interner.assumptions_on_binders() || switch::off() {
        return None;
    }
    let goal = infcx.deeply_resolve_via_unification_table(goal);
    let key = GoalKey::of(goal.predicate, goal.param_env, infcx.typing_mode_raw())?;
    infcx.inner.borrow_mut().opaque_types().is_empty().then_some((key, goal))
}

/// [`SolverDelegateEvalExt::evaluate_root_goal`], answered from the store when the goal is
/// global.
pub(crate) fn evaluate_root_goal<'db>(
    delegate: &SolverContext<'db>,
    goal: Goal<'db, Predicate<'db>>,
    span: Span,
    stalled_on: Option<GoalStalledOn<DbInterner<'db>>>,
) -> Result<GoalEvaluation<DbInterner<'db>>, NoSolution> {
    let infcx = &delegate.0;
    let keyed = match stalled_on {
        None => root_key(infcx, goal),
        Some(_) => None,
    };
    let Some((key, resolved)) = keyed else {
        return delegate.evaluate_root_goal(goal, span, stalled_on);
    };
    let interner = infcx.interner;
    if let Some(found) = goal_store::lookup(interner, key) {
        return found.answer.evaluation(resolved);
    }
    let Some(before) = Mark::of(infcx) else {
        return delegate.evaluate_root_goal(goal, span, None);
    };
    let (result, reads) =
        recorded(interner.db, || delegate.evaluate_root_goal(goal, span, None));
    if let Some(reads) = reads
        && let Some(answer) = Answer::of(resolved, &result)
        && Mark::of(infcx) == Some(before)
    {
        interner.with_global_goals(|table| table.insert(key, answer, &reads));
        goal_store::offer(interner, key, answer, reads);
    }
    result
}

/// What an evaluation can leave in the inference context besides its answer. Equal marks
/// before and after mean it left none: every count only grows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Mark {
    state: u64,
    universe: u32,
    region_vars: usize,
    constraints: usize,
    member_constraints: usize,
    verifys: usize,
    region_obligations: usize,
    overflows: usize,
}

impl Mark {
    /// `None` once the region constraints are taken: then no answer is kept.
    fn of(infcx: &InferCtxt<'_>) -> Option<Mark> {
        let (state, region_obligations, constraints, member_constraints, verifys) = {
            let mut inner = infcx.inner.borrow_mut();
            inner.region_constraint_storage.as_ref()?;
            let state = inner.state_changes();
            let region_obligations = inner.region_obligations().len();
            let collector = inner.unwrap_region_constraints();
            let data = collector.legacy_data();
            (
                state,
                region_obligations,
                data.constraints.len(),
                data.member_constraints.len(),
                data.verifys.len(),
            )
        };
        Some(Mark {
            state,
            universe: infcx.universe().as_u32(),
            region_vars: infcx.num_region_vars(),
            constraints,
            member_constraints,
            verifys,
            region_obligations,
            overflows: infcx.next_solver_overflow_count(),
        })
    }
}

/// Test-only switch: with it off, neither the table nor the shape table answers, so a test
/// can compare them with the plain solver on one thread.
pub(crate) mod switch {
    #[cfg(test)]
    thread_local! {
        static OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[cfg(test)]
    #[inline]
    pub(crate) fn off() -> bool {
        OFF.with(|off| off.get())
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub(crate) fn off() -> bool {
        false
    }

    /// Runs `f` with the tables off on this thread.
    #[cfg(test)]
    pub(crate) fn without<R>(f: impl FnOnce() -> R) -> R {
        let was = OFF.with(|off| off.replace(true));
        let result = f();
        OFF.with(|off| off.set(was));
        result
    }
}
