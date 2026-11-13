//! The plane under a real gate (`testing` skill: dst-not-review, invariants-are-stated): a
//! simulated driver runs the plane's tasks against a real [`Budget`], as the daemon's driver
//! does, and an oracle checks both after every step.
//!
//! The driver's protocol, one task per node, named at the gate by its age
//! (`TaskId(age)`, [`Plane::rank`]):
//! - a [`PlaneOutput::Run`] is a [`Budget::start`] at the task's rank: `Running` runs its
//!   program from the top; `Held` is told back as [`PlaneInput::Yielded`] with `held`;
//! - a task's program reserves, releases and reaches safe points ([`Op`]); a reservation's
//!   victims are told as [`PlaneInput::Wound`], and a safe point that says give way unwinds
//!   the task ([`Budget::wounded_at_safe_point`]) and tells [`PlaneInput::Yielded`] without
//!   `held` (the gate holds it at its next start if what it gave way to still runs);
//! - its end is [`Budget::end`] and [`PlaneInput::Done`] with the peak the gate measured;
//! - every task the gate lets go of is told as [`PlaneInput::Unheld`]; a
//!   [`PlaneOutput::Dropped`] is a [`Budget::end`];
//! - after every event of the gate, [`PlaneInput::Room`] of
//!   [`crate::admission::free_for_starts`].
//!
//! The oracle: what the tasks hold, with core and the ungated reserve, equals what the gate
//! counts committed and is never past the ceiling; the plane never runs a task the gate holds;
//! every task ends and nothing is dropped. Across processes ([`two_processes_never_pass_the_machine`]):
//! the processes' charges on one ledger never sum past the machine's budget.
//!
//! The controls: a driver whose tasks commit without [`Budget::reserve`] (`Plant::SkipReserve`),
//! and a plane that starts a task the gate holds (`Plane::ignore_held`). The control of a pool
//! that runs more threads than its process holds core tokens for lives with the pool, in
//! `pool.rs`'s tests: the plane takes no tokens.

use std::alloc::Global;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::AtomicU64;

use blaze_hash::{HashMap, HashSet};
use blaze_ids::rng::SplitMix64;
use blaze_mem::budget::{
    Budget, Config as GateConfig, Misuse, QUANTUM, Reserved, SafePoint, Started, TaskId,
};

use super::{GIB, MIB, config, node, open};
use crate::{
    admission::free_for_starts,
    ledger::{self, Alive, HEADER, Join, Machine, Process, Release, Reserve, Slot, Unseen, WORDS},
    machine::{NodeId, Outcome, Pool, Spec},
    plane::{Plane, PlaneInput, PlaneOutput, RequestId},
};

/// What the oracle saw broken, or a call the gate refused that the driver's contract allows.
type Checked<T> = Result<T, String>;

/// The smallest task peak measured, in quanta: a 64 MiB grant.
const SMALLEST: u64 = 64 * MIB / QUANTUM;

/// The largest task peak measured, in quanta: 34.4 GiB (one codegen unit of tempo).
const LARGEST: u64 = (34 * GIB + 410 * MIB) / QUANTUM;

/// A step of a task's program, as in `blaze_mem::budget`'s simulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Reserve(u64),
    Release(u64),
    SafePoint,
}

/// A task's steps: it grows to `size` quanta in reservations of up to one overdraft, gives
/// some back now and then, and reaches a safe point before it has reserved more than one
/// overdraft since the last (the driver's contract). Its peak is `size`.
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

/// A planted bug the oracle must catch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plant {
    None,
    /// The driver's tasks commit what they reserve without asking the gate.
    SkipReserve,
    /// The plane starts a task the gate holds as if it were not held (`Plane::ignore_held`).
    IgnoreHeld,
}

/// A task of the simulated driver, by the node it runs as.
#[derive(Default)]
struct Task {
    program: Vec<Op>,
    pc: usize,
    /// Quanta it holds, granted or overdrawn.
    held: u64,
    /// Started at the gate and not given way or ended.
    running: bool,
    ended: bool,
    /// Steps of work in this run: lost if it gives way.
    this_run: u64,
    /// Steps of work lost to giving way, over its runs.
    wasted: u64,
}

/// The work done and the work wounds threw away, in steps of the tasks' programs.
#[derive(Clone, Copy, Debug, Default)]
struct Spent {
    cpu: u64,
    wounded: u64,
}

/// One process: its plane, its gate and its tasks.
struct Sim {
    plane: Plane,
    gate: Budget,
    /// By node id.
    tasks: Vec<Task>,
    /// The node each age at the gate runs as.
    node_of: HashMap<u64, u32, Global>, // alloc: a test's driver
    /// Nodes the gate answered `Held` and has not let go of.
    gate_held: HashSet<u32, Global>, // alloc: a test's driver
    plant: Plant,
    yields: u64,
    dropped: Vec<u64>,
    finished: Option<bool>,
    cpu: u64,
}

fn misuse(misuse: Misuse) -> String {
    format!("the gate refused a call the driver's contract allows: {misuse:?}")
}

impl Sim {
    /// A process of `work` threads gated by `gate` (its room `room` if given), running one
    /// request of `specs`, the task of node `id` running `programs[id]`.
    fn new(
        gate: GateConfig,
        room: Option<u64>,
        work: usize,
        specs: Vec<Spec>,
        programs: Vec<Vec<Op>>,
        plant: Plant,
    ) -> Checked<Sim> {
        let mut plane = Plane::new(config(1, work)).map_err(|it| format!("{it:?}"))?;
        plane.ignore_held = plant == Plant::IgnoreHeld;
        let mut gate = Budget::new(gate).map_err(|it| it.to_string())?;
        if let Some(room) = room {
            gate.room(room);
        }
        let tasks = programs.into_iter().map(|program| Task { program, ..Task::default() });
        let mut sim = Sim {
            plane,
            gate,
            tasks: tasks.collect(),
            node_of: HashMap::default(),
            gate_held: HashSet::default(),
            plant,
            yields: 0,
            dropped: Vec::new(),
            finished: None,
            cpu: 0,
        };
        open(&mut sim.plane, 1, 1, true);
        let out = sim.tell_room()?;
        sim.pump(out)?;
        let mut out =
            sim.step(PlaneInput::Add { at_ns: 0, request: RequestId(1), nodes: specs })?;
        out.extend(sim.step(PlaneInput::Close { at_ns: 0, request: RequestId(1) })?);
        sim.pump(out)?;
        Ok(sim)
    }

    fn step(&mut self, input: PlaneInput) -> Checked<Vec<PlaneOutput>> {
        self.plane.step(input).map_err(|it| format!("the plane refused an input: {it:?}"))
    }

    /// The gate's name for the task node `id` runs as.
    fn task_id(&self, id: u32) -> Checked<TaskId> {
        let (_, age) = self.plane.rank(NodeId(id)).ok_or_else(|| format!("node {id} has no rank"))?;
        Ok(TaskId(age.0))
    }

    /// [`PlaneInput::Room`] of what the gate leaves free for starts now.
    fn tell_room(&mut self) -> Checked<Vec<PlaneOutput>> {
        let mut still = 0;
        for (id, task) in self.tasks.iter().enumerate() {
            if task.running {
                let forecast = self.plane.memory_of(NodeId(id as u32)).unwrap_or(0);
                still += forecast.saturating_sub(task.held * QUANTUM);
            }
        }
        let free = free_for_starts(&self.gate, still);
        self.step(PlaneInput::Room { at_ns: 0, free })
    }

    /// Does what the plane asked, and what that asks in turn.
    fn pump(&mut self, outputs: Vec<PlaneOutput>) -> Checked<()> {
        let mut queue: VecDeque<PlaneOutput> = outputs.into();
        while let Some(output) = queue.pop_front() {
            match output {
                PlaneOutput::Run { id, .. } => {
                    if self.gate_held.contains(&id.0) {
                        return Err(format!("node {} ran while the gate held it", id.0));
                    }
                    let (band, age) =
                        self.plane.rank(id).ok_or_else(|| format!("node {} ran unranked", id.0))?;
                    self.node_of.insert(age.0, id.0);
                    let more = match self.gate.start(TaskId(age.0), band, age).map_err(misuse)? {
                        Started::Running => {
                            let task = &mut self.tasks[id.0 as usize];
                            if task.running || task.ended {
                                return Err(format!("node {} ran twice at once", id.0));
                            }
                            task.running = true;
                            task.pc = 0;
                            task.held = 0;
                            task.this_run = 0;
                            Vec::new()
                        }
                        Started::Held { .. } => {
                            self.gate_held.insert(id.0);
                            self.step(PlaneInput::Yielded { at_ns: 0, id, held: true })?
                        }
                    };
                    queue.extend(more);
                    queue.extend(self.tell_room()?);
                }
                PlaneOutput::Yield { .. } => self.yields += 1,
                PlaneOutput::Dropped { age } => {
                    self.dropped.push(age);
                    let ended = self.gate.end(TaskId(age)).map_err(misuse)?;
                    queue.extend(self.unhold(&ended.unheld)?);
                    queue.extend(self.tell_room()?);
                }
                PlaneOutput::Finished { ok, .. } => self.finished = Some(ok),
                PlaneOutput::Said { .. } => {}
            }
        }
        self.check()
    }

    fn unhold(&mut self, unheld: &[TaskId]) -> Checked<Vec<PlaneOutput>> {
        let mut out = Vec::new();
        for task in unheld {
            let Some(&id) = self.node_of.get(&task.0) else {
                return Err(format!("the gate let go of {task:?}, which never ran"));
            };
            self.gate_held.remove(&id);
            out.extend(self.step(PlaneInput::Unheld { at_ns: 0, id: NodeId(id) })?);
        }
        Ok(out)
    }

    fn wound(&mut self, victims: &[TaskId]) -> Checked<Vec<PlaneOutput>> {
        let mut out = Vec::new();
        for task in victims {
            let Some(&id) = self.node_of.get(&task.0) else {
                return Err(format!("the gate wounded {task:?}, which never ran"));
            };
            out.extend(self.step(PlaneInput::Wound { at_ns: 0, id: NodeId(id) })?);
        }
        Ok(out)
    }

    /// After an event of the gate: the room, then what the plane asks.
    fn after(&mut self, mut out: Vec<PlaneOutput>) -> Checked<()> {
        out.extend(self.tell_room()?);
        self.pump(out)
    }

    /// The machine's word on this process's room.
    fn set_room(&mut self, bytes: u64) -> Checked<()> {
        let roomed = self.gate.room(bytes);
        let mut out = self.wound(&roomed.victims)?;
        out.extend(self.unhold(&roomed.unheld)?);
        self.after(out)
    }

    fn reserve(&mut self, id: u32, quanta: u64) -> Checked<Reserved> {
        let task = self.task_id(id)?;
        if self.plant == Plant::SkipReserve {
            self.tasks[id as usize].held += quanta;
            self.check()?;
            return Ok(Reserved::Granted);
        }
        let reserved = self.gate.reserve(task, quanta).map_err(misuse)?;
        let out = match &reserved {
            Reserved::Granted | Reserved::Overdrawn { .. } => Vec::new(),
            Reserved::Wound { victims, .. } => self.wound(victims)?,
            Reserved::TooBig(too_big) => {
                return Err(format!("{too_big:?}: a task that fits alone was refused"));
            }
        };
        self.tasks[id as usize].held += quanta;
        self.after(out)?;
        Ok(reserved)
    }

    fn release(&mut self, id: u32, quanta: u64) -> Checked<()> {
        let task = self.task_id(id)?;
        let freed = self.gate.release(task, quanta).map_err(misuse)?;
        self.tasks[id as usize].held -= quanta;
        let out = self.unhold(&freed.unheld)?;
        self.after(out)
    }

    fn safe_point(&mut self, id: u32) -> Checked<SafePoint> {
        let task = self.task_id(id)?;
        let answer = self.gate.safe_point(task).map_err(misuse)?;
        let out = match &answer {
            SafePoint::Go => Vec::new(),
            SafePoint::Wait { victims } => self.wound(victims)?,
            SafePoint::GiveWay => self.give_way(id, task)?,
            SafePoint::Fail(too_big) => {
                return Err(format!("{too_big:?}: a task that fits alone was failed"));
            }
        };
        self.after(out)?;
        Ok(answer)
    }

    /// The task unwinds: everything it held goes back, its run's work is lost, and the plane
    /// is told it gave way (not held: the gate says so at its next start if it still holds).
    fn give_way(&mut self, id: u32, task: TaskId) -> Checked<Vec<PlaneOutput>> {
        let unwound = self.gate.wounded_at_safe_point(task).map_err(misuse)?;
        let it = &mut self.tasks[id as usize];
        it.running = false;
        it.held = 0;
        it.wasted += it.this_run;
        it.this_run = 0;
        it.pc = 0;
        let mut out = self.step(PlaneInput::Yielded { at_ns: 0, id: NodeId(id), held: false })?;
        out.extend(self.unhold(&unwound.unheld)?);
        Ok(out)
    }

    fn end(&mut self, id: u32) -> Checked<()> {
        let task = self.task_id(id)?;
        let ended = self.gate.end(task).map_err(misuse)?;
        let it = &mut self.tasks[id as usize];
        it.running = false;
        it.ended = true;
        it.held = 0;
        let done = PlaneInput::Done {
            at_ns: 0,
            id: NodeId(id),
            outcome: Outcome::Built,
            added: Vec::new(),
            group: None,
            peak: ended.peak * QUANTUM,
        };
        let mut out = self.step(done)?;
        out.extend(self.unhold(&ended.unheld)?);
        self.after(out)
    }

    /// One step of the running task of node `id`'s program.
    fn step_task(&mut self, id: u32) -> Checked<()> {
        let task = &self.tasks[id as usize];
        match task.program.get(task.pc).copied() {
            None => self.end(id),
            Some(Op::Reserve(quanta)) => {
                self.reserve(id, quanta)?;
                self.worked(id);
                Ok(())
            }
            Some(Op::Release(quanta)) => {
                self.release(id, quanta)?;
                self.worked(id);
                Ok(())
            }
            Some(Op::SafePoint) => {
                if self.safe_point(id)? == SafePoint::Go {
                    self.tasks[id as usize].pc += 1;
                }
                Ok(())
            }
        }
    }

    fn worked(&mut self, id: u32) {
        let task = &mut self.tasks[id as usize];
        task.pc += 1;
        task.this_run += 1;
        self.cpu += 1;
    }

    fn running(&self) -> Vec<u32> {
        let ids = self.tasks.iter().enumerate().filter(|(_, it)| it.running);
        ids.map(|(id, _)| id as u32).collect()
    }

    /// The oracle after every step: the tasks' memory as the gate counts it, under the ceiling.
    fn check(&self) -> Checked<()> {
        let config = self.gate.config();
        let held: u64 = self.tasks.iter().map(|it| it.held).sum();
        let truth = config.core + config.ungated + held * QUANTUM;
        if truth != self.gate.committed() {
            return Err(format!(
                "the tasks hold {truth} B with core and the ungated reserve; the gate counts {} B",
                self.gate.committed()
            ));
        }
        if truth > self.gate.ceiling() {
            return Err(format!("{truth} B committed, past the ceiling of {} B", self.gate.ceiling()));
        }
        Ok(())
    }

    /// The oracle at the end: every task ended, nothing dropped, the gate empty.
    fn verdict(&self) -> Checked<Spent> {
        if self.finished != Some(true) {
            return Err(format!("the request finished {:?}", self.finished));
        }
        if let Some((id, _)) = self.tasks.iter().enumerate().skip(1).find(|(_, it)| !it.ended) {
            return Err(format!("node {id} never ended"));
        }
        if !self.dropped.is_empty() {
            return Err(format!("tasks of ages {:?} were dropped", self.dropped));
        }
        if self.gate.running() != 0 || self.gate.any_parked() {
            return Err("the gate still has tasks after the request finished".to_owned());
        }
        let wounded = self.tasks.iter().map(|it| it.wasted).sum();
        Ok(Spent { cpu: self.cpu, wounded })
    }

    /// Steps a running task drawn from `random` until the request finishes, within `bound`.
    fn run(&mut self, random: &mut SplitMix64, bound: u64) -> Checked<Spent> {
        for _ in 0..bound {
            if self.finished.is_some() {
                return self.verdict();
            }
            let running = self.running();
            if running.is_empty() {
                return Err("nothing runs and the request has not finished".to_owned());
            }
            self.step_task(running[random.index(running.len())])?;
        }
        Err(format!("the run passed its bound of {bound} steps"))
    }
}

// --- (a) The incident -------------------------------------------------------------------------

const ELDEST: u32 = 1;
const YOUNGER: u32 = 2;
const YOUNGEST: u32 = 3;
const YOUNG: u32 = 4;

/// 0.09 GiB, in quanta.
const SHORT: u64 = 92 * MIB / QUANTUM;

/// The process of 2026-11-12, 15:15:02 through the plane and the gate: a ceiling of 67.08 GiB,
/// 55 GiB held by three tasks (40, 8 and 7 GiB, eldest first) and a fourth holding nothing,
/// and the machine's room lowered to exactly what they hold, so the next 0.09 GiB is short.
/// The tasks are forecast 0 (unknown), so the plane starts each whenever a thread is free.
fn incident(plant: Plant) -> Checked<Sim> {
    let gate = GateConfig {
        ceiling: 67 * GIB + 82 * MIB,
        core: 2 * GIB,
        overdraft_per_worker: 128 * MIB,
        workers: 16,
        ungated: 6 * GIB,
    };
    let specs = (1..=4).map(|id| node(id, &[], Pool::Work, 1, None, 0)).collect();
    let mut sim = Sim::new(gate, None, 4, specs, vec![Vec::new(); 5], plant)?;
    assert_eq!(sim.running(), [ELDEST, YOUNGER, YOUNGEST, YOUNG]);
    let step = 128 * MIB / QUANTUM;
    for (id, gib) in [(ELDEST, 40), (YOUNGER, 8), (YOUNGEST, 7)] {
        for _ in 0..gib * GIB / QUANTUM / step {
            assert_eq!(sim.reserve(id, step)?, Reserved::Granted);
            assert_eq!(sim.safe_point(id)?, SafePoint::Go);
        }
    }
    sim.set_room(2 * GIB + 6 * GIB + 2 * GIB + 55 * GIB)?;
    assert_eq!(sim.running(), [ELDEST, YOUNGER, YOUNGEST, YOUNG]);
    Ok(sim)
}

fn end_all(sim: &mut Sim) -> Checked<Spent> {
    for id in [ELDEST, YOUNGER, YOUNGEST, YOUNG] {
        if sim.tasks[id as usize].running {
            sim.end(id)?;
        }
    }
    sim.verdict()
}

/// The eldest asks for 0.09 GiB in the incident: the gate wounds the youngest holder alone; the
/// plane asks it to give way once; it gives way at its safe point, is started again by the
/// plane and held by the gate under the eldest, and runs again when the eldest ends.
fn the_eldest_asks(plant: Plant) -> Checked<()> {
    let mut sim = incident(plant)?;
    let youngest = sim.task_id(YOUNGEST)?;
    let left = 64 - SHORT;
    assert_eq!(sim.reserve(ELDEST, SHORT)?, Reserved::Wound { victims: vec![youngest], left });
    assert_eq!(sim.yields, 1, "the plane asks the youngest to give way, once");
    assert_eq!(sim.safe_point(YOUNGER)?, SafePoint::Go);
    assert_eq!(sim.safe_point(ELDEST)?, SafePoint::Wait { victims: Vec::new() });
    assert_eq!(sim.safe_point(YOUNGEST)?, SafePoint::GiveWay);
    assert!(sim.plane.is_held(NodeId(YOUNGEST)), "started again, held under the eldest");
    assert!(!sim.tasks[YOUNGEST as usize].running);
    assert_eq!(sim.safe_point(ELDEST)?, SafePoint::Go);
    sim.end(ELDEST)?;
    assert!(sim.tasks[YOUNGEST as usize].running, "the eldest's end lets it run again");
    end_all(&mut sim)?;
    Ok(())
}

/// The invariant: when the eldest is 0.09 GiB short, the youngest holder alone gives way, and
/// only once; it does not run again while the eldest runs; every task ends, nothing is
/// dropped, and committed stays under the ceiling throughout.
#[test]
fn the_incident_s_eldest_wounds_only_the_youngest_and_nothing_is_dropped() {
    the_eldest_asks(Plant::None).unwrap();
}

/// The invariant: a young task 0.09 GiB short in the incident wounds nobody: it gives way, is
/// held under the youngest elder holding memory while all three holders go on, and runs and
/// is granted once that elder ends; nothing is dropped.
#[test]
fn the_incident_s_young_asker_waits_and_nothing_is_dropped() {
    let mut sim = incident(Plant::None).unwrap();
    assert_eq!(sim.reserve(YOUNG, SHORT), Ok(Reserved::Overdrawn { left: 64 - SHORT }));
    assert_eq!(sim.yields, 0, "nobody is wounded");
    for holder in [ELDEST, YOUNGER, YOUNGEST] {
        assert_eq!(sim.safe_point(holder), Ok(SafePoint::Go));
    }
    assert_eq!(sim.safe_point(YOUNG), Ok(SafePoint::GiveWay));
    assert!(sim.plane.is_held(NodeId(YOUNG)), "held under the youngest elder");
    sim.end(YOUNGEST).unwrap();
    assert!(sim.tasks[YOUNG as usize].running);
    assert_eq!(sim.reserve(YOUNG, SHORT), Ok(Reserved::Granted));
    end_all(&mut sim).unwrap();
}

/// The control of the hold: a plane that starts a task the gate holds is caught.
#[test]
fn the_oracle_catches_a_wounded_task_readmitted_under_its_elder() {
    let Err(broke) = the_eldest_asks(Plant::IgnoreHeld) else {
        panic!("the oracle missed a task the gate holds started again");
    };
    assert!(broke.contains("while the gate held it"), "{broke}");
}

// --- (b) Tempo-like peaks ---------------------------------------------------------------------

/// What the plane forecasts each task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Forecasts {
    /// Each task's own peak, as [`crate::admission::Peaks`] says after a run of the same
    /// graph.
    Warm,
    /// One quantum, as `Peaks` says of a kind it never saw: forecast 0 at admission.
    Cold,
}

/// A seeded process: 2 to 8 workers under a 64 GiB ceiling, 12 to 31 tasks with peaks drawn
/// over the measured range (64 MiB to 34.4 GiB, one in six 8 GiB or more), some depending on
/// an earlier one.
fn tempo(seed: u64, forecasts: Forecasts, plant: Plant) -> Checked<Spent> {
    let mut random = SplitMix64::new(seed);
    let workers = 2 + random.below(7);
    let gate = GateConfig {
        ceiling: 64 * GIB,
        core: 2 * GIB,
        overdraft_per_worker: 128 * MIB,
        workers: workers as u32,
        ungated: 2 * GIB,
    };
    let overdraft = gate.overdraft_per_worker / QUANTUM;
    let count = 12 + random.below(20) as u32;
    let mut specs = Vec::new();
    let mut programs = vec![Vec::new()];
    for id in 1..=count {
        let peak = match random.one_in(6) {
            true => random.range(8 * GIB / QUANTUM..LARGEST + 1),
            false => random.range(SMALLEST..GIB / QUANTUM),
        };
        let deps: Vec<u32> = match id > 1 && random.one_in(3) {
            true => vec![1 + random.below(u64::from(id) - 1) as u32],
            false => Vec::new(),
        };
        let memory = match forecasts {
            Forecasts::Warm => peak * QUANTUM,
            Forecasts::Cold => QUANTUM,
        };
        specs.push(node(id, &deps, Pool::Work, 1, None, memory));
        programs.push(program(peak, overdraft, seed ^ (u64::from(id) << 32)));
    }
    let bound = programs.iter().map(|it| it.len() as u64).sum::<u64>() * 64 + 100_000;
    let mut sim = Sim::new(gate, None, workers as usize, specs, programs, plant)?;
    sim.run(&mut random, bound)
}

const SEEDS: u64 = 48;

/// The invariant: with each task forecast its own peak, committed stays under the ceiling at
/// every step, every task (each fits alone) ends, nothing is dropped, and the work wounds
/// throw away is under 1% of the work done. The 1% is a bound for exact forecasts only:
/// [`cold_forecasts_stay_safe_and_live`] holds no bound on it.
#[test]
fn tempo_s_peaks_stay_under_the_ceiling_and_wound_under_one_percent() {
    for seed in 0..SEEDS {
        let spent = tempo(seed, Forecasts::Warm, Plant::None)
            .unwrap_or_else(|broke| panic!("seed {seed}: {broke}"));
        assert!(
            spent.wounded * 100 < spent.cpu,
            "seed {seed}: wounds threw away {} of {} steps of work",
            spent.wounded,
            spent.cpu
        );
    }
}

/// The invariant: with every task forecast 0 (a cold `Peaks`), the plane starts what threads
/// allow and the gate alone keeps committed under the ceiling; every task ends and nothing is
/// dropped. What wounds throw away is said, not bounded.
#[test]
fn cold_forecasts_stay_safe_and_live() {
    let (mut cpu, mut wounded) = (0, 0);
    for seed in 0..SEEDS {
        let spent = tempo(seed, Forecasts::Cold, Plant::None)
            .unwrap_or_else(|broke| panic!("seed {seed}: {broke}"));
        cpu += spent.cpu;
        wounded += spent.wounded;
    }
    eprintln!("cold forecasts: wounds threw away {wounded} of {cpu} steps of work");
}

/// The control of the gate: a driver whose tasks commit without reserving is caught.
#[test]
fn the_oracle_catches_a_task_that_commits_without_reserving() {
    let caught = (0..16).find(|&seed| tempo(seed, Forecasts::Cold, Plant::SkipReserve).is_err());
    assert!(caught.is_some(), "the oracle missed commits that skipped the gate");
}

// --- (c) Two processes on one ledger ----------------------------------------------------------

/// The machine's budget the two processes share.
const MACHINE: u64 = 64 * GIB;

/// What a process asks the ledger for at a time, at least.
const GROW: u64 = GIB;

/// Both processes run, started at 1.
struct Both;

impl Alive for Both {
    fn started(&self, pid: u32) -> Result<Option<u64>, Unseen> {
        Ok(matches!(pid, 1 | 2).then_some(1))
    }
}

/// A process with its charge on the machine's ledger, its gate's room.
struct Proc {
    sim: Sim,
    slot: Slot,
    charge: u64,
}

fn broke(it: ledger::MachineError) -> String {
    format!("the ledger refused: {it:?}")
}

impl Proc {
    /// Asks the ledger for `bytes` more; granted, the gate's room grows by them.
    fn grow(&mut self, machine: &Machine<'_>, bytes: u64) -> Checked<()> {
        let mut op = Reserve::new(self.slot, bytes);
        match ledger::finish(&mut op, machine, &Both, || {}).map_err(broke)? {
            ledger::Reserved::Granted { counted, budget } => {
                if counted > budget {
                    return Err(format!("the ledger granted {counted} B of a {budget} B machine"));
                }
                self.charge += bytes;
                self.sim.set_room(self.charge)
            }
            ledger::Reserved::Refused { .. } => Ok(()),
        }
    }

    /// Grows while a task waits for memory (held by the gate, or by the plane's head of line);
    /// gives back what nothing waits for past one step of slack.
    fn adjust(&mut self, machine: &Machine<'_>) -> Checked<()> {
        if self.sim.finished.is_none() && (self.sim.gate.any_parked() || self.sim.plane.held_back())
        {
            return self.grow(machine, self.sim.plane.wanted().max(GROW));
        }
        let config = self.sim.gate.config();
        let overdrafts = config.overdraft_per_worker * u64::from(config.workers);
        let keep = self.sim.gate.committed() + overdrafts + GROW;
        if self.charge > keep + GROW {
            let give = self.charge - keep;
            let mut op = Release::new(self.slot, give);
            ledger::finish(&mut op, machine, &Both, || {}).map_err(broke)?;
            self.charge -= give;
            self.sim.set_room(self.charge)?;
        }
        Ok(())
    }

    /// One step of node `id`'s task, asking the ledger first when the gate would refuse it.
    fn step(&mut self, machine: &Machine<'_>, id: u32) -> Checked<()> {
        let task = &self.sim.tasks[id as usize];
        if let Some(Op::Reserve(quanta)) = task.program.get(task.pc).copied()
            && free_for_starts(&self.sim.gate, 0) < quanta * QUANTUM
        {
            self.grow(machine, (quanta * QUANTUM).max(GROW))?;
        }
        self.sim.step_task(id)
    }
}

/// A process of the pair: 4 workers, a ceiling of the whole machine, 8 to 19 tasks with peaks
/// of 64 MiB to 12 GiB, forecast their own peaks, its charge first the least its gate needs.
fn process(machine: &Machine<'_>, pid: u32, random: &mut SplitMix64) -> Checked<Proc> {
    let gate = GateConfig {
        ceiling: MACHINE,
        core: GIB,
        overdraft_per_worker: 64 * MIB,
        workers: 4,
        ungated: GIB,
    };
    let floor = gate.core + gate.ungated + gate.overdraft_per_worker * 4 + GROW;
    let mut join = Join::new(Process { pid, start: 1 });
    let slot = ledger::finish(&mut join, machine, &Both, || {}).map_err(broke)?;
    let mut op = Reserve::new(slot, floor);
    let ledger::Reserved::Granted { .. } = ledger::finish(&mut op, machine, &Both, || {}).map_err(broke)?
    else {
        return Err(format!("process {pid}'s floor of {floor} B was refused"));
    };
    let count = 8 + random.below(12) as u32;
    let mut specs = Vec::new();
    let mut programs = vec![Vec::new()];
    for id in 1..=count {
        let peak = match random.one_in(4) {
            true => random.range(2 * GIB / QUANTUM..12 * GIB / QUANTUM),
            false => random.range(SMALLEST..GIB / QUANTUM),
        };
        specs.push(node(id, &[], Pool::Work, 1, None, peak * QUANTUM));
        programs.push(program(peak, gate.overdraft_per_worker / QUANTUM, random.next_u64()));
    }
    let sim = Sim::new(gate, Some(floor), 4, specs, programs, Plant::None)?;
    Ok(Proc { sim, slot, charge: floor })
}

fn pair(seed: u64) -> Checked<()> {
    let header = Machine::header(MACHINE, 64);
    // alloc: the simulated mapping, once a case
    let words: Vec<AtomicU64> =
        (0..WORDS).map(|i| AtomicU64::new(if i < HEADER { header[i] } else { 0 })).collect();
    let machine = Machine::new(Path::new("machine.ledger"), &words).map_err(broke)?;
    let mut random = SplitMix64::new(seed);
    let mut procs = [process(&machine, 1, &mut random)?, process(&machine, 2, &mut random)?];
    for step in 0..2_000_000u64 {
        if procs.iter().all(|it| it.sim.finished.is_some()) {
            for it in &procs {
                it.sim.verdict()?;
            }
            return Ok(());
        }
        for it in &mut procs {
            it.adjust(&machine)?;
        }
        let charged: u64 = procs.iter().map(|it| it.charge).sum();
        if charged > MACHINE {
            return Err(format!("step {step}: the processes hold {charged} B of a {MACHINE} B machine"));
        }
        for it in &procs {
            if it.sim.gate.committed() > it.charge {
                return Err(format!(
                    "step {step}: a process commits {} B on a charge of {} B",
                    it.sim.gate.committed(),
                    it.charge
                ));
            }
        }
        let live: Vec<usize> = (0..2).filter(|&at| !procs[at].sim.running().is_empty()).collect();
        if live.is_empty() {
            continue;
        }
        let at = live[random.index(live.len())];
        let running = procs[at].sim.running();
        procs[at].step(&machine, running[random.index(running.len())])?;
    }
    Err("the pair passed its step bound".to_owned())
}

/// The invariant: two processes, each a plane and a gate whose room is its charge on one
/// shared ledger, never hold more of the ledger together than the machine's budget, and
/// neither commits past its own charge; both finish every task.
#[test]
fn two_processes_never_pass_the_machine() {
    for seed in 0..16 {
        pair(seed).unwrap_or_else(|broke| panic!("seed {seed}: {broke}"));
    }
}
