//! The Budget's simulation (`testing` skill: dst-not-review, invariants-are-stated), and the
//! incident memory.md section 9 names, replayed through the gate.
//!
//! A real [`Budget`] gates a simulated process of W workers running tasks whose peaks are drawn
//! over the measured range (64 MiB to 34.4 GiB, a few past the ceiling) against a 64 GiB
//! ceiling, in up to three bands. Each task runs a fixed program: reservations of up to one
//! overdraft, releases, and safe points spaced so that no more than one overdraft is reserved
//! between two of them (the driver's contract). Workers take queued tasks in rank order; each
//! step one running task, drawn from a seed, takes its next step. Another process lowers and
//! raises the room at drawn steps, and gives it all back at the end. A task told to give way
//! unwinds at its next safe point and is posted again with its age; one told it is too big
//! fails there.
//!
//! After every step the oracle checks:
//! - committed: what the tasks hold plus core and the ungated reserve equals the gate's count
//!   and is under the ceiling; after a grant, under the room too;
//! - wounds: every victim is outranked by its wounder, holds memory and was not marked before;
//! - giving way: a task gives way only while an unmarked task outranking it holds memory, or the
//!   room is below the ceiling;
//! - holds: a task wounded by an elder does not start again before that elder ends.
//!
//! At the end: every task that fits alone ended, with its peak; every other got [`TooBig`]; and
//! in a calm run (one band, the room never lowered) no task gave way more often than there are
//! tasks older than it. A run that stops moving, or passes its step bound, is a failure.
//!
//! Five plants ([`Plant`]) prove the oracle catches what it claims to, each in a control.
//! Replay: a control or [`replay_one_seed`] names a seed of [`plan_of`]; the Hegel test prints its
//! plan.

use std::fmt;

use blaze_rng::SplitMix64;
use hegel::{TestCase, generators as gs};

use super::*;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The ceiling the simulated process runs under.
const CEILING: u64 = 64 * GIB;

/// The smallest task peak measured, in quanta: a 64 MiB grant.
const SMALLEST: u64 = 64 * MIB / QUANTUM;

/// The largest task peak measured, in quanta: 34.4 GiB (one codegen unit).
const LARGEST: u64 = (34 * GIB + 410 * MIB) / QUANTUM;

const MOST_TASKS: u64 = 16;
const MOST_WORKERS: u64 = 16;

/// Seeds of [`plan_of`] a control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 128;

/// The seed [`replay_one_seed`] runs, and its plant: set them to what a failure names.
const REPLAY: (u64, Plant) = (0, Plant::None);

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

#[derive(Clone, Copy, Debug)]
struct TaskPlan {
    band: u8,
    /// Its peak, in quanta.
    size: u64,
    /// Its program's ([`program`]).
    seed: u64,
}

#[derive(Clone, Debug)]
struct Plan {
    workers: u32,
    /// Per worker, in quanta.
    overdraft: u64,
    core: u64,
    ungated: u64,
    tasks: Vec<TaskPlan>,
    /// At a step, the room becomes this many hundredths of the ceiling.
    rooms: Vec<(u64, u64)>,
    /// One band and the room never lowered: where the wound bound is exact.
    calm: bool,
    /// The schedule's: which running task steps next.
    seed: u64,
}

fn plan(d: &mut impl Draw) -> Plan {
    let calm = d.one_in(3);
    let workers = d.int(1, MOST_WORKERS) as u32;
    // 16 to 128 MiB a worker: a step of growth between safe points.
    let overdraft = d.int(8, 64);
    let core = d.int(1, 16) * 256 * MIB;
    let ungated = d.int(0, 24) * 256 * MIB;
    let bands = if calm { 1 } else { d.int(1, 3) };
    let count = d.int(1, MOST_TASKS);
    let ceiling_q = CEILING / QUANTUM;
    // alloc: a plan, once a case
    let tasks = (0..count)
        .map(|_| {
            let band = d.int(0, bands - 1) as u8;
            // Log-uniform over the measured range, and one in twelve around the ceiling: some
            // fit alone, some do not.
            let size = if d.one_in(12) {
                d.int(ceiling_q * 3 / 4, ceiling_q * 5 / 4)
            } else {
                let class = SMALLEST << d.int(0, 9);
                (class + d.int(0, class - 1)).min(LARGEST)
            };
            TaskPlan { band, size, seed: d.int(0, u64::MAX) }
        })
        .collect();
    let rooms = match calm {
        true => Vec::new(),
        // alloc: as above
        false => (0..d.int(0, 6)).map(|_| (d.int(0, 4000), d.int(35, 100))).collect(),
    };
    Plan { workers, overdraft, core, ungated, tasks, rooms, calm, seed: d.int(0, u64::MAX) }
}

fn plan_of(seed: u64) -> Plan {
    plan(&mut SplitMix64::new(seed))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Reserve(u64),
    Release(u64),
    SafePoint,
}

/// A task's steps: it grows to `size` quanta in reservations of up to one overdraft, gives some
/// back now and then, and reaches a safe point before it has reserved more than one overdraft
/// since the last.
fn program(size: u64, overdraft: u64, seed: u64) -> Vec<Op> {
    let mut rng = SplitMix64::new(seed);
    let mut ops = Vec::new(); // alloc: a task's program, once a case
    let (mut held, mut since) = (0, 0);
    while held < size {
        if held > 1 && rng.below(8) == 0 {
            let back = 1 + rng.below(held / 2);
            ops.push(Op::Release(back));
            held -= back;
        }
        let step = (1 + rng.below(overdraft)).min(size - held);
        if since + step > overdraft || rng.below(4) == 0 {
            ops.push(Op::SafePoint);
            since = 0;
        }
        ops.push(Op::Reserve(step));
        held += step;
        since += step;
    }
    ops.push(Op::SafePoint);
    ops
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Queued,
    /// Told [`Started::Held`]; queued again when an event names it unheld.
    Held,
    Running,
    /// At a safe point, told to wait.
    Waiting,
    Ended,
    TooBig,
}

#[derive(Clone, Copy, Debug)]
struct Sim {
    state: State,
    pc: usize,
    /// Quanta it holds, granted or overdrawn.
    held: u64,
    /// Told to give way or to fail, and not yet unwound.
    marked: bool,
    /// The elder that wounded it, until that elder ends.
    wounder: Option<usize>,
    gave_way: u64,
}

/// A rule the oracle saw broken (or a call the gate refused that the contract allows).
struct Broke(String);

impl fmt::Display for Broke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn misuse(misuse: Misuse) -> Broke {
    Broke(format!("the gate refused a call the driver's contract allows: {misuse}"))
}

struct World<'a> {
    plan: &'a Plan,
    programs: Vec<Vec<Op>>,
    /// Task indices in rank order.
    order: Vec<usize>,
    budget: Budget,
    tasks: Vec<Sim>,
    rng: SplitMix64,
    room: u64,
    step: u64,
}

impl<'a> World<'a> {
    fn new(plan: &'a Plan, plant: Plant) -> Result<World<'a>, Broke> {
        let config = Config {
            ceiling: CEILING,
            core: plan.core,
            overdraft_per_worker: plan.overdraft * QUANTUM,
            workers: plan.workers,
            ungated: plan.ungated,
        };
        let mut budget = Budget::new(config).map_err(|why| Broke(format!("harness: {why}")))?;
        budget.plant = plant;
        let programs = plan.tasks.iter().map(|it| program(it.size, plan.overdraft, it.seed)).collect();
        let mut order: Vec<usize> = (0..plan.tasks.len()).collect();
        order.sort_by_key(|&at| (plan.tasks[at].band, at));
        let sim = Sim { state: State::Queued, pc: 0, held: 0, marked: false, wounder: None, gave_way: 0 };
        Ok(World {
            plan,
            programs,
            order,
            budget,
            tasks: vec![sim; plan.tasks.len()],
            rng: SplitMix64::new(plan.seed),
            room: CEILING,
            step: 0,
        })
    }

    fn rank(&self, at: usize) -> (u8, usize) {
        (self.plan.tasks[at].band, at)
    }

    fn running(&self, at: usize) -> bool {
        matches!(self.tasks[at].state, State::Running | State::Waiting)
    }

    fn run(&mut self) -> Result<(), Broke> {
        let mut rooms = self.plan.rooms.clone();
        rooms.sort_unstable();
        if let Some(&(last, _)) = rooms.last() {
            // The other process ends: all of the room comes back.
            rooms.push((last + 1, 100));
        }
        let ops: u64 = self.programs.iter().map(|it| it.len() as u64).sum();
        let tasks = self.tasks.len() as u64;
        let bound = (ops + 1_000) * (tasks + 2) * (u64::from(self.plan.workers) + 2) + 200_000;
        let mut next_room = 0;
        loop {
            while let Some(&(at, percent)) = rooms.get(next_room)
                && at <= self.step
            {
                self.set_room(CEILING * percent / 100)?;
                next_room += 1;
            }
            self.fill()?;
            let running = (0..self.tasks.len()).filter(|&at| self.running(at)).count();
            if running == 0 {
                if self.tasks.iter().all(|it| matches!(it.state, State::Ended | State::TooBig)) {
                    break;
                }
                match rooms.get(next_room) {
                    Some(&(at, _)) => {
                        self.step = at;
                        continue;
                    }
                    None => {
                        return Err(Broke(format!(
                            "stuck at step {}: no task runs and {:?} are held",
                            self.step,
                            (0..self.tasks.len())
                                .filter(|&at| self.tasks[at].state == State::Held)
                                .collect::<Vec<_>>()
                        )));
                    }
                }
            }
            let pick = self.rng.below(running as u64) as usize;
            let Some(at) = (0..self.tasks.len()).filter(|&at| self.running(at)).nth(pick) else {
                return Err(Broke(format!("harness: no running task at {pick} of {running}")));
            };
            self.advance(at)?;
            self.check()?;
            self.step += 1;
            if self.step > bound {
                return Err(Broke(format!("no progress: past {bound} steps")));
            }
        }
        self.verdict()
    }

    /// Workers take queued tasks in rank order.
    fn fill(&mut self) -> Result<(), Broke> {
        let mut running = (0..self.tasks.len()).filter(|&at| self.running(at)).count();
        for index in 0..self.order.len() {
            if running >= self.plan.workers as usize {
                break;
            }
            let at = self.order[index];
            if self.tasks[at].state != State::Queued {
                continue;
            }
            let task = self.plan.tasks[at];
            let started = self
                .budget
                .start(TaskId(at as u64), Band(task.band), Age(at as u64))
                .map_err(misuse)?;
            match started {
                Started::Running => {
                    if let Some(elder) = self.tasks[at].wounder {
                        return Err(Broke(format!(
                            "task {at} started again while task {elder}, which wounded it, has not ended"
                        )));
                    }
                    self.tasks[at].state = State::Running;
                    running += 1;
                }
                Started::Held { .. } => self.tasks[at].state = State::Held,
            }
        }
        Ok(())
    }

    fn advance(&mut self, at: usize) -> Result<(), Broke> {
        if self.tasks[at].state == State::Waiting {
            return self.safe_point(at);
        }
        let Some(&op) = self.programs[at].get(self.tasks[at].pc) else {
            return self.finish(at, State::Ended);
        };
        self.tasks[at].pc += 1;
        match op {
            Op::Reserve(quanta) => self.reserve(at, quanta),
            Op::Release(quanta) => {
                let freed = self.budget.release(TaskId(at as u64), quanta).map_err(misuse)?;
                self.tasks[at].held -= quanta;
                self.unhold(&freed.unheld);
                Ok(())
            }
            Op::SafePoint => self.safe_point(at),
        }
    }

    fn reserve(&mut self, at: usize, quanta: u64) -> Result<(), Broke> {
        let answer = self.budget.reserve(TaskId(at as u64), quanta).map_err(misuse)?;
        self.tasks[at].held += quanta;
        match answer {
            Reserved::Granted => {
                let limit = CEILING.min(self.room);
                if self.committed() > limit {
                    return Err(Broke(format!(
                        "a grant of {quanta} quanta to task {at} took committed to {} B, past the room of {limit} B",
                        self.committed()
                    )));
                }
            }
            Reserved::Overdrawn { .. } => {
                if !self.tasks[at].marked {
                    self.gave_way(at)?;
                }
            }
            Reserved::Wound { victims, .. } => self.wounds(at, &victims)?,
            Reserved::TooBig(too_big) => {
                self.too_big(at, too_big)?;
                self.tasks[at].marked = true;
            }
        }
        Ok(())
    }

    fn safe_point(&mut self, at: usize) -> Result<(), Broke> {
        let id = TaskId(at as u64);
        match self.budget.safe_point(id).map_err(misuse)? {
            SafePoint::Go => self.tasks[at].state = State::Running,
            SafePoint::Wait { victims } => {
                self.tasks[at].state = State::Waiting;
                self.wounds(at, &victims)?;
            }
            SafePoint::GiveWay => {
                if !self.tasks[at].marked {
                    self.gave_way(at)?;
                }
                let unwound = self.budget.wounded_at_safe_point(id).map_err(misuse)?;
                let it = &mut self.tasks[at];
                (it.state, it.pc, it.held, it.marked) = (State::Queued, 0, 0, false);
                self.unhold(&unwound.unheld);
            }
            SafePoint::Fail(too_big) => {
                self.too_big(at, too_big)?;
                self.finish(at, State::TooBig)?;
            }
        }
        Ok(())
    }

    /// A task newly told to give way.
    fn gave_way(&mut self, at: usize) -> Result<(), Broke> {
        let outranked = (0..self.tasks.len()).any(|other| {
            self.running(other)
                && !self.tasks[other].marked
                && self.tasks[other].held > 0
                && self.rank(other) < self.rank(at)
        });
        if !outranked && self.room >= CEILING {
            return Err(Broke(format!(
                "task {at} gave way with no unmarked task outranking it holding memory and the room at the ceiling"
            )));
        }
        self.tasks[at].marked = true;
        self.tasks[at].gave_way += 1;
        Ok(())
    }

    fn wounds(&mut self, asker: usize, victims: &[TaskId]) -> Result<(), Broke> {
        for victim in victims {
            let at = victim.0 as usize;
            if self.rank(at) <= self.rank(asker) {
                return Err(Broke(format!("task {asker} wounded task {at}, which outranks it")));
            }
            if self.tasks[at].marked {
                return Err(Broke(format!("task {asker} wounded task {at}, already told to give way")));
            }
            if !self.running(at) || self.tasks[at].held == 0 {
                return Err(Broke(format!("task {asker} wounded task {at}, which holds nothing")));
            }
            let it = &mut self.tasks[at];
            it.marked = true;
            it.wounder = Some(asker);
            it.gave_way += 1;
        }
        Ok(())
    }

    fn too_big(&self, at: usize, too_big: TooBig) -> Result<(), Broke> {
        let size = self.plan.tasks[at].size;
        if too_big.task != TaskId(at as u64) || size <= self.budget.largest_task() {
            return Err(Broke(format!(
                "task {at} of {size} quanta, which fits alone in {}, was told {too_big}",
                self.budget.largest_task()
            )));
        }
        Ok(())
    }

    fn finish(&mut self, at: usize, state: State) -> Result<(), Broke> {
        let ended = self.budget.end(TaskId(at as u64)).map_err(misuse)?;
        let size = self.plan.tasks[at].size;
        if state == State::Ended && ended.peak != size {
            return Err(Broke(format!("task {at} peaked at {size} quanta and ended with a peak of {}", ended.peak)));
        }
        let it = &mut self.tasks[at];
        (it.state, it.held, it.marked) = (state, 0, false);
        for it in &mut self.tasks {
            if it.wounder == Some(at) {
                it.wounder = None;
            }
        }
        self.unhold(&ended.unheld);
        Ok(())
    }

    fn set_room(&mut self, bytes: u64) -> Result<(), Broke> {
        let raised = bytes >= self.room;
        self.room = bytes;
        let roomed = self.budget.room(bytes);
        if raised && !roomed.victims.is_empty() {
            return Err(Broke(format!("the room was raised to {bytes} B and wounded {:?}", roomed.victims)));
        }
        for victim in &roomed.victims {
            let at = victim.0 as usize;
            if self.tasks[at].marked || !self.running(at) {
                return Err(Broke(format!("the room wounded task {at}, already told to give way or not running")));
            }
            self.tasks[at].marked = true;
            self.tasks[at].gave_way += 1;
        }
        self.unhold(&roomed.unheld);
        Ok(())
    }

    fn unhold(&mut self, unheld: &[TaskId]) {
        for task in unheld {
            let it = &mut self.tasks[task.0 as usize];
            if it.state == State::Held {
                it.state = State::Queued;
            }
        }
    }

    fn committed(&self) -> u64 {
        self.plan.core + self.plan.ungated + self.tasks.iter().map(|it| it.held).sum::<u64>() * QUANTUM
    }

    fn check(&self) -> Result<(), Broke> {
        let committed = self.committed();
        if self.budget.committed() != committed {
            return Err(Broke(format!(
                "the gate counts {} B committed where core, the ungated reserve and the tasks hold {committed} B",
                self.budget.committed()
            )));
        }
        if committed > CEILING {
            return Err(Broke(format!("committed {committed} B, past the ceiling of {CEILING} B")));
        }
        Ok(())
    }

    fn verdict(&self) -> Result<(), Broke> {
        let largest = self.budget.largest_task();
        for (at, it) in self.tasks.iter().enumerate() {
            let size = self.plan.tasks[at].size;
            let fits = size <= largest;
            if (fits && it.state != State::Ended) || (!fits && it.state != State::TooBig) {
                return Err(Broke(format!(
                    "task {at} of {size} quanta (a task may hold {largest} alone) ended {:?}",
                    it.state
                )));
            }
            if self.plan.calm && it.gave_way > at as u64 {
                return Err(Broke(format!(
                    "task {at} gave way {} times in a calm run, with {at} tasks older than it",
                    it.gave_way
                )));
            }
        }
        Ok(())
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    World::new(plan, plant)?.run()
}

// --- The tests ------------------------------------------------------------------------------

/// The invariant: under any schedule of tasks over five orders of magnitude, bands, growth,
/// releases, ends and another process's room, committed (core, the ungated reserve, every
/// reserved quantum and every overdraft in use) never passes the ceiling, and no grant passes
/// the room; and every task that fits alone ends, wounded only by tasks that outrank it, never
/// started again under the elder that wounded it and, in a calm run, wounded at most once per
/// task older than it in its band, while every task that does not fit gets `TooBig` and nothing
/// else fails for memory.
#[hegel::test(test_cases = 100)]
fn committed_stays_under_the_ceiling_and_every_task_that_fits_alone_ends(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

/// The same over seeded plans, so the rule is run where Hegel's server is not.
#[test]
fn the_gate_holds_over_seeded_plans() {
    for seed in 0..CONTROL_SEEDS {
        if let Err(broke) = run(&plan_of(seed), Plant::None) {
            panic!("seed {seed} (REPLAY = ({seed}, Plant::None)): {broke}");
        }
    }
}

/// A plant the oracle must catch in one of [`CONTROL_SEEDS`] seeded plans (test-the-cause on
/// the test itself): a green simulation with a blind oracle proves nothing.
fn control(plant: Plant) {
    for seed in 0..CONTROL_SEEDS {
        if let Err(broke) = run(&plan_of(seed), plant) {
            eprintln!("{plant:?} caught at seed {seed} (REPLAY = ({seed}, Plant::{plant:?})): {broke}");
            return;
        }
    }
    panic!(
        "the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

#[test]
fn the_oracle_catches_a_commit_that_skips_the_gate() {
    control(Plant::SkipGate);
}

#[test]
fn the_oracle_catches_wounding_the_eldest() {
    control(Plant::WoundEldest);
}

#[test]
fn the_oracle_catches_a_wounded_task_readmitted_under_its_elder() {
    control(Plant::ReadmitUnderElder);
}

#[test]
fn the_oracle_catches_an_overdraft_not_counted_in_committed() {
    control(Plant::OverdraftUncounted);
}

#[test]
fn the_oracle_catches_a_wound_storm_that_never_lets_the_eldest_run() {
    control(Plant::WoundStorm);
}

/// One seeded plan, [`REPLAY`]'s: set it to a seed a failure names to run that case alone.
#[test]
fn replay_one_seed() {
    let (seed, plant) = REPLAY;
    if let Err(broke) = run(&plan_of(seed), plant) {
        panic!("seed {seed}, Plant::{plant:?}: {broke}");
    }
}

// --- The gate alone ---------------------------------------------------------------------------

#[test]
fn a_config_that_does_not_fit_is_refused_by_its_numbers() {
    let config = Config {
        ceiling: 8 * GIB,
        core: 4 * GIB,
        overdraft_per_worker: 128 * MIB,
        workers: 16,
        ungated: 2 * GIB,
    };
    let refused = Budget::new(config).err();
    assert_eq!(
        refused,
        Some(ConfigError::DoesNotFit { ceiling: 8 * GIB, core: 4 * GIB, overdrafts: 2 * GIB, ungated: 2 * GIB })
    );
    let odd = Budget::new(Config { overdraft_per_worker: QUANTUM + 1, ..config }).err();
    assert_eq!(odd, Some(ConfigError::OverdraftNotWhole { overdraft_per_worker: QUANTUM + 1 }));
}

const ELDEST: TaskId = TaskId(0);
const YOUNGER: TaskId = TaskId(1);
const YOUNGEST: TaskId = TaskId(2);

/// The process of 2026-11-12, 15:15:02 under the gate: a ceiling of 67.08 GiB, 55 GiB held by
/// three tasks (40, 8 and 7 GiB, eldest first), and the machine's room lowered to exactly what
/// they hold, so the next 0.09 GiB is short.
fn incident() -> Budget {
    let config = Config {
        ceiling: 67 * GIB + 82 * MIB,
        core: 2 * GIB,
        overdraft_per_worker: 128 * MIB,
        workers: 16,
        ungated: 6 * GIB,
    };
    let mut budget = Budget::new(config).expect("the incident's config fits");
    let step = 128 * MIB / QUANTUM;
    for (task, gib) in [(ELDEST, 40), (YOUNGER, 8), (YOUNGEST, 7)] {
        assert_eq!(budget.start(task, Band(0), Age(task.0)), Ok(Started::Running));
        for _ in 0..gib * GIB / QUANTUM / step {
            assert_eq!(budget.reserve(task, step), Ok(Reserved::Granted));
            assert_eq!(budget.safe_point(task), Ok(SafePoint::Go));
        }
    }
    let roomed = budget.room(2 * GIB + 6 * GIB + 2 * GIB + 55 * GIB);
    assert_eq!(roomed, Roomed { victims: Vec::new(), unheld: Vec::new() });
    budget
}

/// 0.09 GiB, in quanta.
const SHORT: u64 = 92 * MIB / QUANTUM;

/// The invariant: when the eldest is 0.09 GiB short, the youngest holder alone is wounded; the
/// eldest's allocation goes on from its overdraft, is paid as the youngest unwinds, and the
/// middle task is not touched. Nothing is dropped; the ceiling holds throughout.
#[test]
fn the_incident_s_eldest_wounds_only_the_youngest_holder() {
    let mut budget = incident();
    assert_eq!(
        budget.reserve(ELDEST, SHORT),
        Ok(Reserved::Wound { victims: vec![YOUNGEST], left: 64 - SHORT })
    );
    assert!(budget.committed() <= budget.ceiling());
    assert_eq!(budget.safe_point(YOUNGER), Ok(SafePoint::Go));
    assert_eq!(budget.safe_point(ELDEST), Ok(SafePoint::Wait { victims: Vec::new() }));
    assert_eq!(budget.safe_point(YOUNGEST), Ok(SafePoint::GiveWay));
    assert_eq!(budget.wounded_at_safe_point(YOUNGEST), Ok(Unwound { unheld: Vec::new() }));
    assert_eq!(budget.safe_point(ELDEST), Ok(SafePoint::Go));
    assert_eq!(budget.start(YOUNGEST, Band(0), Age(YOUNGEST.0)), Ok(Started::Held { by: Hold::Elder(ELDEST) }));
    let ended = budget.end(ELDEST).expect("the eldest ends");
    assert_eq!(ended.unheld, vec![YOUNGEST]);
    assert_eq!(budget.start(YOUNGEST, Band(0), Age(YOUNGEST.0)), Ok(Started::Running));
}

/// The invariant: a young task that is 0.09 GiB short wounds nobody: it gives way at its safe
/// point and waits for the youngest elder holding memory, while all three holders go on.
#[test]
fn the_incident_s_young_asker_waits_and_nothing_is_dropped() {
    let mut budget = incident();
    let young = TaskId(3);
    assert_eq!(budget.start(young, Band(0), Age(3)), Ok(Started::Running));
    assert_eq!(budget.reserve(young, SHORT), Ok(Reserved::Overdrawn { left: 64 - SHORT }));
    assert!(budget.committed() <= budget.ceiling());
    for holder in [ELDEST, YOUNGER, YOUNGEST] {
        assert_eq!(budget.safe_point(holder), Ok(SafePoint::Go));
    }
    assert_eq!(budget.safe_point(young), Ok(SafePoint::GiveWay));
    assert_eq!(budget.wounded_at_safe_point(young), Ok(Unwound { unheld: Vec::new() }));
    assert_eq!(budget.start(young, Band(0), Age(3)), Ok(Started::Held { by: Hold::Elder(YOUNGEST) }));
    assert_eq!(budget.end(YOUNGEST).map(|it| it.unheld), Ok(vec![young]));
    assert_eq!(budget.start(young, Band(0), Age(3)), Ok(Started::Running));
    assert_eq!(budget.reserve(young, SHORT), Ok(Reserved::Granted));
}

// --- The room the machine's ledger is fitted to -------------------------------------------------

/// The build VM's gate of 2026-11-13 (`gate::config_under` on 128 cores: 256 workers, core 3/8
/// and the ungated reserve 1/8 of a 64 GiB ceiling, a 32 MiB overdraft each), with one task, the
/// hello-world's lowering, holding 64 MiB.
fn lone_lowering() -> Budget {
    let config = Config {
        ceiling: 64 * GIB,
        core: 24 * GIB,
        overdraft_per_worker: 32 * MIB,
        workers: 256,
        ungated: 8 * GIB,
    };
    let mut budget = Budget::new(config).expect("the build VM's config fits");
    assert_eq!(budget.start(ELDEST, Band(0), Age(0)), Ok(Started::Running));
    for _ in 0..4 {
        assert_eq!(budget.reserve(ELDEST, 32 * MIB / QUANTUM), Ok(Reserved::Granted));
        assert_eq!(budget.safe_point(ELDEST), Ok(SafePoint::Go));
    }
    budget
}

/// One worker's overdraft on [`lone_lowering`]'s gate, in quanta: the most one reserve asks.
const STEP: u64 = 32 * MIB / QUANTUM;

/// The room the daemon gives the gate for `needs` bytes: the machine's ledger moves its slot in
/// 64 MiB grains, rounded up (`serve::machine::Machine::fit`), and `gate::Gate::needs` adds what
/// its front may be lent; one step stands in for that here.
fn fitted(needs: u64) -> u64 {
    (needs + STEP * QUANTUM).div_ceil(64 * MIB) * (64 * MIB)
}

/// The invariant: a room fitted to [`Budget::needs`] before each step of a lone task marks
/// nobody and grants every step: the task grows from 128 MiB to 1.1 GiB and never gives way.
#[test]
fn a_room_fitted_to_what_the_gate_needs_wounds_nobody() {
    let mut budget = lone_lowering();
    for step in 0..32 {
        let roomed = budget.room(fitted(budget.needs()));
        assert_eq!(roomed, Roomed { victims: Vec::new(), unheld: Vec::new() }, "step {step}");
        assert_eq!(budget.reserve(ELDEST, STEP), Ok(Reserved::Granted), "step {step}");
        assert_eq!(budget.safe_point(ELDEST), Ok(SafePoint::Go), "step {step}");
    }
}

/// The control, the cause put back (test-the-cause): the same fit of the room to
/// [`Budget::committed`], which leaves out the 8 GiB of overdrafts the room must also cover,
/// wounds the lone task at the first fit, whatever it holds: the cause of 2026-11-13's 1,280
/// starts of one lowering.
#[test]
fn a_room_fitted_to_what_the_gate_committed_wounds_the_lone_task() {
    let mut budget = lone_lowering();
    let roomed = budget.room(fitted(budget.committed()));
    assert_eq!(roomed.victims, vec![ELDEST]);
    assert_eq!(budget.safe_point(ELDEST), Ok(SafePoint::GiveWay));
    assert!(budget.wounded_at_safe_point(ELDEST).is_ok());
    // Held for the room it had; fitted to what is committed (none of it), the room never lets
    // it go, while one fitted to what is needed does at once.
    assert!(budget.room(fitted(budget.committed())).unheld.is_empty());
    assert_eq!(budget.room(fitted(budget.needs())).unheld, vec![ELDEST]);
}

/// The invariant: a task that gave way to the room (it grew past the room between two fits)
/// and started again finds the room kept for all it held before, so growing back to it never
/// gives way at the same point again.
#[test]
fn a_task_started_again_finds_room_for_where_it_gave_way() {
    let mut budget = lone_lowering();
    budget.room(fitted(budget.needs()));
    // Past the fitted room with no fit between: the lone task gives way to the room.
    let mut gave_way = false;
    for _ in 0..4 {
        match budget.reserve(ELDEST, STEP) {
            Ok(Reserved::Granted) => assert_eq!(budget.safe_point(ELDEST), Ok(SafePoint::Go)),
            Ok(Reserved::Overdrawn { .. }) => {
                gave_way = budget.safe_point(ELDEST) == Ok(SafePoint::GiveWay);
                break;
            }
            other => panic!("a lone task's step: {other:?}"),
        }
    }
    assert!(gave_way, "the room was not short");
    let reach = budget.held_by(ELDEST).expect("it runs until it unwinds");
    assert!(budget.wounded_at_safe_point(ELDEST).is_ok());
    assert_eq!(budget.room(fitted(budget.needs())).unheld, vec![ELDEST]);
    assert_eq!(budget.start(ELDEST, Band(0), Age(0)), Ok(Started::Running));
    for step in 0..reach.div_ceil(STEP) {
        let roomed = budget.room(fitted(budget.needs()));
        assert!(roomed.victims.is_empty(), "step {step} back to {reach} quanta");
        assert_eq!(budget.reserve(ELDEST, STEP), Ok(Reserved::Granted), "step {step}");
        assert_eq!(budget.safe_point(ELDEST), Ok(SafePoint::Go), "step {step}");
    }
}
