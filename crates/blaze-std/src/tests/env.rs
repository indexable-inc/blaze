//! `Env::only` never widens: after any chain of narrowings, a variable reads only if every
//! narrowing in the chain named it, and then reads as the world set it.
//!
//! Plant: [`Plant::EnvOnlyReplaces`], `only` replacing what is allowed rather than narrowing.

use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control};
use crate::{
    Plant,
    env::VarName,
    io::{Io, Limits, SimWorld},
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
    let mut world = SimWorld::new(plan.seed, Limits::USUAL);
    // Every one but UNSET is set.
    world.env = VARS[..4].iter().map(|name| ((*name).to_owned(), format!("{name} value"))).collect();
    let io = Io::planted(world, plant);
    let mut env = io.need_env().map_err(|e| format!("harness: {e}"))?;
    let names: Vec<VarName> = VARS.iter().map(|raw| VarName::new(raw).expect("a test's variable name")).collect();
    let mut allowed: u64 = (1 << VARS.len()) - 1;
    for (at, &mask) in plan.chain.iter().enumerate() {
        let kept: Vec<VarName> =
            (0..VARS.len()).filter(|i| mask & (1 << i) != 0).map(|i| names[i].clone()).collect();
        env = env.only(&kept);
        allowed &= mask;
        for (i, name) in names.iter().enumerate() {
            let may = allowed & (1 << i) != 0;
            let want = (may && name.as_str() != "UNSET").then(|| format!("{name} value"));
            // A variable narrowed away is refused, never read as unset.
            let got = env.get(name);
            if got.is_ok() != may {
                return Err(format!("after narrowing {at}, a get of {name} gave {got:?}, but the chain allows it: {may}"));
            }
            if got.ok().flatten().map(str::to_owned) != want || env.may_read(name) != may {
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

/// The invariant: `Env::only` never widens. After any chain of narrowings a variable reads
/// only if every narrowing in the chain named it, and then reads as the world set it.
#[hegel::test(test_cases = 200)]
fn env_only_never_widens(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_only_that_replaces() {
    control(Plant::EnvOnlyReplaces, |seeds: &mut Seeds| plan(seeds), run);
}
