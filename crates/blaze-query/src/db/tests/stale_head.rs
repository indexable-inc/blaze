//! A stale inner head of one task's cycle, met by another task that comes to hold its flight
//! between two rounds: the interleaving that panicked a self-build twice on 2026-11-11 ("the
//! cycle recovery of summary_in(..) asked for summary_in itself"), made to happen every run.
//!
//! `StaleA` reads `StaleB`; `StaleB` reads itself and `StaleA`. Asked from `StaleA`, `StaleB`
//! is an inner head: it ends each round provisional, a member of `StaleA`'s cycle, and between
//! two rounds it is stale, its flight free.
//!
//! 1. The asker (task 2) asks for `StaleB` first, reads its word (never made) and stops just
//!    before it asks for the flight ([`probe::BEFORE_FLIGHT`]): where a task that gave way
//!    waits (`sched::before_claim`), and where a preempted thread is.
//! 2. The head's task (task 1) asks for `StaleA`, runs round 1 (`StaleB` runs inside it and
//!    ends provisional) and starts round 2: `StaleB` is stale. It lets the asker go on.
//! 3. The asker takes `StaleB`'s flight, free, and finds the row provisional and stale. With
//!    the bug it runs it: `StaleB` reads itself (it is on the asker's stack, provisional: the
//!    frame is marked re-entered) and then `StaleA`, which task 1 runs: it waits. Fixed, it
//!    lets go of the flight and waits for `StaleA`. Either way it says so as it asks for
//!    `StaleA`'s flight.
//! 4. Task 1 gives way in round 2 (as its wait for a key the asker's stack held would make
//!    it) and unwinds: its cycle's rows are emptied, `StaleB` with them.
//! 5. The asker goes on. With the bug it is inside its run of `StaleB`, runs `StaleA` itself,
//!    which reads `StaleB`: on the stack, no longer provisional, its frame already re-entered:
//!    the panic. Fixed, it runs `StaleB` from nothing as the head of its own cycle.

use std::{
    cell::{Cell as Flag, RefCell},
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, unbounded};

use super::{
    super::function::probe, AssertUnwindSafe, AtomicBool, Cancelled, Cell, CycleKind, Function,
    Id, Ingredient, Ordering, Spec, TestDb, Vtable, catch_unwind, get, message, until_it_holds,
};

/// The longest a task waits for the other's word before the test fails naming what it waited
/// for (rule 18); each wait ends in microseconds.
const BOUND: Duration = Duration::from_secs(60);

/// What the two tasks tell each other, in the order of the steps above.
pub(super) struct Relay {
    /// Set by the test: task 1 gives way in the head's second run, once.
    armed: AtomicBool,
    /// Step 1: the asker read `StaleB`'s word and is about to ask for its flight.
    read: (Sender<()>, Receiver<()>),
    /// Step 2: the head's round 2 began, so `StaleB` is stale.
    stale: (Sender<()>, Receiver<()>),
    /// Step 3: the asker is about to ask for the head's flight.
    decided: (Sender<()>, Receiver<()>),
}

impl Relay {
    pub(super) fn new() -> Relay {
        Relay {
            armed: AtomicBool::new(false),
            read: unbounded(),
            stale: unbounded(),
            decided: unbounded(),
        }
    }
}

query!(
    StaleA,
    u64,
    |db, key| {
        if db.runs_of("StaleA", key.0) == 2 && db.relay.armed.swap(false, Ordering::SeqCst) {
            db.relay.stale.0.send(()).expect("the relay lives as long as the database");
            db.relay
                .decided
                .1
                .recv_timeout(BOUND)
                .expect("the asker took StaleB's flight and asked for StaleA's");
            // Step 4: what a wait that would close a cycle of tasks does to this stack.
            Cancelled::WouldWaitOnItself { asked: None }.throw();
        }
        *get::<StaleB>(db, key)
    };
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);
query!(
    StaleB,
    u64,
    |db, key| {
        let own = *get::<StaleB>(db, key);
        let head = *get::<StaleA>(db, key);
        (own.max(head) + 1).min(3)
    };
    const CYCLES: CycleKind = CycleKind::Fixpoint;
    fn initial(_: &TestDb, _: Cell) -> u64 {
        0
    }
);

/// The asker's ends of the relay and the two functions' ingredients, for [`before_flight`].
struct Asker {
    stale_head: u32,
    head: u32,
    stopped: Flag<bool>,
    read: Sender<()>,
    stale: Receiver<()>,
    decided: Sender<()>,
}

std::thread_local! {
    static ASKER: RefCell<Option<Asker>> = const { RefCell::new(None) };
}

/// The asker's [`probe::BEFORE_FLIGHT`]: steps 1 and 3.
fn before_flight(index: u32, _: Id) {
    ASKER.with(|it| {
        let it = it.borrow();
        let asker = it.as_ref().expect("set before the probe");
        if index == asker.stale_head && !asker.stopped.replace(true) {
            asker.read.send(()).expect("the relay lives as long as the database");
            asker.stale.recv_timeout(BOUND).expect("the head's second round began");
        } else if index == asker.head {
            asker.decided.send(()).expect("the relay lives as long as the database");
        }
    });
}

/// Runs the interleaving. The asker's `StaleB` (or its panic's message), and the head's task's
/// `StaleA` asked again after it gave way (not asked when `plant`: the planted run leaves rows
/// no task may read).
fn two_tasks(plant: bool) -> (Result<u64, String>, Option<u64>) {
    let db = TestDb::new();
    let cell = Cell::new(&db, 0, 0);
    db.relay.armed.store(true, Ordering::SeqCst);
    std::thread::scope(|scope| {
        let asker = scope.spawn(|| {
            ASKER.with(|it| {
                *it.borrow_mut() = Some(Asker {
                    stale_head: <StaleB as Function>::ingredient().index(),
                    head: <StaleA as Function>::ingredient().index(),
                    stopped: Flag::new(false),
                    read: db.relay.read.0.clone(), // clone: a channel's end, for this thread
                    stale: db.relay.stale.1.clone(), // clone: as above
                    decided: db.relay.decided.0.clone(), // clone: as above
                });
            });
            probe::BEFORE_FLIGHT.with(|it| it.set(Some(before_flight)));
            probe::FOREIGN_STALE_RUNS.with(|it| it.set(plant));
            catch_unwind(AssertUnwindSafe(|| until_it_holds::<StaleB>(&db, cell)))
                .map_err(|payload| message(&*payload))
        });
        let head = scope.spawn(|| {
            db.relay.read.1.recv_timeout(BOUND).expect("the asker read StaleB's word");
            let first = Cancelled::catch(AssertUnwindSafe(|| *get::<StaleA>(&db, cell)));
            assert!(
                matches!(first, Err(Cancelled::WouldWaitOnItself { .. })),
                "the head's task gives way in its second round, got {first:?}"
            );
            (!plant).then(|| until_it_holds::<StaleA>(&db, cell))
        });
        (asker.join().expect("the asker's panic is caught"), head.join().expect("the head's task"))
    })
}

/// The invariant: a provisional row is run and written only by the task whose stack holds its
/// cycle's head. A task that comes to hold the flight of a stale inner head of another task's
/// cycle waits for the head instead, so every task gets the fixpoint a single task computes,
/// and no run finds its own row emptied under it.
#[test]
fn a_stale_head_of_another_tasks_cycle_is_waited_for_never_run() {
    let fixpoint = {
        let alone = TestDb::new();
        let cell = Cell::new(&alone, 0, 0);
        (*get::<StaleB>(&alone, cell), *get::<StaleA>(&alone, cell))
    };
    assert_eq!(fixpoint, (3, 3));
    let (asker, head) = two_tasks(false);
    assert_eq!((asker, head), (Ok(fixpoint.0), Some(fixpoint.1)));
}

/// The control: with the guard planted out on the asker's thread, the asker runs the stale
/// head, the head's task empties it as it unwinds, and the asker's run meets its own row no
/// longer provisional: the self-build's panic, word for word.
#[test]
fn the_oracle_catches_a_stale_head_run_by_a_task_that_does_not_head_its_cycle() {
    let (asker, _) = two_tasks(true);
    let message = asker.expect_err("the asker ran another task's stale head");
    assert!(
        message.contains("the cycle recovery of StaleB(") && message.contains("asked for StaleB itself"),
        "{message}"
    );
}
