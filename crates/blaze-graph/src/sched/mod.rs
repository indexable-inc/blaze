//! The scheduler as a state machine that does no IO (docs/v2/plan/lanes/sched.md): it reads no
//! clock, starts no thread and never sleeps. A driver tells it what happened ([`Sched::spawn`],
//! [`Sched::suspended`], [`Sched::done`], [`Sched::key_done`], [`Sched::yielded`],
//! [`Sched::core_idle`], [`Sched::advance_clock`]) and does what it decides
//! ([`Sched::take_decisions`]: [`Decision::Run`], [`Decision::Wake`], [`Decision::Yield`]),
//! waking itself at [`Sched::next_deadline`]. The machine's shape is an input ([`Cores`]), never
//! read here.
//!
//! A waiting job holds no core: [`Sched::suspended`] frees its core, which runs the next job at
//! once, and the job is runnable again when what it waits for is done: a job, a key's flight,
//! a counter (every job of a parallel map: one suspend for all of them, as Naughty Dog's fiber
//! jobs wait) or a time. A wait that would close a cycle is refused with the [`Cycle`] it would
//! close.
//!
//! Policy:
//! - One queue per core, and one for its critical-path jobs, taken first. A job is queued on
//!   the core that spawned it (the owner pops newest, as Chase-Lev and Rayon), unless its
//!   [`Affinity`] names another place.
//! - A waiter wakes on the core that finished what it waited on, in that core's LIFO slot ahead
//!   of its queues, hot in the cache that just wrote what it reads. After [`LIFO_RUNS`] slot
//!   runs in a row the core takes its inbox and queues first, so the slot never starves them.
//! - A job handed to a core from elsewhere (a hint, a timer, an external key) is a message to
//!   that core's inbox: [`Decision::Wake`].
//! - Work-conserving, one sleeper woken per job: while any core is parked no job sits
//!   stealable. A job made runnable on a busy core goes at once to the nearest parked core, by
//!   the tiers of [`Cores`]: a critical-path job to the nearest parked core of the highest
//!   capacity first, any other job to a lower-capacity core first within each tier. A free core
//!   with nothing of its own steals half a victim's queue from the nearest core with work, tier
//!   by tier (critical jobs first for a big core, the others first for a small one). So a steal
//!   crosses a node only when the thief's node has nothing runnable.
//! - A big core never sits idle while a small core runs a critical job: the scheduler asks the
//!   job to move ([`Decision::Yield`]), the job looks at its core's flag at its next query miss
//!   and calls [`Sched::yielded`], and moves to the big core. Nothing is preempted.
//!
//! Every driver of the process is a member of one [`World`]: its placement (cores, queues,
//! clock) is its own, and the wait graph ([`graph`]) is one for all, so a cycle of waits
//! through jobs of two drivers is refused as one of a single driver is. [`Sched`] is a world
//! of one driver.

mod cores;
pub mod fiber;
#[cfg(all(test, not(blaze_check)))]
mod fiber_tests;
mod graph;
mod host;
#[cfg(test)]
mod tests;

use std::{collections::VecDeque, fmt};

use blaze_core::wheel::Wheel;

pub use cores::{CoreInfo, Cores, Distances, LEVELS, Level, MAX_CORES, ShapeError};
pub use fiber::{
    Carried, Idle, IdleReason, NoWait, Switch, SwitchEvent, carry, current_job, miss, observe, observe_idle, on_fiber,
};
pub(crate) use host::{Host, check_stall, join_pool, leave_pool};
pub use host::{
    Event, EventWait, FIBER_STACK, claim, claim_for, close_scope, count_towards, innermost_scope, job_here, key_done, open_scope, release, wait,
};

use crate::Key;
use cores::{any_in, first_from, get, set};
pub use graph::StallWait;
use graph::{Graph, Home, Life, On, Waited};

/// How many slot jobs a core runs in a row before it looks at its inbox and queues (Tokio's
/// `MAX_LIFO_POLLS_PER_TICK`).
pub const LIFO_RUNS: u8 = 3;

/// The most job ids: a job id indexes the scheduler's columns.
pub const MAX_JOBS: u32 = 1 << 30;

const NONE: u32 = u32::MAX;

/// The bit of a member number that says it has its own placement ([`World::join_own`]).
const OWN: u32 = 1 << 31;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OwnState {
    Free,
    Running,
    Waiting,
}

/// A member of its own placement: its jobs in the graph, by local id.
struct Own {
    state: Vec<OwnState>,
    token: Vec<usize>,
    free: Vec<u32>,
    /// The tokens of jobs woken since its member last took them.
    woken: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct JobId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct CoreId(pub u32);

/// A count of jobs still to end ([`Sched::counter`]): a wait on it is a wait for all of them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct CounterId(pub u32);

/// Where a spawned job should run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Affinity {
    /// The spawner's core; with no spawner, the least loaded core.
    Spawner,
    Core(CoreId),
    /// The spawner's core if it is in this L3 group, else the group's least loaded core.
    L3(u32),
    /// The spawner's core if it is on this node, else the node's least loaded core.
    Node(u32),
    /// The least loaded core.
    Any,
}

/// How a job is spawned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spawn {
    /// The running job that spawns it; `None` for the driver.
    pub parent: Option<JobId>,
    pub hint: Affinity,
    /// On the critical path, by the profiler's data: it runs on the highest-capacity cores.
    pub critical: bool,
    /// The counter it counts towards until it ends.
    pub counter: Option<CounterId>,
}

impl Spawn {
    /// A job of the driver's, on the least loaded core.
    pub const fn root() -> Spawn {
        Spawn { parent: None, hint: Affinity::Any, critical: false, counter: None }
    }

    /// A job of `parent`'s, on its core.
    pub const fn child(parent: JobId) -> Spawn {
        Spawn { parent: Some(parent), hint: Affinity::Spawner, critical: false, counter: None }
    }

    pub const fn hint(mut self, hint: Affinity) -> Spawn {
        self.hint = hint;
        self
    }

    pub const fn critical(mut self, critical: bool) -> Spawn {
        self.critical = critical;
        self
    }

    pub const fn counted(mut self, counter: CounterId) -> Spawn {
        self.counter = Some(counter);
        self
    }
}

/// What a suspended job waits for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// The job's end.
    Job(JobId),
    /// The key's current flight ([`Sched::claim`]); with none, nothing.
    Key(Key),
    /// Every job counted towards it ending.
    Counter(CounterId),
    /// The clock reaching this many nanoseconds.
    Until(u64),
}

/// What the scheduler asks of its driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// The core, free now (it reported a job's end, wait or move, or was idle), runs or resumes
    /// the job. The driver clears the core's move flag.
    Run { core: CoreId, job: JobId },
    /// The job is handed to the core: a message to its inbox. A parked core is unparked and
    /// gets [`Decision::Run`] for it at its [`Sched::core_idle`]; a busy core takes it at its
    /// next pick, unless an idle core steals it first.
    Wake { core: CoreId, job: JobId },
    /// Set the core's move flag: the job running there checks it (one relaxed load) at its
    /// next query miss, never on a hit, and calls [`Sched::yielded`] when it is set.
    Yield { core: CoreId, job: JobId },
}

/// What [`Sched::suspended`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Suspend {
    /// The job waits and its core runs something else.
    Waiting,
    /// What it waits for is done already: the job goes on, on its core.
    Ready,
}

/// What [`Sched::yielded`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Yielded {
    /// The job is handed to a bigger core; its own runs something else.
    Moved,
    /// No bigger core is idle any more: the job goes on where it is.
    Stay,
}

/// A wait refused because it closes a cycle of waits: `jobs[0]` asked to wait, each job waits
/// for the next (its job, its key's claimer, one of its counter's jobs) and the last for the
/// first. The asking job goes on running and must give way (unwind, release, be posted again);
/// nothing in the scheduler changed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error, off every hot path
pub struct Cycle {
    pub jobs: Vec<JobId>,
}

/// No wait could ever end: every pool's workers idle with nothing queued, no driver running,
/// and every job in the wait graph waiting on another of them (a job, a key a job claimed, or
/// a counter of such jobs). Each of those waits ends with this, for its job (`job`) to fail
/// with it: the request it served fails naming every wait, instead of hanging.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error, off every hot path
pub struct Stalled {
    pub job: JobId,
    pub waits: Vec<(JobId, StallWait)>,
}

/// The most waits a [`Stalled`]'s message names.
const STALL_NAMED: usize = 64;

impl fmt::Display for Stalled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stalled: nothing can run and no wait can end ({} jobs wait), so job {} fails: ", self.waits.len(), self.job.0)?;
        for (at, (job, wait)) in self.waits.iter().take(STALL_NAMED).enumerate() {
            if at > 0 {
                f.write_str("; ")?;
            }
            write!(f, "job {} waits for ", job.0)?;
            match wait {
                StallWait::Job(other) => write!(f, "job {}", other.0)?,
                StallWait::Key { key, claimer } => {
                    f.write_str("key ")?;
                    for byte in key.0 {
                        write!(f, "{byte:02x}")?;
                    }
                    write!(f, ", claimed by job {}", claimer.0)?;
                }
                StallWait::Counter(counter) => write!(f, "counter {} (a scope's jobs)", counter.0)?,
            }
        }
        if self.waits.len() > STALL_NAMED {
            write!(f, "; and {} more", self.waits.len() - STALL_NAMED)?;
        }
        Ok(())
    }
}

impl fmt::Display for Cycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a wait would close a cycle of waits: ")?;
        for (at, job) in self.jobs.iter().enumerate() {
            if at > 0 {
                f.write_str(" -> ")?;
            }
            write!(f, "job {}", job.0)?;
        }
        if let Some(first) = self.jobs.first() {
            write!(f, " -> job {}", first.0)?;
        }
        Ok(())
    }
}

impl std::error::Error for Cycle {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum JobState {
    /// Never spawned.
    Vacant,
    /// Runnable: in a core's slot, inbox or queues, or handed to a woken core.
    Queued,
    Running,
    Suspended,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum CoreState {
    Busy,
    /// Nothing to run anywhere it may look; waits for a [`Decision::Wake`].
    Parked,
    /// Handed a job by a [`Decision::Wake`]; runs it at its [`Sched::core_idle`].
    Woken,
}

/// Counts of what the scheduler decided, for a driver's report.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub runs: u64,
    pub wakes: u64,
    /// By [`Level`] from thief to victim.
    pub steals: [u64; LEVELS],
    pub cycles: u64,
    pub yields: u64,
    pub moves: u64,
}

/// A steal as the tests see it: who took from whom, and how many jobs were stealable on the
/// thief's node (other than the victim's) at that moment, counted by a scan of every core.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct StealRecord {
    thief: u32,
    victim: u32,
    on_thief_node: usize,
}

/// A job as its member's placement sees it.
#[derive(Clone, Copy)]
struct Job {
    state: JobState,
    critical: bool,
    /// Suspended on a timed wait, which the placement keeps; any other wait is the graph's.
    timed: bool,
    /// Queued: the core that holds it; running: its core; suspended or done: where it last ran.
    core: u32,
    /// Its id in the [`World`].
    global: u32,
}

const _: () = assert!(size_of::<Job>() <= 16);

const VACANT: Job = Job { state: JobState::Vacant, critical: false, timed: false, core: NONE, global: NONE };

/// One driver's cores, queues and clock: where its jobs run. Its job ids are its own.
pub(crate) struct Place {
    cores: Cores,
    now: u64,
    decisions: Vec<Decision>,
    stats: Stats,

    // Per core.
    core_state: Vec<CoreState>,
    running: Vec<u32>,
    reserved: Vec<u32>,
    slot: Vec<u32>,
    lifo_runs: Vec<u8>,
    inbox: Vec<VecDeque<u32>>,
    queue: Vec<VecDeque<u32>>,
    /// Critical-path jobs, taken before `queue`.
    urgent: Vec<VecDeque<u32>>,
    /// Bit per parked core.
    parked: Vec<u64>,
    parked_count: u32,
    /// Bit per core with a job in its slot, inbox or queues.
    stealable: Vec<u64>,
    /// Bit per core below the top capacity running a critical job.
    critical_small: Vec<u64>,
    /// Bit per core asked to move its job and not answered yet.
    yield_sent: Vec<u64>,

    /// By local job id.
    jobs: Vec<Job>,
    timers: Wheel<u32>,
    /// What a step collects before it acts on it: timers fired.
    scratch: Vec<u32>,

    #[cfg(test)]
    steals: Vec<StealRecord>,
}

impl Place {
    /// Every core parked, room for `jobs` job ids before a column grows.
    fn new(cores: Cores, jobs: usize) -> Place {
        let n = cores.len();
        let words = cores.words();
        let mut parked = Vec::with_capacity(words);
        parked.extend_from_slice(cores.all());
        let zeros = |words: usize| vec![0u64; words];
        let mut inbox = Vec::with_capacity(n);
        let mut queue = Vec::with_capacity(n);
        let mut urgent = Vec::with_capacity(n);
        for _ in 0..n {
            inbox.push(VecDeque::with_capacity(64));
            queue.push(VecDeque::with_capacity(256));
            urgent.push(VecDeque::with_capacity(64));
        }
        let mut place = Place {
            now: 0,
            decisions: Vec::with_capacity(64),
            stats: Stats::default(),
            core_state: Vec::with_capacity(n),
            running: Vec::with_capacity(n),
            reserved: Vec::with_capacity(n),
            slot: Vec::with_capacity(n),
            lifo_runs: Vec::with_capacity(n),
            inbox,
            queue,
            urgent,
            parked,
            parked_count: n as u32,
            stealable: zeros(words),
            critical_small: zeros(words),
            yield_sent: zeros(words),
            jobs: Vec::with_capacity(jobs),
            timers: Wheel::new(),
            scratch: Vec::with_capacity(64),
            cores,
            #[cfg(test)]
            steals: Vec::new(), // alloc: test bookkeeping
        };
        place.core_state.resize(n, CoreState::Parked);
        place.running.resize(n, NONE);
        place.reserved.resize(n, NONE);
        place.slot.resize(n, NONE);
        place.lifo_runs.resize(n, 0);
        place
    }

    pub(crate) fn cores(&self) -> &Cores {
        &self.cores
    }

    pub(crate) fn stats(&self) -> Stats {
        self.stats
    }

    pub(crate) fn now(&self) -> u64 {
        self.now
    }

    pub(crate) fn job_state(&self, job: u32) -> JobState {
        self.jobs.get(job as usize).map_or(JobState::Vacant, |it| it.state)
    }

    /// Where the job is held, runs, or last ran.
    pub(crate) fn job_core(&self, job: u32) -> Option<CoreId> {
        let core = self.jobs.get(job as usize).map_or(NONE, |it| it.core);
        (core != NONE).then_some(CoreId(core))
    }

    pub(crate) fn is_critical(&self, job: u32) -> bool {
        self.jobs.get(job as usize).is_some_and(|it| it.critical)
    }

    /// The job's id in the world.
    pub(crate) fn global(&self, job: u32) -> JobId {
        JobId(self.jobs[job as usize].global)
    }

    pub(crate) fn core_state(&self, core: CoreId) -> CoreState {
        self.core_state[core.0 as usize]
    }

    /// The local id of the job `core` runs.
    pub(crate) fn running(&self, core: CoreId) -> Option<u32> {
        let job = self.running[core.0 as usize];
        (job != NONE).then_some(job)
    }

    /// The decisions made since the last call, in the order made, by local job id.
    pub(crate) fn take_decisions(&mut self) -> std::vec::Drain<'_, Decision> {
        self.decisions.drain(..)
    }

    pub(crate) fn next_deadline(&mut self) -> Option<u64> {
        self.timers.next_deadline()
    }

    /// `job` is new, world id `global`; `spawner` is its parent's local id when its parent
    /// runs here.
    fn spawn(&mut self, job: u32, global: JobId, spawner: Option<u32>, hint: Affinity, critical: bool) {
        let need = job as usize + 1;
        if self.jobs.len() < need {
            self.jobs.resize(need, VACANT);
        }
        let j = job as usize;
        assert!(
            matches!(self.jobs[j].state, JobState::Vacant | JobState::Done),
            "job {} spawned while it is live ({:?})",
            global.0,
            self.jobs[j].state
        );
        let spawner = spawner.map(|parent| {
            assert!(self.job_state(parent) == JobState::Running, "job {}'s parent is not running", global.0);
            self.jobs[parent as usize].core
        });
        let home = match hint {
            Affinity::Spawner => match spawner {
                Some(core) => core,
                None => self.least_loaded(critical, |_, _| true),
            },
            Affinity::Core(core) => {
                assert!((core.0 as usize) < self.cores.len(), "job {} hinted to core {}, past the shape", global.0, core.0);
                core.0
            }
            Affinity::L3(l3) => match spawner.filter(|&core| self.cores.in_l3(core, l3)) {
                Some(core) => core,
                None => self.least_loaded(critical, |cores, core| cores.in_l3(core, l3)),
            },
            Affinity::Node(node) => match spawner.filter(|&core| self.cores.in_node(core, node)) {
                Some(core) => core,
                None => self.least_loaded(critical, |cores, core| cores.in_node(core, node)),
            },
            Affinity::Any => self.least_loaded(critical, |_, _| true),
        };
        self.jobs[j] = Job { state: JobState::Queued, critical, timed: false, core: home, global: global.0 };
        if spawner == Some(home) {
            self.enqueue(home, job);
            self.refresh(home);
            self.balance(home);
        } else {
            self.place_remote(job, home);
        }
    }

    /// `job`, running, waits (on the graph); its core runs something else.
    fn suspend(&mut self, job: u32) {
        let j = job as usize;
        self.jobs[j].state = JobState::Suspended;
        let core = self.jobs[j].core;
        self.vacate(core);
        self.pick(core);
    }

    /// `job`, running, waits for the clock to reach `at`.
    fn wait_until(&mut self, job: u32, at: u64) -> Suspend {
        if at <= self.now {
            return Suspend::Ready;
        }
        self.timers.insert(at, job);
        self.jobs[job as usize].timed = true;
        self.suspend(job);
        Suspend::Waiting
    }

    /// `job`, running, ended; `woken` (local ids) wake on its core, the first in its slot.
    fn done(&mut self, job: u32, woken: &[u32]) {
        let j = job as usize;
        let core = self.jobs[j].core;
        self.jobs[j].state = JobState::Done;
        self.vacate(core);
        self.wake_local(core, woken);
        self.pick(core);
        self.balance(core);
    }

    /// `woken` wake on `core`, which finished what they waited for and goes on running.
    fn woken_on(&mut self, core: u32, woken: &[u32]) {
        self.wake_local(core, woken);
        self.balance(core);
    }

    /// A suspended job is runnable again, woken from elsewhere: on the core it last ran on.
    fn ready_remote(&mut self, job: u32) {
        self.ready(job);
        let home = self.jobs[job as usize].core;
        self.place_remote(job, home);
    }

    fn yielded(&mut self, job: u32) -> Yielded {
        let j = job as usize;
        let core = self.jobs[j].core;
        let asked = get(&self.yield_sent, core);
        set(&mut self.yield_sent, core, false);
        if !asked {
            return Yielded::Stay;
        }
        let Some(big) = self.search_parked(core, Some(self.cores.big())) else { return Yielded::Stay };
        self.vacate(core);
        self.jobs[j].state = JobState::Queued;
        self.hand(big, job);
        self.stats.moves += 1;
        self.pick(core);
        Yielded::Moved
    }

    fn set_critical(&mut self, job: u32, critical: bool) {
        let j = job as usize;
        self.jobs[j].critical = critical;
        if self.jobs[j].state == JobState::Running {
            let core = self.jobs[j].core;
            set(&mut self.critical_small, core, critical && !self.cores.is_big(core));
            if !critical {
                set(&mut self.yield_sent, core, false);
            }
            self.maybe_yield();
        }
    }

    fn core_idle(&mut self, core: CoreId) {
        let c = core.0 as usize;
        assert!(self.running[c] == NONE, "core {} idle while it runs job {}", core.0, self.running[c]);
        if self.core_state[c] == CoreState::Parked {
            self.unpark(core.0);
        }
        self.core_state[c] = CoreState::Busy;
        self.pick(core.0);
    }

    fn advance_clock(&mut self, now: u64) {
        self.now = self.now.max(now);
        let mut fired = std::mem::take(&mut self.scratch);
        fired.clear();
        self.timers.advance(self.now, |_, job| fired.push(job));
        for &job in &fired {
            self.ready_remote(job);
        }
        self.scratch = fired;
    }

    /// A suspended job is runnable again.
    fn ready(&mut self, job: u32) {
        let j = job as usize;
        debug_assert_eq!(self.jobs[j].state, JobState::Suspended);
        self.jobs[j].state = JobState::Queued;
        self.jobs[j].timed = false;
    }

    /// `core` no longer runs a job.
    fn vacate(&mut self, core: u32) {
        self.running[core as usize] = NONE;
        set(&mut self.critical_small, core, false);
        set(&mut self.yield_sent, core, false);
    }

    /// On `core`'s critical queue or its other one, by the job's class.
    fn enqueue(&mut self, core: u32, job: u32) {
        self.jobs[job as usize].core = core;
        if self.jobs[job as usize].critical {
            self.urgent[core as usize].push_back(job);
        } else {
            self.queue[core as usize].push_back(job);
        }
    }

    /// Waiters woken by what `core` finished: the first in its slot (the one there before goes
    /// to its queue), the rest on its queues.
    fn wake_local(&mut self, core: u32, woken: &[u32]) {
        let c = core as usize;
        for (at, &waiter) in woken.iter().enumerate() {
            self.ready(waiter);
            if at == 0 {
                self.jobs[waiter as usize].core = core;
                let before = std::mem::replace(&mut self.slot[c], waiter);
                if before != NONE {
                    self.enqueue(core, before);
                }
            } else {
                self.enqueue(core, waiter);
            }
        }
        self.refresh(core);
    }

    /// `job` handed to `home` from elsewhere: to the nearest parked core, else a message to
    /// `home`'s inbox.
    fn place_remote(&mut self, job: u32, home: u32) {
        let critical = self.jobs[job as usize].critical;
        match self.nearest_parked(home, critical) {
            Some(parked) => self.hand(parked, job),
            None => {
                self.inbox[home as usize].push_back(job);
                self.jobs[job as usize].core = home;
                self.refresh(home);
                self.decisions.push(Decision::Wake { core: CoreId(home), job: JobId(job) });
                self.stats.wakes += 1;
            }
        }
    }

    /// Hands `job` to `core`, parked.
    fn hand(&mut self, core: u32, job: u32) {
        self.unpark(core);
        self.core_state[core as usize] = CoreState::Woken;
        self.reserved[core as usize] = job;
        self.jobs[job as usize].core = core;
        self.decisions.push(Decision::Wake { core: CoreId(core), job: JobId(job) });
        self.stats.wakes += 1;
    }

    fn unpark(&mut self, core: u32) {
        debug_assert_eq!(self.core_state[core as usize], CoreState::Parked);
        set(&mut self.parked, core, false);
        self.parked_count -= 1;
    }

    fn park(&mut self, core: u32) {
        let c = core as usize;
        self.core_state[c] = CoreState::Parked;
        self.lifo_runs[c] = 0;
        set(&mut self.parked, core, true);
        self.parked_count += 1;
        if self.cores.is_big(core) {
            self.maybe_yield();
        }
    }

    /// While a core of the top capacity is parked, every small core running a critical job
    /// not asked yet is asked to move it.
    fn maybe_yield(&mut self) {
        if self.parked_count == 0 || !any_in(&self.parked, self.cores.big()) {
            return;
        }
        for w in 0..self.critical_small.len() {
            let mut ask = self.critical_small[w] & !self.yield_sent[w];
            while ask != 0 {
                let core = (w * 64) as u32 + ask.trailing_zeros();
                ask &= ask - 1;
                set(&mut self.yield_sent, core, true);
                let job = self.running[core as usize];
                self.decisions.push(Decision::Yield { core: CoreId(core), job: JobId(job) });
                self.stats.yields += 1;
            }
        }
    }

    fn refresh(&mut self, core: u32) {
        let c = core as usize;
        let has = self.slot[c] != NONE
            || !self.inbox[c].is_empty()
            || !self.queue[c].is_empty()
            || !self.urgent[c].is_empty();
        set(&mut self.stealable, core, has);
    }

    /// The nearest parked core to `core` for a job of this class, itself first: for a critical
    /// job the nearest of the top capacity, then any; for another, within each tier a smaller
    /// core first.
    fn nearest_parked(&self, core: u32, critical: bool) -> Option<u32> {
        if self.parked_count == 0 {
            return None;
        }
        let here = get(&self.parked, core);
        if critical {
            if here && self.cores.is_big(core) {
                return Some(core);
            }
            if let Some(big) = self.search_parked(core, Some(self.cores.big())) {
                return Some(big);
            }
            if here {
                return Some(core);
            }
            return self.search_parked(core, None);
        }
        if here {
            return Some(core);
        }
        let start = core as usize + 1;
        self.cores.tiers(core).find_map(|(_, mask)| {
            first_from(&self.parked, mask, Some(self.cores.small()), start)
                .or_else(|| first_from(&self.parked, mask, None, start))
        })
    }

    /// The nearest parked core other than `core` that `filter` allows, tier by tier.
    fn search_parked(&self, core: u32, filter: Option<&[u64]>) -> Option<u32> {
        if self.parked_count == 0 {
            return None;
        }
        let start = core as usize + 1;
        self.cores.tiers(core).find_map(|(_, mask)| first_from(&self.parked, mask, filter, start))
    }

    /// Of the cores `keep` accepts, the one with the fewest jobs (running, slot, inbox and
    /// queues), then of the job's class (the top capacity for a critical job, a smaller one
    /// for another), then the lowest id.
    fn least_loaded(&self, critical: bool, keep: impl Fn(&Cores, u32) -> bool) -> u32 {
        let mut best: Option<((usize, bool), u32)> = None;
        for core in 0..self.cores.len() as u32 {
            if !keep(&self.cores, core) {
                continue;
            }
            let c = core as usize;
            let load = match self.core_state[c] {
                CoreState::Parked => 0,
                CoreState::Woken => 1,
                CoreState::Busy => {
                    usize::from(self.running[c] != NONE)
                        + usize::from(self.slot[c] != NONE)
                        + self.inbox[c].len()
                        + self.queue[c].len()
                        + self.urgent[c].len()
                }
            };
            let rank = (load, self.cores.is_big(core) != critical);
            if best.is_none_or(|(least, _)| rank < least) {
                best = Some((rank, core));
                if rank == (0, false) {
                    break;
                }
            }
        }
        best.map(|(_, core)| core).expect("a placement hint names a group or node with no core")
    }

    /// `core` is free: it runs its next job, or steals, or parks.
    fn pick(&mut self, core: u32) {
        let c = core as usize;
        debug_assert_eq!(self.running[c], NONE);
        let job = match self.take_own(core) {
            Some(job) => Some(job),
            None => self.steal_for(core),
        };
        match job {
            Some(job) => {
                self.core_state[c] = CoreState::Busy;
                self.running[c] = job;
                self.jobs[job as usize].state = JobState::Running;
                self.jobs[job as usize].core = core;
                self.decisions.push(Decision::Run { core: CoreId(core), job: JobId(job) });
                self.stats.runs += 1;
                if self.jobs[job as usize].critical && !self.cores.is_big(core) {
                    set(&mut self.critical_small, core, true);
                    self.maybe_yield();
                }
            }
            None => self.park(core),
        }
    }

    fn take_own(&mut self, core: u32) -> Option<u32> {
        let c = core as usize;
        let reserved = std::mem::replace(&mut self.reserved[c], NONE);
        if reserved != NONE {
            return Some(reserved);
        }
        if self.slot[c] != NONE && self.lifo_runs[c] < LIFO_RUNS {
            self.lifo_runs[c] += 1;
            let job = std::mem::replace(&mut self.slot[c], NONE);
            self.refresh(core);
            return Some(job);
        }
        self.lifo_runs[c] = 0;
        let job = self.inbox[c]
            .pop_front()
            .or_else(|| self.urgent[c].pop_back())
            .or_else(|| self.queue[c].pop_back())
            .or_else(|| (self.slot[c] != NONE).then(|| std::mem::replace(&mut self.slot[c], NONE)));
        self.refresh(core);
        job
    }

    /// The nearest core with work, tier by tier; the thief runs the oldest of half its queue
    /// and keeps the rest.
    fn steal_for(&mut self, thief: u32) -> Option<u32> {
        let start = thief as usize + 1;
        let (level, victim) = self
            .cores
            .tiers(thief)
            .find_map(|(level, mask)| first_from(&self.stealable, mask, None, start).map(|victim| (level, victim)))?;
        self.stats.steals[level as usize] += 1;
        self.record_steal(thief, victim);
        Some(self.steal_half(thief, victim))
    }

    fn steal_half(&mut self, thief: u32, victim: u32) -> u32 {
        let (t, v) = (thief as usize, victim as usize);
        let big = self.cores.is_big(thief);
        let from_urgent = if big { !self.urgent[v].is_empty() } else { self.queue[v].is_empty() && !self.urgent[v].is_empty() };
        let job = if from_urgent || !self.queue[v].is_empty() {
            let queues = if from_urgent { &mut self.urgent } else { &mut self.queue };
            let take = queues[v].len().div_ceil(2);
            let first = queues[v].pop_front().expect("a victim's queue has a job");
            for _ in 1..take {
                let job = queues[v].pop_front().expect("half a queue is in it");
                queues[t].push_back(job);
                self.jobs[job as usize].core = thief;
            }
            first
        } else if let Some(job) = self.inbox[v].pop_front() {
            job
        } else {
            let job = std::mem::replace(&mut self.slot[v], NONE);
            assert!(job != NONE, "a stealable core has a job");
            job
        };
        self.refresh(victim);
        self.refresh(thief);
        job
    }

    /// The oldest job `core` holds, critical ones first: what a parked core is handed.
    fn take_oldest(&mut self, core: u32) -> u32 {
        let c = core as usize;
        let job = self.urgent[c]
            .pop_front()
            .or_else(|| self.queue[c].pop_front())
            .or_else(|| self.inbox[c].pop_front())
            .or_else(|| (self.slot[c] != NONE).then(|| std::mem::replace(&mut self.slot[c], NONE)))
            .expect("a stealable core has a job");
        self.refresh(core);
        job
    }

    /// While a core is parked, `core`'s stealable jobs go one each to the nearest parked cores.
    fn balance(&mut self, core: u32) {
        while self.parked_count > 0 && get(&self.stealable, core) {
            let job = self.take_oldest(core);
            let thief = self
                .nearest_parked(core, self.jobs[job as usize].critical)
                .expect("a core is parked");
            let level = self.cores.level(thief, core);
            self.stats.steals[level as usize] += 1;
            self.record_steal(thief, core);
            self.hand(thief, job);
        }
    }

    #[cfg(not(test))]
    fn record_steal(&mut self, _thief: u32, _victim: u32) {}

    #[cfg(test)]
    fn record_steal(&mut self, thief: u32, victim: u32) {
        let node = self.cores.info(thief).node;
        let mut on_thief_node = 0;
        for core in 0..self.cores.len() as u32 {
            if core == victim || core == thief || self.cores.info(core).node != node {
                continue;
            }
            let c = core as usize;
            on_thief_node += usize::from(self.slot[c] != NONE)
                + self.inbox[c].len()
                + self.queue[c].len()
                + self.urgent[c].len();
        }
        self.steals.push(StealRecord { thief, victim, on_thief_node });
    }

    /// Every invariant of the placement that a step must keep, checked by a full scan; a
    /// suspended job's wait is checked against the graph by [`World::audit`].
    #[cfg(test)]
    fn audit(&self) {
        let n = self.cores.len();
        let mut seen = vec![0u32; self.jobs.len()]; // alloc: test check
        let mut parked = 0;
        for c in 0..n {
            let core = c as u32;
            let held = self.slot[c] != NONE
                || !self.inbox[c].is_empty()
                || !self.queue[c].is_empty()
                || !self.urgent[c].is_empty();
            assert_eq!(get(&self.stealable, core), held, "core {c}'s stealable bit");
            assert_eq!(get(&self.parked, core), self.core_state[c] == CoreState::Parked, "core {c}'s parked bit");
            match self.core_state[c] {
                CoreState::Parked => {
                    parked += 1;
                    assert!(!held && self.running[c] == NONE && self.reserved[c] == NONE, "parked core {c} holds work");
                }
                CoreState::Woken => assert!(self.running[c] == NONE && self.reserved[c] != NONE, "woken core {c}"),
                CoreState::Busy => assert!(self.running[c] != NONE && self.reserved[c] == NONE, "busy core {c}"),
            }
            let critical_small = self.running[c] != NONE
                && self.jobs[self.running[c] as usize].critical
                && !self.cores.is_big(core);
            assert_eq!(get(&self.critical_small, core), critical_small, "core {c}'s critical bit");
            assert!(!get(&self.yield_sent, core) || critical_small, "core {c} asked to move no critical job");
            if self.running[c] != NONE {
                let job = self.running[c] as usize;
                assert_eq!(self.jobs[job].state, JobState::Running, "core {c} runs job {job}");
                assert_eq!(self.jobs[job].core, core, "job {job}'s core");
            }
            // A job's queue is its class when queued: `set_critical` on a queued job leaves it
            // there, so either queue may hold either class.
            let queued = self.inbox[c].iter().chain(&self.queue[c]).chain(&self.urgent[c]).copied();
            let queued = queued.chain([self.slot[c], self.reserved[c]].into_iter().filter(|&it| it != NONE));
            for job in queued {
                seen[job as usize] += 1;
                assert_eq!(self.jobs[job as usize].state, JobState::Queued, "job {job} held by core {c}");
                assert_eq!(self.jobs[job as usize].core, core, "job {job} held by core {c}");
            }
        }
        assert_eq!(parked, self.parked_count, "parked count");
        if parked > 0 {
            assert!(self.stealable.iter().all(|&it| it == 0), "a job is stealable while a core is parked");
        }
        for (job, record) in self.jobs.iter().enumerate() {
            let state = record.state;
            let held = seen[job];
            assert_eq!(held, u32::from(state == JobState::Queued), "job {job} ({state:?}) is held {held} times");
            assert!(!record.timed || state == JobState::Suspended, "job {job} timed while {state:?}");
        }
    }
}

/// Every driver of the process: one wait graph ([`graph::Graph`]) over all their jobs, and each
/// driver's own placement (its member, a [`Place`]). A job has a world id, unique among live
/// jobs of every driver, and a local id, its index in its member; a wait, a claim and a cycle
/// name world ids, and a member's decisions name local ones. A job woken by a job of another
/// member goes to the core it last ran on, as a wake from a timer does. Sans-IO, as [`Sched`].
///
/// A member of its own placement ([`World::join_own`]: a pool that keeps its own queues, or
/// the process's client threads) has no [`Place`]: its jobs join the graph only when they first
/// claim or wait ([`World::add_own`]), and a woken one is handed back as the token its member
/// gave it ([`World::take_woken`]), for the member to run again where it likes.
pub(crate) struct World {
    graph: Graph,
    /// By member; a driver that stopped leaves `None`, never reused, so a stale home names
    /// no other driver's job.
    members: Vec<Option<Place>>,
    /// By member of its own placement, numbered from [`OWN`].
    own: Vec<Option<Own>>,
    /// By world id: bumped at each end, so a [`fiber::Handle`] tells an old job from a new one.
    generation: Vec<u32>,
    free: Vec<u32>,
    /// Whether job ids come from [`World::alloc`], and go back to `free` as they end: a
    /// [`Sched`] names its own.
    recycles: bool,
    /// Members whose placement made decisions since the last [`World::take_dirty`].
    dirty: Vec<u32>,
    woken: Vec<u32>,
    here: Vec<u32>,
    /// The waits of the last stall ([`World::stall`]), and the jobs it woke not yet told.
    stall: Vec<(JobId, StallWait)>,
    stalled: Vec<JobId>,
}

impl World {
    pub(crate) fn new(jobs: usize) -> World {
        World {
            graph: Graph::new(jobs),
            members: Vec::with_capacity(4),
            own: Vec::with_capacity(4),
            generation: Vec::with_capacity(jobs),
            free: Vec::with_capacity(jobs),
            recycles: true,
            dirty: Vec::with_capacity(4),
            woken: Vec::with_capacity(64),
            here: Vec::with_capacity(64),
            stall: Vec::new(),
            stalled: Vec::new(),
        }
    }

    /// Every pool is idle, as its caller found under the scheduler's lock: if no driver runs
    /// and no wait in the graph can end by what is in it ([`Graph::stuck`]), every waiting job
    /// wakes, to fail with the stall ([`World::take_stall`]). How many woke.
    pub(crate) fn stall(&mut self) -> usize {
        if self.members.iter().any(Option::is_some) {
            return 0;
        }
        let mut waits = std::mem::take(&mut self.stall);
        let stuck = self.graph.stuck(&mut waits);
        self.stall = waits;
        if !stuck {
            return 0;
        }
        let mut woken = std::mem::take(&mut self.woken);
        woken.clear();
        self.graph.wake_all(&mut woken);
        // alloc: a stall, once
        self.stalled.extend(woken.iter().map(|&job| JobId(job)));
        let count = woken.len();
        self.woken = woken;
        self.split(None);
        count
    }

    /// The stall `job` woke for, if it did: told once. The job leaves the scopes it has open
    /// that no job counts towards, as it unwinds out of them; with them, its innermost scope.
    pub(crate) fn take_stall(&mut self, job: JobId) -> Option<(Stalled, Option<CounterId>)> {
        let at = self.stalled.iter().position(|&it| it == job)?;
        self.stalled.swap_remove(at);
        let scope = self.graph.leave_empty_scopes(job);
        // alloc: a stall's report, once per job it fails
        Some((Stalled { job, waits: self.stall.clone() }, scope))
    }

    /// A new member, every core parked.
    pub(crate) fn join(&mut self, cores: Cores, jobs: usize) -> u32 {
        self.members.push(Some(Place::new(cores, jobs)));
        (self.members.len() - 1) as u32
    }

    /// The member stopped; its placement goes.
    pub(crate) fn leave(&mut self, member: u32) -> Option<Place> {
        self.members.get_mut(member as usize).and_then(Option::take)
    }

    pub(crate) fn place(&self, member: u32) -> &Place {
        self.members[member as usize].as_ref().expect("a member of the world that left")
    }

    fn place_mut(&mut self, member: u32) -> &mut Place {
        self.members[member as usize].as_mut().expect("a member of the world that left")
    }

    /// A world id for a new job.
    pub(crate) fn alloc(&mut self) -> JobId {
        match self.free.pop() {
            Some(job) => JobId(job),
            None => {
                self.generation.push(0);
                JobId((self.generation.len() - 1) as u32)
            }
        }
    }

    pub(crate) fn generation(&self, job: JobId) -> u32 {
        self.generation.get(job.0 as usize).copied().unwrap_or(0)
    }

    /// The local id of a live or ended job, and its member.
    pub(crate) fn home(&self, job: JobId) -> (u32, u32) {
        let home = self.graph.home(job);
        (home.member, home.local)
    }

    /// The members whose placements made decisions, taken into `into` (cleared first).
    pub(crate) fn take_dirty(&mut self, into: &mut Vec<u32>) {
        into.clear();
        into.append(&mut self.dirty);
        into.sort_unstable();
        into.dedup();
    }

    pub(crate) fn take_decisions(&mut self, member: u32) -> std::vec::Drain<'_, Decision> {
        self.place_mut(member).take_decisions()
    }

    fn touch(&mut self, member: u32) {
        if !self.dirty.contains(&member) {
            self.dirty.push(member);
        }
    }

    /// `job` (from [`World::alloc`], or the caller's own ids where it is the one member) is
    /// new, local id `local` of `member`, placed as `how` says. A parent of another member
    /// is no spawner.
    ///
    /// # Panics
    ///
    /// Where `job` is live, its parent is not running, its counter is not live, or its hint
    /// names a core, group or node the shape does not have.
    pub(crate) fn spawn(&mut self, member: u32, local: u32, job: JobId, how: Spawn) {
        if self.generation.len() <= job.0 as usize {
            self.generation.resize(job.0 as usize + 1, 0);
        }
        let spawner = how.parent.and_then(|parent| {
            let home = self.graph.home(parent);
            let running = home.member != NONE
                && self.members.get(home.member as usize).is_some_and(|place| {
                    place.as_ref().is_some_and(|place| place.job_state(home.local) == JobState::Running)
                });
            assert!(running, "job {}'s parent {} is not running", job.0, parent.0);
            (home.member == member).then_some(home.local)
        });
        assert!(self.graph.life(job) != Life::Live, "job {} spawned while it is live", job.0);
        self.place_mut(member).spawn(local, job, spawner, how.hint, how.critical);
        self.graph.add(job, Home { member, local }, how.counter);
        self.touch(member);
    }

    pub(crate) fn counter(&mut self) -> CounterId {
        self.graph.counter()
    }

    pub(crate) fn drop_counter(&mut self, counter: CounterId) {
        self.graph.drop_counter(counter);
    }

    fn assert_running(&self, job: JobId, what: &str) -> (u32, u32) {
        let (member, local) = self.home(job);
        let running = member != NONE && self.is_running(member, local);
        assert!(running, "job {} {what} while not running", job.0);
        (member, local)
    }

    fn is_running(&self, member: u32, local: u32) -> bool {
        if member & OWN != 0 {
            return self.own.get((member & !OWN) as usize).is_some_and(|own| {
                own.as_ref().is_some_and(|own| own.state.get(local as usize) == Some(&OwnState::Running))
            });
        }
        self.members.get(member as usize).is_some_and(|place| {
            place.as_ref().is_some_and(|place| place.job_state(local) == JobState::Running)
        })
    }

    fn own_mut(&mut self, member: u32) -> &mut Own {
        assert!(member & OWN != 0, "member {member} has a placement");
        self.own[(member & !OWN) as usize].as_mut().expect("a member of the world that left")
    }

    /// A new member of its own placement ([`World`]), from [`OWN`].
    pub(crate) fn join_own(&mut self) -> u32 {
        self.own.push(Some(Own {
            state: Vec::with_capacity(64),
            token: Vec::with_capacity(64),
            free: Vec::with_capacity(64),
            woken: Vec::with_capacity(64),
        }));
        (self.own.len() - 1) as u32 | OWN
    }

    /// The member of its own placement stopped.
    ///
    /// # Panics
    ///
    /// Where one of its jobs is still in the graph.
    pub(crate) fn leave_own(&mut self, member: u32) {
        let own = self.own[(member & !OWN) as usize].take().expect("a member that left");
        let live = own.state.iter().filter(|&&it| it != OwnState::Free).count();
        assert_eq!(live, 0, "member {member} left with {live} jobs in the wait graph");
    }

    /// A running job of `member` (of its own placement) joins the graph, counted towards
    /// `counter` if given: its world id. `token` is what [`World::take_woken`] hands back when
    /// it wakes.
    pub(crate) fn add_own(&mut self, member: u32, token: usize, counter: Option<CounterId>) -> JobId {
        let own = self.own_mut(member);
        let local = match own.free.pop() {
            Some(local) => local,
            None => {
                own.state.push(OwnState::Free);
                own.token.push(0);
                (own.state.len() - 1) as u32
            }
        };
        own.state[local as usize] = OwnState::Running;
        own.token[local as usize] = token;
        let job = self.alloc();
        if self.generation.len() <= job.0 as usize {
            self.generation.resize(job.0 as usize + 1, 0);
        }
        self.graph.add(job, Home { member, local }, counter);
        job
    }

    /// What [`World::take_woken`] hands back for `job` (of a member of its own placement) from
    /// now on.
    pub(crate) fn set_token(&mut self, job: JobId, token: usize) {
        let (member, local) = self.home(job);
        self.own_mut(member).token[local as usize] = token;
    }

    /// `job` (running, not counted yet) counts towards `counter`.
    pub(crate) fn count_towards(&mut self, job: JobId, counter: CounterId) {
        self.assert_running(job, "counted");
        self.graph.count_towards(job, counter);
    }

    /// `job` (running) opens a scope that counts its jobs itself: a counter held until
    /// [`World::release`], which the job waits on before it ends ([`graph::Graph::open_scope`]).
    pub(crate) fn open_scope(&mut self, job: JobId) -> CounterId {
        self.assert_running(job, "opened a scope");
        let counter = self.graph.counter();
        self.graph.hold(counter);
        self.graph.open_scope(job, counter);
        counter
    }

    /// `job`'s innermost scope, of `counter`, is over, nothing waiting on it: the scope around
    /// it, if any.
    pub(crate) fn close_scope(&mut self, job: JobId, counter: CounterId) -> Option<CounterId> {
        self.assert_running(job, "closed a scope");
        let outer = self.graph.close_scope(job, counter);
        self.graph.drop_counter(counter);
        outer
    }

    /// Closes every scope `job` still has open, innermost first: it panicked out of them, and
    /// no wait of it is on their counters any more.
    pub(crate) fn close_scopes(&mut self, job: JobId) {
        while let Some(counter) = self.graph.innermost_scope(job) {
            self.close_scope(job, counter);
        }
    }

    /// A count of `counter` no job holds: see [`World::release`].
    pub(crate) fn hold(&mut self, counter: CounterId) {
        self.graph.hold(counter);
    }

    /// Lets go of a [`World::hold`]; the counter's waiters wake if it was the last count.
    pub(crate) fn release(&mut self, counter: CounterId) {
        let mut woken = std::mem::take(&mut self.woken);
        woken.clear();
        self.graph.release(counter, &mut woken);
        self.woken = woken;
        self.split(None);
    }

    /// The tokens of `member`'s woken jobs (of its own placement), taken into `into`.
    pub(crate) fn take_woken(&mut self, member: u32, into: &mut Vec<usize>) {
        into.clear();
        if let Some(Some(own)) = self.own.get_mut((member & !OWN) as usize) {
            into.append(&mut own.woken);
        }
    }

    /// `by` (running, or a driver for `None`: IO, a child process) makes `key`'s value.
    /// `false` when the key has a flight already.
    pub(crate) fn claim(&mut self, key: Key, by: Option<JobId>) -> bool {
        if let Some(job) = by {
            self.assert_running(job, "claimed a key");
        }
        self.graph.claim(key, by)
    }

    /// `key` is made by `job`, which is live but need not be running: a waiter claims a run's
    /// flight for its runner when it first waits for it. `false` when the key has a flight.
    pub(crate) fn claim_for(&mut self, key: Key, job: JobId, scope: Option<CounterId>) -> bool {
        assert!(self.graph.life(job) == Life::Live, "key {key:?} claimed for job {}, not live", job.0);
        self.graph.claim_in(key, job, scope)
    }

    /// `job`, running on `member`, waits for `on`; its core runs something else.
    pub(crate) fn suspended(&mut self, member: u32, job: JobId, on: Wait) -> Result<Suspend, Cycle> {
        let (home, local) = self.assert_running(job, "suspended");
        assert_eq!(home, member, "job {} suspended on a member not its own", job.0);
        if member & OWN != 0 {
            let on = match on {
                Wait::Until(_) => panic!("job {} of a member of its own placement waits for a time", job.0),
                Wait::Job(other) => On::Job(other),
                Wait::Key(key) => On::Key(key),
                Wait::Counter(counter) => On::Counter(counter),
            };
            return match self.graph.wait(job, on)? {
                Waited::Ready => Ok(Suspend::Ready),
                Waited::Waiting => {
                    self.own_mut(member).state[local as usize] = OwnState::Waiting;
                    Ok(Suspend::Waiting)
                }
            };
        }
        let on = match on {
            Wait::Until(at) => {
                let waited = self.place_mut(member).wait_until(local, at);
                self.touch(member);
                return Ok(waited);
            }
            Wait::Job(other) => On::Job(other),
            Wait::Key(key) => On::Key(key),
            Wait::Counter(counter) => On::Counter(counter),
        };
        match self.graph.wait(job, on) {
            Ok(Waited::Ready) => Ok(Suspend::Ready),
            Ok(Waited::Waiting) => {
                self.place_mut(member).suspend(local);
                self.touch(member);
                Ok(Suspend::Waiting)
            }
            Err(cycle) => {
                self.place_mut(member).stats.cycles += 1;
                Err(cycle)
            }
        }
    }

    /// The woken jobs in `self.woken` of `member` into `self.here` (local ids), the others
    /// handed to their members.
    fn split(&mut self, member: Option<u32>) {
        let mut woken = std::mem::take(&mut self.woken);
        let mut here = std::mem::take(&mut self.here);
        here.clear();
        for &job in &woken {
            let home = self.graph.home(JobId(job));
            if home.member & OWN != 0 {
                if let Some(Some(own)) = self.own.get_mut((home.member & !OWN) as usize) {
                    own.state[home.local as usize] = OwnState::Running;
                    own.woken.push(own.token[home.local as usize]);
                    self.touch(home.member);
                }
            } else if Some(home.member) == member {
                here.push(home.local);
            } else if let Some(Some(place)) = self.members.get_mut(home.member as usize) {
                place.ready_remote(home.local);
                self.touch(home.member);
            }
        }
        woken.clear();
        self.woken = woken;
        self.here = here;
    }

    /// `job`, running on `member`, ended, and with it every key it claimed. Its waiters and
    /// theirs, and those of its counter if it was the last, wake: those of its member on its
    /// core, the first in its slot.
    pub(crate) fn done(&mut self, member: u32, job: JobId) {
        let (home, local) = self.assert_running(job, "done");
        assert_eq!(home, member, "job {} done on a member not its own", job.0);
        let mut woken = std::mem::take(&mut self.woken);
        woken.clear();
        self.graph.end(job, &mut woken);
        self.woken = woken;
        if member & OWN != 0 {
            self.split(None);
            let own = self.own_mut(member);
            own.state[local as usize] = OwnState::Free;
            own.free.push(local);
            let generation = &mut self.generation[job.0 as usize];
            *generation = generation.wrapping_add(1);
            if self.recycles {
                self.free.push(job.0);
            }
            return;
        }
        self.split(Some(member));
        let here = std::mem::take(&mut self.here);
        self.place_mut(member).done(local, &here);
        self.here = here;
        self.touch(member);
        let generation = &mut self.generation[job.0 as usize];
        *generation = generation.wrapping_add(1);
        if self.recycles {
            self.free.push(job.0);
        }
    }

    /// `key`'s flight ended. Its waiters of its claimer's member wake on the claimer's core,
    /// the first in its slot; every other, and each of a flight of a driver's, on the core it
    /// last ran on.
    pub(crate) fn key_done(&mut self, key: Key) {
        let mut woken = std::mem::take(&mut self.woken);
        woken.clear();
        let claimer = self.graph.key_done(key, &mut woken);
        self.woken = woken;
        match claimer {
            Some(claimer) if self.home(claimer).0 & OWN != 0 => self.split(None),
            Some(claimer) => {
                let (member, local) = self.home(claimer);
                assert!(
                    self.place(member).job_state(local) == JobState::Running,
                    "key {key:?} done while its job {} is not running",
                    claimer.0
                );
                self.split(Some(member));
                let here = std::mem::take(&mut self.here);
                let place = self.place_mut(member);
                let core = place.jobs[local as usize].core;
                place.woken_on(core, &here);
                self.here = here;
                self.touch(member);
            }
            None => self.split(None),
        }
    }

    pub(crate) fn yielded(&mut self, member: u32, job: JobId) -> Yielded {
        let (home, local) = self.assert_running(job, "yielded");
        assert_eq!(home, member, "job {} yielded on a member not its own", job.0);
        let yielded = self.place_mut(member).yielded(local);
        self.touch(member);
        yielded
    }

    pub(crate) fn set_critical(&mut self, job: JobId, critical: bool) {
        let (member, local) = self.home(job);
        assert!(member != NONE, "job {} was never spawned", job.0);
        if let Some(Some(place)) = self.members.get_mut(member as usize) {
            place.set_critical(local, critical);
            self.touch(member);
        }
    }

    pub(crate) fn core_idle(&mut self, member: u32, core: CoreId) {
        self.place_mut(member).core_idle(core);
        self.touch(member);
    }

    pub(crate) fn advance_clock(&mut self, member: u32, now: u64) {
        self.place_mut(member).advance_clock(now);
        self.touch(member);
    }

    pub(crate) fn next_deadline(&mut self, member: u32) -> Option<u64> {
        self.place_mut(member).next_deadline()
    }

    /// Every invariant a step must keep, checked by a full scan.
    #[cfg(test)]
    pub(crate) fn audit(&self) {
        self.graph.audit();
        for place in self.members.iter().flatten() {
            place.audit();
            for job in &place.jobs {
                if job.state == JobState::Suspended {
                    assert_ne!(self.graph.waits(job.global), job.timed, "job {}'s wait", job.global);
                } else if matches!(job.state, JobState::Queued | JobState::Running) {
                    assert!(!self.graph.waits(job.global), "job {} waits while {:?}", job.global, job.state);
                }
            }
        }
    }
}

/// The scheduler of one driver alone: a [`World`] of one member, whose world ids are its local
/// ids (the caller's own).
pub struct Sched {
    world: World,
}

impl Sched {
    /// Every core parked, room for `jobs` job ids before a column grows.
    pub fn new(cores: Cores, jobs: usize) -> Sched {
        let mut world = World::new(jobs);
        world.recycles = false;
        world.join(cores, jobs);
        Sched { world }
    }

    fn place(&self) -> &Place {
        self.world.place(0)
    }

    pub fn cores(&self) -> &Cores {
        self.place().cores()
    }

    pub fn stats(&self) -> Stats {
        self.place().stats()
    }

    pub fn now(&self) -> u64 {
        self.place().now()
    }

    pub fn job_state(&self, job: JobId) -> JobState {
        self.place().job_state(job.0)
    }

    /// Where the job is held, runs, or last ran.
    pub fn job_core(&self, job: JobId) -> Option<CoreId> {
        self.place().job_core(job.0)
    }

    pub fn is_critical(&self, job: JobId) -> bool {
        self.place().is_critical(job.0)
    }

    pub fn core_state(&self, core: CoreId) -> CoreState {
        self.place().core_state(core)
    }

    pub fn running(&self, core: CoreId) -> Option<JobId> {
        self.place().running(core).map(JobId)
    }

    /// The decisions made since the last call, in the order made.
    pub fn take_decisions(&mut self) -> std::vec::Drain<'_, Decision> {
        self.world.dirty.clear();
        self.world.take_decisions(0)
    }

    /// When the next timed wait is due: the driver calls [`Sched::advance_clock`] by then.
    pub fn next_deadline(&mut self) -> Option<u64> {
        self.world.next_deadline(0)
    }

    /// `job` is new, to run where `how` says.
    ///
    /// # Panics
    ///
    /// Where `job` is live (spawned and not done), its parent is not running, its counter is
    /// not live, or its hint names a core, group or node the shape does not have.
    pub fn spawn(&mut self, job: JobId, how: Spawn) {
        self.world.spawn(0, job.0, job, how);
    }

    /// A counter at zero, for a parallel map's jobs to count towards ([`Spawn::counted`]).
    pub fn counter(&mut self) -> CounterId {
        self.world.counter()
    }

    /// The counter's owner is done with it; it is freed once no job counts towards it.
    ///
    /// # Panics
    ///
    /// Where the counter is not live or a job still waits on it.
    pub fn drop_counter(&mut self, counter: CounterId) {
        self.world.drop_counter(counter);
    }

    /// `by` (running, or the driver for `None`: IO, a child process) makes `key`'s value.
    /// `false` when the key has a flight already: wait for it instead.
    ///
    /// # Panics
    ///
    /// Where `by` is not running.
    pub fn claim(&mut self, key: Key, by: Option<JobId>) -> bool {
        self.world.claim(key, by)
    }

    /// `job`, running, waits for `on`; its core runs something else.
    ///
    /// # Errors
    ///
    /// The [`Cycle`] the wait would close; the job goes on running and nothing changed.
    ///
    /// # Panics
    ///
    /// Where `job` is not running, or waits for a job never spawned or a counter not live.
    pub fn suspended(&mut self, job: JobId, on: Wait) -> Result<Suspend, Cycle> {
        self.world.suspended(0, job, on)
    }

    /// `job`, running, ended, and with it every key it claimed. Its waiters and theirs, and
    /// those of its counter if it was the last, wake on its core, the first in its slot; the
    /// core runs its next job.
    ///
    /// # Panics
    ///
    /// Where `job` is not running.
    pub fn done(&mut self, job: JobId) {
        self.world.done(0, job);
    }

    /// `key`'s flight ended. Its waiters wake on its claimer's core, the first in its slot, or
    /// for a flight of the driver's, each on the core it last ran on.
    ///
    /// # Panics
    ///
    /// Where the key has no flight, or its claimer is not running.
    pub fn key_done(&mut self, key: Key) {
        self.world.key_done(key);
    }

    /// `job`, running, saw its core's move flag at a query miss. It moves to the nearest idle
    /// core of the highest capacity, if one is still idle and the flag is not stale (the job
    /// stopped being critical since).
    ///
    /// # Panics
    ///
    /// Where `job` is not running.
    pub fn yielded(&mut self, job: JobId) -> Yielded {
        self.world.yielded(0, job)
    }

    /// The profiler says whether `job` is on the critical path. A running job moves at its
    /// next miss if it should; a queued one keeps its queue until it next moves.
    pub fn set_critical(&mut self, job: JobId, critical: bool) {
        self.world.set_critical(job, critical);
    }

    /// `core`'s thread is free: it started, was woken, or woke for no reason. It runs what it
    /// was handed, or its own next job, or steals; with nothing, it parks.
    ///
    /// # Panics
    ///
    /// Where `core` is running a job.
    pub fn core_idle(&mut self, core: CoreId) {
        self.world.core_idle(0, core);
    }

    /// The clock reads `now` nanoseconds: every wait due by then is runnable, on the core its
    /// job last ran on.
    pub fn advance_clock(&mut self, now: u64) {
        self.world.advance_clock(0, now);
    }

    /// Every invariant a step must keep, checked by a full scan.
    #[cfg(test)]
    pub(crate) fn audit(&self) {
        self.world.audit();
    }

    #[cfg(test)]
    pub(crate) fn steals(&mut self) -> &mut Vec<StealRecord> {
        &mut self.world.place_mut(0).steals
    }
}
