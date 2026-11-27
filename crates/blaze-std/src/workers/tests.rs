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

use std::{
    collections::BTreeSet,
    format,
    sync::{
        Barrier,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    vec::Vec as StdVec,
};

use core::{fmt, time::Duration};

use blaze_own::Alloc;
use blaze_rng::SplitMix64;
use blaze_sim::{Broken, Config, NoPlant, Tape, check, controls};

use super::{
    Held, Limits, MOST_CALLERS, MOST_LEASE_ATTEMPTS, Slot, SlotOpts, WorkerId, Workers, WorkersError, WorkersOpts, WorkersPlant,
    WorkersSetup, charge_wait,
};
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

#[test]
fn caller_leases_span_multi_word_bitmap_and_reclaim_freed_bits() {
    let workers = Workers::bare(WorkersSetup {
        limits: Limits { most_workers: 4, callers: 130, most_slots: 0 },
        seed: None,
        models: 2,
    })
    .expect("130 callers across 3 bitmap words");
    let first = 4 + 2;
    let mut leases: StdVec<_> = (0..130)
        .map(|i| {
            let lease = workers.lease().expect("caller cell within 130");
            assert_eq!(lease.id().index(), first + i, "leases hand out caller indices in order");
            // Prove the leased caller cell can actually be held and used.
            let _held = workers.hold(lease.id()).expect("leased caller cell is holdable");
            lease
        })
        .collect();
    match workers.lease() {
        Err(WorkersError::CallersTaken { most: 130 }) => {}
        Ok(extra) => panic!("131st caller lease must fail, got {:?}", extra.id()),
        Err(other) => panic!("expected CallersTaken {{ most: 130 }}, got {other}"),
    }

    // Drop one lease in each of word 0 (bit 5), word 1 (bit 70), and word 2 (bit 129).
    let freed = [5usize, 70, 129];
    for &idx in freed.iter().rev() {
        let removed = leases.remove(idx);
        assert_eq!(removed.id().index(), first + u32::try_from(idx).unwrap());
        drop(removed);
    }

    // Re-leasing returns the three freed bits in ascending bitmap order.
    let mut reclaimed = StdVec::new();
    for &idx in &freed {
        let lease = workers.lease().expect("freed caller cell is re-leasable");
        assert_eq!(lease.id().index(), first + u32::try_from(idx).unwrap());
        reclaimed.push(lease);
    }
    assert!(matches!(workers.lease(), Err(WorkersError::CallersTaken { most: 130 })));
    drop(reclaimed);
    drop(leases);
}

#[test]
fn caller_lease_boundaries_and_limit_checks() {
    assert_eq!(MOST_CALLERS, 512);
    assert_eq!(Limits::SHIPPED.callers, MOST_CALLERS);

    // Zero callers is refused.
    assert!(matches!(
        WorkersOpts::new(WorkersSetup { limits: Limits { most_workers: 1, callers: 0, most_slots: 0 }, seed: None, models: 0 }),
        Err(WorkersError::LimitZero { limit: "Limits::callers" })
    ));

    // MOST_CALLERS + 1 is refused.
    assert!(matches!(
        WorkersOpts::new(WorkersSetup {
            limits: Limits { most_workers: 1, callers: MOST_CALLERS + 1, most_slots: 0 },
            seed: None,
            models: 0,
        }),
        Err(WorkersError::LimitTooLarge { limit: "callers", wanted: 513, most: 512 })
    ));

    // Boundary counts: 1 (single bit), 64 (exact 1 word), 65 (crosses into word 1), 512 (exact 8 words).
    for &count in &[1u32, 64, 65, MOST_CALLERS] {
        let workers = Workers::bare(WorkersSetup {
            limits: Limits { most_workers: 2, callers: count, most_slots: 0 },
            seed: None,
            models: 0,
        })
        .expect("valid caller count");
        let mut leases = StdVec::with_capacity(usize::try_from(count).unwrap());
        for i in 0..count {
            let lease = workers.lease().expect("lease within count");
            assert_eq!(lease.id().index(), 2 + i);
            leases.push(lease);
        }
        assert!(matches!(workers.lease(), Err(WorkersError::CallersTaken { most }) if most == count));

        // Drop the last lease (highest bit in the final word) and re-lease it.
        let last = leases.pop().expect("at least one lease");
        let last_id = last.id();
        drop(last);
        let again = workers.lease().expect("re-lease highest bit");
        assert_eq!(again.id(), last_id);
        assert!(matches!(workers.lease(), Err(WorkersError::CallersTaken { most }) if most == count));
    }
}

/// The invariants the multi-word caller lease DST checks against its reference oracle.
#[derive(Clone, PartialEq, Eq, Debug)]
enum LeaseBroken {
    /// Two live leases held the same caller [`WorkerId`].
    Duplicate { id: u32 },
    /// A lease returned a [`WorkerId`] outside `[first_caller, first_caller + callers)`.
    OutOfBounds { id: u32, first: u32, callers: u32 },
    /// `lease` succeeded even though `callers` leases were already live.
    MissedBound { callers: u32, live: usize },
    /// `lease` returned `CallersTaken` even though fewer than `callers` leases were live (or after
    /// all leases were dropped, indicating a leak).
    SpuriousFull { callers: u32, live: usize },
    /// `lease` returned `CallersContended` unexpectedly (e.g. a leaked bitmap bit exhausted scan
    /// passes while `leased < callers`).
    Contended { attempts: u32, live: usize },
    /// `CallersTaken` reported the wrong limit.
    WrongBound { want: u32, got: u32 },
}

impl fmt::Display for LeaseBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeaseBroken::Duplicate { id } => write!(f, "caller cell {id} was leased twice while still live"),
            LeaseBroken::OutOfBounds { id, first, callers } => {
                write!(f, "caller cell {id} is outside [{first}, {})", first + callers)
            }
            LeaseBroken::MissedBound { callers, live } => {
                write!(f, "lease succeeded with {live} already live (bound {callers})")
            }
            LeaseBroken::SpuriousFull { callers, live } => {
                write!(f, "CallersTaken returned with only {live}/{callers} leases live")
            }
            LeaseBroken::Contended { attempts, live } => {
                write!(f, "CallersContended after {attempts} attempts with {live} leases live")
            }
            LeaseBroken::WrongBound { want, got } => {
                write!(f, "CallersTaken reported bound {got}, expected {want}")
            }
        }
    }
}

blaze_sim::plants! {
    /// Planted bugs in the multi-word caller lease bitmap, each caught by its oracle invariant.
    enum LeasePlant breaks LeaseBroken {
        /// The multi-word caller lease bitmap as shipped.
        None,
        /// Final-word mask and capacity check are skipped: hands out bits past `Limits::callers`.
        FinalMaskSkipped => LeaseBroken::OutOfBounds { .. } | LeaseBroken::MissedBound { .. },
        /// Dropping a lease clears word 0 instead of its own word: duplicates a live word-0 ID or
        /// leaks the higher-word bit until `Contended`.
        DropWrongWord => LeaseBroken::Duplicate { .. } | LeaseBroken::Contended { .. },
        /// Dropping a lease forgets to clear its bitmap bit: leaks slots until `Contended`.
        BitLeaked => LeaseBroken::Contended { .. },
    }
}

fn to_workers_plant(plant: LeasePlant) -> WorkersPlant {
    match plant {
        LeasePlant::None => WorkersPlant::None,
        LeasePlant::FinalMaskSkipped => WorkersPlant::CallerFinalMaskSkipped,
        LeasePlant::DropWrongWord => WorkersPlant::CallerDropWrongWord,
        LeasePlant::BitLeaked => WorkersPlant::CallerBitLeaked,
    }
}

/// Deterministic simulation of interleaved multi-word caller `lease` / `drop` operations against
/// an exact allocation/release oracle (`BTreeSet<u32>` + live count).
fn lease_sim(tape: &Tape, plant: LeasePlant) -> Result<(), Broken<LeaseBroken>> {
    let callers = match tape.below(4, "caller shape") {
        0 => 65,
        1 => 130,
        2 => 192,
        _ => 65 + u32::try_from(tape.below(u64::from(MOST_CALLERS - 64), "callers")).unwrap_or(0),
    };
    let most_workers = 1 + u32::try_from(tape.below(8, "most_workers")).unwrap_or(0);
    let models = u32::try_from(tape.below(4, "models")).unwrap_or(0);
    let first = most_workers + models;
    let workers = Workers::bare(WorkersSetup {
        limits: Limits { most_workers, callers, most_slots: 0 },
        seed: None,
        models,
    })
    .map_err(|error| Broken::Harness(format!("workers setup: {error}")))?;

    let wp = to_workers_plant(plant);
    let mut held = StdVec::new();
    let mut live = BTreeSet::new();

    let acquire_one = |held: &mut StdVec<super::Lease>, live: &mut BTreeSet<u32>| -> Result<(), Broken<LeaseBroken>> {
        match workers.lease_planted(wp) {
            Ok(lease) => {
                let id = lease.id().index();
                if id < first || id >= first + callers {
                    return Err(Broken::Oracle(LeaseBroken::OutOfBounds { id, first, callers }));
                }
                if live.len() >= usize::try_from(callers).unwrap_or(usize::MAX) {
                    return Err(Broken::Oracle(LeaseBroken::MissedBound { callers, live: live.len() }));
                }
                if !live.insert(id) {
                    return Err(Broken::Oracle(LeaseBroken::Duplicate { id }));
                }
                let _ = workers.hold(lease.id()).map_err(|busy| Broken::Harness(format!("hold {id}: {busy:?}")))?;
                held.push(lease);
                Ok(())
            }
            Err(WorkersError::CallersTaken { most }) => {
                if most != callers {
                    return Err(Broken::Oracle(LeaseBroken::WrongBound { want: callers, got: most }));
                }
                if live.len() < usize::try_from(callers).unwrap_or(usize::MAX) {
                    return Err(Broken::Oracle(LeaseBroken::SpuriousFull { callers, live: live.len() }));
                }
                Ok(())
            }
            Err(WorkersError::CallersContended { attempts }) => {
                Err(Broken::Oracle(LeaseBroken::Contended { attempts, live: live.len() }))
            }
            Err(other) => Err(Broken::Harness(format!("unexpected lease error: {other}"))),
        }
    };

    // Drive interleaved multi-word acquire/release steps: start with a burst past word 0 so
    // multiple words are populated, then alternate random acquires and drops, including probing
    // past the configured bound.
    let initial = 65usize.min(usize::try_from(callers).unwrap_or(65));
    for _ in 0..initial {
        acquire_one(&mut held, &mut live)?;
    }

    let steps = 32 + tape.index(160, "steps");
    for _ in 0..steps {
        let do_acquire = held.is_empty() || tape.below(3, "op") != 0;
        if do_acquire {
            acquire_one(&mut held, &mut live)?;
        } else {
            let at = tape.index(held.len(), "drop index");
            let lease = held.swap_remove(at);
            live.remove(&lease.id().index());
            drop(lease);
        }
    }

    // Saturate to the exact bound, probe one past it, then drop everything and prove zero leaks
    // by re-leasing all `callers` slots cleanly.
    while live.len() < usize::try_from(callers).unwrap_or(usize::MAX) {
        acquire_one(&mut held, &mut live)?;
    }
    acquire_one(&mut held, &mut live)?;

    held.clear();
    live.clear();

    for _ in 0..callers {
        acquire_one(&mut held, &mut live)?;
    }
    acquire_one(&mut held, &mut live)?;
    Ok(())
}

#[test]
fn multiword_caller_leases_match_oracle() {
    check(Config::cases(128), |tape| lease_sim(tape, LeasePlant::None));
}

#[test]
fn every_lease_plant_is_caught_by_its_invariant() {
    controls(Config::cases(128), lease_sim);
}

/// Runs real concurrent multi-threaded `lease` / `drop` contention across multiple 64-bit words
/// with a lock-free atomic ownership oracle:
/// - Phase A (under/at exact bound): total quota across all threads == `callers`, so `CallersTaken`
///   must NEVER be returned spuriously even when the sole free bit moves backward across words.
/// - Phase B (over bound): threads contest beyond `callers`, verifying no duplicate live IDs, no
///   out-of-bounds IDs, and exact `CallersTaken { most: callers }`.
/// - Phase C (reclaim): after all threads drop, every one of `callers` slots is re-leasable once
///   and the `(callers + 1)`th returns `CallersTaken { most: callers }`.
fn run_concurrent_lease_contention(seed: u64, callers: u32, plant: LeasePlant) -> Result<(), LeaseBroken> {
    const THREADS: usize = 8;
    const FIRST: u32 = 4;
    let workers = Workers::bare(WorkersSetup {
        limits: Limits { most_workers: FIRST, callers, most_slots: 0 },
        seed: None,
        models: 0,
    })
    .expect("workers setup");
    let wp = to_workers_plant(plant);
    let callers_usize = usize::try_from(callers).unwrap();
    let oracle: StdVec<AtomicBool> = (0..callers_usize).map(|_| AtomicBool::new(false)).collect();
    let failed = AtomicBool::new(false);

    // Phase A: exact-capacity contention (sum of thread quotas == callers).
    let barrier_a = Barrier::new(THREADS);
    let phase_a_err: StdVec<Option<LeaseBroken>> = thread::scope(|scope| {
        let mut handles = StdVec::with_capacity(THREADS);
        for tid in 0..THREADS {
            let workers = &workers;
            let oracle = &oracle;
            let barrier = &barrier_a;
            let failed = &failed;
            let base_quota = callers_usize / THREADS;
            let extra = usize::from(tid < (callers_usize % THREADS));
            let quota = base_quota + extra;
            handles.push(scope.spawn(move || {
                let mut rng = SplitMix64::new(seed ^ (u64::try_from(tid).unwrap().wrapping_mul(0x9E37_79B9_7F4A_7C15)));
                let mut held: StdVec<super::Lease> = StdVec::with_capacity(quota);
                let mut broken: Option<LeaseBroken> = None;
                barrier.wait();
                for _ in 0..160 {
                    if failed.load(Ordering::Relaxed) {
                        break;
                    }
                    let want_acquire = held.is_empty() || (held.len() < quota && (rng.next_u64() & 1) == 0);
                    if want_acquire {
                        match workers.lease_planted(wp) {
                            Ok(lease) => {
                                let id = lease.id().index();
                                if id < FIRST || id >= FIRST + callers {
                                    broken = Some(LeaseBroken::OutOfBounds { id, first: FIRST, callers });
                                    failed.store(true, Ordering::Relaxed);
                                    drop(lease);
                                    break;
                                }
                                let slot = usize::try_from(id - FIRST).unwrap();
                                if oracle[slot].swap(true, Ordering::AcqRel) {
                                    broken = Some(LeaseBroken::Duplicate { id });
                                    failed.store(true, Ordering::Relaxed);
                                    drop(lease);
                                    break;
                                }
                                held.push(lease);
                            }
                            Err(WorkersError::CallersTaken { most }) => {
                                broken = if most != callers {
                                    Some(LeaseBroken::WrongBound { want: callers, got: most })
                                } else {
                                    Some(LeaseBroken::SpuriousFull { callers, live: held.len() })
                                };
                                failed.store(true, Ordering::Relaxed);
                                break;
                            }
                            Err(WorkersError::CallersContended { attempts }) => {
                                broken = Some(LeaseBroken::Contended { attempts, live: held.len() });
                                failed.store(true, Ordering::Relaxed);
                                break;
                            }
                            Err(other) => panic!("unexpected lease error: {other}"),
                        }
                    } else {
                        let pick = usize::try_from(rng.next_u64()).unwrap() % held.len();
                        let lease = held.swap_remove(pick);
                        let slot = usize::try_from(lease.id().index() - FIRST).unwrap();
                        // Release oracle slot before dropping lease (synchronized by Lease::drop's
                        // wait-free Release fetch_and -> next lease's Acquire CAS).
                        oracle[slot].store(false, Ordering::Release);
                        drop(lease);
                    }
                }
                for lease in held {
                    let slot = usize::try_from(lease.id().index() - FIRST).unwrap();
                    oracle[slot].store(false, Ordering::Release);
                    drop(lease);
                }
                broken
            }));
        }
        handles.into_iter().map(|h| h.join().expect("phase A worker panicked")).collect()
    });
    if let Some(err) = phase_a_err.into_iter().flatten().next() {
        return Err(err);
    }

    // Phase B: over-bound contention (each thread tries to hold up to callers / 2 + 16 leases).
    let barrier_b = Barrier::new(THREADS);
    let over_quota = (callers_usize / 2) + 16;
    let phase_b_err: StdVec<Option<LeaseBroken>> = thread::scope(|scope| {
        let mut handles = StdVec::with_capacity(THREADS);
        for tid in 0..THREADS {
            let workers = &workers;
            let oracle = &oracle;
            let barrier = &barrier_b;
            let failed = &failed;
            handles.push(scope.spawn(move || {
                let mut rng = SplitMix64::new(seed.wrapping_add(1) ^ (u64::try_from(tid).unwrap().wrapping_mul(0xBF58_476D_1CE4_E5B9)));
                let mut held: StdVec<super::Lease> = StdVec::with_capacity(over_quota);
                let mut broken: Option<LeaseBroken> = None;
                barrier.wait();
                for _ in 0..160 {
                    if failed.load(Ordering::Relaxed) {
                        break;
                    }
                    let want_acquire = held.is_empty() || (held.len() < over_quota && (rng.next_u64() % 3) != 0);
                    if want_acquire {
                        match workers.lease_planted(wp) {
                            Ok(lease) => {
                                let id = lease.id().index();
                                if id < FIRST || id >= FIRST + callers {
                                    broken = Some(LeaseBroken::OutOfBounds { id, first: FIRST, callers });
                                    failed.store(true, Ordering::Relaxed);
                                    drop(lease);
                                    break;
                                }
                                let slot = usize::try_from(id - FIRST).unwrap();
                                if oracle[slot].swap(true, Ordering::AcqRel) {
                                    broken = Some(LeaseBroken::Duplicate { id });
                                    failed.store(true, Ordering::Relaxed);
                                    drop(lease);
                                    break;
                                }
                                held.push(lease);
                            }
                            Err(WorkersError::CallersTaken { most }) => {
                                if most != callers {
                                    broken = Some(LeaseBroken::WrongBound { want: callers, got: most });
                                    failed.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                            Err(WorkersError::CallersContended { attempts }) => {
                                broken = Some(LeaseBroken::Contended { attempts, live: held.len() });
                                failed.store(true, Ordering::Relaxed);
                                break;
                            }
                            Err(other) => panic!("unexpected lease error: {other}"),
                        }
                    } else {
                        let pick = usize::try_from(rng.next_u64()).unwrap() % held.len();
                        let lease = held.swap_remove(pick);
                        let slot = usize::try_from(lease.id().index() - FIRST).unwrap();
                        oracle[slot].store(false, Ordering::Release);
                        drop(lease);
                    }
                }
                for lease in held {
                    let slot = usize::try_from(lease.id().index() - FIRST).unwrap();
                    oracle[slot].store(false, Ordering::Release);
                    drop(lease);
                }
                broken
            }));
        }
        handles.into_iter().map(|h| h.join().expect("phase B worker panicked")).collect()
    });
    if let Some(err) = phase_b_err.into_iter().flatten().next() {
        return Err(err);
    }

    // Phase C: zero leaks after concurrent contention — every single slot is re-leasable once,
    // and one more returns `CallersTaken { most: callers }`.
    let mut reclaimed = StdVec::with_capacity(callers_usize);
    let mut seen = BTreeSet::new();
    for _ in 0..callers {
        match workers.lease_planted(wp) {
            Ok(lease) => {
                let id = lease.id().index();
                if id < FIRST || id >= FIRST + callers {
                    return Err(LeaseBroken::OutOfBounds { id, first: FIRST, callers });
                }
                if !seen.insert(id) {
                    return Err(LeaseBroken::Duplicate { id });
                }
                reclaimed.push(lease);
            }
            Err(WorkersError::CallersTaken { most }) => {
                return Err(if most != callers {
                    LeaseBroken::WrongBound { want: callers, got: most }
                } else {
                    LeaseBroken::SpuriousFull { callers, live: reclaimed.len() }
                });
            }
            Err(WorkersError::CallersContended { attempts }) => {
                return Err(LeaseBroken::Contended { attempts, live: reclaimed.len() });
            }
            Err(other) => panic!("unexpected lease error during reclaim: {other}"),
        }
    }
    match workers.lease_planted(wp) {
        Ok(extra) => {
            let id = extra.id().index();
            if id < FIRST || id >= FIRST + callers {
                return Err(LeaseBroken::OutOfBounds { id, first: FIRST, callers });
            }
            return Err(LeaseBroken::MissedBound { callers, live: reclaimed.len() });
        }
        Err(WorkersError::CallersTaken { most }) if most == callers => {}
        Err(WorkersError::CallersTaken { most }) => {
            return Err(LeaseBroken::WrongBound { want: callers, got: most });
        }
        Err(other) => panic!("unexpected lease error at bound: {other}"),
    }
    Ok(())
}

#[test]
fn concurrent_multiword_leases_never_duplicate_leak_or_spuriously_exhaust() {
    let bounds = [65u32, 130, 192, MOST_CALLERS];
    for seed in 0u64..16 {
        let callers = bounds[usize::try_from(seed).unwrap() % bounds.len()];
        if let Err(broken) = run_concurrent_lease_contention(seed.wrapping_add(1), callers, LeasePlant::None) {
            panic!("seed {seed}, callers {callers}: {broken}");
        }
    }
}

#[test]
fn concurrent_multiword_lease_harness_catches_every_planted_bug() {
    for &(plant, callers) in &[
        (LeasePlant::FinalMaskSkipped, 65u32),
        (LeasePlant::DropWrongWord, 130u32),
        (LeasePlant::BitLeaked, 130u32),
    ] {
        let caught = (1u64..=8).any(|seed| run_concurrent_lease_contention(seed, callers, plant).is_err());
        assert!(caught, "concurrent harness failed to catch planted bug {plant:?}");
    }
}

#[test]
fn contended_lease_returns_callers_contended_and_rolls_back_reservation() {
    const CALLERS: u32 = 130;
    let workers = Workers::bare(WorkersSetup {
        limits: Limits { most_workers: 2, callers: CALLERS, most_slots: 0 },
        seed: None,
        models: 0,
    })
    .expect("workers setup");

    // Exhausting Step B must report `CallersContended` (never `CallersTaken`) and roll back
    // `core.leased` so zero capacity is leaked.
    match workers.lease_planted(WorkersPlant::CallerScanContended) {
        Err(err @ WorkersError::CallersContended { attempts }) => {
            assert_eq!(attempts, MOST_LEASE_ATTEMPTS);
            let msg = format!("{err}");
            assert!(msg.contains("contention") && msg.contains("4096"), "unexpected display: {msg}");
        }
        Ok(lease) => panic!("contended plant must not succeed, got {:?}", lease.id()),
        Err(other) => panic!("expected CallersContended, got {other}"),
    }

    // Prove zero leaked capacity: all 130 caller slots remain leaseable, and the 131st fails with
    // `CallersTaken { most: 130 }`.
    let leases: StdVec<_> = (0..CALLERS).map(|_| workers.lease().expect("all 130 slots available after rollback")).collect();
    assert!(matches!(workers.lease(), Err(WorkersError::CallersTaken { most: CALLERS })));
    drop(leases);
}

#[test]
fn sole_free_bit_churning_across_words_never_falsely_exhausts() {
    // Hold `callers - 2` leases pinned across words 0..=2 while two threads continuously swap the
    // remaining 2 slots across word 0 and word 2: total live leases never exceed `callers`, so
    // neither thread may ever observe spurious `CallersTaken`.
    const CALLERS: u32 = 130;
    const FIRST: u32 = 2;
    let workers = Workers::bare(WorkersSetup {
        limits: Limits { most_workers: FIRST, callers: CALLERS, most_slots: 0 },
        seed: None,
        models: 0,
    })
    .expect("workers setup");

    // Lease all 130, then drop bit 0 (word 0) and bit 129 (word 2), keeping 128 pinned.
    let mut pinned: StdVec<_> = (0..CALLERS).map(|_| workers.lease().expect("initial fill")).collect();
    let _free_high = pinned.pop().expect("bit 129");
    let _free_low = pinned.remove(0);
    drop(_free_high);
    drop(_free_low);

    let barrier = Barrier::new(2);
    thread::scope(|scope| {
        for _ in 0..2 {
            let workers = &workers;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..2_000 {
                    let lease = workers.lease().expect("one of the 2 free slots must always be claimable");
                    let id = lease.id().index();
                    assert!(id >= FIRST && id < FIRST + CALLERS, "out-of-bounds id {id}");
                    drop(lease);
                }
            });
        }
    });
    drop(pinned);
}
