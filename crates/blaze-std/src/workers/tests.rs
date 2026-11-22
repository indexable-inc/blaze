//! The workers' cells under tasks that hop between workers (D171), as a simulation over a
//! stand-in scheduler with its planted-bug controls, the module doc's worked example, and a
//! simulated world's waits charging themselves.
//!
//! The stand-in: `workers` simulated workers, `tasks` tasks, each a run of steps drawn from the
//! tape. A step is a wait of `d` ns on the virtual clock, then the task resumes on a worker the
//! tape picks (a hop), holds that worker's cell, charges the wait to it (the wait's way out),
//! checks the cell is that worker's, writes a value and reads it back, and works `w` ns. The
//! scheduler interleaves the tasks' steps in a tape-chosen order.
//!
//! The oracle: one owner per cell (a cell is that worker's), no two holders (a hold while one is
//! held is refused), a value written by worker `i` is read back by `i`, and every task's work and
//! waits sum to its time on the clock (no wait returns uncharged).

use std::{format, vec::Vec as StdVec};

use core::{fmt, time::Duration};

use blaze_own::Alloc;
use blaze_sim::{Broken, Config, NoPlant, Tape, check, controls};

use super::{Held, Limits, Slot, SlotOpts, WorkerId, Workers, WorkersOpts, WorkersPlant, WorkersSetup, charge_wait};
use crate::io::{Io, Limits as IoLimits, SimWorld};

/// A cell of the stand-in: whose it is and what was last written.
#[derive(Clone, Copy, Debug)]
struct Owned {
    owner: u32,
    value: u64,
}

/// A second type of `Owned`'s layout: what the slot-reuse control reads one column as.
#[derive(Clone, Copy, Debug)]
struct Mark {
    owner: u32,
    value: u64,
}

/// The invariants the simulation checks.
#[derive(Clone, PartialEq, Eq, Debug)]
enum CellBroken {
    /// A cell was held while another hold had it.
    TwoHolders { worker: u32 },
    /// The cell a task holds on worker `on` is another worker's.
    NotOwner { cell: u32, on: u32 },
    /// A value written on worker `worker` read back as another.
    Differs { worker: u32, want: u64, got: u64 },
    /// The tasks' work and charged waits fall short of their time on the clock.
    Unaccounted { elapsed_ns: u64, accounted_ns: u64 },
}

impl fmt::Display for CellBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellBroken::TwoHolders { worker } => write!(f, "worker {worker}'s cell was held by two at once"),
            CellBroken::NotOwner { cell, on } => write!(f, "a task on worker {on} held worker {cell}'s cell"),
            CellBroken::Differs { worker, want, got } => {
                write!(f, "worker {worker} wrote {want} and read back {got}")
            }
            CellBroken::Unaccounted { elapsed_ns, accounted_ns } => write!(
                f,
                "the tasks ran {elapsed_ns} ns on the clock, their work and charged waits sum to {accounted_ns} ns"
            ),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the stand-in, each with the invariant that must catch it.
    enum CellPlant breaks CellBroken {
        /// The workers as they are.
        None,
        /// A task keeps its cell held across its wait.
        HandedAcrossWait => CellBroken::TwoHolders { .. },
        /// A slot's key aliases the slot registered before it, read as another type of one layout.
        SlotIndexReused => CellBroken::Differs { .. },
        /// A task that moved holds the cell of the worker it started on.
        StaleWorker => CellBroken::NotOwner { .. },
        /// A wait returns without charging its time.
        WaitUncharged => CellBroken::Unaccounted { .. },
    }
}

/// The most simulated workers, tasks and steps a case draws.
const MOST_WORKERS: u32 = 4;
const MOST_TASKS: usize = 6;
const MOST_STEPS: usize = 6;

/// One task of the stand-in.
struct Task<'a> {
    /// The worker it started on.
    first: u32,
    steps: usize,
    /// Its time on the clock: every wait and every stretch of work.
    elapsed_ns: u64,
    /// The work the scheduler measured (as a span would).
    work_ns: u64,
    /// The cell it kept across its wait, under `HandedAcrossWait`.
    kept: Option<Held<'a>>,
}

fn workers_for(models: u32, plant: CellPlant) -> Result<(Workers, Slot<Owned>, Slot<Mark>), Broken<CellBroken>> {
    let setup = WorkersSetup { limits: Limits { most_workers: models, callers: 1, most_slots: 4 }, seed: Some(7), models: 0 };
    let refused = |error: super::WorkersError| Broken::Harness(format!("the stand-in's workers: {error}"));
    let mut opts = WorkersOpts::new(setup).map_err(refused)?;
    let alloc = blaze_own::testing::heap().shared();
    let owned: Slot<Owned> = opts.slot(SlotOpts {
        name: "test.owned",
        alloc,
        make: |worker: WorkerId, _alloc: Alloc<'_>| Owned { owner: worker.index(), value: 0 },
    });
    let reused = match plant {
        CellPlant::SlotIndexReused => WorkersPlant::SlotIndexReused,
        _ => WorkersPlant::None,
    };
    let mark: Slot<Mark> = opts.slot_planted(
        SlotOpts { name: "test.mark", alloc, make: |worker: WorkerId, _alloc: Alloc<'_>| Mark { owner: worker.index(), value: 0 } },
        reused,
    );
    let workers = Workers::new(opts).map_err(refused)?;
    Ok((workers, owned, mark))
}

/// The stand-in scheduler's run of one case.
fn hop(tape: &Tape, plant: CellPlant) -> Result<(), Broken<CellBroken>> {
    let models = 1 + u32::try_from(tape.below(u64::from(MOST_WORKERS), "workers")).unwrap_or(0);
    let (workers, owned, mark) = workers_for(models, plant)?;
    let count = 1 + tape.index(MOST_TASKS, "tasks");
    // alloc: the test's tasks.
    let mut tasks: StdVec<Task<'_>> = (0..count)
        .map(|_| Task {
            first: u32::try_from(tape.below(u64::from(models), "first worker")).unwrap_or(0),
            steps: 1 + tape.index(MOST_STEPS, "steps"),
            elapsed_ns: 0,
            work_ns: 0,
            kept: None,
        })
        .collect();
    let uncharged = match plant {
        CellPlant::WaitUncharged => WorkersPlant::WaitUncharged,
        _ => WorkersPlant::None,
    };
    let mut stamp = 0u64;
    loop {
        let open: StdVec<usize> = (0..tasks.len()).filter(|&at| tasks[at].steps > 0).collect();
        if open.is_empty() {
            break;
        }
        let at = open[tape.index(open.len(), "next task")];
        let task = &mut tasks[at];
        task.steps -= 1;
        // The wait: the task is parked `d` ns; a kept cell stays held through it.
        let waited = 1 + tape.below(1_000, "wait ns");
        task.elapsed_ns += waited;
        // The resume, on a worker the tape picks; under `StaleWorker` the task holds the cell of
        // the worker it started on.
        let on = u32::try_from(tape.below(u64::from(models), "resumes on")).unwrap_or(0);
        let cell = match plant {
            CellPlant::StaleWorker => task.first,
            _ => on,
        };
        let held = workers.hold(workers.pool_worker(cell));
        let mut held = match held {
            Ok(held) => held,
            Err(_) => return Err(Broken::Oracle(CellBroken::TwoHolders { worker: cell })),
        };
        // Given back before this hold: a kept cell is released only after another hold was tried.
        task.kept = None;
        let mut w = held.worker();
        let task_id = u64::try_from(at).unwrap_or(u64::MAX);
        charge_wait(&mut w, "test.wait", Some(task_id), waited, uncharged);
        let mine = *w.slot(owned);
        if mine.owner != on {
            return Err(Broken::Oracle(CellBroken::NotOwner { cell: mine.owner, on }));
        }
        stamp += 1;
        let want = task_id << 32 | stamp;
        w.slot(owned).value = want;
        // The second slot is written too: a key that aliases the first overwrites it.
        w.slot(mark).value = !want;
        let got = w.slot(owned).value;
        if got != want {
            return Err(Broken::Oracle(CellBroken::Differs { worker: on, want, got }));
        }
        let work = tape.below(1_000, "work ns");
        task.work_ns += work;
        task.elapsed_ns += work;
        drop(w);
        if plant == CellPlant::HandedAcrossWait && task.steps > 0 {
            task.kept = Some(held);
        }
    }
    // Every task's work and waits against the clock: the waits as the workers' meters hold them.
    let elapsed_ns: u64 = tasks.iter().map(|task| task.elapsed_ns).sum();
    let work_ns: u64 = tasks.iter().map(|task| task.work_ns).sum();
    drop(tasks);
    let mut waited_ns = 0u64;
    for worker in 0..models {
        let mut held = workers.hold(workers.pool_worker(worker)).map_err(|busy| Broken::Harness(format!("{busy:?} after the run")))?;
        waited_ns += held.worker().meter().waits().total().1;
    }
    if work_ns + waited_ns != elapsed_ns {
        return Err(Broken::Oracle(CellBroken::Unaccounted { elapsed_ns, accounted_ns: work_ns + waited_ns }));
    }
    Ok(())
}

#[test]
fn tasks_that_hop_hold_their_own_workers_cell_and_account_every_wait() {
    check(Config::cases(500), |tape| hop(tape, CellPlant::None));
}

#[test]
fn every_cell_plant_is_caught_by_its_invariant() {
    controls(Config::cases(500), |tape, plant: CellPlant| hop(tape, plant));
}

/// The module doc's core: a function of values.
fn count_words(text: &str, seen: &mut WordCounts) -> u32 {
    let mut n = 0;
    for _ in text.split(' ').filter(|word| !word.is_empty()) {
        n += 1;
    }
    seen.texts += 1;
    n
}

/// The core's per-worker state.
struct WordCounts {
    texts: u32,
}

/// The module doc's driver: it lends the core the worker's buffer.
fn count_driver(cx: &crate::Cx, words: Slot<WordCounts>, text: &str) -> u32 {
    cx.on_worker(|w| count_words(text, w.slot(words)))
}

#[test]
fn a_driver_lends_its_core_the_workers_buffer() {
    let mut opts = WorkersOpts::new(WorkersSetup { limits: Limits::SHIPPED, seed: Some(7), models: 4 }).expect("the workers' reservation");
    let words: Slot<WordCounts> = opts.slot(SlotOpts {
        name: "test.words",
        alloc: blaze_own::testing::heap().shared(),
        make: |_worker: WorkerId, _alloc: Alloc<'_>| WordCounts { texts: 0 },
    });
    let workers = Workers::new(opts).expect("the workers");
    let io = Io::simulated_on(SimWorld::new(1, IoLimits::USUAL), workers);
    let cx = io.cx(Duration::MAX, crate::mem::TASK_FRAME).expect("a root");
    assert_eq!(count_driver(&cx, words, "a b a"), 3);
    assert_eq!(count_driver(&cx, words, "c"), 1);
    // One task that did not move: both calls on one cell.
    assert_eq!(cx.on_worker(|w| w.slot(words).texts), 2);
}

#[test]
fn a_simulated_worlds_wait_charges_the_worker_it_returns_on() {
    let io = Io::simulated(SimWorld::new(3, IoLimits::USUAL));
    let cx = io.cx(Duration::MAX, crate::mem::TASK_FRAME).expect("a root");
    let clock = io.clock().expect("the simulated world's clock");
    clock.sleep(&cx, Duration::from_millis(5)).expect("the sleep");
    let (count, longest) = cx.on_worker(|w| (w.meter().waits().total().0, w.meter().waits().longest()));
    assert!(count >= 1, "the sleep's wait was charged to no worker");
    let longest = longest.expect("a row for the sleep's wait");
    assert_eq!(longest.on, "main", "the wait is charged under the waiting task's name");
}

#[test]
fn a_cell_held_is_refused_a_second_holder() {
    let workers = Workers::bare(WorkersSetup { limits: Limits::BARE, seed: None, models: 2 }).expect("the workers");
    let first = workers.hold(workers.model(0)).expect("an idle cell");
    assert!(workers.hold(workers.model(0)).is_err(), "two holds of one cell");
    assert!(workers.hold(workers.model(1)).is_ok(), "another worker's cell is free");
    drop(first);
    assert!(workers.hold(workers.model(0)).is_ok(), "a cell given back is free");
}

/// A wait under telemetry is charged to the worker's meter AND to its telemetry: the cell's
/// tally IS the meter (`Worker::telemetry` hands `own.meter` as the tally, workers.rs), so
/// `charge_wait_at` charges both through `TelemetryCell::wait_ended`. And it is seen by a
/// snapshot that adds the worker's tally (`Meter::now_with`), never by one of the thread alone
/// (`Meter::now`, whose `waited` is zero by construction, blaze-meter lib.rs `Meter::read`):
/// the "0 waits" on a phase line that spanned a 177 ms wait (tempo, score-c0230wave85; blaze-cli
/// phases.rs `Phases::mark` reads `Meter::now`) is the reader's, not the charge's.
#[test]
fn a_wait_under_telemetry_is_in_the_meter_and_the_telemetry() {
    let setup = WorkersSetup { limits: Limits::BARE, seed: None, models: 1 };
    let mut opts = WorkersOpts::new(setup).expect("the workers' reservation");
    opts.telemetry(crate::telemetry::Level::Spans, blaze_own::testing::heap().shared());
    let workers = Workers::new(opts).expect("the workers");
    let mut held = workers.hold(workers.model(0)).expect("an idle cell");
    let mut w = held.worker();
    assert!(w.telemetry().is_some(), "a cell of a Workers made with telemetry keeps it");
    let thread_before = blaze_meter::Meter::now();
    let before = blaze_meter::Meter::now_with(w.meter());
    super::charge_wait_at(&mut w, "test.wait", Some(9), 1_000, 178_000, NoPlant);
    assert_eq!(w.meter().waits().total(), (1, 177_000), "the meter has the wait");
    let with = blaze_meter::Meter::now_with(w.meter()).since(&before).expect("one tally's pair");
    assert_eq!(with.waited.total(), (1, 177_000), "a snapshot with the worker's tally sees it");
    let alone = blaze_meter::Meter::now().since(&thread_before).expect("one thread's pair");
    assert_eq!(alone.waited.total(), (0, 0), "a snapshot of the thread alone cannot see a worker's wait");
    // The CONTROL (rule 23): the planted uncharged wait leaves the meter as it was.
    super::charge_wait_at(&mut w, "test.wait", Some(9), 0, 5_000, WorkersPlant::WaitUncharged);
    assert_eq!(w.meter().waits().total(), (1, 177_000), "the plant charges nothing");
}

#[test]
fn the_shipped_charge_charges() {
    let workers = Workers::bare(WorkersSetup { limits: Limits::BARE, seed: None, models: 1 }).expect("the workers");
    let mut held = workers.hold(workers.model(0)).expect("an idle cell");
    let mut w = held.worker();
    charge_wait(&mut w, "test.wait", Some(9), 41_800, NoPlant);
    assert_eq!(w.meter().waits().total(), (1, 41_800));
}
