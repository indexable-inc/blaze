//! The same-work-again verdict under simulation: a driver whose tasks restart, end and see the
//! room move in an order Hegel draws, against a reference model of the rule kept beside the core.

use hegel::{TestCase, generators as gs};

use super::*;

const GIB: u64 = 1 << 30;
const NOTHING: Inputs = Inputs { ended: 0, room: Some(GIB) };

/// The invariant: a step that always answers "again" with nothing changed ends with
/// [`SameAgain`] at its start number `AGAIN_BOUND + 1`, never before, and is said once.
#[test]
fn a_step_that_always_answers_again_is_said_within_the_bound() {
    let mut again = Again::new();
    for start in 1..=AGAIN_BOUND {
        assert_eq!(again.restarted(7, 3, Restart::Wounded, NOTHING), None, "start {start}");
    }
    let said = again.restarted(7, 3, Restart::Wounded, NOTHING);
    let want = SameAgain {
        build: 7,
        node: 3,
        times: AGAIN_BOUND + 1,
        restart: Restart::Wounded,
        waited: NOTHING,
    };
    assert_eq!(said, Some(want));
    assert_eq!(again.restarted(7, 3, Restart::Wounded, NOTHING), None, "said once");
}

/// The control: a step that answers "again" once and then finishes is never said, and is
/// forgotten.
#[test]
fn a_step_that_answers_again_once_then_finishes_is_not_said() {
    let mut again = Again::new();
    assert_eq!(again.restarted(7, 3, Restart::Wounded, NOTHING), None);
    again.forget(3);
    assert!(again.is_empty());
}

/// The control: a step restarted many times, each time after an input of it really changed (a
/// task ended, or the room grew past the most it had), is never said.
#[test]
fn a_step_restarted_after_a_real_change_is_not_said() {
    let mut again = Again::new();
    for ended in 0..4 * u64::from(AGAIN_BOUND) {
        let now = Inputs { ended, room: Some(GIB) };
        assert_eq!(again.restarted(1, 9, Restart::Wounded, now), None, "after {ended} ended");
    }
    let mut room = GIB;
    for _ in 0..4 * AGAIN_BOUND {
        room += 64 << 20;
        again.room(room);
        // The room falls back before the task starts again: it grew in between all the same.
        again.room(GIB / 2);
        let now = Inputs { ended: 99, room: Some(GIB / 2) };
        assert_eq!(again.restarted(1, 9, Restart::Wounded, now), None);
    }
}

/// A room that swings up and down with the task's own memory, never past the most it had at a
/// start, is no change: the loop of 2026-11-13 (the ledger's slot fitted to what the gate
/// committed, given back as the wounded lowering unwound, 1,280 starts) is said.
#[test]
fn a_room_that_swings_with_the_tasks_own_memory_is_no_change() {
    let mut again = Again::new();
    let high = 3 * GIB;
    let mut said = None;
    for start in 0..=AGAIN_BOUND {
        again.room(high);
        said = again.restarted(2, 4, Restart::Wounded, Inputs { ended: 5, room: Some(high) });
        again.room(GIB);
        if start < AGAIN_BOUND {
            assert_eq!(said, None, "start {start}");
        }
    }
    assert!(said.is_some(), "the swinging room hid the loop");
}

/// The control of the planted bugs, each on the case it breaks: counting every restart as
/// progress never says the loop; ignoring the room says a task whose room grew.
#[test]
fn each_planted_bug_breaks_its_case() {
    let mut flawed = Again::new();
    flawed.flaw = Flaw::RestartIsProgress;
    let said = (0..4 * AGAIN_BOUND)
        .filter_map(|_| flawed.restarted(1, 1, Restart::Wounded, NOTHING))
        .count();
    assert_eq!(said, 0, "the flaw is not planted");
    let mut flawed = Again::new();
    flawed.flaw = Flaw::RoomIgnored;
    let mut room = GIB;
    let mut said = 0;
    for _ in 0..4 * AGAIN_BOUND {
        room += GIB;
        flawed.room(room);
        flawed.room(GIB);
        said += usize::from(flawed.restarted(1, 1, Restart::Wounded, NOTHING).is_some());
    }
    assert_eq!(said, 1, "the flaw is not planted");
}

/// The model's view of one node: starts in a row unchanged, the outcome and inputs at the last,
/// the most room at any of them, whether the room grew past it since, and whether it was said.
#[derive(Clone, Copy)]
struct Model {
    times: u32,
    restart: Restart,
    at: Inputs,
    high: Option<u64>,
    grew: bool,
    said: bool,
}

const NODES: usize = 4;

/// One run Hegel draws: tasks of the driver end, the room moves (mostly back down or level,
/// sometimes past what it was), nodes end and nodes start again; each start's verdict held to
/// the model. How many verdicts it saw, or the first disagreement.
fn simulate(tc: &TestCase, flaw: Flaw) -> Result<u32, String> {
    let mut again = Again::new();
    again.flaw = flaw;
    let mut model: [Option<Model>; NODES] = [None; NODES];
    let (mut ended, mut room) = (0u64, Some(GIB));
    let mut said = 0;
    let steps = tc.draw(gs::integers::<u32>().min_value(1).max_value(200));
    for _ in 0..steps {
        let node = tc.draw(gs::integers::<usize>().max_value(NODES - 1));
        match tc.draw(gs::integers::<u8>().max_value(9)) {
            0 => ended += 1,
            1 => {
                let bytes = GIB + (tc.draw(gs::integers::<u64>().max_value(3)) << 26);
                room = Some(bytes);
                again.room(bytes);
                for it in model.iter_mut().flatten() {
                    if Some(bytes) > it.high {
                        it.grew = true;
                    }
                }
            }
            2 => {
                again.forget(node as u64);
                model[node] = None;
            }
            _ => {
                let restart = match tc.draw(gs::integers::<u8>().max_value(7)) {
                    0 => Restart::GaveWay,
                    _ => Restart::Wounded,
                };
                let now = Inputs { ended, room };
                let got = again.restarted(1, node as u64, restart, now);
                let was = model[node].unwrap_or(Model {
                    times: 0,
                    restart,
                    at: now,
                    high: now.room,
                    grew: false,
                    said: false,
                });
                let changed = was.times > 0
                    && (was.grew
                        || was.restart != restart
                        || was.at.ended != now.ended
                        || now.room > was.high);
                let times = if changed { 1 } else { was.times + 1 };
                let high = if changed { now.room } else { was.high.max(now.room) };
                let due = times > AGAIN_BOUND && !was.said;
                model[node] =
                    Some(Model { times, restart, at: now, high, grew: false, said: was.said || due });
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

/// The invariant: under any order of restarts, ends and moves of the room, the core says a task
/// exactly when it has started more than [`AGAIN_BOUND`] times in a row with nothing changed,
/// once, and never otherwise.
#[hegel::test(test_cases = 300)]
fn the_core_says_exactly_the_tasks_started_again_unchanged(tc: TestCase) {
    if let Err(why) = simulate(&tc, Flaw::None) {
        panic!("{why}");
    }
}
