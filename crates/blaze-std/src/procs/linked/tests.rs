//! The spawner's provider: none in the simulation, refused by name.

use crate::io::{Io, Missing};

/// The simulation offers no spawner: `Host::spawner` is `Missing { capability: "spawner" }`,
/// never a spawner that would start a real process from a simulated world. Prints
/// "spawner: the simulation's is missing".
#[test]
fn the_simulation_has_no_spawner() {
    let host = Io::sim(7).need_host().expect("the simulation's host");
    let missing = host.spawner().err().expect("no spawner in the simulation");
    assert_eq!(missing, Missing { capability: "spawner" });
    std::println!("spawner: the simulation's is missing");
}
