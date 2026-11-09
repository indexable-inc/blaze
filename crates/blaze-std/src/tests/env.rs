//! `Env::only` never widens: after any chain of narrowings, a variable reads only if every
//! narrowing in the chain named it, and then reads as the world set it.
//!
//! Plant: [`Plant::EnvOnlyReplaces`], `only` replacing what is allowed rather than narrowing.

use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control};
use crate::{
    Plant,
    io::{Io, SimWorld},
};

const VARS: [&str; 5] = ["HOME", "PATH", "TOKEN", "LANG", "UNSET"];

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// Each narrowing, as a mask over [`VARS`].
    chain: Vec<u64>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let chain = (0..d.int(0, 5)).map(|_| d.int(0, (1 << VARS.len()) - 1)).collect();
    Plan { seed, chain }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let mut world = SimWorld::new(plan.seed);
    // Every one but UNSET is set.
    world.env = VARS[..4].iter().map(|name| ((*name).to_owned(), format!("{name} value"))).collect();
    let io = Io::planted(world, plant);
    let mut env = io.env();
    let mut allowed: u64 = (1 << VARS.len()) - 1;
    for (at, &mask) in plan.chain.iter().enumerate() {
        let names: Vec<&str> = (0..VARS.len()).filter(|i| mask & (1 << i) != 0).map(|i| VARS[i]).collect();
        env = env.only(&names);
        allowed &= mask;
        for (i, name) in VARS.iter().enumerate() {
            let may = allowed & (1 << i) != 0;
            let want = (may && *name != "UNSET").then(|| format!("{name} value"));
            if env.get(name).map(str::to_owned) != want || env.may_read(name) != may {
                return Err(format!(
                    "after narrowing {at}, {name} reads {:?} (may read: {}), but the chain allows it: {may}",
                    env.get(name),
                    env.may_read(name)
                ));
            }
        }
    }
    Ok(())
}

#[hegel::test(test_cases = 200)]
fn env_only_never_widens(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_only_that_replaces() {
    control(Plant::EnvOnlyReplaces, |seeds: &mut Seeds| plan(seeds), run);
}
