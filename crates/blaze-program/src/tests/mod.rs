//! The crate's tests: a deterministic simulation of programs under edit ([`world`]) over a
//! model front end and code maker ([`model`]), the planted-bug controls that show its oracles
//! catch what they are for ([`controls`]), and Hegel properties of the key rules ([`props`]).
//!
//! Replay a failure by its seed: `run_seed(seed, steps, plant)` gives the same steps.

mod model;
mod props;
mod world;

use crate::Plant;

/// Each planted bug is caught by some seed. A plant that panics (a maker asked for an instance
/// the model has not, a write past a section) counts as caught: it stopped loudly.
///
/// The invariant: for each plant, the oracles of [`world::World::step`] and
/// [`world::World::check`] fail on at least one of the first 64 seeds.
#[test]
fn controls() {
    let plants = [Plant::InlineeLeftOut, Plant::SeenDrops, Plant::SiteNotRepatched, Plant::ReachWithoutStatics];
    for plant in plants {
        let mut caught = None;
        for seed in 0..64 {
            let outcome = std::panic::catch_unwind(|| world::run_seed(seed, 12, plant));
            match outcome {
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
