//! The shared ledger's DST (docs/v2/plan/design/end-state/memory.md, sections 4.5 and 9.8;
//! `testing` skill, dst-not-review).
//!
//! Two to six blaze processes, or one past [`SLOTS`], share one simulated ledger: real atomic
//! words, stepped one access at a time through the real operations ([`Op`]), so the processes
//! interleave at every atomic. Their acts are drawn: reserves that sleep until they fit
//! ([`Acquire`]), tries ([`Reserve`]), releases, core takes and gives, censuses, clean exits,
//! deaths at any step, restarts, pids the system gives again (to a restarted blaze or to a
//! process that is no blaze), a process that stops (it never steps again, its charge held), the
//! end of a driver's wait (a tick: every sleeper wakes), and beats.
//!
//! The oracles, each stated where it is checked:
//! - [`World::check`]: the live processes' grants sum to at most the budget; their cores to at
//!   most the cores and the floors given past them;
//! - [`World::saw`]: a sum counts no more than the live slots held while it ran, and a slot of a
//!   process dead before it began is freed by its end;
//! - [`World::quiet`]: no wake is lost.
//!
//! Four plants ([`Plant`]) prove the oracles catch what they claim to, each in a control.
//! Replay: a control or [`replay_one_seed`] names a seed of [`plan_of`]; the Hegel test prints
//! its plan.

use std::{fmt, path::Path, sync::atomic::AtomicU64};

use blaze_rng::SplitMix64;
use hegel::{TestCase, generators as gs};

use super::*;

/// The simulated ledger's path, which every error names.
const PATH: &str = "/run/blaze-501/machine.1";

/// Most acts a plan draws: a case runs in milliseconds.
const MOST_ACTS: u64 = 200;

/// Processes that are no blaze at once, each holding a pid.
const MOST_SQUATTERS: usize = 3;

/// Seeds of [`plan_of`] a control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 512;

/// The seed [`replay_one_seed`] runs, and its plant: set them to what a failure names.
const REPLAY: (u64, Plant) = (0, Plant::None);

/// Every process's core floor, as the driver asks it.
const FLOOR: u64 = 1;

/// The most steps one settling of every process may take: past it, something spins.
const SETTLE: u64 = 4_000_000;

// --- The plan ------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum What {
    /// Reserve these bytes, sleeping until they fit.
    Acquire(u64),
    /// Try once.
    Try(u64),
    /// Release `1 + pick % holds` bytes.
    Release(u64),
    /// Take these cores past what it holds.
    Take(u64),
    /// Give back `1 + pick % cores` cores.
    Give(u64),
    Census,
    Leave,
}

#[derive(Debug, Clone, Copy)]
enum Act {
    /// Up to `burst` steps of the process's operation.
    Step { actor: usize, burst: u32 },
    /// An idle, joined process starts an operation.
    Begin { actor: usize, what: What },
    /// The process dies, whatever step it is at.
    Kill { actor: usize },
    /// A dead or gone process starts again, with the free pid `pick` (modulo how many are free).
    Restart { actor: usize, pick: usize },
    /// An idle process stops, holding what it holds, and never steps again.
    Stall { actor: usize },
    /// A process that is no blaze takes the free pid `pick`.
    Squat { pick: usize },
    /// The oldest such process exits.
    Unsquat,
    /// Every driver's wait ends at its deadline: every sleeper wakes.
    Tick,
    /// An idle process beats.
    Beat { actor: usize },
}

#[derive(Debug)]
struct Plan {
    budget: u64,
    cores: u64,
    actors: usize,
    acts: Vec<Act>,
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
    let budget = d.int(64, 1 << 20);
    let cores = d.int(1, 32);
    // One plan in 16 has a process past the bound: it is refused, naming it.
    let actors = if d.one_in(16) { SLOTS + 1 } else { d.int(2, 6) as usize };
    // Two such amounts can pass the budget together, and each fits alone.
    let most = (budget * 2 / 3).max(1);
    let count = d.int(1, MOST_ACTS);
    // alloc: a plan, once a case
    let acts = (0..count).map(|_| act(d, actors, most, cores)).collect();
    Plan { budget, cores, actors, acts }
}

fn act(d: &mut impl Draw, actors: usize, most: u64, cores: u64) -> Act {
    let actor = d.int(0, actors as u64 - 1) as usize;
    match d.int(0, 99) {
        0..=59 => {
            let long = if actors > SLOTS { 40 } else { 400 };
            let burst = if d.one_in(4) { d.int(1, long) } else { d.int(1, 4) };
            Act::Step { actor, burst: burst as u32 }
        }
        60..=81 => {
            let what = match d.int(0, 11) {
                0..=2 => What::Acquire(d.int(1, most)),
                3 => What::Try(d.int(1, most)),
                4..=6 => What::Release(d.int(0, 1 << 20)),
                7 => What::Take(d.int(1, cores)),
                8 => What::Give(d.int(0, 64)),
                9 | 10 => What::Census,
                _ => What::Leave,
            };
            Act::Begin { actor, what }
        }
        82..=85 => Act::Kill { actor },
        86..=89 => Act::Restart { actor, pick: d.int(0, 15) as usize },
        90 => Act::Stall { actor },
        91..=93 => Act::Squat { pick: d.int(0, 15) as usize },
        94 | 95 => Act::Unsquat,
        96..=98 => Act::Tick,
        _ => Act::Beat { actor },
    }
}

/// The plan seed `seed` makes: what the controls search and [`REPLAY`] replays.
fn plan_of(seed: u64) -> Plan {
    plan(&mut SplitMix64::new(seed))
}

// --- The simulated machine ----------------------------------------------------------------

/// What broke a run: the oracle (a bug of the core, or a plant), or the test.
enum Broke {
    Oracle(String),
    Harness(String),
}

impl fmt::Display for Broke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Broke::Oracle(why) => write!(f, "the oracle failed: {why}"),
            Broke::Harness(why) => write!(f, "the test broke: {why}"),
        }
    }
}

fn oracle(why: impl fmt::Display) -> Broke {
    // alloc: a failure's message
    Broke::Oracle(why.to_string())
}

fn harness(why: impl fmt::Display) -> Broke {
    // alloc: a failure's message
    Broke::Harness(why.to_string())
}

/// A fresh ledger's words: the header a first process stamps, then zeros.
fn words(budget: u64, cores: u64) -> Vec<AtomicU64> {
    let header = Machine::header(budget, cores);
    // alloc: the simulated mapping, once a case
    (0..WORDS).map(|i| AtomicU64::new(if i < HEADER { header[i] } else { 0 })).collect()
}

fn machine(words: &[AtomicU64], plant: Plant) -> Machine<'_> {
    let Ok(mut machine) = Machine::new(Path::new(PATH), words) else {
        panic!("a ledger the test stamped reads");
    };
    machine.plant = plant;
    machine
}

/// The simulated system's processes: the start of the one each pid runs, if any.
struct Table(Vec<Option<u64>>);

impl Alive for Table {
    fn started(&self, pid: u32) -> Result<Option<u64>, Unseen> {
        Ok(self.0.get(pid as usize).copied().flatten())
    }
}

/// An operation in flight.
enum Running {
    Join(Join),
    Acquire(Acquire),
    Try(Reserve),
    Release(Release),
    Take(TakeCores),
    Give(GiveCores),
    Census(Census),
    Leave(Leave),
}

enum Out {
    Joined(Slot),
    Acquired(Acquired),
    Reserved(Reserved),
    Lowered,
    Took(Cores),
    Counted(Counted),
    Left,
}

fn step_op(op: &mut Running, m: &Machine<'_>, alive: &Table) -> Result<Step<Out>, MachineError> {
    Ok(match op {
        Running::Join(o) => o.step(m, alive)?.map(Out::Joined),
        Running::Acquire(o) => o.step(m, alive)?.map(Out::Acquired),
        Running::Try(o) => o.step(m, alive)?.map(Out::Reserved),
        Running::Release(o) => o.step(m, alive)?.map(|_| Out::Lowered),
        Running::Take(o) => o.step(m, alive)?.map(Out::Took),
        Running::Give(o) => o.step(m, alive)?.map(|_| Out::Lowered),
        Running::Census(o) => o.step(m, alive)?.map(Out::Counted),
        Running::Leave(o) => o.step(m, alive)?.map(|()| Out::Left),
    })
}

/// What the live slots held, at most, while a summing operation ran: per slot, the largest
/// value any live process wrote there since it began.
struct Watch {
    began: u64,
    bytes: [u64; SLOTS],
    cores: [u64; SLOTS],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Life {
    Running,
    /// Dead, exited, or refused a slot.
    Gone,
}

struct Actor {
    pid: u32,
    start: u64,
    life: Life,
    /// Stopped: it holds its charge and never steps again.
    stalled: bool,
    slot: Option<Slot>,
    op: Option<Running>,
    /// Asleep on the epoch while it holds this.
    blocked: Option<u32>,
    /// How many processes had died when it fell asleep.
    deaths_seen: u64,
    /// What its operation asks: bytes of an acquire or a try, cores of a take.
    pending: u64,
    /// The bytes granted and not released: what it uses.
    holds: u64,
    cores: u64,
    /// Of its cores, those given as its floor past what fit.
    credit: u64,
    watch: Option<Watch>,
}

impl Actor {
    fn new(pid: u32, start: u64) -> Actor {
        Actor {
            pid,
            start,
            life: Life::Running,
            stalled: false,
            slot: None,
            op: Some(Running::Join(Join::new(Process { pid, start }))),
            blocked: None,
            deaths_seen: 0,
            pending: 0,
            holds: 0,
            cores: 0,
            credit: 0,
            watch: None,
        }
    }
}

/// A process that died holding `slot` with the owner word `owner`, at `died_at`.
struct Dead {
    slot: usize,
    owner: u64,
    died_at: u64,
}

struct World {
    plant: Plant,
    budget: u64,
    cores: u64,
    planned: usize,
    words: Vec<AtomicU64>,
    table: Table,
    actors: Vec<Actor>,
    dead: Vec<Dead>,
    squatters: Vec<u32>,
    clock: u64,
    starts: u64,
    deaths: u64,
}

/// A wake of every sleeper: what a futex wake of all does.
fn wake_all(actors: &mut [Actor]) {
    for actor in actors {
        actor.blocked = None;
    }
}

impl World {
    fn new(plan: &Plan, plant: Plant) -> World {
        // Pids: few past the processes, so they are given again often.
        let pids = plan.actors + MOST_SQUATTERS;
        // alloc: the simulated system, once a case
        let mut table = Table(vec![None; pids + 1]);
        let mut starts = 0;
        // alloc: the simulated processes, once a case
        let mut actors = Vec::with_capacity(plan.actors);
        for i in 0..plan.actors {
            starts += 1;
            let pid = i as u32 + 1;
            table.0[pid as usize] = Some(starts);
            actors.push(Actor::new(pid, starts));
        }
        World {
            plant,
            budget: plan.budget,
            cores: plan.cores,
            planned: plan.actors,
            words: words(plan.budget, plan.cores),
            table,
            actors,
            // alloc: the dead, once a case
            dead: Vec::new(),
            // alloc: the squatters, once a case
            squatters: Vec::new(),
            clock: 0,
            starts,
            deaths: 0,
        }
    }

    fn load(&self, word: usize) -> u64 {
        self.words[word].load(SeqCst)
    }

    fn runnable(&self, a: usize) -> bool {
        let actor = &self.actors[a];
        actor.life == Life::Running && !actor.stalled && actor.blocked.is_none() && actor.op.is_some()
    }

    fn idle(&self, a: usize) -> Option<Slot> {
        let actor = &self.actors[a];
        let idle = actor.life == Life::Running && !actor.stalled && actor.op.is_none();
        if idle { actor.slot } else { None }
    }

    fn free_pids(&self) -> Vec<u32> {
        // alloc: a pick among the free pids
        (1..self.table.0.len() as u32).filter(|&pid| self.table.0[pid as usize].is_none()).collect()
    }

    /// What the live slots hold now: the start of a watch.
    fn watch(&self) -> Watch {
        let mut watch = Watch { began: self.clock, bytes: [0; SLOTS], cores: [0; SLOTS] };
        for actor in &self.actors {
            if let (Life::Running, Some(slot)) = (actor.life, actor.slot) {
                watch.bytes[slot.index] = self.load(at(slot.index, RESERVED));
                watch.cores[slot.index] = self.load(at(slot.index, CORES));
            }
        }
        watch
    }

    fn act(&mut self, act: Act) -> Result<(), Broke> {
        self.clock += 1;
        match act {
            Act::Step { actor, burst } => {
                for _ in 0..burst {
                    if !self.runnable(actor) {
                        break;
                    }
                    self.step(actor)?;
                }
            }
            Act::Begin { actor, what } => self.begin(actor, what)?,
            Act::Kill { actor } => self.kill(actor),
            Act::Restart { actor, pick } => {
                let free = self.free_pids();
                if self.actors[actor].life == Life::Gone && !free.is_empty() {
                    let pid = free[pick % free.len()];
                    self.starts += 1;
                    self.table.0[pid as usize] = Some(self.starts);
                    self.actors[actor] = Actor::new(pid, self.starts);
                }
            }
            Act::Stall { actor } => {
                if self.idle(actor).is_some() {
                    self.actors[actor].stalled = true;
                }
            }
            Act::Squat { pick } => {
                let free = self.free_pids();
                if self.squatters.len() < MOST_SQUATTERS && !free.is_empty() {
                    let pid = free[pick % free.len()];
                    self.starts += 1;
                    self.table.0[pid as usize] = Some(self.starts);
                    self.squatters.push(pid);
                }
            }
            Act::Unsquat => {
                if !self.squatters.is_empty() {
                    let pid = self.squatters.remove(0);
                    self.table.0[pid as usize] = None;
                }
            }
            Act::Tick => wake_all(&mut self.actors),
            Act::Beat { actor } => {
                if let Some(slot) = self.idle(actor) {
                    let m = machine(&self.words, self.plant);
                    m.beat(slot, self.clock)
                        .map_err(|e| oracle(format_args!("process {actor} beats: {e}")))?;
                }
            }
        }
        self.check()?;
        self.quiet()
    }

    fn begin(&mut self, a: usize, what: What) -> Result<(), Broke> {
        let Some(slot) = self.idle(a) else { return Ok(()) };
        let watch = self.watch();
        let actor = &mut self.actors[a];
        let op = match what {
            What::Acquire(bytes) => {
                actor.pending = bytes;
                actor.watch = Some(watch);
                Running::Acquire(Acquire::new(slot, bytes))
            }
            What::Try(bytes) => {
                actor.pending = bytes;
                actor.watch = Some(watch);
                Running::Try(Reserve::new(slot, bytes))
            }
            What::Release(pick) => {
                if actor.holds == 0 {
                    return Ok(());
                }
                let bytes = 1 + pick % actor.holds;
                // It stops using them before it gives them back.
                actor.holds -= bytes;
                Running::Release(Release::new(slot, bytes))
            }
            What::Take(cores) => {
                actor.pending = cores;
                actor.watch = Some(watch);
                Running::Take(TakeCores::new(slot, cores, FLOOR))
            }
            What::Give(pick) => {
                if actor.cores == 0 {
                    return Ok(());
                }
                let cores = 1 + pick % actor.cores;
                actor.cores -= cores;
                actor.credit -= actor.credit.min(cores);
                Running::Give(GiveCores::new(slot, cores))
            }
            What::Census => {
                actor.watch = Some(watch);
                Running::Census(Census::new())
            }
            What::Leave => {
                actor.holds = 0;
                actor.cores = 0;
                actor.credit = 0;
                Running::Leave(Leave::new(slot))
            }
        };
        actor.op = Some(op);
        Ok(())
    }

    fn kill(&mut self, a: usize) {
        let actor = &mut self.actors[a];
        if actor.life != Life::Running {
            return;
        }
        // A claim in flight is not recorded: its slot counts nothing (the window the core's doc
        // names), and a slot it holds is known only once its join ends.
        if let Some(slot) = actor.slot {
            self.dead.push(Dead { slot: slot.index, owner: slot.owner, died_at: self.clock });
        }
        self.table.0[actor.pid as usize] = None;
        self.deaths += 1;
        let (pid, start) = (actor.pid, actor.start);
        *actor = Actor::new(pid, start);
        actor.life = Life::Gone;
        actor.op = None;
    }

    /// One step of process `a`'s operation: one access of a shared word at most.
    fn step(&mut self, a: usize) -> Result<(), Broke> {
        self.clock += 1;
        let stepped = {
            let World { words, table, actors, plant, .. } = &mut *self;
            let m = machine(words, *plant);
            let Some(op) = actors[a].op.as_mut() else {
                return Err(harness(format_args!("process {a} stepped with no operation")));
            };
            step_op(op, &m, table)
        };
        match stepped {
            Err(error) => self.failed(a, error)?,
            Ok(Step::Next) => {}
            Ok(Step::Wake) => wake_all(&mut self.actors),
            Ok(Step::Block { expected }) => {
                // The kernel compares the word as it blocks.
                if self.load(EPOCH) as u32 == expected {
                    let deaths = self.deaths;
                    let actor = &mut self.actors[a];
                    actor.blocked = Some(expected);
                    actor.deaths_seen = deaths;
                }
            }
            Ok(Step::Done(out)) => {
                self.actors[a].op = None;
                self.done(a, out)?;
            }
        }
        self.watched(a);
        self.check()
    }

    /// Every watch takes in what process `a`'s step wrote in its slot.
    fn watched(&mut self, a: usize) {
        let actor = &self.actors[a];
        let (Life::Running, Some(slot)) = (actor.life, actor.slot) else { return };
        let bytes = self.load(at(slot.index, RESERVED));
        let cores = self.load(at(slot.index, CORES));
        for actor in &mut self.actors {
            if let Some(watch) = &mut actor.watch {
                watch.bytes[slot.index] = watch.bytes[slot.index].max(bytes);
                watch.cores[slot.index] = watch.cores[slot.index].max(cores);
            }
        }
    }

    fn failed(&mut self, a: usize, error: MachineError) -> Result<(), Broke> {
        match error.why {
            // One past the bound: refused, naming it, and it exits.
            Why::Full if self.planned > SLOTS => {
                let actor = &mut self.actors[a];
                self.table.0[actor.pid as usize] = None;
                actor.life = Life::Gone;
                actor.op = None;
                Ok(())
            }
            Why::Full | Why::Lost { .. } | Why::Over { .. } | Why::Churn { .. } | Why::Stuck { .. } => {
                Err(oracle(format_args!("process {a} (pid {}): {error}", self.actors[a].pid)))
            }
            _ => Err(harness(format_args!("process {a} (pid {}): {error}", self.actors[a].pid))),
        }
    }

    fn done(&mut self, a: usize, out: Out) -> Result<(), Broke> {
        match out {
            Out::Joined(slot) => self.actors[a].slot = Some(slot),
            Out::Acquired(got) => {
                self.saw(a, Some(got.counted), None)?;
                let actor = &mut self.actors[a];
                actor.holds += actor.pending;
            }
            Out::Reserved(reserved) => {
                let counted = match reserved {
                    Reserved::Granted { counted, .. } | Reserved::Refused { counted, .. } => counted,
                };
                self.saw(a, Some(counted), None)?;
                if let Reserved::Granted { .. } = reserved {
                    let actor = &mut self.actors[a];
                    actor.holds += actor.pending;
                }
            }
            Out::Lowered => {}
            Out::Took(took) => {
                self.saw(a, None, Some(took.counted))?;
                let actor = &mut self.actors[a];
                let fit = actor.pending.saturating_sub(took.over);
                actor.cores += took.granted;
                actor.credit += took.granted - took.granted.min(fit);
            }
            Out::Counted(counted) => self.saw(a, Some(counted.reserved), Some(counted.cores_held))?,
            Out::Left => {
                let actor = &mut self.actors[a];
                self.table.0[actor.pid as usize] = None;
                actor.slot = None;
                actor.life = Life::Gone;
            }
        }
        self.actors[a].watch = None;
        Ok(())
    }

    /// The oracles of a sum, at the end of process `a`'s summing operation.
    fn saw(&mut self, a: usize, bytes: Option<u64>, cores: Option<u64>) -> Result<(), Broke> {
        let Some(watch) = &self.actors[a].watch else {
            return Err(harness(format_args!("process {a} summed with no watch")));
        };
        // The invariant: a sum counts only the slots of live processes, each at most what it
        // held while the sum ran: a freed slot of the dead is never counted.
        let held: u64 = watch.bytes.iter().sum();
        if let Some(counted) = bytes
            && counted > held
        {
            return Err(oracle(format_args!(
                "process {a}'s sum counted {counted} bytes, and the live slots held at most {held} \
                 while it ran: it counted the slot of a dead process"
            )));
        }
        let held: u64 = watch.cores.iter().sum();
        if let Some(counted) = cores
            && counted > held
        {
            return Err(oracle(format_args!(
                "process {a}'s sum counted {counted} cores, and the live slots held at most {held} \
                 while it ran: it counted the slot of a dead process"
            )));
        }
        // The invariant: a dead process's charge is gone after one sum by any other: a sum that
        // began after the death freed its slot.
        for dead in &self.dead {
            if dead.died_at < watch.began && self.load(at(dead.slot, OWNER)) == dead.owner {
                return Err(oracle(format_args!(
                    "slot {} still holds the process that died at {}, after process {a}'s sum, \
                     begun at {}, passed it",
                    dead.slot, dead.died_at, watch.began
                )));
            }
        }
        let words = &self.words;
        self.dead.retain(|dead| words[at(dead.slot, OWNER)].load(SeqCst) == dead.owner);
        Ok(())
    }

    /// The oracles of every instant.
    fn check(&self) -> Result<(), Broke> {
        let live = || self.actors.iter().filter(|actor| actor.life == Life::Running);
        // The invariant: the sum of live reservations never exceeds the budget.
        let held: u64 = live().map(|actor| actor.holds).sum();
        if held > self.budget {
            return Err(oracle(format_args!(
                "the live processes hold {held} bytes granted, past the budget of {}",
                self.budget
            )));
        }
        // The invariant: cores held never exceed the cores and the floors of the processes
        // given them past what fit (at most one each).
        let cores: u64 = live().map(|actor| actor.cores).sum();
        let credit: u64 = live().map(|actor| actor.credit).sum();
        if let Some(actor) = live().find(|actor| actor.credit > FLOOR) {
            return Err(oracle(format_args!(
                "pid {} holds {} cores past what fit, more than its floor of {FLOOR}",
                actor.pid, actor.credit
            )));
        }
        if cores > self.cores + credit {
            return Err(oracle(format_args!(
                "the live processes hold {cores} cores, past the machine's {} and their {credit} \
                 floors given past it",
                self.cores
            )));
        }
        Ok(())
    }

    /// The invariant: no wake is lost. Once nothing runs, a sleeper sleeps only on the epoch
    /// now, and only while it would not fit beside what the live slots hold; a death since it
    /// fell asleep excuses it until its driver's wait ends (a tick), as the dead process may
    /// have lowered without bumping.
    fn quiet(&self) -> Result<(), Broke> {
        if (0..self.actors.len()).any(|a| self.runnable(a)) {
            return Ok(());
        }
        let epoch = self.load(EPOCH) as u32;
        let held: u64 = (0..SLOTS)
            .filter(|&slot| state_of(self.load(at(slot, OWNER))) == LIVE)
            .map(|slot| self.load(at(slot, RESERVED)))
            .sum();
        for (a, actor) in self.actors.iter().enumerate() {
            let Some(expected) = actor.blocked else { continue };
            if actor.life != Life::Running || actor.deaths_seen != self.deaths {
                continue;
            }
            if epoch != expected {
                return Err(oracle(format_args!(
                    "process {a} sleeps on epoch {expected}, which moved to {epoch} with no wake"
                )));
            }
            let Some(Running::Acquire(acquire)) = &actor.op else {
                return Err(harness(format_args!("process {a} sleeps outside an acquire")));
            };
            let bytes = acquire.bytes();
            if held + bytes <= self.budget {
                return Err(oracle(format_args!(
                    "process {a} sleeps for {bytes} bytes, which fit beside the {held} the live \
                     slots hold in the budget of {}: a lost wake",
                    self.budget
                )));
            }
        }
        Ok(())
    }

    /// Runs every process that can run, each alone to the end of its operation or its sleep.
    fn settle(&mut self) -> Result<(), Broke> {
        let mut steps = 0;
        loop {
            let mut moved = false;
            for a in 0..self.actors.len() {
                while self.runnable(a) {
                    self.step(a)?;
                    moved = true;
                    steps += 1;
                    if steps > SETTLE {
                        return Err(oracle(format_args!(
                            "the processes did not settle in {SETTLE} steps"
                        )));
                    }
                }
            }
            if !moved {
                return Ok(());
            }
        }
    }

    /// The end: every process gives back what it holds, round by round, and every wait ends
    /// (bounded-progress): what is still asleep at the end does not fit beside what the live
    /// slots hold (a stalled process's charge, a dead one's whose pid runs again).
    fn drain(&mut self) -> Result<(), Broke> {
        for _ in 0..self.actors.len() + 4 {
            self.settle()?;
            self.quiet()?;
            // `1 + pick % n` with `pick = n - 1` is all `n`.
            for a in 0..self.actors.len() {
                if let Some(pick) = self.actors[a].holds.checked_sub(1) {
                    self.begin(a, What::Release(pick))?;
                }
            }
            self.settle()?;
            self.quiet()?;
            for a in 0..self.actors.len() {
                if let Some(pick) = self.actors[a].cores.checked_sub(1) {
                    self.begin(a, What::Give(pick))?;
                }
            }
            self.settle()?;
            self.quiet()?;
            wake_all(&mut self.actors);
        }
        self.settle()?;
        self.quiet()
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    let mut world = World::new(plan, plant);
    for &act in &plan.acts {
        world.act(act)?;
    }
    world.drain()
}

// --- The tests ------------------------------------------------------------------------------

/// The invariant: whatever the interleaving at every atomic, deaths, pid reuse and stopped
/// processes, the live reservations never sum past the budget, the cores held pass the cores by
/// at most the floors given past them, a sum counts no dead slot and frees every slot of a
/// process dead before it began, and no wake is lost.
#[hegel::test(test_cases = 200)]
fn live_reservations_fit_the_budget_and_no_wake_is_lost(tc: TestCase) {
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
fn the_oracle_catches_a_sum_before_the_add() {
    control(Plant::SumThenAdd);
}

#[test]
fn the_oracle_catches_a_wake_before_the_slot_is_lowered() {
    control(Plant::WakeBeforeLower);
}

#[test]
fn the_oracle_catches_a_pruned_slot_still_counted() {
    control(Plant::PrunedStillCounted);
}

#[test]
fn the_oracle_catches_a_pid_matched_without_its_start() {
    control(Plant::PidWithoutStart);
}

/// One seeded plan, [`REPLAY`]'s: set it to a seed a failure names to run that case alone.
#[test]
fn replay_one_seed() {
    let (seed, plant) = REPLAY;
    if let Err(broke) = run(&plan_of(seed), plant) {
        panic!("seed {seed}, Plant::{plant:?}: {broke}");
    }
}

// --- Single cases -----------------------------------------------------------------------------

fn join(m: &Machine<'_>, table: &Table, pid: u32, start: u64) -> Result<Slot, MachineError> {
    finish(&mut Join::new(Process { pid, start }), m, table, || {})
}

/// The invariant: a process past [`SLOTS`] is refused, naming the bound and the file, and joins
/// once a slot's process has died.
#[test]
fn the_sixty_fifth_process_is_refused_naming_the_bound() {
    let words = words(1 << 20, 8);
    let m = machine(&words, Plant::None);
    // alloc: the simulated system
    let mut table = Table(vec![None; SLOTS + 2]);
    for pid in 1..=SLOTS as u32 {
        table.0[pid as usize] = Some(u64::from(pid) * 10);
        let slot = join(&m, &table, pid, u64::from(pid) * 10).expect("a slot is free");
        assert_eq!(slot.index(), pid as usize - 1);
    }
    let last = SLOTS as u32 + 1;
    table.0[last as usize] = Some(7);
    let refused = join(&m, &table, last, 7).expect_err("every slot holds a live process");
    assert_eq!(refused.why, Why::Full);
    let said = refused.to_string();
    assert!(said.contains("64") && said.contains(PATH), "{said}");
    // Pid 3 dies: the join's sum frees its slot, and takes it.
    table.0[3] = None;
    let slot = join(&m, &table, last, 7).expect("the dead process's slot is freed");
    assert_eq!(slot.index(), 2);
}

/// Two reservers stepped in lockstep, so each one's add and sum fall between the other's.
fn race(plant: Plant) -> [Reserved; 2] {
    let words = words(100, 4);
    let m = machine(&words, plant);
    // alloc: the simulated system
    let table = Table(vec![None, Some(1), Some(2)]);
    let a = join(&m, &table, 1, 1).expect("a slot is free");
    let b = join(&m, &table, 2, 2).expect("a slot is free");
    let mut ops = [Reserve::new(a, 60), Reserve::new(b, 60)];
    let mut outs = [None, None];
    for _ in 0..2 * MOST_STEPS {
        for (op, out) in ops.iter_mut().zip(&mut outs) {
            if out.is_some() {
                continue;
            }
            match op.step(&m, &table).expect("a reserve steps") {
                Step::Done(reserved) => *out = Some(reserved),
                Step::Next | Step::Wake => {}
                Step::Block { .. } => panic!("a reserve never sleeps"),
            }
        }
        if let [Some(a), Some(b)] = outs {
            return [a, b];
        }
    }
    panic!("two reserves did not end in {} steps", 2 * MOST_STEPS);
}

/// The invariant: of two reservers whose bytes together pass the budget, cut between add and
/// sum, at most one passes; summing before the add lets both.
#[test]
fn two_reservers_cut_between_add_and_sum_never_both_pass() {
    let granted = |reserved: &Reserved| matches!(reserved, Reserved::Granted { .. });
    let outs = race(Plant::None);
    assert!(!outs.iter().all(granted), "both passed: {outs:?}");
    let planted = race(Plant::SumThenAdd);
    assert!(planted.iter().all(granted), "the planted sum-then-add did not race: {planted:?}");
}

/// The invariant: a file of another layout, or another length, is refused naming the file.
#[test]
fn a_ledger_of_another_layout_is_refused_naming_its_file() {
    let words = words(100, 4);
    words[AT_LAYOUT].store(LAYOUT + 1, SeqCst);
    let Err(error) = Machine::new(Path::new(PATH), &words) else {
        panic!("a ledger of layout {} read as layout {LAYOUT}", LAYOUT + 1);
    };
    assert_eq!(error.why, Why::Layout { found: LAYOUT + 1 });
    assert!(error.to_string().contains(PATH), "{error}");
    words[AT_LAYOUT].store(LAYOUT, SeqCst);
    let Err(error) = Machine::new(Path::new(PATH), &words[..WORDS - 1]) else {
        panic!("a ledger one word short read");
    };
    assert_eq!(error.why, Why::Length { words: WORDS - 1 });
    assert!(error.to_string().contains(PATH), "{error}");
}

/// The invariant: a live process that stops beating keeps its charge: no sum frees it, and
/// another's reserve is refused by it.
#[test]
fn a_live_process_that_stops_beating_keeps_its_charge() {
    let words = words(100, 4);
    let m = machine(&words, Plant::None);
    // alloc: the simulated system
    let table = Table(vec![None, Some(1), Some(2)]);
    let a = join(&m, &table, 1, 1).expect("a slot is free");
    let granted = finish(&mut Reserve::new(a, 80), &m, &table, || {}).expect("a reserve ends");
    assert_eq!(granted, Reserved::Granted { counted: 80, budget: 100 });
    m.beat(a, 1).expect("its slot is its");
    let b = join(&m, &table, 2, 2).expect("a slot is free");
    let counted = finish(&mut Census::new(), &m, &table, || {}).expect("a census ends");
    assert_eq!((counted.processes, counted.pruned, counted.reserved), (2, 0, 80));
    let refused = finish(&mut Reserve::new(b, 30), &m, &table, || {}).expect("a reserve ends");
    assert!(
        matches!(refused, Reserved::Refused { over_by: 10, counted: 110, budget: 100, .. }),
        "{refused:?}"
    );
}
