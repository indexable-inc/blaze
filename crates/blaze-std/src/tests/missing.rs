//! A capability the host withholds is missing at the root, never halfway through an op: each
//! root accessor of `Io` is `Some` exactly when the host offers it, and its `need_` form is
//! `Ok` then and otherwise a `Missing` naming it, whatever is offered.
//!
//! Plant: [`Plant::OffersIgnored`], accessors that hand out what the host does not offer.

use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control};
use crate::{
    Plant,
    io::{Io, Missing, Offers, SimWorld},
};

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// One bit an accessor, in the order of [`NAMES`].
    offered: u64,
}

const NAMES: [&str; 8] = ["fs", "net", "env", "args", "clock", "rng", "mem", "host"];

fn plan(d: &mut impl Draw) -> Plan {
    Plan { seed: d.int(0, u64::MAX), offered: d.int(0, (1 << NAMES.len()) - 1) }
}

/// Whether `io` hands out the capability `name`, and what its `need_` form says.
fn asked(io: &Io, name: &str) -> (bool, Result<(), Missing>) {
    fn need<T>(got: Result<T, Missing>) -> Result<(), Missing> {
        got.map(|_| ())
    }
    match name {
        "fs" => (io.fs().is_some(), need(io.need_fs())),
        "net" => (io.net().is_some(), need(io.need_net())),
        "env" => (io.env().is_some(), need(io.need_env())),
        "args" => (io.args().is_some(), need(io.need_args())),
        "clock" => (io.clock().is_some(), need(io.need_clock())),
        "rng" => (io.rng().is_some(), need(io.need_rng())),
        "mem" => (io.mem().is_some(), need(io.need_mem())),
        _ => (io.host().is_some(), need(io.need_host())),
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let bit = |at: usize| plan.offered & (1 << at) != 0;
    let offers = Offers {
        fs: bit(0),
        net: bit(1),
        env: bit(2),
        args: bit(3),
        clock: bit(4),
        rng: bit(5),
        mem: bit(6),
        host: bit(7),
    };
    let mut world = SimWorld::new(plan.seed);
    world.offers = offers;
    let io = Io::planted(world, plant);
    if io.offers() != offers {
        return Err(format!("the world offers {:?}, not the {offers:?} it was made with", io.offers()));
    }
    for (at, name) in NAMES.iter().enumerate() {
        let offered = bit(at);
        let (handed, needed) = asked(&io, name);
        if handed != offered {
            return Err(format!("io.{name}() is {} though the host {} it", if handed { "Some" } else { "None" }, if offered { "offers" } else { "withholds" }));
        }
        match (offered, needed) {
            (true, Ok(())) => {}
            (false, Err(Missing { capability })) if capability == *name => {}
            (_, needed) => return Err(format!("io.need_{name}() gave {needed:?} with it offered {offered}")),
        }
    }
    Ok(())
}

#[hegel::test(test_cases = 100)]
fn a_withheld_capability_is_missing_and_named(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_accessors_that_ignore_the_offers() {
    control(Plant::OffersIgnored, |seeds: &mut Seeds| plan(seeds), run);
}

#[test]
fn sim_offers_everything_and_sim_with_withholds() {
    assert_eq!(Io::sim(1).offers(), Offers::ALL);
    let io = Io::sim_with(1, Offers { net: false, ..Offers::ALL });
    assert!(io.net().is_none());
    assert!(io.fs().is_some());
    let missing = io.need_net().expect_err("net is withheld");
    assert_eq!(missing, Missing { capability: "net" });
    assert!(missing.to_string().contains("`net`"), "{missing}");
}
