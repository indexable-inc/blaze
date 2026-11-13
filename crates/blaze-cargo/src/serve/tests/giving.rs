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

/// The idle budget's model: `/w`'s front end 360 MiB (60 of values), 20 MiB the daemon needs
/// besides, 8 MiB the allocators keep freed, and the process-wide caches: 30 MiB of warm
/// chunks, 4 of flight names, 200 of process tables. Under a limit of 64 GiB, so nothing is
/// short until the daemon is held to its budget. Its remake took `remake`.
fn idle_model(remake: Duration) {
    sim::REMAKE.set(Some(remake));
    sim::FRONTS.with_borrow_mut(|it| {
        it.insert("/w".to_owned(), Front { core: 300 * MIB, values: 60 * MIB }) // alloc: a test's model
    });
    sim::TOLD.set(Some(Told {
        limit: 64 * GIB,
        other: 20 * MIB,
        kept: 8 * MIB,
        chunks: 30 * MIB,
        names: 4 * MIB,
        tables: 200 * MIB,
        ..Told::default()
    }));
}

/// A daemon that built `/w` once, its remake said to have taken `remake` (the simulation's
/// lowering takes no time), with `plant` planted: its front end is kept.
fn built_idle(seed: u64, remake: Duration, plant: Plant) -> (SimDaemon, Client) {
    sim::reset();
    sim::REMAKE.set(Some(remake));
    let (daemon, client) = built_keeping(seed);
    // Planted once the build is done: a wait of zero then would hold the build's own turns,
    // on the test process's memory, to the budget.
    sim::PLANT.set(plant);
    idle_model(remake);
    (daemon, client)
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

/// The daemon is left alone for `long`, turning every `step`: bounded by `long / step` turns.
fn idle_for(daemon: &mut SimDaemon, client: &mut Client, long: Duration, step: Duration) {
    let mut gone = Duration::ZERO;
    while gone < long {
        daemon.driver().advance(step);
        daemon.turn();
        client.take(daemon);
        gone += step;
    }
}

/// The idle budget's oracle: the told footprint is at most [`idle::BUDGET`], and the driver's
/// last listing has nothing left to give. The pools' pages are left out (the simulation's
/// stores are pumped, and the rung hangs on the test process's other tests, as [`gave`]
/// says), and so are live link images, which are never listed. What is held past it, by name.
fn held_to_budget() -> Result<(), String> {
    let footprint = sim::footprint();
    let left: Vec<String> = sim::LISTED.with_borrow(|it| {
        it.iter()
            .filter(|it| it.rung != Rung::Pool && it.gives != 0)
            .map(|it| format!("{:?} of owner {}: {} bytes", it.rung, it.owner, it.gives)) // alloc: a test's record
            .collect()
    });
    let fronts: Vec<String> = sim::FRONTS.with_borrow(|it| it.keys().cloned().collect()); // clone: a test's record
    if footprint <= idle::BUDGET && left.is_empty() && fronts.is_empty() {
        return Ok(());
    }
    Err(format!("footprint {footprint} of a budget of {}; still listed: {left:?}; front ends kept: {fronts:?}", idle::BUDGET))
}

fn gives_down(seed: u64, plant: Plant) -> Result<(), String> {
    let (mut daemon, mut client) = built_idle(seed, Duration::from_millis(100), plant);
    // The budget's wait, and time for every rung's give and its measure after it.
    idle_for(&mut daemon, &mut client, idle::BUDGET_AFTER + Duration::from_secs(20), Duration::from_millis(100));
    let held = held_to_budget();
    sim::reset();
    held
}

/// The invariant: after [`idle::BUDGET_AFTER`] with no request (and a front end whose remake
/// is cheap, so its own wait is that too), every source lists `gives == 0` or is a live
/// image, and the told footprint is at most [`idle::BUDGET`]: the front end went whole, the
/// process tables after it.
#[test]
fn an_idle_daemon_gives_down_to_its_budget() {
    for seed in 400..403 {
        if let Err(held) = gives_down(seed, Plant::Nothing) {
            panic!("seed {seed}: the idle daemon is past its budget: {held}");
        }
    }
}

/// The control (test-the-cause): a floor that ignores the idle wait keeps the front end,
/// and the oracle names it.
#[test]
fn the_oracle_catches_an_idle_daemon_that_keeps_its_floor() {
    let held = gives_down(400, Plant::IdleKeepsFloor).expect_err("the plant was not caught");
    assert!(held.contains("/w"), "the oracle does not name the front end kept: {held}");
}

/// Whether an edit `wait` into an idle stretch lost anything of value: the gives of values
/// and front ends, and how its build lowered (anew, into the kept front end). The front end
/// is 36 GiB and the daemon 46 of a limit of 67: under the limit's line, nothing short.
fn edit_after(seed: u64, wait: Duration, plant: Plant) -> (Vec<(Rung, String, u64)>, usize, usize) {
    let (mut daemon, mut client) = built_idle(seed, Duration::from_millis(100), plant);
    sim::FRONTS.with_borrow_mut(|it| {
        it.insert("/w".to_owned(), Front { core: 30 * GIB, values: 6 * GIB }) // alloc: a test's model
    });
    sim::TOLD.set(Some(Told { limit: 67 * GIB, other: 10 * GIB, ..Told::default() }));
    idle_for(&mut daemon, &mut client, wait, Duration::from_millis(100));
    let (anew, into_kept) = (ANEW.get(), INTO_KEPT.get());
    let second = client.send(&mut daemon, build());
    let got = client.wait(&mut daemon, second);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
    let valued: Vec<(Rung, String, u64)> =
        gave().into_iter().filter(|it| matches!(it.0, Rung::Values | Rung::FrontEnd)).collect();
    let cost = (valued, ANEW.get() - anew, INTO_KEPT.get() - into_kept);
    sim::reset();
    cost
}

/// The invariant: before [`idle::BUDGET_AFTER`] no give of `Values` or `FrontEnd` is planned
/// at a footprint under the ceiling's line, and an edit then lowers into the kept front end.
#[test]
fn an_edit_inside_the_wait_loses_nothing_of_value() {
    let inside = idle::BUDGET_AFTER - Duration::from_secs(1);
    for seed in 410..413 {
        assert_eq!(edit_after(seed, inside, Plant::Nothing), (Vec::new(), 0, 1), "seed {seed}"); // alloc: none
    }
}

/// The control: with the wait set to zero the idle daemon gives the front end at once, and the
/// edit lowers anew.
#[test]
fn the_oracle_catches_an_idle_budget_with_no_wait() {
    let (valued, anew, into_kept) = edit_after(410, idle::BUDGET_AFTER - Duration::from_secs(1), Plant::NoWait);
    assert!(valued.iter().any(|it| it.0 == Rung::FrontEnd), "the plant was not caught: {valued:?}");
    assert_eq!((anew, into_kept), (1, 0), "the plant was not caught");
}

/// At each give of the process tables, how many front ends were kept, once the daemon was
/// idle well past its budget's wait.
fn tables_given(seed: u64, plant: Plant) -> Vec<usize> {
    let (mut daemon, mut client) = built_idle(seed, Duration::from_millis(100), plant);
    idle_for(&mut daemon, &mut client, 2 * idle::BUDGET_AFTER, Duration::from_millis(100));
    let given = sim::TABLES_GIVEN.with_borrow(Clone::clone); // clone: a test's record
    sim::reset();
    given
}

/// The invariant: no give of the process tables while a front end, a kept link image or a
/// walk exists (the simulation keeps no image and runs no walk once idle: the front end is
/// what it tests), and they are given once none is.
#[test]
fn the_process_tables_go_only_after_every_front_end_walk_and_image() {
    for seed in 420..423 {
        let given = tables_given(seed, Plant::Nothing);
        assert!(!given.is_empty(), "seed {seed}: the process tables were never given");
        assert!(given.iter().all(|&fronts| fronts == 0), "seed {seed}: given with front ends kept: {given:?}");
    }
}

/// The control: tables listed whatever else is kept are given beside the front end.
#[test]
fn the_oracle_catches_tables_given_beside_a_front_end() {
    let given = tables_given(420, Plant::TablesAlways);
    assert!(given.iter().any(|&fronts| fronts > 0), "the plant was not caught: {given:?}");
}

/// What of `/w`'s kept front end is left at `at` into an idle stretch, its remake said to have
/// taken `remake`: its bytes, and whether the driver's last listing made it the floor.
fn front_at(seed: u64, remake: Duration, plant: Plant, at: Duration) -> (u64, bool) {
    let (mut daemon, mut client) = built_idle(seed, remake, plant);
    idle_for(&mut daemon, &mut client, at, Duration::from_millis(250));
    let floor = sim::LISTED.with_borrow(|it| it.iter().any(|it| it.rung == Rung::FrontEnd && it.floor));
    let left = (sim::held("/w"), floor);
    sim::reset();
    left
}

/// The invariant (`idle::front_after`): a front end waits 64 times its last remake before it
/// loses its floor, within 30 s and 30 minutes. With a remake of 20 s it is still kept whole,
/// values and all, and the floor at 60 s idle, and gone at 30 minutes; with one of 100 ms it is
/// gone at 30 s (and the time its gives take).
#[test]
fn a_front_end_that_is_dear_to_remake_waits_longer() {
    let whole = 360 * MIB;
    let dear = Duration::from_secs(20);
    assert_eq!(front_at(430, dear, Plant::Nothing, Duration::from_secs(60)), (whole, true), "the dear one at 60 s");
    assert_eq!(front_at(430, dear, Plant::Nothing, idle::FRONT_MOST).0, 0, "the dear one at 30 minutes");
    let cheap = Duration::from_millis(100);
    assert_eq!(front_at(430, cheap, Plant::Nothing, idle::BUDGET_AFTER + Duration::from_secs(5)).0, 0, "the cheap one");
}

/// The control: the one constant wait this replaced takes the dear one's floor (and its values)
/// at 30 s.
#[test]
fn the_oracle_catches_a_constant_front_end_wait() {
    let left = front_at(430, Duration::from_secs(20), Plant::ConstantWait, Duration::from_secs(60));
    assert_ne!(left, (360 * MIB, true), "the plant was not caught");
}

/// What an idle stretch past `/w`'s front end's wait left (R3, rule (b)): whether the front end
/// was let go, how many second stores of its members were made, and whether those pages are
/// in the store after a crash right then (`Sim::crash` tears what was not synced) and a start.
fn let_go_idle(seed: u64, plant: Plant) -> (bool, usize, bool) {
    MEMBER_STORES.set(0);
    let (mut daemon, mut client) = built_idle(seed, Duration::from_millis(100), plant);
    idle_for(&mut daemon, &mut client, idle::BUDGET_AFTER + Duration::from_secs(20), Duration::from_millis(100));
    let let_go = sim::held("/w") == 0;
    let stores = MEMBER_STORES.get();
    sim::reset();
    let mut disk = daemon.into_driver();
    disk.crash();
    let steps = Arc::new(Fake::default());
    let restarted = start(disk, &steps, seed + 1000);
    (let_go, stores, holds_members(restarted.store(), "/w"))
}

/// The invariant: an idle front end is let go only once its workspace members' pages are
/// written and durable: after the idle wait it is gone, its members were stored once, and a
/// crash at that moment keeps their pages, so the next daemon reads its members from them.
#[test]
fn an_idle_front_end_is_let_go_after_its_wait_and_its_members_pages_are_durable_first() {
    for seed in 440..444 {
        assert_eq!(let_go_idle(seed, Plant::Nothing), (true, 1, true), "seed {seed}");
    }
}

/// The control: a driver that lets the front end go with its members unwritten loses their
/// pages, which the oracle above sees.
#[test]
fn the_oracle_catches_a_front_end_let_go_before_its_members_are_written() {
    let (let_go, stores, durable) = let_go_idle(450, Plant::DropsUnwritten);
    assert!(let_go, "the planted driver let the front end go");
    assert_eq!((stores, durable), (0, false), "the plant was not caught");
}
