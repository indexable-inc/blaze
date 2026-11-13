//! The gate over real regions and frames: core's count is exactly what its regions hold, a task
//! past a small ceiling fails typed, a younger task gives way to its elder and is let in once the
//! elder ends, and a span is its task's while it runs and core's after.

use std::panic::{AssertUnwindSafe, catch_unwind};

use blaze_rng::SplitMix64;
use hegel::{TestCase, generators as gs};

use super::{Gate, HeldBack, Ranked, config_under, machine_config, scoped};
use crate::{
    MemError, Region, Tag,
    budget::{Age, Band, Budget, Config, Hold, Misuse, TaskId, TooBig},
    frame,
    huge::Pages,
};

const MIB: u64 = 1 << 20;

/// A gate of 64 MiB: 8 MiB core, a 4 MiB overdraft for each of `workers`, so one task holds
/// at most `(64 - 8 - 4 * workers) MiB`.
fn small(workers: u32) -> &'static Gate {
    let config = Config {
        ceiling: 64 * MIB,
        core: 8 * MIB,
        overdraft_per_worker: 4 * MIB,
        workers,
        ungated: 0,
    };
    Gate::new(config).unwrap_or_else(|why| panic!("the test's gate {config:?}: {why}"))
}

// --- Core: the count is what the regions hold -------------------------------------------------

/// Regions per run, and each one's reservation in bytes.
const REGIONS: usize = 3;
const RESERVED: usize = 16 << 20;

#[derive(Clone, Copy, Debug)]
enum Step {
    Ensure { r: usize, len: usize },
    Decommit { r: usize, keep: usize },
    Purge { r: usize, from: usize, len: usize },
    Reuse { r: usize, from: usize, len: usize },
}

/// A source of choices: Hegel's, or a seeded one where its server is not.
trait Draw {
    /// In `lo..=hi`.
    fn int(&mut self, lo: usize, hi: usize) -> usize;
}

struct Drawn<'a>(&'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: usize, hi: usize) -> usize {
        self.0.draw(gs::integers::<usize>().min_value(lo).max_value(hi))
    }
}

impl Draw for SplitMix64 {
    fn int(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below((hi - lo + 1) as u64) as usize
    }
}

/// A whole number of pages in `0..=RESERVED`.
fn at(draw: &mut impl Draw) -> usize {
    let page = crate::page_size();
    draw.int(0, RESERVED / page) * page
}

fn plan(draw: &mut impl Draw) -> Vec<Step> {
    let steps = draw.int(1, 40);
    let mut plan = Vec::with_capacity(steps); // alloc: a test's plan
    for _ in 0..steps {
        let r = draw.int(0, REGIONS - 1);
        plan.push(match draw.int(0, 3) {
            0 => Step::Ensure { r, len: at(draw) },
            1 => Step::Decommit { r, keep: at(draw) },
            2 => Step::Purge { r, from: at(draw), len: at(draw) },
            _ => Step::Reuse { r, from: at(draw), len: at(draw) },
        });
    }
    plan
}

/// The invariant: the gate's core count equals, to the byte, the sum over its regions of what
/// each holds committed less what purges gave back, and stays within the core size.
fn oracle(gate: &Gate, regions: &[Region]) -> Result<(), String> {
    let counted: usize = regions.iter().map(Region::core_counted).sum();
    let held = gate.held();
    if held.core != counted as u64 {
        return Err(format!("core counts {} B, its regions hold {counted} B", held.core));
    }
    if held.core > 8 * MIB * 32 {
        return Err(format!("core counts {} B, past its size", held.core));
    }
    Ok(())
}

fn core_gate() -> &'static Gate {
    let config = Config {
        ceiling: 1 << 30,
        core: 8 * MIB * 32,
        overdraft_per_worker: 2 * MIB,
        workers: 1,
        ungated: 0,
    };
    Gate::new(config).unwrap_or_else(|why| panic!("the test's gate {config:?}: {why}"))
}

/// Runs `plan` on fresh regions of a fresh gate, checking the oracle after every step and that
/// core is back to nothing once the regions drop.
fn run(plan: &[Step]) -> Result<(), String> {
    let gate = core_gate();
    let mut regions: Vec<Region> = (0..REGIONS) // alloc: a test's regions
        .map(|_| {
            Region::reserve_under(gate, RESERVED, Pages::Base, Tag::Other)
                .unwrap_or_else(|error| panic!("a test region of {RESERVED} B: {error}"))
        })
        .collect();
    for (n, &step) in plan.iter().enumerate() {
        match step {
            Step::Ensure { r, len } => {
                regions[r].ensure(len).map_err(|error| format!("step {n} {step:?}: {error}"))?;
            }
            Step::Decommit { r, keep } => {
                regions[r]
                    .decommit_after(keep)
                    .map_err(|error| format!("step {n} {step:?}: {error}"))?;
            }
            Step::Purge { r, from, len } => {
                // SAFETY: the test never reads or writes its regions' bytes.
                unsafe { regions[r].purge(from, len) };
            }
            Step::Reuse { r, from, len } => {
                let committed = regions[r].committed();
                let from = from.min(committed);
                let len = len.min(committed - from);
                regions[r].reuse(from, len).map_err(|error| format!("step {n} {step:?}: {error}"))?;
            }
        }
        oracle(gate, &regions).map_err(|broke| format!("after step {n} {step:?}: {broke}"))?;
    }
    drop(regions);
    match gate.held().core {
        0 => Ok(()),
        left => Err(format!("core counts {left} B with every region dropped")),
    }
}

/// The invariant: whatever a core region commits, decommits, purges and reuses, its gate's core
/// count is exactly what the regions hold, and nothing once they are gone.
#[hegel::test(test_cases = 100)]
fn core_counts_exactly_what_its_regions_hold(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan) {
        panic!("{broke}\nplan {plan:?}");
    }
}

/// The same over seeded plans, so the rule is run where Hegel's server is not.
#[test]
fn core_counts_exactly_what_its_regions_hold_over_seeded_plans() {
    for seed in 0..64 {
        let plan = plan(&mut SplitMix64::new(seed));
        if let Err(broke) = run(&plan) {
            panic!("seed {seed}: {broke}\nplan {plan:?}");
        }
    }
}

/// The control (test-the-cause on the oracle): one commit skips the gate, on a grant no count
/// stands behind. The oracle must name it; the count is then put right so the region's drop
/// gives back only what core holds.
#[test]
fn the_oracle_catches_a_commit_that_skips_the_gate() {
    let gate = core_gate();
    let regions = [Region::reserve_under(gate, RESERVED, Pages::Base, Tag::Other)
        .unwrap_or_else(|error| panic!("a test region of {RESERVED} B: {error}"))];
    regions[0].ensure(1 << 20).expect("a counted commit");
    oracle(gate, &regions).expect("a counted commit keeps the count");
    gate.skip_next.store(true, std::sync::atomic::Ordering::Relaxed);
    let before = regions[0].committed();
    regions[0].ensure(before + (4 << 20)).expect("the planted commit");
    let skipped = regions[0].committed() - before;
    let caught = oracle(gate, &regions).expect_err("a commit that skipped the gate is caught");
    assert!(caught.contains("its regions hold"), "named by the count it broke: {caught}");
    assert!(gate.core_take(skipped), "the skipped bytes fit core once counted");
    oracle(gate, &regions).expect("counted again");
}

/// The invariant: the core path allocates nothing, granted or refused, so an allocator's commit
/// hook (the general heap's arena commits through `Region::reuse` from inside its own) may take
/// it. The refusal goes through the process gate's trim of the chunk cache too.
#[test]
fn a_core_commit_granted_or_refused_allocates_nothing() {
    let gate = core_gate();
    gate.process.store(true, std::sync::atomic::Ordering::Relaxed);
    let region = Region::reserve_under(gate, 512 << 20, Pages::Base, Tag::Other)
        .unwrap_or_else(|error| panic!("a test region of 512 MiB: {error}"));
    region.ensure(1 << 20).expect("a first commit, warming what the first commit touches");
    let granted = blaze_alloc_count::assert_at_most(0, "a granted core commit", || {
        region.ensure(8 << 20)
    });
    granted.expect("a commit core has room for");
    let refused = blaze_alloc_count::assert_at_most(0, "a refused core commit", || {
        region.ensure(300 << 20)
    });
    match refused {
        Err(MemError::CoreFull { tag: Tag::Other, used, core, .. }) => {
            assert_eq!(core, 8 * MIB * 32);
            assert!(used <= core, "core never passes its size: {used}");
        }
        other => panic!("a commit past core is refused as `CoreFull`: {other:?}"),
    }
    oracle(gate, std::slice::from_ref(&region)).expect("a refusal keeps the count");
}

/// The config a daemon installs under its ceiling keeps the machine config's proportions.
#[test]
fn a_config_under_a_ceiling_is_accepted() {
    let config = config_under(16 << 30);
    Budget::new(config).unwrap_or_else(|why| panic!("the config under 16 GiB {config:?}: {why}"));
    assert_eq!((config.ceiling, config.core, config.ungated), (16 << 30, 6 << 30, 2 << 30));
}

// --- Tasks: typed failure, giving way --------------------------------------------------------

fn ranked(task: u64, age: u64) -> Ranked {
    Ranked { task: TaskId(task), band: Band(1), age: Age(age) }
}

/// A task that alone does not fit under a small ceiling fails with `TooBig` naming it, as an
/// unwind its caller catches, never an allocation failure or an abort; its quanta all come back.
#[test]
fn a_task_past_a_small_ceiling_fails_typed() {
    let gate = small(1);
    let unwound = scoped(gate, || {
        catch_unwind(AssertUnwindSafe(|| {
            frame::task_ranked(Tag::Codegen, ranked(7, 0), |frame| {
                frame.alloc_slice_fill_with((100 * MIB) as usize, |i| i as u8).len()
            })
            .expect("nothing holds the only task back")
        }))
    });
    let payload = unwound.expect_err("a task past the ceiling unwinds");
    let too_big = payload.downcast_ref::<TooBig>().expect("its payload is `TooBig`");
    assert_eq!(too_big.task, TaskId(7), "it names the task");
    assert_eq!(too_big.ceiling, 64 * MIB);
    assert!(too_big.wanted > 64 * MIB - 8 * MIB - 4 * MIB, "past what one task may hold");
    let held = gate.held();
    assert_eq!(held.tasks, 0, "every quantum of the failed task came back: {held:?}");
    assert!(gate.committed() <= gate.ceiling());
}

/// Two tasks that each fit alone but not together: the younger, asking last, is told to give
/// way and unwinds with `Cancelled` at its next safe point; posted again while its elder runs,
/// it is held back at once (no wait), named by `take_unheld` once the elder ends, and then runs.
#[test]
fn a_younger_task_gives_way_to_its_elder_and_runs_after_it() {
    let gate = small(2);
    let (elder_holds, holds) = crossbeam_channel::bounded::<()>(1);
    let (younger_gave_way, gave_way) = crossbeam_channel::bounded::<()>(1);
    let elder = std::thread::spawn(move || {
        scoped(gate, || {
            frame::task_ranked(Tag::Codegen, ranked(1, 0), |frame| {
                let held = frame.alloc_slice_fill_with((20 * MIB) as usize, |i| i as u8);
                elder_holds.send(()).expect("the younger waits for it");
                gave_way.recv().expect("the younger says it gave way");
                held[12_345]
            })
        })
    });
    holds.recv().expect("the elder holds its 20 MiB");
    let younger = || {
        frame::task_ranked(Tag::Codegen, ranked(2, 1), |frame| {
            frame.alloc_slice_fill_with((30 * MIB) as usize, |i| i as u8).len()
        })
    };
    let first = scoped(gate, || catch_unwind(AssertUnwindSafe(younger)));
    let payload = first.expect_err("the younger cannot fit beside its elder");
    assert!(payload.is::<frame::Cancelled>(), "it gives way, it does not fail");
    assert!(!frame::cancelling(), "its outermost task ended");
    assert_eq!(
        scoped(gate, younger),
        Err(HeldBack { task: TaskId(2), by: Hold::Elder(TaskId(1)) }),
        "posted again while its elder runs, it is held back without waiting"
    );
    assert_eq!(
        gate.end_parked(TaskId(1)),
        Err(Misuse::Running { task: TaskId(1), call: "end_parked" }),
        "a running task is not parked"
    );
    assert!(!gate.any_unheld());
    younger_gave_way.send(()).expect("the elder waits for it");
    assert_eq!(elder.join().expect("the elder"), Ok(12_345usize as u8), "the elder was never cancelled");
    let mut unheld = Vec::new();
    gate.take_unheld(&mut unheld);
    assert_eq!(unheld, [TaskId(2)], "the elder's end lets the younger go");
    assert_eq!(scoped(gate, younger), Ok((30 * MIB) as usize), "posted again, it runs");
    assert_eq!(gate.held().tasks, 0, "both tasks gave every quantum back");
    assert_eq!(gate.end_parked(TaskId(2)), Ok(false), "an ended task is not known");
}

/// A task that gave way and was dropped by its scheduler is forgotten, and what it held back
/// goes: `end_parked` ends its parked row once.
#[test]
fn a_dropped_parked_task_is_forgotten_once() {
    let gate = small(1);
    let unwound = scoped(gate, || {
        catch_unwind(AssertUnwindSafe(|| {
            frame::task_ranked(Tag::Codegen, ranked(3, 0), |frame| {
                let held = frame.alloc_slice_fill_with((8 * MIB) as usize, |i| i as u8);
                // Room for 4 quanta of tasks (20 MiB less core and the overdraft): the task,
                // holding 5 or more, gives way.
                gate.room(20 * MIB);
                frame.alloc_slice_fill_with((8 * MIB) as usize, |i| i as u8).len() + held.len()
            })
        }))
    });
    let payload = unwound.expect_err("lowered room makes it give way");
    assert!(payload.is::<frame::Cancelled>());
    assert_eq!(gate.end_parked(TaskId(3)), Ok(true), "parked under the room, now forgotten");
    assert_eq!(gate.end_parked(TaskId(3)), Ok(false), "forgotten once");
    gate.room(u64::MAX);
}

/// What is free for new starts: the limit less committed, the workers' overdrafts, and what
/// running tasks are forecast to take past what they hold.
#[test]
fn free_for_starts_takes_off_the_overdrafts_and_the_forecasts() {
    let gate = small(2);
    // 64 MiB less 8 MiB core and two 4 MiB overdrafts.
    assert_eq!(gate.free_for_starts(|_| 0), 48 * MIB, "no task runs");
    let (free, holds) = scoped(gate, || {
        frame::task_ranked(Tag::Codegen, ranked(4, 0), |frame| {
            let held = frame.alloc_slice_fill_with((8 * MIB) as usize, |i| i as u8);
            let holds = gate.held().tasks;
            let free = gate.free_for_starts(|task| if task == TaskId(4) { 30 * MIB } else { 0 });
            assert_eq!(held[7], 7);
            (free, holds)
        })
        .expect("nothing holds it back")
    });
    assert_eq!(free, 48 * MIB - holds - (30 * MIB - holds), "its holding and its forecast's rest");
}

/// A task's span is its task's quanta while it runs, counted in its gate's tasks, and none of
/// them once it ends: the span went to the thread's shelf, which is core.
#[test]
fn a_tasks_span_is_its_quanta_while_it_runs() {
    let gate = small(1);
    let during = scoped(gate, || {
        frame::task(Tag::Codegen, |frame| {
            let big = frame.alloc_slice_fill_with((8 * MIB) as usize, |i| i as u8);
            assert_eq!(big[12_345], 12_345usize as u8);
            gate.held().tasks
        })
    });
    assert!(during >= 8 * MIB, "a span's pages are reserved from the task's gate: {during}");
    assert_eq!(gate.held().tasks, 0, "a span on the shelf holds none of the task's quanta");
}

// --- Config -----------------------------------------------------------------------------------

/// The config a process that installed none runs under is one the Budget takes.
#[test]
fn the_machines_config_is_accepted() {
    let config = machine_config();
    Budget::new(config).unwrap_or_else(|why| panic!("the machine's config {config:?}: {why}"));
    assert!(config.ceiling > 0 && config.core < config.ceiling);
}
