use std::cell::Cell;

use rustc_next_trait_solver::solve::{GoalEvaluation, HasChanged, SolverDelegateEvalExt};
use rustc_type_ir::{
    Interner,
    solve::{Certainty, NoSolution},
};

use super::*;

thread_local! {
    static OLD_ROUNDS: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn old_rounds() -> bool {
    OLD_ROUNDS.get()
}

/// Runs `f` with rustc's loop over every pending obligation in place of the watch lists.
pub(crate) fn with_old_rounds<R>(f: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            OLD_ROUNDS.set(false);
        }
    }
    OLD_ROUNDS.set(true);
    let _reset = Reset;
    f()
}

/// Runs `f` with the global-goal and shape tables off, so every root goal goes to the solver.
pub(crate) fn without_goal_tables<R>(f: impl FnOnce() -> R) -> R {
    global_goals::switch::without(f)
}

impl<'db> FulfillmentCtxt<'db> {
    /// The loop the watch lists replaced, as it was: every pending obligation every round.
    pub(super) fn try_evaluate_obligations_old(
        &mut self,
        infcx: &InferCtxt<'db>,
    ) -> Vec<NextSolverError<'db>> {
        let delegate = <&SolverContext<'db>>::from(infcx);
        let recursion_limit = infcx.interner.recursion_limit();
        let mut errors = Vec::new();
        let mut overflowed = false;
        loop {
            let mut any_changed = false;
            let round_start = infcx.state_changes();

            self.pending.retain(|obligation, opt_stalled_on| {
                if overflowed {
                    return true;
                }
                if let Some(stalled_on) = opt_stalled_on
                    && delegate.goal_remains_stalled(stalled_on)
                {
                    return true;
                }

                let goal = obligation.as_goal();
                let span = obligation.cause.span();
                let result =
                    global_goals::evaluate_root_goal(delegate, goal, span, opt_stalled_on.take());
                infcx.inspect_evaluated_obligation(obligation, &result, || {
                    Some(delegate.evaluate_root_goal_for_proof_tree(goal, span).1)
                });
                let GoalEvaluation { goal, certainty, has_changed, stalled_on } = match result {
                    Ok(result) => result,
                    Err(NoSolution) => {
                        errors.push(NextSolverError::TrueError(obligation.clone()));
                        return false;
                    }
                };

                obligation.predicate = goal.predicate;
                if has_changed == HasChanged::Yes {
                    if obligation.recursion_depth as usize > recursion_limit {
                        errors.push(NextSolverError::Overflow(obligation.clone()));
                        overflowed = true;
                        return false;
                    }
                    obligation.recursion_depth = obligation.recursion_depth.saturating_add(1);
                    any_changed = true;
                }

                match certainty {
                    Certainty::Yes => false,
                    Certainty::Maybe(_) => {
                        *opt_stalled_on = stalled_on;
                        true
                    }
                }
            });

            if overflowed || !any_changed {
                if !overflowed && infcx.state_changes() == round_start {
                    self.fixpoint = Some(round_start);
                }
                break;
            }
        }

        errors
    }
}

#[test]
fn next_ready_visits_ready_rows_in_order() {
    let mut watches = Watches::default();
    watches.admit(200);
    for at in 0..200 {
        if at % 3 != 0 {
            clear(&mut watches.ready, at);
        }
    }
    let mut seen = Vec::new();
    let mut from = 0;
    while let Some(at) = watches.next_ready(from) {
        seen.push(at);
        from = at + 1;
    }
    assert_eq!(seen, (0..200).filter(|at| at % 3 == 0).collect::<Vec<_>>());
}

#[test]
fn a_row_woken_below_the_visit_waits_for_the_next_pass() {
    let mut watches = Watches::default();
    watches.admit(130);
    for at in 0..130 {
        clear(&mut watches.ready, at);
    }
    set(&mut watches.ready, 100);
    let at = watches.next_ready(0).unwrap();
    assert_eq!(at, 100);
    set(&mut watches.ready, 5);
    set(&mut watches.ready, 120);
    assert_eq!(watches.next_ready(at + 1), Some(120), "a later row is visited in this pass");
    assert_eq!(watches.next_ready(0), Some(5), "an earlier row is ready for the next pass");
}

#[test]
fn draining_a_list_wakes_each_watcher_and_reuses_its_nodes() {
    let mut watches = Watches::default();
    watches.admit(10);
    for at in 0..10 {
        clear(&mut watches.ready, at);
    }
    watches.link(VarTable::Ty, 3, 2);
    watches.link(VarTable::Ty, 3, 7);
    watches.link(VarTable::Int, 3, 4);
    let head = std::mem::replace(&mut watches.heads[VarTable::Ty as usize][3], NONE);
    watches.wake_list(head);
    assert!(get(&watches.ready, 2) && get(&watches.ready, 7));
    assert!(!get(&watches.ready, 4), "another table's variable of the same index");
    assert_eq!(watches.linked, 1);
    let nodes = watches.nodes.len();
    blaze_alloc_count::assert_at_most(0, "relinking freed watch nodes", || {
        watches.link(VarTable::Ty, 1, 2);
        watches.link(VarTable::Ty, 3, 7);
    });
    assert_eq!(watches.nodes.len(), nodes);
}
