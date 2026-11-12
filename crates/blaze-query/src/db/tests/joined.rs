//! A cycle that spans tasks is one node computed by one task: a task whose wait for a row would
//! close a cycle of tasks joins the cycle instead of giving way (`function::State::Joined`). It
//! is lent the head's seed, ends the rows it holds as provisional members, hands them to the
//! head's task and waits for the head. Nothing unwinds and nothing runs twice in a round.
//!
//! `Ring` is three queries in a ring: place `at` reads place `at + 1` and is that plus one, up
//! to its own cap (its cell's `b`). From `cycle_initial`'s 0 the least fixpoint is every place
//! at the smallest cap, plus its distance to it, up to its own.
//!
//! Each interleaving is forced, with no sleeps: a task that enters the ring at a place stops
//! inside that place's first run until every entering task is inside its own, and (but the one
//! at place 0) until the task before it in the ring is parked on its flight; so the last to
//! ask is always the one whose wait closes the cycle. A task from outside asks when a run of
//! the ring tells it to, and that run goes on once the outside task is parked on the head.

use std::{sync::atomic::AtomicU32, time::Duration};

use blaze_graph::spin::SpinWait;
use crossbeam_channel::{Receiver, Sender, unbounded};

use super::{
    super::function::probe, AssertUnwindSafe, AtomicBool, AtomicUsize, Cancelled, Cell, CycleKind,
    Function, Ingredient, Ordering, Spec, Storage, TestDb, Vtable, get,
};

/// The places of the ring.
const RING: usize = 3;

/// Every place's cap, unless a test writes another.
const CAP: u64 = 7;

/// The longest a task waits for another's step before the test fails naming it (rule 18); each
/// wait ends in microseconds.
const BOUND: Duration = Duration::from_secs(60);

/// [`Relay::outside`]: no task asks from outside.
const NEVER: u32 = 0;
/// [`Relay::outside`]: the outside task asks while the member it asks for is provisional: after
/// the head's first run read the ring, before its round ends.
const WHILE_PROVISIONAL: u32 = 1;
/// [`Relay::outside`]: the outside task asks for a member the head's second round has emptied
/// and not yet reached: as place 1's second run begins.
const BEFORE_ITS_RUN: u32 = 2;

/// What the tasks of a test tell each other through the ring's runs.
pub(super) struct Relay {
    /// How many tasks enter the ring, each at its own place.
    entering: AtomicUsize,
    /// How many of them are inside their place's run.
    inside: AtomicUsize,
    /// The places a task enters at: its first run there stops as the module says.
    enters: [AtomicBool; RING],
    /// When the outside task asks ([`NEVER`], [`WHILE_PROVISIONAL`], [`BEFORE_ITS_RUN`]); set
    /// back to [`NEVER`] by the run that tells it.
    outside: AtomicU32,
    /// Told to the outside task: ask now.
    asked: (Sender<()>, Receiver<()>),
}

impl Relay {
    pub(super) fn new() -> Relay {
        Relay {
            entering: AtomicUsize::new(0),
            inside: AtomicUsize::new(0),
            enters: [const { AtomicBool::new(false) }; RING],
            outside: AtomicU32::new(NEVER),
            asked: unbounded(),
        }
    }
}

fn until(what: &str, done: impl FnMut() -> bool) {
    SpinWait::within(what, BOUND, done);
}

query!(
    Ring,
    u64,
    |db, key| {
        let at = key.a(db) as usize;
        let cap = key.b(db);
        let ring = db.cells.lock().clone(); // clone: the ring's cells, a test's
        let relay = &db.joined;
        let flight =
            |at: usize| Storage::flight(<Ring as Function>::ingredient().index(), ring[at]);
        if relay.enters[at].swap(false, Ordering::SeqCst) {
            relay.inside.fetch_add(1, Ordering::SeqCst);
            until("every entering task to be inside its place's run", || {
                relay.inside.load(Ordering::SeqCst) == relay.entering.load(Ordering::SeqCst)
            });
            if at != 0 {
                until("the task before it in the ring to wait for this place", || {
                    db.storage.rows.waiters(flight(at)) == 1
                });
            }
        }
        let tell_outside = |when: u32| {
            if relay
                .outside
                .compare_exchange(when, NEVER, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                relay.asked.0.send(()).expect("the relay lives as long as the database");
                until("the outside task to wait for the head", || {
                    db.storage.rows.waiters(flight(0)) == 1
                });
            }
        };
        if at == 1 && db.runs_of("Ring", key.0) == 2 {
            tell_outside(BEFORE_ITS_RUN);
        }
        let next = *get::<Ring>(db, Cell(ring[(at + 1) % RING]));
        if at == 0 && db.runs_of("Ring", key.0) == 1 {
            tell_outside(WHILE_PROVISIONAL);
        }
        (next + 1).min(cap)
    };
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);

/// A database with a ring of the given caps.
fn ring_of(caps: [u64; RING]) -> (TestDb, [Cell; RING]) {
    let db = TestDb::new();
    let ring = [0, 1, 2].map(|at| Cell::new(&db, at as u64, caps[at]));
    db.cells.lock().extend(ring.iter().map(|it| it.0));
    (db, ring)
}

/// How often each place's body ran.
fn runs(db: &TestDb, ring: &[Cell; RING]) -> [usize; RING] {
    ring.map(|cell| db.runs_of("Ring", cell.0))
}

/// The oracle's count: every place's body ran as often as every other's, which is once in each
/// round of the fixpoint, whichever task ran it. A place run by two tasks in one round (one of
/// which gave way and lost its run) ran more often than the head. The rounds, or the counts
/// that differ.
fn once_a_round(db: &TestDb, ring: &[Cell; RING]) -> Result<usize, [usize; RING]> {
    let runs = runs(db, ring);
    if runs.iter().all(|&it| it == runs[0]) { Ok(runs[0]) } else { Err(runs) }
}

/// What one task alone computes for a ring of `caps`, asked from place 0, and its rounds.
fn alone(caps: [u64; RING]) -> ([u64; RING], usize) {
    let (db, ring) = ring_of(caps);
    let values = ring.map(|cell| *get::<Ring>(&db, cell));
    (values, once_a_round(&db, &ring).expect("one task runs each place once a round"))
}

/// `place`'s value as a task with no query below it asks for it, and how often the ask gave
/// way before it held.
fn ask(db: &TestDb, ring: &[Cell; RING], place: usize) -> (u64, usize) {
    let mut gave_way = 0;
    loop {
        match Cancelled::catch(AssertUnwindSafe(|| *get::<Ring>(db, ring[place]))) {
            Ok(value) => return (value, gave_way),
            Err(Cancelled::WouldWaitOnItself { .. }) => gave_way += 1,
            Err(other) => panic!("cancelled: {other}"),
        }
    }
}

/// What a test's tasks got.
struct Asked {
    db: TestDb,
    ring: [Cell; RING],
    /// Each entering task's value, in the order of `entries`, then the outside task's.
    values: Vec<u64>,
    /// How often a task's ask gave way, over every task.
    gave_way: usize,
}

/// One task per place of `entries` enters the ring there, as the module says; with `outside`,
/// one more asks for that place when the ring's runs tell it to. With `plant`, every task but
/// the one at place 0 gives way where it could join ([`probe::GIVES_WAY_FOR_A_CYCLE`]).
fn tasks(entries: &[usize], outside: Option<(u32, usize)>, plant: bool) -> Asked {
    let (db, ring) = ring_of([CAP; RING]);
    db.joined.entering.store(entries.len(), Ordering::SeqCst);
    for &place in entries {
        db.joined.enters[place].store(true, Ordering::SeqCst);
    }
    if let Some((when, _)) = outside {
        db.joined.outside.store(when, Ordering::SeqCst);
    }
    let got: Vec<(u64, usize)> = std::thread::scope(|scope| {
        let (db, ring) = (&db, &ring);
        let entering: Vec<_> = entries
            .iter()
            .map(|&place| {
                scope.spawn(move || {
                    probe::GIVES_WAY_FOR_A_CYCLE.with(|it| it.set(plant && place != 0));
                    ask(db, ring, place)
                })
            })
            .collect();
        let outside = outside.map(|(_, place)| {
            scope.spawn(move || {
                db.joined.asked.1.recv_timeout(BOUND).expect("a run of the ring told it to ask");
                ask(db, ring, place)
            })
        });
        entering.into_iter().chain(outside).map(|it| it.join().expect("the task ended")).collect()
    });
    let values: Vec<u64> = got.iter().map(|it| it.0).collect();
    let gave_way: usize = got.iter().map(|it| it.1).sum();
    Asked { db, ring, values, gave_way }
}

/// The invariant: a cycle of rows that spans tasks is computed by one task, the one whose
/// stack holds its head, and each member's body runs once in each round of it, whichever task
/// ran it; no task gives way, and every task gets the fixpoint one task alone computes.
///
/// Two tasks enter the ring at places 0 and 1. The second, inside place 2, asks for place 0,
/// which the first runs while it waits for place 1: its wait would close the cycle, so it
/// joins. Then one input only a row it handed over read is written: the cycle must run again,
/// which it does only if that row's reads reached the head's edges with it.
#[test]
fn two_tasks_that_enter_a_cycle_at_different_members_run_each_member_once_a_round() {
    let Asked { mut db, ring, values, gave_way } = tasks(&[0, 1], None, false);
    assert_eq!(values, [CAP, CAP]);
    assert_eq!(gave_way, 0, "a task gave way where it could join");
    let rounds = once_a_round(&db, &ring).expect("a place ran more often than another");
    assert!(rounds > 1, "the fixpoint took rounds");
    assert_eq!(ring.map(|cell| *get::<Ring>(&db, cell)), alone([CAP; RING]).0);
    assert_eq!(runs(&db, &ring), [rounds; RING], "a hit ran a place again");

    let caps = [CAP, CAP, CAP - 2];
    ring[2].set_b(&mut db, caps[2]);
    assert_eq!(ring.map(|cell| *get::<Ring>(&db, cell)), alone(caps).0);
}

/// As the invariant above, with three tasks, one inside each place: the last to ask joins the cycle
/// headed at place 0, and the task between them, woken to a row that is that head's, joins in
/// turn rather than wait for a head that waits on it.
#[test]
fn three_tasks_that_enter_a_cycle_at_each_member_run_each_member_once_a_round() {
    let Asked { db, ring, values, gave_way } = tasks(&[0, 1, 2], None, false);
    assert_eq!(values, [CAP; RING]);
    assert_eq!(gave_way, 0, "a task gave way where it could join");
    once_a_round(&db, &ring).expect("a place ran more often than another");
    assert_eq!(ring.map(|cell| *get::<Ring>(&db, cell)), alone([CAP; RING]).0);
}

/// As the invariant above, for a task from outside that asks for a member in the middle of
/// the fixpoint. While the member is provisional the outside task waits for the head and runs
/// nothing: the ring runs exactly as one task alone runs it.
#[test]
fn a_provisional_member_asked_from_outside_is_waited_for_and_runs_nowhere_else() {
    let Asked { db, ring, values, gave_way } =
        tasks(&[0], Some((WHILE_PROVISIONAL, 2)), false);
    assert_eq!(values, [CAP, CAP]);
    assert_eq!(gave_way, 0);
    let (fixpoint, rounds) = alone([CAP; RING]);
    assert_eq!(once_a_round(&db, &ring), Ok(rounds), "the outside task ran a member");
    assert_eq!(ring.map(|cell| *get::<Ring>(&db, cell)), fixpoint);
}

/// As the invariant above, for a task from outside that asks for a member the head's round has emptied
/// and not reached: it runs that member itself and waits for the head, and the head's task,
/// reaching the member, joins the cycle now headed there instead of giving way. The fixpoint
/// moves to the outside task; each place still runs once a round.
#[test]
fn a_member_run_from_outside_mid_fixpoint_becomes_the_head_and_nothing_gives_way() {
    let Asked { db, ring, values, gave_way } = tasks(&[0], Some((BEFORE_ITS_RUN, 2)), false);
    assert_eq!(values, [CAP, CAP]);
    assert_eq!(gave_way, 0, "the head's task gave way where it could join");
    once_a_round(&db, &ring).expect("a place ran more often than another");
    assert_eq!(ring.map(|cell| *get::<Ring>(&db, cell)), alone([CAP; RING]).0);
}

/// The control: with the join planted out on the second task's thread it gives way as every
/// task did before, unwinding the two runs it held; the head's task runs those places again in
/// the same round, and the oracle's count says so. The values still hold: giving way was
/// always correct, only wasted.
#[test]
fn the_oracle_catches_a_task_that_gives_way_where_it_could_join() {
    let Asked { db, ring, values, gave_way } = tasks(&[0, 1], None, true);
    assert_eq!(values, [CAP, CAP]);
    assert!(gave_way > 0, "the planted task joined");
    let runs = once_a_round(&db, &ring).expect_err("the oracle missed a member run twice in a round");
    assert!(runs[1] > runs[0] && runs[2] > runs[0], "the places the task gave up ran again: {runs:?}");
}
