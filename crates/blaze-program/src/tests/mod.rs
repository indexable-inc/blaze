//! The crate's tests: a deterministic simulation of programs under edit ([`world`]) over a
//! model front end and code maker ([`model`]), the planted-bug controls that show its oracles
//! catch what they are for ([`controls`]), and properties of the key rules ([`props`]), every
//! one a function of a `blaze_sim` tape.
//!
//! Replay a failure by the tape its report prints: `run(tape, steps, plant)` gives the same steps.

mod model;
mod props;
mod world;

use std::panic::{AssertUnwindSafe, catch_unwind};

use blaze_sim::{Broke, Config, Tape};

use crate::Plant;

/// `world::run` as a control sees it: an oracle's error, or a panic (a maker asked for an
/// instance the model has not, a write past a section), is the oracle's finding.
fn caught(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    match catch_unwind(AssertUnwindSafe(|| world::run(tape, 12, plant))) {
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

/// Each planted bug is caught by some case. A plant that panics (a maker asked for an instance
/// the model has not, a write past a section) counts as caught: it stopped loudly.
///
/// The invariant: for each plant, the oracles of [`world::World::step`] and
/// [`world::World::check`] fail on at least one of the first 64 seeds.
#[test]
fn controls() {
    let plants = [Plant::InlineeLeftOut, Plant::SeenDrops, Plant::SiteNotRepatched, Plant::ReachWithoutStatics];
    for plant in plants {
        // Each plant is caught by a different oracle (a node that differs, a run that differs,
        // a key that did not move, a panic), so the control asks only that one of them fires.
        blaze_sim::control(Config::cases(64), Plant::None, plant, "", caught);
    }
}
