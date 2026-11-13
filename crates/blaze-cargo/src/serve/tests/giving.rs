//! What the daemon gives back when memory is short (`serve::job::giving`), on the
//! simulation's daemon: the real loop, plane and jobs, the fakes' front end, and a model of
//! the daemon's memory in place of the test process's own (`job::giving::sim`). The rule
//! itself is held by its own simulation (`blaze_graph::giveback::tests`); here, that the
//! driver carries it out: it gives only what the planner planned, a kept front end's values
//! go on a worker, and a build asked meanwhile waits for the front end and lowers into it.
//!
//! The model: the gate's core of a size the test says, used by what the test says besides
//! the front ends, what the allocators keep freed, and each kept front end's core and values;
//! every turn of the driver is a reading; a purge returns the freed pages at once. The driver
//! gives back what the core holds past its line (an eighth under its size).

use blaze_graph::giveback::Rung;

use super::{
    super::{
        idle,
        job::giving::sim::{self, Front, Plant, Told},
    },
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
    built_keeping(seed)
}

/// The daemon of 2026-11-12, 15:15:02, on the gate's core: a core of 64 GiB (its line 56)
/// holding 56.09 GiB, `/w`'s front end 36 GiB of it (6 of values), `freed` GiB of the rest
/// pages the allocators keep freed. 0.09 GiB past the line: a purge of 19 GiB covers it; with
/// none, the target is that and the 64 MiB margin, 156 MiB.
fn incident(freed: u64) {
    sim::FRONTS.with_borrow_mut(|it| {
        it.insert("/w".to_owned(), Front { core: 30 * GIB, values: 6 * GIB }) // alloc: a test's model
    });
    sim::TOLD.set(Some(Told {
        core: 64 * GIB,
        other: (20 - freed) * GIB + 92 * MIB,
        kept: freed * GIB,
        stuck: 0,
        ..Told::default()
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

/// What the driver gave, in order.
fn gave() -> Vec<(Rung, String, u64)> {
    sim::GAVE.with_borrow(|it| it.clone()) // alloc: a test's record
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

/// The invariant: a core past its line by less than the allocators hold freed costs a purge
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
/// proportion to what the core is past its line (0.09 GiB and the 64 MiB margin: 156 MiB), and the front end is
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

/// The idle rule's model (`serve::idle`): `/w`'s front end 360 MiB (60 of values), 20 MiB the
/// daemon needs besides, 8 MiB the allocators keep freed, 30 MiB of warm chunks and 4 of flight
/// names. In a core of 64 GiB, so nothing is short: whatever goes, goes for idleness.
fn idle_model() {
    sim::FRONTS.with_borrow_mut(|it| {
        it.insert("/w".to_owned(), Front { core: 300 * MIB, values: 60 * MIB }) // alloc: a test's model
    });
    sim::TOLD.set(Some(Told { core: 64 * GIB, other: 20 * MIB, kept: 8 * MIB, chunks: 30 * MIB, names: 4 * MIB, ..Told::default() }));
}

/// A daemon that built `/w` once, its remake said to have taken 100 ms (a hello-world's; the
/// simulation's lowering takes no time), with `plant` planted: its front end is kept.
fn built_idle(seed: u64, plant: Plant) -> (SimDaemon, Client) {
    sim::reset();
    MEMBER_STORES.set(0);
    sim::REMAKE.set(Some(Duration::from_millis(100)));
    let (daemon, client) = built_keeping(seed);
    // Planted once the build is done: the build's own turns run on the rule under test.
    sim::PLANT.set(plant);
    idle_model();
    (daemon, client)
}

/// The footprint the idle rule is judged by while the daemon stays: 48 MiB (D29's number,
/// under Relax's 50 MB one-run peak for a hello-world).
const SMALL: u64 = 48 * MIB;

/// What a give for idleness may take: what costs nothing to make again (the allocator's freed
/// pages, warm chunks, flight names). A front end, its values or a kept walk is dear.
fn dear(give: &(Rung, String, u64)) -> bool {
    match give.0 {
        Rung::Allocator => false,
        Rung::Caches => give.1 != "chunks" && give.1 != "flight names",
        Rung::Values | Rung::FrontEnd => true,
    }
}

/// What an idle stretch of `long` did to a daemon: when it began its stop (since the stretch
/// began), the told footprint at its end or at the stop, and the dear gives.
#[derive(Debug)]
struct Stretch {
    stopped_at: Option<Duration>,
    footprint: u64,
    dear: Vec<(Rung, String, u64)>,
}

/// The daemon is left alone for `long`, turning every 100 ms, its client hung up first unless
/// `held`; stops turning at the turn its stop begins.
fn stretch(daemon: &mut SimDaemon, client: &mut Client, long: Duration, held: bool) -> Stretch {
    if !held {
        daemon.driver().peer_hangup(client.conn);
    }
    let step = Duration::from_millis(100);
    let mut gone = Duration::ZERO;
    let mut stopped_at = None;
    while gone < long {
        daemon.driver().advance(step);
        daemon.turn();
        if held {
            client.take(daemon);
        }
        gone += step;
        if daemon.stopping() {
            stopped_at = Some(gone);
            break;
        }
    }
    Stretch { stopped_at, footprint: sim::footprint(), dear: gave().into_iter().filter(dear).collect() }
}

/// The idle rule's oracle, on a stretch of 45 s after the last edit with nothing connected:
/// nothing dear was given for idleness while the daemon stayed, the daemon did not stop before
/// its wait ([`idle::exit_after`] of the 100 ms remake, less a second for the settle before
/// the stretch), and at its end it had stopped or held at most [`SMALL`]. What broke, by name.
fn idle_broken(seen: &Stretch) -> Option<String> {
    let least = idle::exit_after(Duration::from_millis(100)) - Duration::from_secs(1);
    if seen.stopped_at.is_none() && seen.footprint > SMALL {
        return Some(format!("it stayed, holding {} bytes of {SMALL}", seen.footprint)); // alloc: a test's failure
    }
    if seen.stopped_at.is_some_and(|at| at < least) {
        return Some(format!("it stopped at {:?}, before its wait", seen.stopped_at)); // alloc: a test's failure
    }
    if !seen.dear.is_empty() {
        return Some(format!("dear things given for idleness: {:?}", seen.dear)); // alloc: a test's failure
    }
    None
}

fn idle_run(seed: u64, plant: Plant) -> Option<String> {
    let (mut daemon, mut client) = built_idle(seed, plant);
    let seen = stretch(&mut daemon, &mut client, Duration::from_secs(45), false);
    sim::reset();
    idle_broken(&seen)
}

/// The invariant (`serve::idle`'s rule): an idle daemon nothing holds gives back, while it
/// stays, only what costs nothing to make again, and 45 s after its last edit it has begun
/// its stop (a hello-world's 100 ms remake waits the 30 s floor): its footprint is then the
/// process's exit, every byte.
#[test]
fn an_idle_daemon_drops_nothing_dear_and_exits_after_its_wait() {
    for seed in 400..403 {
        if let Some(why) = idle_run(seed, Plant::Nothing) {
            panic!("seed {seed}: {why}");
        }
    }
}

/// The control (the ladder before 2026-11-13, as measured): the daemon stays and drops its
/// front end, and the allocator keeps the pages the drop freed. The give says the front end
/// went; the footprint, which the oracle reads first, does not fall: caught by it.
#[test]
fn the_oracle_catches_a_drop_that_frees_no_pages() {
    let why = idle_run(400, Plant::DropsKeepingPages).expect("the plant was not caught");
    assert!(why.contains("it stayed"), "caught, but not by the footprint: {why}");
}

/// The control: the daemon stays and drops its front end, its pages freed, so it is small;
/// the floor went while the daemon stays, and the next edit pays its remake: caught.
#[test]
fn the_oracle_catches_the_floor_dropped_while_the_daemon_stays() {
    let why = idle_run(400, Plant::DropsFreeing).expect("the plant was not caught");
    assert!(why.contains("dear"), "{why}");
}

/// A body edit `wait` after the last with the client still connected (so the daemon stays):
/// the dear gives and how the build lowered (anew, into the kept front end).
fn edit_after(seed: u64, wait: Duration, plant: Plant) -> (Vec<(Rung, String, u64)>, usize, usize) {
    let (mut daemon, mut client) = built_idle(seed, plant);
    let seen = stretch(&mut daemon, &mut client, wait, true);
    assert_eq!(seen.stopped_at, None, "seed {seed}: a daemon a client holds stopped");
    let (anew, into_kept) = (ANEW.get(), INTO_KEPT.get());
    let second = client.send(&mut daemon, build());
    let got = client.wait(&mut daemon, second);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
    let cost = (seen.dear, ANEW.get() - anew, INTO_KEPT.get() - into_kept);
    sim::reset();
    cost
}

/// The invariant: a body edit after 60 s idle in a daemon that stays remakes no more than one
/// with no idle: no dear give, and its build lowers into the kept front end, as the edit at
/// once does.
#[test]
fn a_body_edit_after_idle_remakes_no_more_than_one_with_no_idle() {
    for seed in 410..413 {
        let at_once = edit_after(seed, Duration::ZERO, Plant::Nothing);
        assert_eq!(at_once, (Vec::new(), 0, 1), "seed {seed}: the edit with no idle"); // alloc: none
        assert_eq!(edit_after(seed, Duration::from_secs(60), Plant::Nothing), at_once, "seed {seed}");
    }
}

/// The control: a daemon that drops its front end for idleness lowers anew at the edit after.
#[test]
fn the_oracle_catches_an_edit_that_remakes_after_idle() {
    let (dear, anew, into_kept) = edit_after(410, Duration::from_secs(60), Plant::DropsFreeing);
    assert!(!dear.is_empty() && (anew, into_kept) == (1, 0), "the plant was not caught: {dear:?} {anew} {into_kept}");
}

/// What the rule allows the first edit after an exit: the start of a daemon, which reads the
/// edited checkout's members from their pages. The invariant: the daemon that exited for
/// idleness wrote its kept front end's members once, durable before it ended (a crash right
/// after its stop keeps them), and the next daemon builds the edit.
#[test]
fn an_exit_for_idleness_leaves_the_members_pages_for_the_next_daemon() {
    for seed in 440..443 {
        let (mut daemon, mut client) = built_idle(seed, Plant::Nothing);
        let seen = stretch(&mut daemon, &mut client, Duration::from_secs(45), false);
        assert!(seen.stopped_at.is_some(), "seed {seed}: it never stopped: {seen:?}");
        let mut turns = 0;
        while daemon.turn() {
            daemon.driver().advance(Duration::from_millis(1));
            turns += 1;
            assert!(turns < MOST, "seed {seed}: its stop did not end");
        }
        assert_eq!(MEMBER_STORES.get(), 1, "seed {seed}: the stop stores the members once");
        sim::reset();
        let mut disk = daemon.into_driver();
        disk.crash();
        let steps = Arc::new(Fake::default());
        let mut next = start(disk, &steps, seed + 1000);
        assert!(holds_members(next.store(), "/w"), "seed {seed}: the members' pages are lost");
        let mut client = dial(&mut next);
        let edit = client.send(&mut next, build());
        let got = client.wait(&mut next, edit);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
    }
}

/// As [`built`], without clearing what the test said first.
fn built_keeping(seed: u64) -> (SimDaemon, Client) {
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
