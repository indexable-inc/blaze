//! The forecast's DST (`testing` skill, dst-not-review): a build's task list replayed through a
//! real [`Plane`] three times (a cold daemon, then warm), each task forecast by
//! [`forecast`] from what the builds before it measured, admitted against a budget, and ended
//! in a seeded order holding its drawn peak, which the history then learns ([`learn`], the
//! kinds' [`Estimate`]s, as `blaze-cargo`'s plane and `blaze-history` keep them).
//!
//! The oracle:
//! - a task with its own record is forecast within [`WITHIN_TENTHS`] tenths of the peak it then
//!   holds (its peak moves at most 4% from run to run);
//! - after every step of the plane, no first-run task is ready with a work thread free while the
//!   budget holds the peaks of what runs and its own: a guess holds nothing back the budget
//!   holds (the smoke's cold build: `program (Codegen)` granted the whole budget, run alone,
//!   for a peak 354 times smaller);
//! - every task ends.
//!
//! Two plants ([`Plant`]) prove the oracle catches each rule it claims, in a control.

use blaze_protocol::events::{NodeKind, NodeState};
use blaze_rng::SplitMix64;

use super::*;
use crate::{
    admission::Estimate,
    machine::{NodeId, Outcome, Pool, Spec},
    plane::{Budget, Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId},
};

/// The builds a case replays: a cold daemon's, then two warm.
const BUILDS: u32 = 3;

/// Seeds a control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 64;

/// A wrong forecast the oracle must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// A guess is the whole budget, as `push_program` forecast a program never measured until
    /// 2026-11-12: it runs alone.
    GuessIsTheBudget,
    /// An own record under what the task's size guesses gives way to the guess, as
    /// `push_sized` did until 2026-11-12.
    SizeOverOwn,
}

/// One task of the build's list: its kind, its source, and the peak it holds (before the
/// run's jitter).
#[derive(Debug, Clone, Copy)]
struct Task {
    kind: usize,
    source: u64,
    peak: u64,
}

#[derive(Debug)]
struct Plan {
    budget: u64,
    work: usize,
    tasks: Vec<Task>,
    seed: u64,
}

fn plan_of(seed: u64) -> Plan {
    let mut random = SplitMix64::new(seed);
    let budget = (8 + random.below(57)) << 30;
    let work = 1 + random.below(8) as usize;
    let kinds = 1 + random.below(4) as usize;
    // Each kind's bytes per KiB, from a KiB a KiB to 64 KiB a KiB.
    // alloc: a plan, once a case
    let rates: Vec<u64> = (0..kinds).map(|_| 1024 + random.below(64 << 10)).collect();
    // Every peak within half a thread's share: what an own record forecasts never holds a
    // first run back by itself.
    let most = budget / work as u64 / 2;
    let count = 2 + random.below(40);
    // alloc: a plan, once a case
    let tasks = (0..count)
        .map(|_| {
            let kind = random.below(kinds as u64) as usize;
            let source = (1 + random.below(4096)) << 10;
            // Half to one and a half times the kind's rate: units of a kind differ.
            let factor = 8 + random.below(17);
            let peak = (source / 1024 * rates[kind] * factor / 16).clamp(1 << 20, most);
            Task { kind, source, peak }
        })
        .collect();
    Plan { budget, work, tasks, seed: random.next_u64() }
}

/// What the daemon learned, kept across the builds of a case.
#[derive(Default)]
struct Learned {
    // alloc: a case's history, once a case
    records: Vec<Option<u64>>,
    per_kib: Vec<Estimate>,
    peaks: Vec<Estimate>,
}

/// What broke a case.
#[derive(Debug)]
enum Broke {
    Oracle(String),
    Harness(String),
}

fn oracle(why: impl std::fmt::Display) -> Broke {
    Broke::Oracle(why.to_string()) // alloc: a failure's message, once
}

fn harness(why: impl std::fmt::Display) -> Broke {
    Broke::Harness(why.to_string()) // alloc: a failure's message, once
}

/// The forecast of task `at`, as the plane is given it.
fn forecast_of(plan: &Plan, learned: &Learned, at: usize, plant: Plant) -> Forecast {
    let task = plan.tasks[at];
    let seen = |it: &Estimate| (it.seen > 0).then_some(it.predicted);
    let known = Known {
        own: learned.records[at],
        source: task.source,
        per_kib: seen(&learned.per_kib[task.kind]),
        kind: seen(&learned.peaks[task.kind]),
        least: 0,
    };
    let share = share(Some(plan.budget), plan.work);
    let it = forecast(&known, share);
    match plant {
        Plant::None => it,
        Plant::GuessIsTheBudget if it.basis != Basis::Own => {
            Forecast { bytes: plan.budget, basis: it.basis }
        }
        Plant::GuessIsTheBudget => it,
        Plant::SizeOverOwn => {
            let sized = forecast(&Known { own: None, ..known }, share);
            match it.basis == Basis::Own && sized.bytes > it.bytes {
                true => sized,
                false => it,
            }
        }
    }
}

/// One build of the plan, learning into `learned`; what its own records' forecasts were checked
/// against, into `check`.
fn build(
    plan: &Plan,
    learned: &mut Learned,
    check: &mut Check,
    plant: Plant,
    round: u32,
) -> Result<(), Broke> {
    let request = RequestId(round);
    let budget = Budget {
        bytes: plan.budget,
        from: "the replay's budget".to_owned(), // alloc: the budget, once a build
        kept_back: 0,
    };
    let config = PlaneConfig { io: 1, work: plan.work, budget: Some(budget), starve_after: 8 };
    let mut plane = Plane::new(config).map_err(harness)?;
    let mut random = SplitMix64::new(plan.seed ^ u64::from(round));
    // alloc: a build's forecasts and peaks, once a build
    let forecasts: Vec<Forecast> =
        (0..plan.tasks.len()).map(|at| forecast_of(plan, learned, at, plant)).collect();
    let first_run: Vec<bool> = learned.records.iter().map(Option::is_none).collect();
    // This run's peaks: within 4% of the task's.
    let peaks: Vec<u64> = plan
        .tasks
        .iter()
        .map(|it| it.peak - it.peak / 25 + random.below(it.peak / 25 * 2 + 1))
        .collect();
    let mut running: Vec<usize> = Vec::new(); // alloc: a build's running tasks
    let mut ended = 0;
    let mut at_ns = 0;
    let step = |plane: &mut Plane, input: PlaneInput, running: &mut Vec<usize>| {
        let outputs = plane.step(input).map_err(harness)?;
        for output in outputs {
            if let PlaneOutput::Run { id, .. } = output {
                let at = id.0 as usize;
                let granted = plane.admitted(id).map_or(0, |it| it.granted);
                if granted != forecasts[at].bytes.min(plan.budget) {
                    return Err(harness(format_args!(
                        "task {at} forecast {} was granted {granted}",
                        forecasts[at].bytes
                    )));
                }
                running.push(at);
            }
        }
        // A first run is never held back while the budget holds the peaks of what runs and
        // its own, with a thread free.
        let held: u64 = running.iter().map(|&it| peaks[it]).sum();
        if running.len() < plan.work {
            for at in 0..plan.tasks.len() {
                let ready = plane.state(NodeId(at as u32)) == Some(&NodeState::Ready);
                if ready && first_run[at] && held + peaks[at] <= plan.budget {
                    return Err(oracle(format_args!(
                        "build {round}: first-run task {at} (forecast {} by {:?}, peak {}) is ready with {} of {} threads running, whose peaks are {held} of a budget of {}",
                        forecasts[at].bytes,
                        forecasts[at].basis,
                        peaks[at],
                        running.len(),
                        plan.work,
                        plan.budget,
                    )));
                }
            }
        }
        Ok::<(), Broke>(())
    };
    step(&mut plane, PlaneInput::Open { at_ns, request, weight: 1, keep_going: true, idle: false }, &mut running)?;
    // alloc: the nodes, once a build
    let nodes: Vec<Spec> = (0..plan.tasks.len())
        .map(|at| Spec {
            id: NodeId(at as u32),
            kind: NodeKind::Codegen,
            name: format!("task {at}"), // alloc: a node's name, once a task
            deps: Vec::new(),
            pool: Pool::Work,
            cost: 1,
            key: None,
            memory: forecasts[at].bytes,
        })
        .collect();
    step(&mut plane, PlaneInput::Add { at_ns, request, nodes }, &mut running)?;
    step(&mut plane, PlaneInput::Close { at_ns, request }, &mut running)?;
    // Bounded: each turn ends one task.
    while ended < plan.tasks.len() {
        if running.is_empty() {
            return Err(oracle(format_args!(
                "build {round}: {} of {} tasks ended and none runs",
                ended,
                plan.tasks.len()
            )));
        }
        at_ns += 1;
        let at = running.swap_remove(random.below(running.len() as u64) as usize);
        let (forecast, peak) = (forecasts[at], peaks[at]);
        // Held by whether the task has its own record, not by the basis the forecast claims:
        // a forecast that set its record aside is as wrong as one far from it.
        if !first_run[at] && (forecast.basis != Basis::Own || !within(forecast.bytes, peak)) {
            return Err(oracle(format_args!(
                "build {round}: task {at} has its own record, was forecast {} by {:?}, then held {peak}: not by its record within a tenth",
                forecast.bytes, forecast.basis
            )));
        }
        check.observe(forecast, peak);
        let task = plan.tasks[at];
        learned.records[at] = Some(learn(learned.records[at], peak));
        learned.per_kib[task.kind].observe(peak / task.source.div_ceil(1024));
        learned.peaks[task.kind].observe(peak);
        ended += 1;
        let done = PlaneInput::Done {
            at_ns,
            id: NodeId(at as u32),
            outcome: Outcome::Built,
            added: Vec::new(), // alloc: none, empty
            group: None,
            peak,
        };
        step(&mut plane, done, &mut running)?;
    }
    Ok(())
}

fn run(plan: &Plan, plant: Plant, check: &mut Check) -> Result<(), Broke> {
    let kinds = plan.tasks.iter().map(|it| it.kind).max().map_or(0, |it| it + 1);
    let mut learned = Learned {
        records: vec![None; plan.tasks.len()], // alloc: a case's records
        per_kib: vec![Estimate::default(); kinds], // alloc: a case's kinds
        peaks: vec![Estimate::default(); kinds], // alloc: a case's kinds
    };
    for round in 0..BUILDS {
        build(plan, &mut learned, check, plant, round)?;
    }
    Ok(())
}

/// The invariant: over many replayed builds, every task with its own record is forecast
/// within a tenth of what it then holds, and no first run is held back while the budget holds
/// it; the own records were checked (the warm builds ran).
#[test]
fn forecasts_hold_own_records_within_a_tenth_and_hold_no_first_run_back() {
    let mut check = Check::default();
    for seed in 0..200 {
        let plan = plan_of(seed);
        if let Err(broke) = run(&plan, Plant::None, &mut check) {
            panic!("seed {seed}: {broke:?}\nplan {plan:?}");
        }
    }
    let own = check.tally(Basis::Own);
    assert!(own.tasks > 1000 && own.within == own.tasks, "{own:?}");
    let guessed: u64 =
        [Basis::Sized, Basis::Kind, Basis::Unseen].iter().map(|it| check.tally(*it).tasks).sum();
    assert!(guessed > 100, "first runs forecast: {guessed}");
}

/// A plant the oracle must catch in one of [`CONTROL_SEEDS`] plans.
fn control(plant: Plant) {
    for seed in 0..CONTROL_SEEDS {
        match run(&plan_of(seed), plant, &mut Check::default()) {
            Err(Broke::Oracle(why)) => {
                eprintln!("{plant:?} caught at seed {seed}: {why}");
                return;
            }
            Err(broke @ Broke::Harness(_)) => panic!("seed {seed}, {plant:?}: {broke:?}"),
            Ok(()) => {}
        }
    }
    panic!("the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}");
}

#[test]
fn the_oracle_catches_a_guess_that_takes_the_budget() {
    control(Plant::GuessIsTheBudget);
}

#[test]
fn the_oracle_catches_a_size_guess_over_an_own_record() {
    control(Plant::SizeOverOwn);
}

/// The rule's cases: an own record stands under a larger guess; a guess is held to a thread's
/// share and the floor; with no size, the kind's peak.
#[test]
fn an_own_record_stands_and_a_guess_is_held_to_a_threads_share() {
    let mib = 1u64 << 20;
    let share = share(Some(1024 * mib), 4);
    assert_eq!(share, 256 * mib);
    let small = Known { own: Some(10 * mib), source: 4096 << 10, per_kib: Some(mib), ..Known::default() };
    assert_eq!(forecast(&small, share), Forecast { bytes: 10 * mib, basis: Basis::Own });
    let cold = Known { source: 4096 << 10, ..Known::default() };
    assert_eq!(forecast(&cold, share), Forecast { bytes: 256 * mib, basis: Basis::Unseen });
    let sized = Known { source: 100 << 10, per_kib: Some(mib), kind: Some(80 * mib), ..Known::default() };
    assert_eq!(forecast(&sized, share), Forecast { bytes: 80 * mib, basis: Basis::Sized });
    let tiny = Known { source: 1 << 10, per_kib: Some(1024), ..Known::default() };
    assert_eq!(forecast(&tiny, share), Forecast { bytes: GUESS_FLOOR, basis: Basis::Sized });
    let kind = Known { kind: Some(100 * mib), ..Known::default() };
    assert_eq!(forecast(&kind, share), Forecast { bytes: 100 * mib, basis: Basis::Kind });
    assert_eq!(forecast(&Known::default(), share).basis, Basis::Unseen);
    assert_eq!(learn(Some(100), 50), 75);
    assert_eq!(learn(Some(100), 150), 150);
    assert!(within(110, 100) && within(90, 100) && !within(111, 100));
}
