//! Admission by footprint's DST (docs/v2/plan/admission-by-footprint.md, "Proof"; `testing`
//! skill, dst-not-review), and a few cases of [`Charge`] alone.
//!
//! A real [`Plane`] admits drawn tasks against a ceiling, fed what a simulated allocator says.
//! The allocator's footprint is a baseline, the bytes each running task holds, what tasks freed
//! that it keeps until a purge, and a part of each free stuck for good (fragmentation). Each task
//! grows in steps to its peak, which may pass its grant, then ends. Growth within its grant is
//! allocated with no gate, as the daemon's allocator does. Growth past its grant, or growth that
//! would pass the ceiling, goes through the real gate ([`past_ceiling`]). The driver gives the
//! plane each running task's realized bytes and a footprint reading every few ticks and right
//! after gated growth, and purges when the plane or the gate asks. The gate's purge is done at
//! once (the guard's thread waits for it); the plane's lands a drawn lag of ticks after its ask,
//! as the daemon's purger thread's does, with readings before it not marked purged. A task that
//! ends or gives way in the lag frees bytes the purge may not reach (a worker that already gave
//! back keeps them), so the driver asks again rather than read it as purged.
//!
//! After every act the oracle checks:
//! - the footprint is at most the ceiling;
//! - growth within a task's grant always fits, unless that task runs alone and alone passes
//!   the ceiling (admission let in nothing the ceiling has no room for);
//! - the gate refuses growth (wait, give way or fail) only when a purge could not help: the
//!   allocator keeps nothing, or the driver purged for it since what it keeps last grew;
//! - a task fails only with [`TooBig`], only when it runs alone and alone passes the ceiling.
//!
//! At the end every other task has been built within [`MOST_TICKS`]: no deadlock
//! (bounded-progress).
//!
//! Four plants ([`Plant`]) prove the oracle catches what it claims to, each in a control.
//! Replay: a control or [`replay_one_seed`] names a seed of [`plan_of`]; the Hegel test prints
//! its plan.

use std::fmt;

use blaze_protocol::events::NodeKind;
use blaze_rng::SplitMix64;
use hegel::{TestCase, generators as gs};

use super::*;
use crate::{
    machine::{NodeId, Outcome, Pool, Spec},
    plane::{Budget, Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId},
};

/// Most tasks a plan draws: a case runs in milliseconds.
const MOST_TASKS: u64 = 12;

/// The ticks every task must have ended in: far more than any plan takes with nothing wrong.
const MOST_TICKS: u64 = 20_000;

/// The simulated time of one tick: 10 ms, so the plane asks a purge at most every 25 ticks.
const TICK_NS: u64 = 10_000_000;

/// Seeds of [`plan_of`] a control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 256;

/// The seed [`replay_one_seed`] runs, and its plant: set them to what a failure names.
const REPLAY: (u64, Plant) = (0, Plant::None);

/// The one request the plane has open.
const REQUEST: RequestId = RequestId(1);

/// A wrong driver the oracle must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// Admission by grants and live bytes alone, as before this lane: the driver gives the
    /// plane `Held` of the live bytes, never a footprint or what a task realized.
    GrantsOnly,
    /// A driver that never purges: it ignores [`Plane::purge_due`], and the gate is told a
    /// purge was always tried.
    NoPurge,
    /// The daemon before 2026-11-09: a plane's purge read as purged as it is asked, with what
    /// the workers keep still in the footprint.
    EarlyPurged,
    /// A plane's purge read as purged when it lands though a task freed bytes in its lag, which
    /// the purge left kept.
    LandsDespiteFrees,
}

// --- The plan ------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Task {
    grant: u64,
    peak: u64,
    steps: u64,
}

impl Task {
    /// What it holds after `step` of its steps (1-based).
    fn level(&self, step: u64) -> u64 {
        (self.peak * step).div_ceil(self.steps).max(1)
    }
}

#[derive(Debug)]
struct Plan {
    ceiling: u64,
    baseline: u64,
    work: usize,
    tasks: Vec<Task>,
    /// Ticks between footprint readings.
    read_every: u64,
    /// Sixteenths of each free that stick for good, and the most that may stick.
    sticky: u64,
    most_stuck: u64,
    /// The scheduler's choices: which running task acts each tick.
    seed: u64,
    /// Ticks from a plane's purge ask to its landing: the purger's thread's wait for the
    /// workers' heaps.
    purge_lag: u64,
}

/// A source of draws: Hegel's (shrunk, replayed by Hegel) or a seed's ([`plan_of`]).
trait Draw {
    /// In `lo..=hi`.
    fn int(&mut self, lo: u64, hi: u64) -> u64;

    fn one_in(&mut self, n: u64) -> bool {
        self.int(0, n - 1) == 0
    }
}

struct Drawn<'a>(&'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
    }
}

impl Draw for SplitMix64 {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        match (hi - lo).checked_add(1) {
            Some(n) => lo + self.below(n),
            None => self.next_u64(),
        }
    }
}

fn plan(d: &mut impl Draw) -> Plan {
    let ceiling = d.int(64, 4096);
    let baseline = d.int(0, ceiling / 4);
    let work = d.int(1, 4) as usize;
    let count = d.int(1, MOST_TASKS);
    // alloc: a plan, once a case
    let tasks = (0..count)
        .map(|_| {
            let grant = d.int(1, ceiling / 2);
            // One in four passes its grant, some of those past the ceiling alone.
            let peak =
                if d.one_in(4) { d.int(grant + 1, ceiling + ceiling / 2) } else { d.int(1, grant) };
            Task { grant, peak, steps: d.int(1, 4) }
        })
        .collect();
    Plan {
        ceiling,
        baseline,
        work,
        tasks,
        read_every: d.int(1, 8),
        sticky: d.int(0, 4),
        most_stuck: d.int(0, ceiling / 4),
        seed: d.int(0, u64::MAX),
        purge_lag: d.int(0, 4),
    }
}

/// The plan of a seed, for the controls and [`replay_one_seed`].
fn plan_of(seed: u64) -> Plan {
    plan(&mut SplitMix64::new(seed))
}

// --- The simulation ------------------------------------------------------------------------

/// What broke a case: the oracle (the code under test is wrong) or the harness (the test is).
#[derive(Debug)]
enum Broke {
    Oracle(String),
    Harness(String),
}

impl fmt::Display for Broke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Broke::Oracle(why) => write!(f, "the oracle: {why}"),
            Broke::Harness(why) => write!(f, "the harness: {why}"),
        }
    }
}

fn oracle(why: impl fmt::Display) -> Broke {
    Broke::Oracle(why.to_string()) // alloc: a failure's message, once
}

fn harness(why: impl fmt::Display) -> Broke {
    Broke::Harness(why.to_string()) // alloc: a failure's message, once
}

/// A task that runs.
#[derive(Debug, Clone, Copy)]
struct Job {
    task: usize,
    id: NodeId,
    /// Its grant now: the plane's, raised to what gated growth took past it.
    grant: u64,
    holds: u64,
    /// The steps it took.
    step: u64,
    /// Waits at the gate.
    waiting: bool,
    /// The driver purged for its growth since what the allocator keeps last grew.
    tried: bool,
    /// When it started, against the others: the newest gives way.
    stamp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ended {
    Built,
    Failed(String),
}

struct Sim<'p> {
    plan: &'p Plan,
    plant: Plant,
    plane: Plane,
    jobs: Vec<Job>,
    ended: Vec<Option<Ended>>,
    stuck: u64,
    kept: u64,
    clock: u64,
    stamps: u64,
    finished: Option<bool>,
    /// The plane asked a purge since the driver last did one.
    purge_due: bool,
    /// The plane's purge asked of the purger and not yet landed.
    purging: Option<Purging>,
}

/// A plane's purge on its way ([`Sim::ask_purge`]).
#[derive(Debug, Clone, Copy)]
struct Purging {
    /// The tick clock it lands at.
    lands: u64,
    /// What the allocator kept as it was asked: what it gives back.
    kept: u64,
    /// A task freed bytes since it was asked.
    freed: bool,
}

impl<'p> Sim<'p> {
    fn new(plan: &'p Plan, plant: Plant) -> Result<Sim<'p>, Broke> {
        let budget = Budget {
            bytes: plan.ceiling,
            from: "the simulated daemon's ceiling".to_owned(), // alloc: the budget, once a case
            kept_back: 0,
        };
        let plane =
            Plane::new(PlaneConfig { io: 1, work: plan.work, budget: Some(budget), starve_after: 8 })
                .map_err(harness)?;
        let mut sim = Sim {
            plan,
            plant,
            plane,
            jobs: Vec::new(),                     // alloc: a case's jobs
            ended: vec![None; plan.tasks.len()], // alloc: a case's tasks
            stuck: 0,
            kept: 0,
            clock: 0,
            stamps: 0,
            finished: None,
            purge_due: false,
            purging: None,
        };
        sim.step(PlaneInput::Open {
            at_ns: 0,
            request: REQUEST,
            weight: 1,
            keep_going: true,
            idle: false,
        })?;
        sim.read()?;
        let nodes = plan
            .tasks
            .iter()
            .enumerate()
            .map(|(at, task)| Spec {
                id: NodeId(at as u32),
                kind: NodeKind::Codegen,
                name: format!("task {at}"), // alloc: a node's name, once a task
                deps: Vec::new(),
                pool: Pool::Work,
                cost: 1,
                key: None,
                memory: task.grant,
            })
            .collect(); // alloc: the nodes, once a case
        sim.step(PlaneInput::Add { at_ns: 0, request: REQUEST, nodes })?;
        sim.step(PlaneInput::Close { at_ns: 0, request: REQUEST })?;
        Ok(sim)
    }

    fn footprint(&self) -> u64 {
        self.plan.baseline + self.stuck + self.kept + self.jobs.iter().map(|it| it.holds).sum::<u64>()
    }

    /// Steps the plane, starting a job for each task it runs.
    fn step(&mut self, input: PlaneInput) -> Result<(), Broke> {
        let outputs = self.plane.step(input).map_err(harness)?;
        self.purge_due |= self.plane.purge_due();
        for output in outputs {
            match output {
                PlaneOutput::Run { id, .. } => {
                    let task = id.0 as usize;
                    if task >= self.plan.tasks.len()
                        || self.ended[task].is_some()
                        || self.jobs.iter().any(|it| it.id == id)
                    {
                        return Err(harness(format_args!("the plane ran {id}, which waits for no run")));
                    }
                    let Some(admitted) = self.plane.admitted(id) else {
                        return Err(harness(format_args!("the plane ran {id} and admitted nothing")));
                    };
                    self.stamps += 1;
                    self.jobs.push(Job {
                        task,
                        id,
                        grant: admitted.granted,
                        holds: 0,
                        step: 0,
                        waiting: false,
                        tried: false,
                        stamp: self.stamps,
                    });
                }
                PlaneOutput::Finished { ok, .. } => self.finished = Some(ok),
                PlaneOutput::Yield { id } => {
                    return Err(harness(format_args!("{id} asked to give way: no request is idle")));
                }
                PlaneOutput::Said { .. } => {}
            }
        }
        Ok(())
    }

    /// The driver measures: each running task's realized bytes, then the footprint; under
    /// [`Plant::GrantsOnly`] the live bytes alone.
    fn read(&mut self) -> Result<(), Broke> {
        self.read_after(false)
    }

    fn read_after(&mut self, purged: bool) -> Result<(), Broke> {
        let at_ns = self.clock;
        if self.plant == Plant::GrantsOnly {
            let live = self.plan.baseline + self.jobs.iter().map(|it| it.holds).sum::<u64>();
            return self.step(PlaneInput::Held { at_ns, bytes: live });
        }
        // alloc: the running ids, once a reading
        let realized: Vec<(NodeId, u64)> = self.jobs.iter().map(|it| (it.id, it.holds)).collect();
        for (id, bytes) in realized {
            self.step(PlaneInput::Realized { at_ns, id, bytes })?;
        }
        let bytes = self.footprint();
        self.step(PlaneInput::Footprint { at_ns, bytes, kept: self.kept, purged })
    }

    /// The gate's purge: done at once.
    fn purge(&mut self) -> Result<(), Broke> {
        self.kept = 0;
        self.purge_due = false;
        self.read_after(true)
    }

    /// The plane's purge is asked of the purger: it lands [`Plan::purge_lag`] ticks on.
    fn ask_purge(&mut self) -> Result<(), Broke> {
        self.purge_due = false;
        let lands = self.clock + self.plan.purge_lag * TICK_NS;
        self.purging = Some(Purging { lands, kept: self.kept, freed: false });
        if self.plant == Plant::EarlyPurged {
            return self.read_after(true);
        }
        Ok(())
    }

    /// A plane's purge whose time came lands: what the allocator kept at its ask is given back
    /// and the plane reads it purged; or, a task having freed bytes since, it is asked again.
    fn land_purge(&mut self) -> Result<(), Broke> {
        let Some(purging) = self.purging else { return Ok(()) };
        if self.clock < purging.lands {
            return Ok(());
        }
        self.kept -= purging.kept.min(self.kept);
        if purging.freed && self.plant != Plant::LandsDespiteFrees {
            self.purging = None;
            return self.ask_purge();
        }
        self.purging = None;
        self.read_after(true)
    }

    /// `bytes` freed: a part sticks, the rest the allocator keeps; no purge tried since.
    fn free(&mut self, bytes: u64) {
        let stuck = (bytes * self.plan.sticky / 16).min(self.plan.most_stuck - self.stuck);
        self.stuck += stuck;
        self.kept += bytes - stuck;
        for job in self.jobs.iter_mut() {
            job.tried = false;
        }
        if let Some(purging) = &mut self.purging {
            purging.freed = true;
        }
    }

    /// Ends job `j`: its bytes freed, the plane told.
    fn end(&mut self, j: usize, outcome: Outcome) -> Result<(), Broke> {
        let job = self.jobs.swap_remove(j);
        self.free(job.holds);
        self.ended[job.task] = Some(match &outcome {
            Outcome::Failed { why } => Ended::Failed(why.clone()), // clone: the case's record
            _ => Ended::Built,
        });
        self.step(PlaneInput::Done {
            at_ns: self.clock,
            id: job.id,
            outcome,
            added: Vec::new(), // alloc: none, empty
            group: None,
            peak: job.holds,
        })
    }

    /// One tick: the clock moves, a purge the plane asked is asked of the purger and one whose
    /// lag passed lands, and one running task acts.
    fn tick(&mut self, tick: u64, rng: &mut SplitMix64) -> Result<(), Broke> {
        self.clock += TICK_NS;
        // As the daemon's plane does: an ask while one is on its way is dropped, since the
        // plane asks no other until a reading after a purge.
        let due = std::mem::take(&mut self.purge_due);
        if due && self.plant != Plant::NoPurge && self.purging.is_none() {
            self.ask_purge()?;
        }
        self.land_purge()?;
        if self.jobs.is_empty() || tick % self.plan.read_every == 0 {
            self.read()?;
        }
        if !self.jobs.is_empty() {
            let j = rng.below(self.jobs.len() as u64) as usize;
            self.act(j)?;
        }
        let footprint = self.footprint();
        if footprint > self.plan.ceiling {
            return Err(oracle(format_args!(
                "the footprint is {footprint}, past the ceiling {}",
                self.plan.ceiling
            )));
        }
        Ok(())
    }

    /// Whether job `j`, holding `level`, alone passes the ceiling: beside the baseline and what
    /// stuck, with nothing else running.
    fn alone_passes(&self, level: u64) -> bool {
        self.jobs.len() == 1 && self.plan.baseline + self.stuck + level > self.plan.ceiling
    }

    fn act(&mut self, j: usize) -> Result<(), Broke> {
        let job = self.jobs[j];
        let task = self.plan.tasks[job.task];
        if job.step == task.steps {
            return self.end(j, Outcome::Built);
        }
        let level = task.level(job.step + 1).max(job.holds);
        let bytes = level - job.holds;
        let within = level <= job.grant;
        let footprint = self.footprint();
        if within && footprint + bytes <= self.plan.ceiling {
            let job = &mut self.jobs[j];
            job.holds = level;
            job.step += 1;
            job.waiting = false;
            return Ok(());
        }
        if within && !self.alone_passes(level) {
            return Err(oracle(format_args!(
                "{} grew within its grant of {} to {level} beside a footprint of {footprint}, past the ceiling {}: admission let in more than fits ({} running, {} kept, {} stuck)",
                job.id,
                job.grant,
                self.plan.ceiling,
                self.jobs.len(),
                self.kept,
                self.stuck,
            )));
        }
        let others = self.jobs.iter().enumerate().filter(|&(at, _)| at != j).map(|(_, it)| it);
        let others_unrealized = others.clone().map(|it| it.grant.saturating_sub(it.holds)).sum();
        let others_waiting = others.clone().filter(|it| it.waiting).count();
        let newest = others
            .clone()
            .filter(|it| it.waiting)
            .chain(std::iter::once(&job))
            .max_by_key(|it| it.stamp)
            .map_or(job.id, |it| it.id);
        let past = Past {
            ceiling: self.plan.ceiling,
            footprint,
            kept: self.kept,
            bytes,
            asker: job.id,
            asker_holds: job.holds,
            others_unrealized,
            others_running: self.jobs.len() - 1,
            others_waiting,
            newest,
            tried: job.tried || self.plant == Plant::NoPurge,
        };
        let step = past_ceiling(&past);
        if !matches!(step, Step::Take | Step::Purge) && self.kept > 0 && !job.tried {
            return Err(oracle(format_args!(
                "the gate said {step:?} to {} with {} kept that no purge was tried for",
                job.id, self.kept
            )));
        }
        match step {
            Step::Take => {
                let job = &mut self.jobs[j];
                job.holds = level;
                job.grant = job.grant.max(level);
                job.step += 1;
                job.waiting = false;
                job.tried = false;
                self.read()
            }
            Step::Purge => {
                self.jobs[j].tried = true;
                self.purge()
            }
            Step::Wait => {
                self.jobs[j].waiting = true;
                Ok(())
            }
            Step::GiveWay(id) => {
                let Some(at) = self.jobs.iter().position(|it| it.id == id) else {
                    return Err(oracle(format_args!("the gate gave way to {id}, which runs not")));
                };
                if !(at == j || self.jobs[at].waiting) {
                    return Err(oracle(format_args!("the gate gave way to {id}, which waits not")));
                }
                let gone = self.jobs.swap_remove(at);
                self.free(gone.holds);
                self.step(PlaneInput::Yielded { at_ns: self.clock, id })
            }
            Step::Fail(too_big) => {
                if too_big.id != job.id || !self.alone_passes(level) {
                    return Err(oracle(format_args!(
                        "the gate failed {} ({too_big}), which alone fits: {} running, {} baseline, {} stuck, {level} asked of a ceiling {}",
                        job.id,
                        self.jobs.len(),
                        self.plan.baseline,
                        self.stuck,
                        self.plan.ceiling,
                    )));
                }
                self.end(j, Outcome::Failed { why: too_big.to_string() })
            }
        }
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    let mut sim = Sim::new(plan, plant)?;
    let mut rng = SplitMix64::new(plan.seed);
    let mut tick = 0;
    while sim.ended.iter().any(Option::is_none) {
        if tick == MOST_TICKS {
            let left: Vec<usize> = (0..plan.tasks.len()).filter(|&at| sim.ended[at].is_none()).collect(); // alloc: a failure's list
            return Err(oracle(format_args!(
                "tasks {left:?} did not end in {MOST_TICKS} ticks: {} running, {} waiting, footprint {} of {} ({} kept, {} stuck), {} charged",
                sim.jobs.len(),
                sim.jobs.iter().filter(|it| it.waiting).count(),
                sim.footprint(),
                plan.ceiling,
                sim.kept,
                sim.stuck,
                sim.plane.charged(),
            )));
        }
        sim.tick(tick, &mut rng)?;
        tick += 1;
    }
    let failed = sim.ended.iter().any(|it| matches!(it, Some(Ended::Failed(_))));
    match sim.finished {
        Some(ok) if ok == !failed => Ok(()),
        finished => {
            Err(harness(format_args!("the request finished {finished:?} with a failure {failed}")))
        }
    }
}

// --- The tests ------------------------------------------------------------------------------

/// The invariant: whatever the peaks, the grants and what the allocator keeps, the footprint
/// never passes the ceiling; growth within a grant never needs the gate; a task fails only
/// when it alone passes the ceiling, with [`TooBig`]; every other task is built.
#[hegel::test(test_cases = 300)]
fn footprint_stays_under_the_ceiling_unless_one_task_alone_exceeds_it(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

/// A plant the oracle must catch in one of [`CONTROL_SEEDS`] seeded plans (test-the-cause on
/// the test itself): a green DST with a blind oracle proves nothing.
fn control(plant: Plant) {
    for seed in 0..CONTROL_SEEDS {
        match run(&plan_of(seed), plant) {
            Err(Broke::Oracle(why)) => {
                eprintln!(
                    "{plant:?} caught at seed {seed} (REPLAY = ({seed}, Plant::{plant:?})): {why}"
                );
                return;
            }
            Err(broke @ Broke::Harness(_)) => panic!("seed {seed}, {plant:?}: {broke}"),
            Ok(()) => {}
        }
    }
    panic!(
        "the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

#[test]
fn the_oracle_catches_admission_by_grants_only() {
    control(Plant::GrantsOnly);
}

#[test]
fn the_oracle_catches_a_gate_that_never_purges() {
    control(Plant::NoPurge);
}

#[test]
fn the_oracle_catches_a_purge_read_before_it_lands() {
    control(Plant::EarlyPurged);
}

#[test]
fn the_oracle_catches_a_purge_read_purged_though_a_task_freed_in_its_lag() {
    control(Plant::LandsDespiteFrees);
}

/// One seeded plan, [`REPLAY`]'s: set it to a seed a failure names to run that case alone.
#[test]
fn replay_one_seed() {
    let (seed, plant) = REPLAY;
    if let Err(broke) = run(&plan_of(seed), plant) {
        panic!("seed {seed}, Plant::{plant:?}: {broke}");
    }
}

// --- The charge alone -------------------------------------------------------------------------

/// A grant counts until the task's bytes are in a reading, then only what is left of it.
#[test]
fn a_grant_counts_until_a_reading_holds_its_bytes() {
    let mut charge = Charge::<u32>::new();
    charge.read(100, 0, false);
    charge.started(1, 50);
    assert_eq!(charge.charged(50), 150);
    assert_eq!(charge.realized(1, 30), 0);
    // Not in a reading yet: still the whole grant.
    assert_eq!(charge.charged(50), 150);
    charge.read(130, 0, false);
    assert_eq!(charge.charged(50), 150);
    // Past its grant: raised at once.
    assert_eq!(charge.realized(1, 80), 30);
    charge.read(180, 0, false);
    assert_eq!(charge.charged(80), 180);
}

/// A task that ended stays charged for what it grew since the reading, until the next.
#[test]
fn an_ended_task_stays_charged_until_the_next_reading() {
    let mut charge = Charge::<u32>::new();
    charge.read(100, 0, false);
    charge.started(1, 50);
    charge.realized(1, 10);
    charge.read(110, 0, false);
    charge.ended(1, Some(20));
    assert_eq!(charge.charged(0), 120);
    charge.read(120, 20, false);
    assert_eq!(charge.charged(0), 120);
    assert_eq!(charge.grades(), Grades { under: 1, under_bytes: 30, ..Grades::default() });
}

/// Under grants alone the charge is `max(granted, held)`, as before.
#[test]
fn held_alone_charges_the_grants_or_the_live_bytes() {
    let mut charge = Charge::<u32>::new();
    charge.held(70);
    charge.started(1, 50);
    assert_eq!(charge.charged(50), 70);
    assert!(charge.alone_ok());
    assert!(!charge.ask_purge(0));
}

/// A purge is asked once until a reading after a purge, at most every [`PURGE_EVERY_NS`], and a
/// task starts alone only after that reading: a reading the driver took before it purged is not
/// one.
#[test]
fn a_purge_is_asked_once_a_purge_and_lets_a_task_start_alone() {
    let mut charge = Charge::<u32>::new();
    charge.read(100, 60, false);
    assert!(!charge.alone_ok());
    assert!(charge.ask_purge(0));
    assert!(!charge.ask_purge(1));
    charge.read(100, 60, false);
    assert!(!charge.alone_ok());
    assert!(!charge.ask_purge(PURGE_EVERY_NS));
    charge.read(80, 40, true);
    assert!(charge.alone_ok());
    assert!(!charge.ask_purge(PURGE_EVERY_NS - 1));
    assert!(charge.ask_purge(PURGE_EVERY_NS));
    assert_eq!(charge.grades().purges, 2);
}

/// The gate purges before it refuses, fails only a task alone, and names it.
#[test]
fn the_gate_purges_before_it_fails_a_task_alone() {
    let past = Past {
        ceiling: 100,
        footprint: 90,
        kept: 20,
        bytes: 30,
        asker: NodeId(7),
        asker_holds: 40,
        others_unrealized: 0,
        others_running: 0,
        others_waiting: 0,
        newest: NodeId(7),
        tried: false,
    };
    assert_eq!(past_ceiling(&past), Step::Purge);
    let tried = Past { footprint: 70, kept: 0, tried: true, ..past };
    assert_eq!(past_ceiling(&tried), Step::Take);
    let alone = Past { footprint: 80, kept: 0, tried: true, ..past };
    let Step::Fail(too_big) = past_ceiling(&alone) else { panic!("not failed alone") };
    assert_eq!(too_big, TooBig { id: NodeId(7), peak: 70, besides: 40, ceiling: 100 });
    assert!(too_big.to_string().contains("task #7"), "{too_big}");
    let others = Past { others_running: 2, others_waiting: 1, ..alone };
    assert_eq!(past_ceiling(&others), Step::Wait);
    let stuck = Past { others_running: 2, others_waiting: 2, newest: NodeId(9), ..alone };
    assert_eq!(past_ceiling(&stuck), Step::GiveWay(NodeId(9)));
}
