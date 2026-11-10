//! The ledger's DST (docs/memory-budget.md, section 7, "Proof"; `testing` skill, dst-not-review).
//!
//! Two to five daemons share one simulated ledger file. Each daemon is a real [`Plane`] that
//! admits against the real core ([`Ledger::turn`]), and its tasks' grants and real use are
//! drawn. Their acts interleave with crashes (a daemon gone without leaving), restarts, pids the
//! system gives again (to a new daemon, or to a process that is no daemon), and clean exits. A
//! turn is one atomic read-modify-write of the file, as the driver's `flock` makes it; under
//! [`Plant::UnlockedRmw`] it is a read, then other acts, then a write of what was read.
//!
//! Every daemon has a request open, so each takes a lease at its turns ([`Plane::lease`]) and
//! starts the tasks that fit it at their `Add`, with no turn between.
//!
//! After every act the oracle checks:
//! - the live daemons' grants sum to at most the machine budget;
//! - so do their rooms, what each may grant with no turn, its lease in it;
//! - no live daemon is granted more than the ledger records of it, nor has more room (so what
//!   it grants between turns is room no other daemon is told is free), and every live daemon
//!   has its record;
//! - a turn admits nothing while the other live daemons' charges sum to the machine budget.
//!
//! At the end every daemon's tasks end, round by round (each finishes what runs, says what it
//! holds, and takes a turn): every task, each of which fits alone, starts within a bounded
//! number of rounds; then a probe as large as one daemon may be allowed with every other idle
//! starts at its next turn (bounded-progress).
//!
//! Four plants ([`Plant`]) prove the oracle catches what it claims to, each in a control.
//! Replay: a control or [`replay_one_seed`] names a seed of [`plan_of`]; the Hegel test prints
//! its plan.

use std::{fmt, path::Path};

use blaze_protocol::events::NodeKind;
use blaze_rng::SplitMix64;
use hegel::{TestCase, generators as gs};

use super::*;
use crate::{
    machine::{NodeId, Outcome, Pool, Spec},
    plane::{Budget, Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId},
};

/// The simulated ledger's path, which every error names.
const PATH: &str = "/tmp/blaze-501/machine";

/// Pids the simulated system has: few, so they are given again often. Five daemons and
/// [`MOST_SQUATTERS`] other processes leave one free for a restart.
const PIDS: u32 = 9;
const MOST_SQUATTERS: usize = 3;

/// Most acts a plan draws: a case runs in milliseconds.
const MOST_ACTS: u64 = 160;

/// Seeds of [`plan_of`] a control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 256;

/// The seed [`replay_one_seed`] runs, and its plant: set them to what a failure names.
const REPLAY: (u64, Plant) = (0, Plant::None);

/// The one request each daemon's plane has open.
const REQUEST: RequestId = RequestId(1);

// --- The plan ------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Daemon {
    ceiling: u64,
    /// What the daemon holds with nothing running (a front end, caches).
    baseline: u64,
    work: usize,
}

#[derive(Debug, Clone, Copy)]
enum Act {
    /// A task asking `memory` that holds `real` once it runs: more or less than it asked.
    Submit { daemon: usize, memory: u64, real: u64 },
    /// The running task `pick` (modulo how many run) ends.
    Finish { daemon: usize, pick: usize },
    /// The daemon measures what it holds.
    Held { daemon: usize },
    /// A turn of the ledger.
    Turn { daemon: usize },
    Crash { daemon: usize },
    /// A dead daemon starts again, with the free pid `pick` (modulo how many are free).
    Restart { daemon: usize, pick: usize },
    /// A clean exit.
    Leave { daemon: usize },
    /// A process that is no daemon takes the free pid `pick`.
    Squat { pick: usize },
    /// The oldest such process exits.
    Unsquat,
}

#[derive(Debug)]
struct Plan {
    budget: u64,
    daemons: Vec<Daemon>,
    acts: Vec<Act>,
}

/// A source of draws: Hegel's (shrunk, replayed by Hegel) or a seed's ([`plan_of`]).
trait Draw {
    /// In `lo..=hi`.
    fn int(&mut self, lo: u64, hi: u64) -> u64;

    fn coin(&mut self) -> bool {
        self.int(0, 1) == 1
    }

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
    let budget = d.int(256, 1 << 20);
    let count = d.int(2, 5) as usize;
    // alloc: a plan, once a case
    let daemons = (0..count)
        .map(|_| Daemon {
            // Half the daemons may be allowed the whole machine: the probe at the end then
            // sees every byte another daemon's record keeps.
            ceiling: if d.coin() { budget } else { d.int(budget / 2, budget) },
            baseline: d.int(1, budget / 16),
            work: d.int(1, 4) as usize,
        })
        .collect();
    let n = d.int(1, MOST_ACTS);
    // alloc: a plan, once a case
    let acts = (0..n).map(|_| act(d, budget, count)).collect();
    Plan { budget, daemons, acts }
}

/// Every task fits alone: at most 3/16 of the budget, and five baselines take at most 5/16 of
/// it, under any ceiling (at least half the budget).
fn act(d: &mut impl Draw, budget: u64, count: usize) -> Act {
    let daemon = d.int(0, count as u64 - 1) as usize;
    match d.int(0, 99) {
        0..=29 => {
            let memory = d.int(1, budget * 3 / 16);
            Act::Submit { daemon, memory, real: d.int(0, 2 * memory) }
        }
        30..=49 => Act::Finish { daemon, pick: d.int(0, 7) as usize },
        50..=63 => Act::Held { daemon },
        64..=85 => Act::Turn { daemon },
        86..=89 => Act::Crash { daemon },
        90..=94 => Act::Restart { daemon, pick: d.int(0, 7) as usize },
        95 | 96 => Act::Leave { daemon },
        97 | 98 => Act::Squat { pick: d.int(0, 7) as usize },
        _ => Act::Unsquat,
    }
}

/// The plan seed `seed` makes: what the controls search and [`REPLAY`] replays.
fn plan_of(seed: u64) -> Plan {
    plan(&mut SplitMix64::new(seed))
}

// --- The simulated machine ----------------------------------------------------------------

/// What broke a run: the oracle (a bug of the core or the plane, or a plant), or the test.
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

fn oracle(why: String) -> Broke {
    Broke::Oracle(why)
}

fn harness(why: impl fmt::Display) -> Broke {
    // alloc: a failure's message
    Broke::Harness(why.to_string())
}

struct Task {
    id: NodeId,
    memory: u64,
    real: u64,
    running: bool,
}

/// A daemon that runs.
struct Live {
    process: Process,
    plane: Plane,
    tasks: Vec<Task>,
    /// Under [`Plant::UnlockedRmw`]: the file as its turn read it, not yet written back.
    read: Option<Vec<u8>>,
}

impl Live {
    fn held(&self, daemon: &Daemon) -> u64 {
        daemon.baseline + self.tasks.iter().filter(|it| it.running).map(|it| it.real).sum::<u64>()
    }

    fn running(&self) -> usize {
        self.tasks.iter().filter(|it| it.running).count()
    }

    /// Steps the plane, marking the tasks it starts as running.
    fn step(&mut self, input: PlaneInput) -> Result<(), Broke> {
        let outputs = self.plane.step(input).map_err(harness)?;
        self.started(&outputs)
    }

    fn started(&mut self, outputs: &[PlaneOutput]) -> Result<(), Broke> {
        for output in outputs {
            if let PlaneOutput::Run { id, .. } = output {
                let Some(task) = self.tasks.iter_mut().find(|it| it.id == *id && !it.running)
                else {
                    return Err(harness(format_args!("the plane ran {id}, which waits for no run")));
                };
                task.running = true;
            }
        }
        Ok(())
    }
}

/// The processes of the simulated system: what [`Alive`] asks.
struct Procs<'a>(&'a [Process]);

impl Alive for Procs<'_> {
    fn started(&self, pid: u32) -> Option<u64> {
        self.0.iter().find(|it| it.pid == pid).map(|it| it.start)
    }
}

/// A plane taking a turn: what it starts is kept for the daemon to mark.
struct Admitting<'a> {
    plane: &'a mut Plane,
    at_ns: u64,
    outputs: Result<Vec<PlaneOutput>, crate::Error>,
}

impl Admits for Admitting<'_> {
    fn allow(&mut self, allowance: &Allowance) {
        self.outputs =
            self.plane.step(PlaneInput::Allowance { at_ns: self.at_ns, bytes: allowance.bytes });
    }

    fn held(&self) -> u64 {
        self.plane.held()
    }

    fn granted(&self) -> u64 {
        self.plane.granted()
    }

    fn lease(&self) -> u64 {
        self.plane.lease()
    }
}

struct World<'p> {
    plant: Plant,
    budget: u64,
    daemons: &'p [Daemon],
    live: Vec<Option<Live>>,
    /// Every process that runs: daemons and squatters.
    procs: Vec<Process>,
    squatters: Vec<Process>,
    /// The ledger file's bytes.
    file: Vec<u8>,
    /// Process start times, node ids and the writers' clock: one count.
    clock: u64,
    submitted: usize,
}

impl<'p> World<'p> {
    fn new(plan: &'p Plan, plant: Plant) -> World<'p> {
        World {
            plant,
            budget: plan.budget,
            daemons: &plan.daemons,
            live: plan.daemons.iter().map(|_| None).collect(), // alloc: a case's daemons
            procs: Vec::new(),                                 // alloc: a case's processes
            squatters: Vec::new(),                             // alloc: a case's squatters
            file: Vec::new(),                                  // alloc: the simulated file
            clock: 0,
            submitted: 0,
        }
    }

    fn path() -> &'static Path {
        Path::new(PATH)
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// The file as it is, decoded with the plant in place.
    fn ledger(&self, bytes: &[u8]) -> Result<Ledger<'static>, Broke> {
        let mut ledger = Ledger::decode(World::path(), bytes).map_err(harness)?;
        ledger.plant = self.plant;
        Ok(ledger)
    }

    fn write(&mut self, ledger: &Ledger<'_>) {
        let mut bytes = [0; LEN];
        ledger.encode(&mut bytes);
        self.file.clear();
        self.file.extend_from_slice(&bytes);
    }

    fn free_pids(&self) -> Vec<u32> {
        // alloc: a short list, once an act
        (1..=PIDS).filter(|pid| self.procs.iter().all(|it| it.pid != *pid)).collect()
    }

    fn act(&mut self, act: Act) -> Result<(), Broke> {
        let at_ns = self.tick();
        match act {
            Act::Submit { daemon, memory, real } => self.submit(daemon, memory, real)?,
            Act::Finish { daemon, pick } => {
                if let Some(live) = &mut self.live[daemon] {
                    finish(live, pick, at_ns)?;
                }
            }
            Act::Held { daemon } => {
                let config = self.daemons[daemon];
                if let Some(live) = &mut self.live[daemon] {
                    let bytes = live.held(&config);
                    live.step(PlaneInput::Held { at_ns, bytes })?;
                }
            }
            Act::Turn { daemon } => self.turn(daemon)?,
            Act::Crash { daemon } => {
                if let Some(live) = self.live[daemon].take() {
                    self.procs.retain(|it| *it != live.process);
                }
            }
            Act::Restart { daemon, pick } => self.start(daemon, pick)?,
            Act::Leave { daemon } => {
                if let Some(live) = self.live[daemon].take() {
                    let mut ledger = self.ledger(&self.file)?;
                    if let Err(why) = ledger.leave(live.process) {
                        return Err(oracle(format!("a clean exit found no record: {why}")));
                    }
                    self.write(&ledger);
                    self.procs.retain(|it| *it != live.process);
                }
            }
            Act::Squat { pick } => {
                let free = self.free_pids();
                if self.squatters.len() < MOST_SQUATTERS && !free.is_empty() {
                    let process = Process { pid: free[pick % free.len()], start: self.tick() };
                    self.procs.push(process);
                    self.squatters.push(process);
                }
            }
            Act::Unsquat => {
                if !self.squatters.is_empty() {
                    let gone = self.squatters.remove(0);
                    self.procs.retain(|it| *it != gone);
                }
            }
        }
        self.check()
    }

    fn submit(&mut self, daemon: usize, memory: u64, real: u64) -> Result<(), Broke> {
        let id = NodeId(u32::try_from(self.tick()).map_err(harness)?);
        let at_ns = self.clock;
        let Some(live) = &mut self.live[daemon] else { return Ok(()) };
        live.tasks.push(Task { id, memory, real, running: false });
        self.submitted += 1;
        let spec = Spec {
            id,
            kind: NodeKind::Codegen,
            name: format!("task {}", id.0), // alloc: a node's name, once a task
            deps: Vec::new(),
            pool: Pool::Work,
            cost: 1,
            key: None,
            memory,
        };
        // alloc: one node a step
        live.step(PlaneInput::Add { at_ns, request: REQUEST, nodes: vec![spec] })
    }

    /// A daemon starts: a new process, its plane, its record joined, and its first turn (the
    /// driver's: no request comes before it), all before any other act.
    fn start(&mut self, daemon: usize, pick: usize) -> Result<(), Broke> {
        if self.live[daemon].is_some() {
            return Ok(());
        }
        let free = self.free_pids();
        let Some(&pid) = free.get(pick % free.len().max(1)) else {
            return Err(harness("no pid is free for a restart"));
        };
        let process = Process { pid, start: self.tick() };
        self.procs.push(process);
        let config = self.daemons[daemon];
        let budget = Budget {
            bytes: config.ceiling,
            from: "the simulated daemon's ceiling".to_owned(), // alloc: a daemon's budget
            kept_back: 0,
        };
        let plane = Plane::new(PlaneConfig {
            io: 1,
            work: config.work,
            budget: Some(budget),
            starve_after: 8,
        })
        .map_err(harness)?;
        let mut live = Live { process, plane, tasks: Vec::new(), read: None }; // alloc: a daemon's tasks
        let at_ns = self.clock;
        live.step(PlaneInput::Open {
            at_ns,
            request: REQUEST,
            weight: 1,
            keep_going: true,
            idle: false,
        })?;
        live.step(PlaneInput::Held { at_ns, bytes: config.baseline })?;
        let mut ledger = self.ledger(&self.file)?;
        ledger.prune(&Procs(&self.procs));
        let record = Record {
            process,
            budget: self.budget,
            ceiling: config.ceiling,
            held: config.baseline,
            granted: 0,
            lease: 0,
            at_ns,
            version: Label::head(b"blaze 0.0.0-sim", 1),
            home: Label::tail(b"/sim/home/.blaze", fnv1a(b"/sim/home/.blaze")),
        };
        match ledger.join(record) {
            Ok(()) => {}
            Err(LedgerError { why: Why::Full, .. }) => {
                return Err(oracle(format!(
                    "the ledger filled with {MOST} records while at most {} daemons ran: dead \
                     daemons' records were kept",
                    self.daemons.len()
                )));
            }
            Err(why) => return Err(harness(why)),
        }
        self.write(&ledger);
        let bytes = self.file.clone(); // alloc: the file a turn reads
        self.live[daemon] = Some(live);
        self.commit(daemon, &bytes)
    }

    /// A turn of `daemon`: one atomic read-modify-write of the file, or under
    /// [`Plant::UnlockedRmw`] its read now and its write at its next turn.
    fn turn(&mut self, daemon: usize) -> Result<(), Broke> {
        let Some(live) = &mut self.live[daemon] else { return Ok(()) };
        if self.plant == Plant::UnlockedRmw {
            match live.read.take() {
                None => {
                    live.read = Some(self.file.clone()); // alloc: the file a turn read
                    return Ok(());
                }
                Some(read) => return self.commit(daemon, &read),
            }
        }
        let bytes = self.file.clone(); // alloc: the file a turn reads
        self.commit(daemon, &bytes)
    }

    /// The rest of a turn, on the file's `bytes` as the turn read them.
    fn commit(&mut self, daemon: usize, bytes: &[u8]) -> Result<(), Broke> {
        let mut ledger = self.ledger(bytes)?;
        let at_ns = self.tick();
        let budget = self.budget;
        let procs = &self.procs;
        let Some(live) = &mut self.live[daemon] else { return Ok(()) };
        let me = live.process;
        // What the other daemons that truly run are charged, as the turn reads it.
        let others: u64 = ledger
            .records()
            .filter(|it| it.process != me && procs.contains(&it.process))
            .map(Record::charge)
            .sum();
        let before = live.plane.granted();
        let mut admitting =
            Admitting { plane: &mut live.plane, at_ns, outputs: Ok(Vec::new()) }; // alloc: an empty list
        let turn = match ledger.turn(me, at_ns, &Procs(procs), &mut admitting) {
            Ok(turn) => turn,
            Err(LedgerError { why: Why::NotJoined { .. }, .. }) => {
                return Err(oracle(format!(
                    "pid {} took a turn with no record of it in the ledger: another daemon's \
                     write dropped it",
                    me.pid
                )));
            }
            Err(why) => return Err(harness(why)),
        };
        let outputs = std::mem::replace(&mut admitting.outputs, Ok(Vec::new())).map_err(harness)?; // alloc: an empty list
        live.started(&outputs)?;
        let after = live.plane.granted();
        if others >= budget && after > before {
            return Err(oracle(format!(
                "pid {} admitted {} bytes while the other daemons' charges sum to {others}, the \
                 machine budget {budget} or more (its allowance: {:?})",
                me.pid,
                after - before,
                turn.allowance
            )));
        }
        self.write(&ledger);
        Ok(())
    }

    /// The invariants every act keeps.
    fn check(&self) -> Result<(), Broke> {
        let granted: u64 = self.live.iter().flatten().map(|it| it.plane.granted()).sum();
        if granted > self.budget {
            return Err(oracle(format!(
                "the live daemons' grants sum to {granted}, over the machine budget {}: {}",
                self.budget,
                self.grants()
            )));
        }
        // A daemon is live only once its first turn is done ([`World::start`]), so each has a
        // room: none is the `u64::MAX` of a plane never told an allowance.
        let mut rooms = 0u64;
        for live in self.live.iter().flatten() {
            rooms = rooms.saturating_add(live.plane.room());
        }
        if rooms > self.budget {
            return Err(oracle(format!(
                "the live daemons' rooms, what each may grant with no turn, sum to {rooms}, over \
                 the machine budget {}: {}",
                self.budget,
                self.grants()
            )));
        }
        let ledger = self.ledger(&self.file)?;
        for live in self.live.iter().flatten() {
            let Some(record) = ledger.record(live.process) else {
                return Err(oracle(format!(
                    "pid {} runs and the ledger has no record of it: {}",
                    live.process.pid,
                    self.grants()
                )));
            };
            if live.plane.granted() > record.charge() {
                return Err(oracle(format!(
                    "pid {} is granted {}, more than the {} the ledger records of it, room \
                     another daemon may be told is free: {}",
                    live.process.pid,
                    live.plane.granted(),
                    record.charge(),
                    self.grants()
                )));
            }
            if live.plane.room() > record.charge() {
                return Err(oracle(format!(
                    "pid {} has room to grant up to {} with no turn, more than the {} the ledger \
                     records of it (its lease {}): the rest is room another daemon is told is \
                     free: {}",
                    live.process.pid,
                    live.plane.room(),
                    record.charge(),
                    record.lease,
                    self.grants()
                )));
            }
        }
        Ok(())
    }

    /// Each live daemon's grant beside what the ledger records of it, for a failure's message.
    fn grants(&self) -> String {
        // alloc: a failure's message
        let mut text = String::new();
        let ledger = Ledger::decode(World::path(), &self.file).ok();
        for live in self.live.iter().flatten() {
            let charge = ledger.as_ref().and_then(|it| it.record(live.process)).map(Record::charge);
            text.push_str(&format!(
                "[pid {} granted {} room {} recorded {charge:?}] ",
                live.process.pid,
                live.plane.granted(),
                live.plane.room()
            ));
        }
        if let Some(ledger) = &ledger {
            for record in ledger.records() {
                text.push_str(&format!(
                    "(record pid {} start {} charge {}) ",
                    record.process.pid,
                    record.process.start,
                    record.charge()
                ));
            }
        }
        text
    }

    /// Every daemon's tasks end, round by round; then the probe (bounded-progress).
    fn drain(&mut self) -> Result<(), Broke> {
        for daemon in 0..self.live.len() {
            if let Some(read) = self.live[daemon].as_mut().and_then(|it| it.read.take()) {
                self.commit(daemon, &read)?;
                self.check()?;
            }
        }
        if self.live.iter().all(Option::is_none) {
            self.start(0, 0)?;
            self.check()?;
        }
        // Each task fits alone, and in a round in which tasks wait at least one starts at the
        // latest by the next round's turns. The leases do not change that: from the second
        // round on no task was started since a daemon's last turn but at that turn, so a
        // daemon that starts none leases 1/16 of its ceiling at most, and five baselines and
        // four such leases leave any daemon its largest task. The first round's records may
        // still hold the leases of the acts, which is the two rounds more.
        let bound = 2 * self.submitted + 6;
        let mut rounds = 0;
        while self.waiting() > 0 {
            if rounds == bound {
                return Err(oracle(format!(
                    "{} tasks, each of which fits alone, still wait after {bound} rounds in which \
                     every running task ended and every daemon took a turn: {}",
                    self.waiting(),
                    self.grants()
                )));
            }
            self.round()?;
            rounds += 1;
        }
        self.round()?;
        self.probe()
    }

    /// The tasks not yet ended, of every live daemon: waiting or running.
    fn waiting(&self) -> usize {
        self.live.iter().flatten().map(|it| it.tasks.len()).sum()
    }

    /// Every running task ends and every daemon says what it holds, then each takes a turn.
    fn round(&mut self) -> Result<(), Broke> {
        for daemon in 0..self.live.len() {
            let config = self.daemons[daemon];
            let at_ns = self.tick();
            let Some(live) = &mut self.live[daemon] else { continue };
            while live.running() > 0 {
                finish(live, 0, at_ns)?;
            }
            live.step(PlaneInput::Held { at_ns, bytes: live.held(&config) })?;
            self.check()?;
        }
        for daemon in 0..self.live.len() {
            if self.live[daemon].is_some() {
                let bytes = self.file.clone(); // alloc: the file a turn reads
                self.commit(daemon, &bytes)?;
                self.check()?;
            }
        }
        Ok(())
    }

    /// With every daemon idle, a task as large as the ledger should let the daemon with the
    /// largest ceiling grant starts at its next turn: no dead daemon's charge is kept.
    fn probe(&mut self) -> Result<(), Broke> {
        let Some(daemon) = (0..self.live.len())
            .filter(|&it| self.live[it].is_some())
            .max_by_key(|&it| self.daemons[it].ceiling)
        else {
            return Err(harness("no daemon runs for the probe"));
        };
        let me = self.live[daemon].as_ref().expect("a live daemon").process;
        let ledger = self.ledger(&self.file)?;
        let others: u64 = ledger
            .records()
            .filter(|it| it.process != me && self.procs.contains(&it.process))
            .map(Record::charge)
            .sum();
        let fits = self.daemons[daemon].ceiling.min(self.budget.saturating_sub(others));
        if fits == 0 {
            return Ok(());
        }
        self.submit(daemon, fits, fits)?;
        let bytes = self.file.clone(); // alloc: the file a turn reads
        self.commit(daemon, &bytes)?;
        self.check()?;
        let live = self.live[daemon].as_ref().expect("a live daemon");
        // Granted all of it too: a task as large as the ceiling may start alone with less.
        if live.tasks.iter().any(|it| !it.running) || live.plane.granted() < fits {
            return Err(oracle(format!(
                "pid {} did not start a task of {fits} bytes, all its ceiling and the machine \
                 budget less the {others} the other live daemons hold allow, with every daemon \
                 idle: {}",
                me.pid,
                self.grants()
            )));
        }
        Ok(())
    }
}

/// The running task `pick` (modulo how many run) of `live` ends.
fn finish(live: &mut Live, pick: usize, at_ns: u64) -> Result<(), Broke> {
    // alloc: a short list, once an act
    let running: Vec<usize> = (0..live.tasks.len()).filter(|&it| live.tasks[it].running).collect();
    if running.is_empty() {
        return Ok(());
    }
    let task = live.tasks.remove(running[pick % running.len()]);
    live.step(PlaneInput::Done {
        at_ns,
        id: task.id,
        outcome: Outcome::Built,
        added: Vec::new(), // alloc: none added
        group: None,
        peak: task.memory.max(task.real),
    })
}

fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    let mut world = World::new(plan, plant);
    for daemon in 0..plan.daemons.len() {
        world.start(daemon, 0)?;
        world.check()?;
    }
    for &act in &plan.acts {
        world.act(act)?;
    }
    world.drain()
}

// --- The tests ------------------------------------------------------------------------------

/// The invariant: whatever crashes, restarts and pid reuse, the live daemons' grants never sum
/// past the machine budget, nor does what they may grant with no turn (each one's room: its
/// charge and its lease), none grants or has room the ledger does not hold for it, none admits
/// while the others' charges fill the machine, and every task that fits starts.
#[hegel::test(test_cases = 300)]
fn live_grants_fit_the_machine_budget_after_any_crash(tc: TestCase) {
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
fn the_oracle_catches_a_read_modify_write_without_the_lock() {
    control(Plant::UnlockedRmw);
}

#[test]
fn the_oracle_catches_a_prune_that_ignores_start_time() {
    control(Plant::PruneIgnoresStart);
}

#[test]
fn the_oracle_catches_an_allowance_that_leaves_out_a_daemon() {
    control(Plant::AllowanceSkipsOne);
}

#[test]
fn the_oracle_catches_a_lease_the_ledger_does_not_record() {
    control(Plant::LeaseUnrecorded);
}

/// One seeded plan, [`REPLAY`]'s: set it to a seed a failure names to run that case alone.
#[test]
fn replay_one_seed() {
    let (seed, plant) = REPLAY;
    if let Err(broke) = run(&plan_of(seed), plant) {
        panic!("seed {seed}, Plant::{plant:?}: {broke}");
    }
}

// --- The file ---------------------------------------------------------------------------------

/// A record of `pid` with every other field drawn, labels longer and shorter than they keep.
fn record(d: &mut impl Draw, pid: u32) -> Record {
    let version_len = d.int(0, 40) as usize;
    let version: Vec<u8> = (0..version_len).map(|_| d.int(1, 255) as u8).collect(); // alloc: a drawn label
    let home_len = d.int(0, 60) as usize;
    let home: Vec<u8> = (0..home_len).map(|_| d.int(1, 255) as u8).collect(); // alloc: a drawn label
    Record {
        process: Process { pid, start: d.int(0, u64::MAX) },
        budget: d.int(0, u64::MAX),
        ceiling: d.int(0, u64::MAX),
        held: d.int(0, u64::MAX),
        granted: d.int(0, u64::MAX),
        lease: d.int(0, u64::MAX),
        at_ns: d.int(0, u64::MAX),
        version: Label::head(&version, d.int(0, u64::MAX)),
        home: Label::tail(&home, d.int(0, u64::MAX)),
    }
}

/// What is written reads back as the same ledger, and writes again to the same bytes
/// (persisted-state-round-trips); one byte changed anywhere is an error naming the file, never
/// a ledger read.
#[hegel::test(test_cases = 200)]
fn a_ledger_round_trips_byte_for_byte_and_refuses_a_changed_byte(tc: TestCase) {
    let mut d = Drawn(&tc);
    let path = Path::new(PATH);
    let mut ledger = Ledger::empty(path);
    for pid in 1..=d.int(0, MOST as u64) as u32 {
        ledger.join(record(&mut d, pid)).expect("a free slot");
    }
    for _ in 0..d.int(0, 4) {
        let pid = d.int(1, MOST as u64) as u32;
        let start = ledger.records().find(|it| it.process.pid == pid).map(|it| it.process);
        if let Some(process) = start {
            ledger.leave(process).expect("a joined record");
        }
    }
    let mut bytes = [0; LEN];
    ledger.encode(&mut bytes);
    let read = Ledger::decode(path, &bytes).expect("what was written reads");
    assert_eq!(read, ledger);
    let mut again = [0; LEN];
    read.encode(&mut again);
    assert_eq!(again, bytes, "a ledger read back writes the same bytes");

    let at = d.int(0, LEN as u64 - 1) as usize;
    let flip = d.int(1, 255) as u8;
    bytes[at] ^= flip;
    match Ledger::decode(path, &bytes) {
        Ok(_) => panic!("byte {at} changed by {flip:#04x} and the ledger still read"),
        Err(error) => {
            assert!(error.why.corrupt(), "{error}");
            let said = error.to_string();
            assert!(said.contains(PATH), "the error names the file: {said}");
        }
    }
}

#[test]
fn an_empty_file_is_an_empty_ledger_and_a_short_one_is_corrupt() {
    let path = Path::new(PATH);
    assert_eq!(Ledger::decode(path, &[]).expect("empty reads").records().count(), 0);
    let error = Ledger::decode(path, &[0; 100]).expect_err("a short file");
    assert_eq!(error.why, Why::Length { len: 100 });
    assert!(error.to_string().contains(PATH));
}

/// A file of the layout before the lease is refused by its layout, naming the file and both
/// layouts, never read as a ledger whose daemons lease nothing: a daemon of that layout would
/// not count this one's lease.
#[test]
fn a_file_of_the_layout_before_the_lease_is_refused_naming_both_layouts() {
    let path = Path::new(PATH);
    let mut ledger = Ledger::empty(path);
    ledger.join(record(&mut SplitMix64::new(3), 1)).expect("a free slot");
    let mut bytes = [0; LEN];
    ledger.encode(&mut bytes);
    assert_eq!(bytes[8..12], FORMAT.to_le_bytes());
    bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
    let error = Ledger::decode(path, &bytes).expect_err("layout 1");
    assert_eq!(error.why, Why::Format { found: 1 });
    assert!(error.why.corrupt());
    let said = error.to_string();
    assert!(
        said.contains(PATH) && said.contains("layout 1") && said.contains("layout 2"),
        "{said}"
    );
}

/// A record's lease is part of its charge, so another daemon's allowance is less by it.
#[test]
fn a_lease_counts_against_every_other_daemons_allowance() {
    let path = Path::new(PATH);
    let mut ledger = Ledger::empty(path);
    let of = |pid: u32, held: u64, lease: u64| Record {
        process: Process { pid, start: 1 },
        budget: 100,
        ceiling: 100,
        held,
        granted: 0,
        lease,
        at_ns: 0,
        version: Label::EMPTY,
        home: Label::EMPTY,
    };
    ledger.join(of(1, 10, 20)).expect("a free slot");
    ledger.join(of(2, 5, 0)).expect("a free slot");
    assert_eq!(ledger.record(Process { pid: 1, start: 1 }).expect("joined").charge(), 30);
    let allowance = ledger.allowance(Process { pid: 2, start: 1 }).expect("joined");
    assert_eq!((allowance.bytes, allowance.others), (70, 30));
}

/// A daemon past the [`MOST`]th is refused, naming the file and the bound.
#[test]
fn joining_past_the_most_is_refused() {
    let path = Path::new(PATH);
    let mut ledger = Ledger::empty(path);
    let mut rng = SplitMix64::new(7);
    for pid in 1..=MOST as u32 {
        ledger.join(record(&mut rng, pid)).expect("a free slot");
    }
    let error = ledger.join(record(&mut rng, MOST as u32 + 1)).expect_err("the ledger is full");
    assert_eq!(error.why, Why::Full);
    let said = error.to_string();
    assert!(said.contains(PATH) && said.contains("64"), "{said}");
}
