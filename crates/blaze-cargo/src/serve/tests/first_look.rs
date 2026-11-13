//! The first request on a checkout the daemon has not watched yet, on the simulation's daemon:
//! it waits for the watcher's own look (its start's sweep of the checkout), asks the journal
//! for no flush, and sees every change made before it, on an empty home and on a daemon
//! restored with a saved cursor whose checkout changed while no daemon ran.

use blaze_core::sim::Journal;

use super::{super::heads::plant_passes_before_the_look, *};

/// How many seeds each half runs.
const SEEDS: u64 = 64;

fn check() -> Request {
    Request::Check { view: ViewRef::Head("/w".into()), targets: agent() }
}

/// The first request of a daemon on `/w`: whether it built with the manifest last saved,
/// `restored` on a daemon started again after `/w` changed while it was down, `planted` with
/// that request handed on before the look (the restored daemon's first build is not).
fn first_request(seed: u64, restored: bool, planted: bool) -> Result<(), String> {
    plant_passes_before_the_look(false);
    let mut sim = Sim::new(seed, Faults::default());
    sim.set_journal(Journal::FsEvents);
    checkout(&mut sim, "before\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    let saved = match restored {
        false => "before\n",
        true => {
            let mut client = dial(&mut daemon);
            let id = client.send(&mut daemon, check());
            let got = client.wait(&mut daemon, id);
            assert_eq!(warnings(&got), vec!["before\n".to_owned()], "seed {seed}: {got:#?}");
            daemon.stop();
            let mut turns = 0;
            while daemon.turn() {
                turns += 1;
                assert!(turns < TURNS, "the daemon never stopped");
            }
            let mut sim = daemon.into_driver();
            sim.change(&name("w/Cargo.toml"), b"changed while down\n");
            daemon = start(sim, &steps, seed);
            "changed while down\n"
        }
    };
    plant_passes_before_the_look(planted);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, check());
    let got = client.wait(&mut daemon, id);
    plant_passes_before_the_look(false);
    match (got.last(), warnings(&got)) {
        (Some(Response::Done { ok: true }), seen) if seen == [saved] => Ok(()),
        (_, seen) => Err(format!("seed {seed}, restored {restored}: saw {seen:?}, not {saved:?}: {got:#?}")),
    }
}

/// The invariant: a daemon's first request on a checkout sees every change made before it,
/// with no flush of the journal asked for it: the watcher's own look stands for one. On an
/// empty home, and on a restored daemon whose checkout changed while it was down. Control:
/// handed on before the look commits, the request builds old bytes or none.
#[test]
fn a_first_request_sees_every_change_made_before_it_without_a_flush() {
    for restored in [false, true] {
        for seed in 0..SEEDS {
            first_request(seed, restored, false).unwrap_or_else(|why| panic!("{why}"));
        }
        let caught = (0..SEEDS).find_map(|seed| first_request(seed, restored, true).err());
        assert!(
            caught.is_some(),
            "no seed caught a first request handed on before the look (restored {restored})"
        );
    }
}
