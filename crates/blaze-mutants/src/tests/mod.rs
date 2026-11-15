//! The crate's tests: a deterministic simulation of mutation sessions over a model program
//! ([`world`]), the planted-bug controls that show its oracles catch what they are for, and
//! properties of the operators, the records and the planner ([`props`]), every one a function
//! of a `blaze_sim` tape.
//!
//! Replay a failure by the tape its report prints: `world::run(tape, plant)` runs the same
//! sessions.

mod props;
mod world;

use std::panic::{AssertUnwindSafe, catch_unwind};

use blaze_sim::{Broke, Config, Tape};

use crate::Plant;

/// With no plant, every oracle holds on every case.
///
/// The invariant: for 64 seeds, every session of [`world::run`] passes every oracle.
#[test]
fn sessions_hold_every_oracle() {
    blaze_sim::check(Config::cases(64), |tape| world::run(tape, Plant::None));
}

/// `world::run` as a control sees it: an oracle's error, or a panic, is the oracle's finding.
fn caught(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    match catch_unwind(AssertUnwindSafe(|| world::run(tape, plant))) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(why)) => Err(Broke::Oracle(why)),
        Err(payload) => {
            let text = match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
                (Some(text), _) => (*text).to_string(),
                (None, Some(text)) => text.clone(),
                (None, None) => "a panic whose payload is not text".to_string(),
            };
            Err(Broke::Oracle(format!("panicked: {text}")))
        }
    }
}

/// Each planted bug is caught by some case. A plant that panics counts as caught: it stopped
/// loudly.
///
/// The invariant: for each plant, the oracles of [`world::run`] fail on at least one of
/// the first 64 seeds.
#[test]
fn controls() {
    let plants = [Plant::RunsUnreachingTests, Plant::VerdictKeyWithoutTests, Plant::AridSwallowsReal];
    for plant in plants {
        // Each plant is caught by its own oracle (a test run that does not reach, a mutant made
        // again, a site not decided), so the control asks only that one of them fires.
        blaze_sim::control(Config::cases(64), Plant::None, plant, "", caught);
    }
}
