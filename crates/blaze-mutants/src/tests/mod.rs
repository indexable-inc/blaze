//! The crate's tests: a deterministic simulation of mutation sessions over a model program
//! ([`world`]), the planted-bug controls that show its oracles catch what they are for, and Hegel
//! properties of the operators, the records and the planner ([`props`]).
//!
//! Replay a failure by its seed: `world::run_seed(seed, plant)` runs the same sessions.

mod props;
mod world;

use crate::Plant;

/// With no plant, every oracle holds on every seed.
///
/// The invariant: for 64 seeds, every session of [`world::run_seed`] passes every oracle.
#[test]
fn sessions_hold_every_oracle() {
    for seed in 0..64 {
        if let Err(why) = world::run_seed(seed, Plant::None) {
            panic!("seed {seed}: {why}");
        }
    }
}

/// Each planted bug is caught by some seed. A plant that panics counts as caught: it stopped
/// loudly.
///
/// The invariant: for each plant, the oracles of [`world::run_seed`] fail on at least one of
/// the first 64 seeds.
#[test]
fn controls() {
    let plants = [Plant::RunsUnreachingTests, Plant::VerdictKeyWithoutTests, Plant::AridSwallowsReal];
    for plant in plants {
        let mut caught = None;
        for seed in 0..64 {
            match std::panic::catch_unwind(|| world::run_seed(seed, plant)) {
                Ok(Ok(())) => {}
                Ok(Err(why)) => {
                    caught = Some(why);
                    break;
                }
                Err(_) => {
                    caught = Some(format!("seed {seed} panicked"));
                    break;
                }
            }
        }
        match caught {
            Some(why) => eprintln!("{plant:?} caught: {why}"),
            None => panic!("the planted bug {plant:?} passed every oracle on 64 seeds"),
        }
    }
}
