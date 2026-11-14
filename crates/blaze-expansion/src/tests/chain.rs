//! The deterministic simulation of a stateful expander's chain: every call starts at once from
//! a predicted state, runs end in an order the tape picks, and the chain names the runs to redo.
//!
//! The oracle: once every run has ended, the chain is settled, each call's confirmed state is
//! the state the calls before it leave run one after another in source order, and no call ran
//! more than twice.

use std::sync::Arc;

use blaze_keyed::kind::StartState;
use blaze_sim::{Broke, Config, Tape};

use super::support::CONTROL;
use crate::{Digest, Subject, What, chain::Chain, macro_call::StateOut};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// The driver drops the reruns the chain names.
    Reruns,
}

#[derive(Clone, Debug)]
struct Plan {
    /// Whether each call moves the state (else it leaves it as it found it).
    moves: Vec<bool>,
}

fn plan(tape: &Tape) -> Plan {
    Plan { moves: (0..tape.int_in(1..=10u64, "calls")).map(|_| tape.coin("moves")).collect() }
}

/// What call `at` leaves when it starts from `with`: a function of both, as an expander's
/// statics are of its input and the statics it found.
fn effect(moves: &[bool], at: usize, with: StartState) -> StateOut {
    if !moves[at] {
        return StateOut::Unchanged;
    }
    let found = match with {
        StartState::Fresh => [0; 32],
        StartState::Statics(it) | StartState::Replayed(it) => it.0,
    };
    let mut bytes = found.to_vec();
    bytes.extend_from_slice(&(at as u64).to_le_bytes());
    StateOut::Moved(StartState::Statics(Digest::of("test statics", &bytes)))
}

/// The plan's calls, their runs ending in the order `tape` chooses.
fn run(plan: &Plan, tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let harness = |it: crate::chain::Misstep| Broke::Harness(it.to_string());
    let n = plan.moves.len();
    let mut chain = Chain::new(Subject { what: What::ProcMacro, name: Arc::from("counter!") }, StartState::Fresh, n);
    // Runs in flight: the call and the state it runs with.
    let mut running = Vec::new();
    for at in 0..n {
        running.push((at, chain.start(at).map_err(harness)?));
    }
    let bound = 4 * n + 4;
    for _ in 0..bound {
        if running.is_empty() {
            break;
        }
        let (at, with) = running.swap_remove(tape.index(running.len(), "which run ends"));
        let reruns = chain.end(at, with, effect(&plan.moves, at, with)).map_err(harness)?;
        if plant != Plant::Reruns {
            running.extend(reruns);
        }
    }
    if !running.is_empty() {
        return Err(Broke::Oracle(format!("{} runs still in flight after {bound} ends", running.len())));
    }
    if !chain.settled() {
        return Err(Broke::Oracle("every run ended, and the chain is not settled".into()));
    }
    let mut state = StartState::Fresh;
    for at in 0..n {
        let left = match effect(&plan.moves, at, state) {
            StateOut::Unchanged => state,
            StateOut::Moved(it) => it,
        };
        if chain.confirmed(at) != Some((state, left)) {
            return Err(Broke::Oracle(format!(
                "call {at} confirmed {:?}, in source order it runs with {state:?} and leaves {left:?}",
                chain.confirmed(at)
            )));
        }
        if chain.runs(at) > 2 {
            return Err(Broke::Oracle(format!("call {at} ran {} times", chain.runs(at))));
        }
        state = left;
    }
    Ok(())
}

/// The invariant: whatever order runs end in, the chain settles on the source-order states,
/// each call run at most twice.
#[test]
fn a_chain_settles_on_source_order_with_at_most_two_runs() {
    blaze_sim::check(Config::cases(300), |tape| {
        let plan = plan(tape);
        tape.note(format_args!("plan {plan:?}"));
        run(&plan, tape, Plant::None)
    });
}

/// The minimal case the property test shrank to on 2026-11-13: call 0 moves the state and ends
/// first while call 1 runs from the stale prediction; the chain then handed call 1 a second run
/// beside the first, and the stale run's end came after the confirmed one's, an end of a call no
/// longer running. Its schedule was seed 6's, which `Tape::seeded(6)` replays draw for draw.
#[test]
fn a_stale_run_ending_after_its_rerun_is_not_a_misstep() {
    let plan = Plan { moves: vec![true, false] };
    if let Err(broke) = run(&plan, &Tape::seeded(6), Plant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

#[test]
fn the_oracle_catches_reruns_dropped() {
    blaze_sim::control(CONTROL, Plant::None, Plant::Reruns, "not settled", |tape, plant| {
        run(&plan(tape), tape, plant)
    });
}
