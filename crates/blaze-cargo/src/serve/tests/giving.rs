//! What the daemon gives back when memory is short (`serve::job::giving`), on the
//! simulation's daemon: the real loop, plane and jobs, the fakes' front end, and a model of
//! the daemon's memory in place of the test process's own (`job::giving::sim`). The rule
//! itself is held by its own simulation (`blaze_graph::giveback::tests`); here, that the
//! driver carries it out: it gives only what the planner planned, a kept front end's values
//! go on a worker, and a build asked meanwhile waits for the front end and lowers into it.
//!
//! The model: the footprint is what the test says besides the front ends, what the
//! allocators keep freed, and each kept front end's core and values; every turn of the
//! driver is a reading; a purge returns the freed pages at once. A task waits for what the
//! footprint and its own bytes are past the limit.

use blaze_graph::giveback::Rung;

use super::{
    super::job::giving::sim::{self, Front, Told},
    *,
};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The most turns a wait of these tests takes, a millisecond each: far more than any needs.
const MOST: usize = 5_000;

fn build() -> Request {
    Request::Build { view: ViewRef::Head("/w".into()), targets: agent() }
}

/// A daemon that built `/w` once, with what the build left settled: its front end is kept.
fn built(seed: u64) -> (SimDaemon, Client) {
    sim::reset();
    let mut disk = Sim::new(seed, Faults::default());
    checkout(&mut disk, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(disk, &steps, seed);
    let mut client = dial(&mut daemon);
    let first = client.send(&mut daemon, build());
    let got = client.wait(&mut daemon, first);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
    settle(&mut daemon, &mut client);
    (daemon, client)
}

/// The daemon of 2026-11-12, 15:15:02, as the model: 55 GiB held of a limit of 67.08 GiB,
/// `/w`'s front end 36 GiB of it (6 of values), `freed` GiB of the rest pages the allocators
/// keep freed, and a task that waits for 0.09 GiB.
fn incident(freed: u64) {
    sim::FRONTS.with_borrow_mut(|it| {
        it.insert("/w".to_owned(), Front { core: 30 * GIB, values: 6 * GIB }) // alloc: a test's model
    });
    sim::TOLD.set(Some(Told {
        limit: 67 * GIB + 82 * MIB,
        other: (19 - freed) * GIB,
        kept: freed * GIB,
        stuck: 0,
        task: 12 * GIB + 174 * MIB,
    }));
}

/// A millisecond passes and the daemon turns, until `done` says so.
fn pass_until(daemon: &mut SimDaemon, client: &mut Client, what: &str, mut done: impl FnMut(&mut SimDaemon) -> bool) {
    for _ in 0..MOST {
        if done(daemon) {
            return;
        }
        daemon.driver().advance(Duration::from_millis(1));
        daemon.turn();
        client.take(daemon);
    }
    panic!("the daemon never got there: {what}");
}

/// What the driver gave, the pool's pages left out: the simulation's stores are pumped and
/// give none, and whether the rung is asked hangs on what the test process's other tests
/// hold in the `store` tag.
fn gave() -> Vec<(Rung, String, u64)> {
    sim::GAVE.with_borrow(|it| it.iter().filter(|it| it.0 != Rung::Pool).cloned().collect()) // alloc: a test's record
}

/// What a small wait cost: what the driver gave, and how the checkout's next build lowered
/// (anew, into the kept front end).
#[derive(Debug, PartialEq, Eq)]
struct Cost {
    gave: Vec<(Rung, String, u64)>,
    anew: usize,
    into_kept: usize,
}

/// The incident with 19 GiB of freed pages held, then the checkout's next build.
fn small_wait(seed: u64, no_plan: bool) -> Cost {
    let (mut daemon, mut client) = built(seed);
    incident(19);
    sim::PLANT_NO_PLAN.set(no_plan);
    // Long enough for the purge to be asked, land and be read, and for anything after it.
    let mut turns = 0;
    pass_until(&mut daemon, &mut client, "fifty turns under the model", |_| {
        turns += 1;
        turns > 50
    });
    let (anew, into_kept) = (ANEW.get(), INTO_KEPT.get());
    let second = client.send(&mut daemon, build());
    let got = client.wait(&mut daemon, second);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
    let cost = Cost { gave: gave(), anew: ANEW.get() - anew, into_kept: INTO_KEPT.get() - into_kept };
    sim::reset();
    cost
}

/// The invariant: a task that waits for less than the allocators hold freed costs a purge
/// and nothing else; the kept front end stays, and the checkout's next build lowers into it.
#[test]
fn a_small_wait_costs_a_purge_and_the_next_build_lowers_into_the_kept_front_end() {
    for seed in 300..310 {
        let cost = small_wait(seed, false);
        let purge = vec![(Rung::Allocator, String::new(), 19 * GIB)]; // alloc: a test's expectation
        assert_eq!(cost, Cost { gave: purge, anew: 0, into_kept: 1 }, "seed {seed}");
    }
}

/// The control (test-the-cause): a driver that gives with no plan, as the daemon did until
/// 2026-11-12, drops the front end for the same wait, and the next build lowers anew.
#[test]
fn the_oracle_catches_a_driver_that_gives_without_a_plan() {
    let cost = small_wait(300, true);
    assert_eq!(cost, Cost { gave: Vec::new(), anew: 1, into_kept: 0 }, "the plant was not caught"); // alloc: none
}

/// The invariant: with nothing cheaper to give, a kept front end's values go on a worker, in
/// proportion to the wait (0.09 GiB and the 64 MiB margin: 156 MiB), and the front end is
/// never dropped. A build asked while the give is out waits for the front end, the give
/// stops for it at once, and the build lowers into the front end; the give is then made
/// again and takes the values.
#[test]
fn values_go_on_a_worker_and_a_build_asked_meanwhile_waits_and_lowers_into_the_front_end() {
    for seed in 310..320 {
        let (mut daemon, mut client) = built(seed);
        incident(0);
        daemon.hold_tasks_for_test(blaze_graph::admission::kinds::GIVE_BACK, true);
        pass_until(&mut daemon, &mut client, "the give of values is posted", |daemon| {
            daemon.giving_values() > 0
        });
        let asked = 156 * MIB;
        assert_eq!(gave(), vec![(Rung::Values, "/w".to_owned(), asked)], "seed {seed}"); // alloc: a test's expectation
        // The checkout's next build is asked while the give is held on its worker.
        let (anew, into_kept) = (ANEW.get(), INTO_KEPT.get());
        let second = client.send(&mut daemon, build());
        turn_until(&mut daemon, |daemon| {
            client.take(daemon);
            let held =
                daemon.held_lowerings().iter().any(|it| it.checkout == "/w" && it.on.is_some());
            held || client.done(second)
        });
        assert!(
            !client.done(second),
            "seed {seed}: the build never waited for the front end: {:#?}",
            client.of(second)
        );
        daemon.hold_tasks_for_test(blaze_graph::admission::kinds::GIVE_BACK, false);
        let got = client.wait(&mut daemon, second);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        assert_eq!(
            (ANEW.get() - anew, INTO_KEPT.get() - into_kept),
            (0, 1),
            "seed {seed}: the build lowers into the front end the give gave back"
        );
        // The give stopped for the build before its first slice: nothing went.
        assert_eq!(sim::values("/w"), 6 * GIB, "seed {seed}");
        // The task still waits: once the build's front end is kept again the give is made
        // again, and this time it runs.
        settle(&mut daemon, &mut client);
        pass_until(&mut daemon, &mut client, "the values are given", |_| sim::values("/w") < 6 * GIB);
        pass_until(&mut daemon, &mut client, "the front end is back with its checkout", |daemon| {
            daemon.giving_values() == 0
        });
        assert_eq!(sim::values("/w"), 6 * GIB - asked, "seed {seed}");
        assert_eq!(sim::held("/w"), 36 * GIB - asked, "seed {seed}: the front end is kept");
        assert!(gave().iter().all(|it| it.0 != Rung::FrontEnd), "seed {seed}: {:?}", gave());
        sim::reset();
    }
}
