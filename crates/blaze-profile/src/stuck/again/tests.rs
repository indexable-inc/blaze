//! The same-work-again verdict under simulation: a driver whose tasks restart and end in an
//! order Hegel draws, against a reference model of the rule kept beside the core.

use hegel::{TestCase, generators as gs};

use super::*;

const NOTHING: Inputs = Inputs { ended: 0 };

/// The invariant: a step that always answers "again" with nothing changed ends with
/// [`SameAgain`] at its start number `AGAIN_BOUND + 1`, never before, and is said once.
#[test]
fn a_step_that_always_answers_again_is_said_within_the_bound() {
    let mut again = Again::new();
    for start in 1..=AGAIN_BOUND {
        assert_eq!(again.restarted(7, 3, NOTHING), None, "start {start}");
    }
    let said = again.restarted(7, 3, NOTHING);
    let want = SameAgain { build: 7, node: 3, times: AGAIN_BOUND + 1, waited: NOTHING };
    assert_eq!(said, Some(want));
    assert_eq!(again.restarted(7, 3, NOTHING), None, "said once");
}

/// The control: a step that answers "again" once and then finishes is never said, and is
/// forgotten.
#[test]
fn a_step_that_answers_again_once_then_finishes_is_not_said() {
    let mut again = Again::new();
    assert_eq!(again.restarted(7, 3, NOTHING), None);
    again.forget(3);
    assert!(again.is_empty());
}

/// The control: a step restarted many times, each time after a task of the driver ended, is
/// never said.
#[test]
fn a_step_restarted_after_a_real_change_is_not_said() {
    let mut again = Again::new();
    for ended in 0..4 * u64::from(AGAIN_BOUND) {
        let now = Inputs { ended };
        assert_eq!(again.restarted(1, 9, now), None, "after {ended} ended");
    }
}

/// The control of the planted bug: counting every restart as progress never says the loop.
#[test]
fn the_planted_bug_breaks_its_case() {
    let mut flawed = Again::new();
    flawed.flaw = Flaw::RestartIsProgress;
    let said = (0..4 * AGAIN_BOUND).filter_map(|_| flawed.restarted(1, 1, NOTHING)).count();
    assert_eq!(said, 0, "the flaw is not planted");
}

/// The model's view of one node: starts in a row unchanged, the inputs at the last, and
/// whether it was said.
#[derive(Clone, Copy)]
struct Model {
    times: u32,
    at: Inputs,
    said: bool,
}

const NODES: usize = 4;

/// One run Hegel draws: tasks of the driver end, nodes end and nodes start again; each start's
/// verdict held to the model. How many verdicts it saw, or the first disagreement.
fn simulate(tc: &TestCase, flaw: Flaw) -> Result<u32, String> {
    let mut again = Again::new();
    again.flaw = flaw;
    let mut model: [Option<Model>; NODES] = [None; NODES];
    let mut ended = 0u64;
    let mut said = 0;
    let steps = tc.draw(gs::integers::<u32>().min_value(1).max_value(200));
    for _ in 0..steps {
        let node = tc.draw(gs::integers::<usize>().max_value(NODES - 1));
        match tc.draw(gs::integers::<u8>().max_value(9)) {
            0 => ended += 1,
            1 => {
                again.forget(node as u64);
                model[node] = None;
            }
            _ => {
                let now = Inputs { ended };
                let got = again.restarted(1, node as u64, now);
                let was = model[node].unwrap_or(Model { times: 0, at: now, said: false });
                let changed = was.times > 0 && was.at.ended != now.ended;
                let times = if changed { 1 } else { was.times + 1 };
                let due = times > AGAIN_BOUND && !was.said;
                model[node] = Some(Model { times, at: now, said: was.said || due });
                match (due, got) {
                    (false, None) => {}
                    (true, Some(it)) if it.times == times && it.node == node as u64 => said += 1,
                    (due, got) => {
                        return Err(format!(
                            "node {node}, start {times} in a row unchanged: the model says due={due}, the core said {got:?}"
                        ));
                    }
                }
            }
        }
    }
    Ok(said)
}

/// The invariant: under any order of restarts and ends, the core says a task exactly when it
/// has started more than [`AGAIN_BOUND`] times in a row with nothing changed, once, and never
/// otherwise.
#[hegel::test(test_cases = 300)]
fn the_core_says_exactly_the_tasks_started_again_unchanged(tc: TestCase) {
    if let Err(why) = simulate(&tc, Flaw::None) {
        panic!("{why}");
    }
}
