//! The driver of the [`Budget`]: the one process-wide gate every commit of memory passes
//! (docs/v2/plan/design/end-state/memory.md 4.1, 4.2 and 9).
//!
//! Two owners. **Core** is a byte count under the core size the process installed: every
//! [`crate::Region`] that is not a task's asks it before the kernel commits a page
//! ([`Gate::core_reserve`]) and gives back what it decommits or purges. **Task** is what a
//! running task holds, in quanta of the frame's chunk, asked of the [`Budget`]: a task's frame
//! asks before each chunk it takes and each step its span grows ([`TaskRef::reserve`]). The
//! kernel's commit calls (`os::commit`, `os::commit_reserved`, `os::reuse`) each take a
//! [`Grant`], which only this module makes, and a region keeps from it exactly what it
//! publishes as committed: a commit past its grant stops, naming the region's tag.
//!
//! The lock and the front. The Budget is behind one lock (`state`), taken when a task starts and
//! ends, at a safe point a task was told to reach, and when the front runs dry. In front of it is
//! `front`, an atomic count of quanta the Budget lent ([`Budget::lend`]): a grant that fits is
//! one compare-and-swap on it and one add on the task's own slot, no lock. The front cannot
//! pass the ceiling: every quantum in it was counted in the Budget's `reserved` (as lent, by no
//! task) under the lock before it was put there, a grant only moves quanta out of it, and a
//! quantum leaves the Budget's count only when the lock absorbs the slot that took it. So at
//! every instant the Budget's `committed()` is at least what the tasks hold, and the Budget keeps
//! that under the ceiling. The Budget lends only while nobody waits for room (no debt, no marked
//! task, no task the room holds back), so the front never takes room an elder needs; every slow
//! call takes what is in the front back before it decides. A task's releases are an add on its
//! own slot, absorbed by the next slow call: freed room reaches the front only through the lock.
//!
//! Safe points. A refusal is never an allocation failure (section 9.1): the Budget marks the
//! task, the gate sets its slot's `told`, and the task's frame reaches [`TaskRef::safe_point`]
//! at its next chunk boundary or span step (one load while untold). There, outside any lock
//! ([`in_lock`]) and not already unwinding, it asks the Budget: it goes on, waits until its
//! overdraft is paid, unwinds with [`crate::frame::Cancelled`] (it gives way, and is posted
//! again), or unwinds with [`crate::budget::TooBig`] (its job fails, naming it). A frame step never asks more
//! than one worker's overdraft, so between two safe points a task commits at most one.
//!
//! Starts. A scheduler's task is ranked and never waits to start ([`Gate::try_start`]): held
//! back behind an elder or the room, it is parked with [`HeldBack`] and posted again when
//! [`Gate::take_unheld`] names it. Only an unranked outermost task (a tool's, a test's, a query
//! run on its caller's thread) waits in [`Gate::start`].
//!
//! Waits (an unranked start held back, a safe point told to wait) are bounded by [`WAIT_MOST`],
//! after which the waiter stops naming what it waited for (rule bounded-progress), and each is a
//! span naming what it waits for through the hooks [`set_wait_spans`] installs (rule
//! every-wait-says-what).
//!
//! The core path is callable from an allocator's commit hook: [`Gate::core_reserve`], its
//! refusal ([`MemError::CoreFull`], plain numbers formatted only where printed),
//! [`Gate::core_release`] and [`Gate::core_take`] allocate nothing and take no lock (atomics,
//! and the chunk cache's lock-free trim). The general heap's arena commits through
//! [`crate::Region::reuse`] from inside its own commit callback, so a lock or an allocation
//! there would re-enter it (`gate/tests.rs` holds both paths to zero allocations).
//!
//! Why std's `Mutex` and `Condvar`, not `blaze_core::lock` and `blaze_core::wait` (rules
//! standard-structures and deadline-waits): blaze-core depends on blaze-mem, so blaze-mem
//! cannot reach them, and the gate's lock is taken inside a frame's allocation, where
//! parking_lot's parking table would allocate. The lock is held only for the Budget's O(running
//! tasks) steps; the one wait is the `Condvar`'s, bounded by a deadline and spanned as above.

use std::{
    cell::Cell,
    fmt, mem,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    MemError, Tag,
    budget::{
        Age, Band, Budget, Config, ConfigError, Hold, Misuse, QUANTUM, Reserved, SafePoint,
        Started, TaskId,
    },
};

#[cfg_attr(
    not(blaze_check),
    expect(
        clippy::disallowed_types,
        reason = "below blaze-core: blaze-mem is a dependency of blaze-core, so no `blaze_core::lock`; the gate's lock is taken inside a frame's allocation, where parking_lot's parking table would allocate (brief-locks.md's exception)"
    )
)]
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

/// The most a start held back or a safe point told to wait waits before it stops, naming what
/// it waited for: a victim unwinds within one overdraft of work, so a wait this long is a task
/// that cannot reach its safe point (a lost wake, or a task parked on a fiber while it holds
/// room).
pub const WAIT_MOST: Duration = Duration::from_secs(30 * 60);

/// The most tasks one gate runs at once (each has a slot): far past the threads and fibers a
/// process runs tasks on.
pub const SLOTS: usize = 4096;

/// Quanta the front is lent per worker at a time: a worker's next 16 MiB of chunks without the
/// lock.
const LEND_PER_WORKER: u64 = 8;

/// The ids of tasks started without a rank ([`crate::frame::task`]): above every id a caller
/// names ([`Ranked`]).
const UNRANKED: u64 = 1 << 63;

/// A task's place, given by its scheduler and kept across the times it is posted again: what
/// lets a wounded task keep its age and be held until the elder it gave way to ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ranked {
    /// The caller's name for it: below `1 << 63`.
    pub task: TaskId,
    pub band: Band,
    pub age: Age,
}

/// Why [`install`] refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstallError {
    Config(ConfigError),
    /// The process gate already runs tasks: a new config takes effect only between them.
    TasksRunning { running: usize },
    /// Core regions already hold more than the new core size.
    CoreOver { used: u64, core: u64 },
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            InstallError::Config(why) => why.fmt(f),
            InstallError::TasksRunning { running } => write!(
                f,
                "memory budget: install with {running} tasks running (install at the process's start)"
            ),
            InstallError::CoreOver { used, core } => write!(
                f,
                "memory budget: core regions hold {used} B, more than the installed core of {core} B"
            ),
        }
    }
}

impl std::error::Error for InstallError {}

/// What a gate counts now, bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Held {
    /// What core regions hold committed.
    pub core: u64,
    /// What running tasks hold, in whole quanta, absorbed from their slots.
    pub tasks: u64,
    /// What the front holds lent and no task took.
    pub lent: u64,
}

/// A running task's row of the front: written by its own thread alone, read by the lock.
struct Slot {
    used: AtomicBool,
    task: AtomicU64,
    /// Quanta it took from the front since the lock last absorbed it.
    taken: AtomicU64,
    /// Quanta it gave back since the lock last absorbed it.
    given: AtomicU64,
    /// The Budget marked it, or it must wait: it asks at its next safe point.
    told: AtomicBool,
    /// Its safe point told it to give way: it is unwinding for that.
    gave_way: AtomicBool,
}

struct State {
    budget: Budget,
    /// Slots ended and free for the next start.
    free: Vec<u32>,
    /// Slots ever used: `0..high`.
    high: u32,
    /// Ranked tasks let start again and not yet taken ([`Gate::take_unheld`]).
    unheld: Vec<TaskId>,
}

/// Why a ranked task did not start ([`crate::frame::task_ranked`]): its scheduler parks it until
/// [`Gate::take_unheld`] names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HeldBack {
    pub task: TaskId,
    pub by: Hold,
}

impl fmt::Display for HeldBack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "memory gate: task {} is held back by {:?} until it lets go", self.task.0, self.by)
    }
}

/// What a gate's wait waits for: named by its span and by the stop past [`WAIT_MOST`].
#[derive(Clone, Copy, Debug)]
enum Waited {
    Start { task: TaskId, by: Hold },
    SafePoint { task: TaskId },
}

impl Waited {
    /// The name its span carries.
    fn name(self) -> &'static str {
        match self {
            Waited::Start { .. } => "memory gate: a start held back",
            Waited::SafePoint { .. } => "memory gate: a safe point waiting for its overdraft",
        }
    }
}

/// How a gate's wait opens and closes its span: installed by the process's lock and profiler
/// home (`blaze_core::lock::install`), which blaze-mem sits below. `begin` names what is
/// waited for and gives a token, or `None` with no profiler; `end` closes it.
#[derive(Clone, Copy, Debug)]
pub struct WaitSpans {
    pub begin: fn(&'static str) -> Option<u64>,
    pub end: fn(u64),
}

static WAIT_SPANS: OnceLock<WaitSpans> = OnceLock::new();

/// Installs the span hooks of the gate's waits, once a process: a later call keeps the
/// first's (every caller passes the same two functions).
pub fn set_wait_spans(spans: WaitSpans) {
    _ = WAIT_SPANS.set(spans);
}

impl fmt::Display for Waited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Waited::Start { task, by } => {
                write!(f, "the start of task {} held back by {by:?}", task.0)
            }
            Waited::SafePoint { task } => {
                write!(f, "task {} waiting at a safe point for its overdraft to be paid", task.0)
            }
        }
    }
}

/// The gate: a [`Budget`] behind its lock, its lock-free front, and the core count.
#[cfg_attr(
    not(blaze_check),
    expect(
        clippy::disallowed_types,
        reason = "the gate's own lock and wake (see the `use` above): std's, inside the allocator"
    )
)]
pub struct Gate {
    state: Mutex<State>,
    changed: Condvar,
    front: AtomicU64,
    /// Threads waiting on `changed`: a release then takes the lock to wake them.
    waiting: AtomicU32,
    core: AtomicU64,
    core_used: AtomicU64,
    lend_most: AtomicU64,
    /// A quantum per step of a span: a worker's overdraft.
    step: AtomicU64,
    slots: &'static [Slot],
    next_task: AtomicU64,
    next_age: AtomicU64,
    /// Whether this is the process's gate, whose core holds the chunk cache.
    process: AtomicBool,
    /// `state.unheld` is not empty: one load for a driver's poll.
    unheld_ready: AtomicBool,
    /// The oracle's control (`gate/tests.rs`): the next core commit skips the gate, on a grant
    /// it never counted.
    #[cfg(test)]
    pub(crate) skip_next: AtomicBool,
}

impl fmt::Debug for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gate")
            .field("core", &self.core.load(Ordering::Relaxed))
            .field("core_used", &self.core_used.load(Ordering::Relaxed))
            .field("front", &self.front.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

static PROCESS: OnceLock<&'static Gate> = OnceLock::new();

thread_local! {
    /// The gate this thread's tasks start under, when a test or tool scoped one
    /// ([`scoped`]); else the process's.
    static TASK_GATE: Cell<Option<&'static Gate>> = const { Cell::new(None) };
    /// Named locks the thread holds ([`lock_taken`]): no safe point while any is.
    static LOCKS: Cell<u32> = const { Cell::new(0) };
}

/// Installs the process's gate from `config`: what a process does at its start, before any
/// task runs. A process that never installs one runs under one sized from the machine
/// ([`machine_config`]). Installing again replaces the config while no task runs.
pub fn install(config: Config) -> Result<&'static Gate, InstallError> {
    if let Some(gate) = PROCESS.get() {
        gate.reconfigure(config)?;
        return Ok(gate);
    }
    let made = Gate::new(config).map_err(InstallError::Config)?;
    made.process.store(true, Ordering::Relaxed);
    match PROCESS.set(made) {
        Ok(()) => Ok(made),
        Err(_) => {
            let gate = process();
            gate.reconfigure(config)?;
            Ok(gate)
        }
    }
}

/// The process's gate: the installed one, else one sized from the machine.
pub fn process() -> &'static Gate {
    PROCESS.get_or_init(|| {
        let config = machine_config();
        match Gate::new(config) {
            Ok(gate) => {
                gate.process.store(true, Ordering::Relaxed);
                gate
            }
            Err(why) => panic!("the machine's memory budget {config:?} is refused: {why}"),
        }
    })
}

/// The config of a process that installed none, from the machine: the ceiling is 7/8 of its
/// physical memory (the rest is the kernel's and other processes'); of that, 3/8 is core (the
/// standing tables, the chunk cache, the query engine's heap until memos are records), 1/8 the
/// ungated reserve (thread stacks, C libraries' malloc zones, mimalloc's arenas and fiber stacks
/// until they pass the gate) and at most 1/8 the overdrafts, 128 MiB a worker at most; the
/// workers are twice the cores, for the IO threads and fibers that also run tasks.
pub fn machine_config() -> Config {
    config_under(crate::os::physical_memory() / 8 * 7)
}

/// The config under `ceiling` in [`machine_config`]'s proportions: what a daemon installs
/// under the ceiling it was given.
pub fn config_under(ceiling: u64) -> Config {
    let workers = std::thread::available_parallelism().map_or(2, |it| it.get()).saturating_mul(2);
    let workers = workers.clamp(2, 1024) as u32;
    let per_worker = (ceiling / 8 / u64::from(workers)).min(128 << 20);
    let overdraft_per_worker = (per_worker / QUANTUM).max(1) * QUANTUM;
    Config { ceiling, core: ceiling / 8 * 3, overdraft_per_worker, workers, ungated: ceiling / 8 }
}

/// Runs `f` with the tasks this thread starts under `gate` instead of the process's: for tests
/// and tools that set a small ceiling. A task started inside keeps its gate to its end.
pub fn scoped<R>(gate: &'static Gate, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<&'static Gate>);
    impl Drop for Restore {
        fn drop(&mut self) {
            _ = TASK_GATE.try_with(|it| it.set(self.0));
        }
    }
    let _restore = Restore(TASK_GATE.with(|it| it.replace(Some(gate))));
    f()
}

/// The gate a task this thread starts runs under.
pub(crate) fn for_tasks() -> &'static Gate {
    TASK_GATE.try_with(Cell::get).ok().flatten().unwrap_or_else(process)
}

/// The calling thread took a named lock: no safe point until it lets it go. What
/// `blaze_core::lock` calls on each acquire, and blaze-mem's own locks.
#[inline]
pub fn lock_taken() {
    _ = LOCKS.try_with(|it| it.set(it.get() + 1));
}

/// The calling thread let a lock [`lock_taken`] counted go.
#[inline]
pub fn lock_released() {
    _ = LOCKS.try_with(|it| it.set(it.get().saturating_sub(1)));
}

/// Whether the calling thread holds a lock [`lock_taken`] counted.
#[inline]
pub fn in_lock() -> bool {
    LOCKS.try_with(|it| it.get() > 0).unwrap_or(true)
}

/// Bytes a region was granted before the kernel commits them: the proof `os::commit`,
/// `os::commit_reserved` and `os::reuse` take. Made only here ([`Gate::core_reserve`],
/// [`TaskRef::reserve`]); a region keeps from it what it publishes, and a core grant gives back
/// what was not kept when it drops.
pub(crate) struct Grant {
    bytes: usize,
    core: Option<&'static Gate>,
}

impl Grant {
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// `bytes` of it are committed now and held by the region: taken off what goes back.
    #[track_caller]
    pub(crate) fn keep(&mut self, bytes: usize, tag: Tag) {
        if bytes > self.bytes {
            panic!(
                "{tag}: committing {bytes} bytes on a grant of {} the memory gate made: every \
                 commit passes the gate (blaze_mem::budget) first",
                self.bytes
            );
        }
        self.bytes -= bytes;
    }

    /// A grant of nothing: what a block holds that its task has no quanta in.
    pub(crate) fn empty() -> Grant {
        Grant { bytes: 0, core: None }
    }

    /// Adds another of its task's grants to this one (a span's unspent quanta).
    pub(crate) fn join(&mut self, mut other: Grant) {
        assert!(
            self.core.is_none() && other.core.is_none(),
            "only a task's grants are joined: a core grant is kept or given back where it was made"
        );
        self.bytes += mem::take(&mut other.bytes);
    }

    /// Pages kept from this task's grant went back to the kernel: their bytes are the grant's
    /// again, still the task's quanta.
    pub(crate) fn regain(&mut self, bytes: usize) {
        assert!(self.core.is_none(), "a core grant regains nothing: core's count fell with the pages");
        self.bytes += bytes;
    }

    /// Takes the whole quanta out of this task's grant, for its frame to give back to its task:
    /// their bytes.
    pub(crate) fn whole_quanta(&mut self) -> usize {
        assert!(self.core.is_none(), "a core grant has no quanta");
        let whole = self.bytes / QUANTUM as usize * QUANTUM as usize;
        self.bytes -= whole;
        whole
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        if let Some(gate) = self.core
            && self.bytes > 0
        {
            gate.core_release(self.bytes);
        }
    }
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grant").field("bytes", &self.bytes).finish()
    }
}

/// A running task of a gate: what its frames ask with. Copied into each frame of the task.
#[derive(Clone, Copy)]
pub(crate) struct TaskRef {
    gate: &'static Gate,
    slot: u32,
    id: TaskId,
    ranked: bool,
}

impl fmt::Debug for TaskRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskRef").field("slot", &self.slot).field("id", &self.id).finish()
    }
}

impl Gate {
    /// The core's size and what it holds now, lock-free.
    pub fn core(&self) -> (u64, u64) {
        (self.core.load(Ordering::Acquire), self.core_used.load(Ordering::Acquire))
    }

    /// A gate of its own for `config`, for the process ([`install`]) or a test ([`scoped`]);
    /// it lives as long as the process.
    #[cfg_attr(
        not(blaze_check),
        expect(
            clippy::disallowed_types,
            reason = "the gate's own lock and wake (see the `use` above): std's, inside the allocator"
        )
    )]
    pub fn new(config: Config) -> Result<&'static Gate, ConfigError> {
        let budget = Budget::new(config)?;
        // alloc: once per gate, its slots
        let slots: Vec<Slot> = (0..SLOTS)
            .map(|_| Slot {
                used: AtomicBool::new(false),
                task: AtomicU64::new(0),
                taken: AtomicU64::new(0),
                given: AtomicU64::new(0),
                told: AtomicBool::new(false),
                gave_way: AtomicBool::new(false),
            })
            .collect();
        let gate = Gate {
            // alloc: once per gate, room for the slots freed at once
            state: Mutex::new(State {
                budget,
                free: Vec::with_capacity(SLOTS),
                high: 0,
                unheld: Vec::new(),
            }),
            changed: Condvar::new(),
            front: AtomicU64::new(0),
            waiting: AtomicU32::new(0),
            core: AtomicU64::new(config.core),
            core_used: AtomicU64::new(0),
            lend_most: AtomicU64::new(LEND_PER_WORKER * u64::from(config.workers)),
            step: AtomicU64::new(config.overdraft_per_worker / QUANTUM),
            slots: Vec::leak(slots),
            next_task: AtomicU64::new(0),
            next_age: AtomicU64::new(0),
            process: AtomicBool::new(false),
            unheld_ready: AtomicBool::new(false),
            #[cfg(test)]
            skip_next: AtomicBool::new(false),
        };
        // alloc: once per gate; it lives as long as the process, as every region names it
        Ok(Box::leak(Box::new(gate)))
    }

    fn reconfigure(&self, config: Config) -> Result<(), InstallError> {
        let budget = Budget::new(config).map_err(InstallError::Config)?;
        let mut state = self.lock();
        let running = state.budget.running();
        if running > 0 {
            return Err(InstallError::TasksRunning { running });
        }
        let used = self.core_used.load(Ordering::Acquire);
        if used > config.core {
            return Err(InstallError::CoreOver { used, core: config.core });
        }
        self.front.store(0, Ordering::Release);
        state.budget = budget;
        self.core.store(config.core, Ordering::Release);
        self.lend_most.store(LEND_PER_WORKER * u64::from(config.workers), Ordering::Relaxed);
        self.step.store(config.overdraft_per_worker / QUANTUM, Ordering::Relaxed);
        Ok(())
    }

    /// What the Budget answers for: core and the ungated reserve as configured, every task's
    /// quanta and every overdraft in use. Never past the ceiling.
    pub fn committed(&self) -> u64 {
        self.lock().budget.committed()
    }

    /// The room this process needs from the machine before anything is short
    /// ([`Budget::needs`]), plus what the front may still be lent, so a task's next steps are
    /// not short before the machine's ledger is fitted again. At most the ceiling. What the
    /// daemon fits its slot of the machine's ledger to; never [`Gate::committed`], which leaves
    /// out the overdrafts the room must also cover.
    pub fn needs(&self) -> u64 {
        let mut state = self.lock();
        self.absorb_all(&mut state);
        let front = self.lend_most.load(Ordering::Relaxed).saturating_mul(QUANTUM);
        state.budget.needs().saturating_add(front).min(state.budget.ceiling())
    }

    pub fn ceiling(&self) -> u64 {
        self.lock().budget.ceiling()
    }

    /// What it counts now, with every task's slot absorbed.
    pub fn held(&self) -> Held {
        let mut state = self.lock();
        self.absorb_all(&mut state);
        let lent = state.budget.lent();
        let mut tasks = 0;
        for slot in &self.slots[..state.high as usize] {
            if slot.used.load(Ordering::Acquire) {
                tasks += state.budget.held_by(TaskId(slot.task.load(Ordering::Relaxed))).unwrap_or(0);
            }
        }
        Held { core: self.core_used.load(Ordering::Acquire), tasks: tasks * QUANTUM, lent: lent * QUANTUM }
    }

    /// The machine's word on what this process may commit ([`Budget::room`]).
    pub fn room(&self, bytes: u64) {
        let mut state = self.lock();
        self.absorb_all(&mut state);
        self.reclaim(&mut state);
        let roomed = state.budget.room(bytes);
        for victim in roomed.victims {
            self.tell(&state, victim);
        }
        self.unheld(&mut state, roomed.unheld);
        self.refill(&mut state);
        drop(state);
        self.wake();
    }

    // --- Core -----------------------------------------------------------------------------

    /// `bytes` of core for a region of `tag` to commit. Short of room, the warm chunks the
    /// process caches (the one part of core held for reuse, not use) are purged first
    /// ([`crate::chunks::trim`]: lock-free stack pops, one `madvise` a chunk and atomic counts;
    /// no lock, no allocation); still short, the commit is refused with
    /// [`MemError::CoreFull`], plain numbers. Callable from an allocator's commit hook.
    pub(crate) fn core_reserve(&'static self, tag: Tag, bytes: usize) -> Result<Grant, MemError> {
        if bytes == 0 {
            return Ok(Grant { bytes: 0, core: Some(self) });
        }
        #[cfg(test)]
        if self.skip_next.swap(false, Ordering::AcqRel) {
            // The planted bug: a grant no count stands behind.
            return Ok(Grant { bytes, core: None });
        }
        let core = self.core.load(Ordering::Acquire);
        let fits = |used: u64| used.checked_add(bytes as u64).filter(|&after| after <= core);
        if fits(self.core_used.load(Ordering::Acquire)).is_none()
            && self.process.load(Ordering::Relaxed)
        {
            crate::chunks::trim();
        }
        match self.core_used.try_update(Ordering::AcqRel, Ordering::Acquire, fits) {
            Ok(_) => Ok(Grant { bytes, core: Some(self) }),
            Err(used) => Err(MemError::CoreFull { tag, bytes, used, core }),
        }
    }

    /// Core regions gave `bytes` back to the kernel.
    pub(crate) fn core_release(&self, bytes: usize) {
        let before = self.core_used.fetch_sub(bytes as u64, Ordering::AcqRel);
        debug_assert!(before >= bytes as u64, "core gave back {bytes} B holding {before} B");
    }

    /// `bytes` committed already move into core from a task (a chunk or span a task's frame
    /// gave back to a shelf or the cache): counted whatever the room, as they were the task's
    /// until now. Whether core is within its size after: if not, the giver gives the pages
    /// to the kernel rather than keep them.
    pub(crate) fn core_take(&self, bytes: usize) -> bool {
        let after = self.core_used.fetch_add(bytes as u64, Ordering::AcqRel) + bytes as u64;
        after <= self.core.load(Ordering::Acquire)
    }

    // --- Tasks ----------------------------------------------------------------------------

    /// Starts a ranked task now, or says what holds it back without waiting: its scheduler
    /// parks it (`Yielded { held: true }`) and posts it again when [`Gate::take_unheld`] names
    /// it, so no thread or core token waits behind an elder.
    pub(crate) fn try_start(&'static self, rank: Ranked) -> Result<TaskRef, HeldBack> {
        assert!(
            rank.task.0 < UNRANKED,
            "task {}: a ranked task's id is below 1 << 63 (the ones above are unranked)",
            rank.task.0
        );
        let mut state = self.lock();
        match state.budget.start(rank.task, rank.band, rank.age) {
            Ok(Started::Running) => Ok(self.admit(&mut state, rank.task, true)),
            Ok(Started::Held { by }) => Err(HeldBack { task: rank.task, by }),
            Err(misuse) => misused(misuse),
        }
    }

    /// Starts a task with no rank, waiting (at most [`WAIT_MOST`]) while the Budget holds it
    /// behind an elder the room holds back. What an outermost [`crate::frame::task`] outside
    /// any scheduler does (tools, tests, a query run on its caller's thread); a scheduler ranks
    /// its tasks and never waits ([`Gate::try_start`]).
    pub(crate) fn start(&'static self) -> TaskRef {
        let n = self.next_task.fetch_add(1, Ordering::Relaxed);
        let age = self.next_age.fetch_add(1, Ordering::Relaxed);
        let (id, band, age) = (TaskId(UNRANKED | n), Band(1), Age(UNRANKED | age));
        let mut state = self.lock();
        let deadline = Instant::now() + WAIT_MOST;
        loop {
            match state.budget.start(id, band, age) {
                Ok(Started::Running) => return self.admit(&mut state, id, false),
                Ok(Started::Held { by }) => {
                    state = self.wait(state, deadline, Waited::Start { task: id, by });
                }
                Err(misuse) => misused(misuse),
            }
        }
    }

    /// A slot for task `id`, which the Budget just started.
    fn admit(&'static self, state: &mut State, id: TaskId, ranked: bool) -> TaskRef {
        let slot = match state.free.pop() {
            Some(slot) => slot,
            None if (state.high as usize) < SLOTS => {
                state.high += 1;
                state.high - 1
            }
            None => panic!("memory gate: {SLOTS} tasks run at once, the most a gate has slots for"),
        };
        let row = &self.slots[slot as usize];
        row.task.store(id.0, Ordering::Relaxed);
        row.taken.store(0, Ordering::Relaxed);
        row.given.store(0, Ordering::Relaxed);
        row.told.store(false, Ordering::Relaxed);
        row.gave_way.store(false, Ordering::Relaxed);
        row.used.store(true, Ordering::Release);
        self.refill(state);
        TaskRef { gate: self, slot, id, ranked }
    }

    /// Moves the ranked tasks freed room or an ended elder let start again into `into`, oldest
    /// event first: its scheduler posts each again ([`Gate::try_start`] then runs it). Whether
    /// any were waiting is one load ([`Gate::any_unheld`]).
    pub fn take_unheld(&self, into: &mut Vec<TaskId>) {
        if !self.unheld_ready.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.lock();
        into.append(&mut state.unheld);
        self.unheld_ready.store(false, Ordering::Release);
    }

    /// Whether [`Gate::take_unheld`] has any to give.
    pub fn any_unheld(&self) -> bool {
        self.unheld_ready.load(Ordering::Acquire)
    }

    /// Keeps what an event let start again, for [`Gate::take_unheld`]: the ranked ones (an
    /// unranked start waits in [`Gate::start`] and sees it there).
    fn unheld(&self, state: &mut State, unheld: Vec<TaskId>) {
        let ranked = unheld.into_iter().filter(|task| task.0 < UNRANKED);
        let before = state.unheld.len();
        state.unheld.extend(ranked); // alloc: a held task let go, once per hold
        if state.unheld.len() > before {
            self.unheld_ready.store(true, Ordering::Release);
        }
    }

    /// What is free for new starts now: the Budget's limit (the ceiling, or the machine's
    /// room under it) less what it has committed, the workers' overdrafts, and what each
    /// running task is still `forecast` to take past what it holds (bytes). One lock and
    /// O(running tasks).
    pub fn free_for_starts(&self, forecast: impl Fn(TaskId) -> u64) -> u64 {
        let mut state = self.lock();
        self.absorb_all(&mut state);
        self.reclaim(&mut state);
        let config = state.budget.config();
        let mut free = state
            .budget
            .limit()
            .saturating_sub(state.budget.committed())
            .saturating_sub(config.overdraft_per_worker.saturating_mul(u64::from(config.workers)));
        for slot in &self.slots[..state.high as usize] {
            if !slot.used.load(Ordering::Acquire) {
                continue;
            }
            let task = TaskId(slot.task.load(Ordering::Relaxed));
            let held = state.budget.held_by(task).unwrap_or(0).saturating_mul(QUANTUM);
            free = free.saturating_sub(forecast(task).saturating_sub(held));
        }
        self.refill(&mut state);
        free
    }

    /// Forgets a parked task its scheduler dropped (every node of it skipped before it ran
    /// again), so the tasks held behind it start: `Ok(false)` for an id the gate does not know
    /// (it ended as it unwound, or never started), [`Misuse::Running`] for one still running.
    pub fn end_parked(&self, task: TaskId) -> Result<bool, Misuse> {
        let mut state = self.lock();
        if state.budget.held_by(task).is_some() {
            return Err(Misuse::Running { task, call: "end_parked" });
        }
        if !state.budget.is_parked(task) {
            return Ok(false);
        }
        let ended = state.budget.end(task)?;
        self.unheld(&mut state, ended.unheld);
        self.refill(&mut state);
        drop(state);
        self.wake();
        Ok(true)
    }

    #[cfg_attr(
        not(blaze_check),
        expect(
            clippy::disallowed_types,
            reason = "the guard of the gate's own lock (see the `use` above)"
        )
    )]
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for a change, at most until `deadline`, then stops naming `what`. The wait is a
    /// span naming `what` (rule every-wait-says-what) through the hooks the process's profiler
    /// installed ([`set_wait_spans`]).
    #[cfg_attr(
        not(blaze_check),
        expect(
            clippy::disallowed_types,
            reason = "the guard of the gate's own lock (see the `use` above)"
        )
    )]
    fn wait<'a>(
        &'a self,
        state: MutexGuard<'a, State>,
        deadline: Instant,
        what: Waited,
    ) -> MutexGuard<'a, State> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            drop(state);
            panic!("memory gate: {what} for {WAIT_MOST:?}, its bound");
        }
        let spans = WAIT_SPANS.get();
        let token = spans.and_then(|it| (it.begin)(what.name()));
        self.waiting.fetch_add(1, Ordering::AcqRel);
        // wakes: every release, end, unwind, unlend and change of room notifies `changed`
        // while `waiting` is above 0, and this waiter counted itself under the lock
        let (state, _) =
            self.changed.wait_timeout(state, left).unwrap_or_else(PoisonError::into_inner);
        self.waiting.fetch_sub(1, Ordering::AcqRel);
        if let (Some(spans), Some(token)) = (spans, token) {
            (spans.end)(token);
        }
        state
    }

    fn wake(&self) {
        if self.waiting.load(Ordering::Acquire) > 0 {
            self.changed.notify_all();
        }
    }

    /// Every running task's slot into the Budget: what it took from the front, then what it
    /// gave back.
    fn absorb_all(&self, state: &mut State) {
        for slot in &self.slots[..state.high as usize] {
            if slot.used.load(Ordering::Acquire) {
                self.absorb(state, slot);
            }
        }
    }

    fn absorb(&self, state: &mut State, slot: &Slot) {
        let task = TaskId(slot.task.load(Ordering::Relaxed));
        let taken = slot.taken.swap(0, Ordering::AcqRel);
        if taken > 0
            && let Err(misuse) = state.budget.absorb(task, taken as i64)
        {
            misused(misuse);
        }
        let given = slot.given.swap(0, Ordering::AcqRel);
        if given > 0 {
            match state.budget.release(task, given) {
                Ok(freed) => self.unheld(state, freed.unheld),
                Err(misuse) => misused(misuse),
            }
        }
    }

    /// Takes back what the front holds, so the Budget decides on the room as it is.
    fn reclaim(&self, state: &mut State) {
        let front = self.front.swap(0, Ordering::AcqRel);
        if front > 0 {
            state.budget.unlend(front);
        }
    }

    /// Lends the front what the Budget allows.
    fn refill(&self, state: &mut State) {
        let want = self.lend_most.load(Ordering::Relaxed);
        let have = self.front.load(Ordering::Acquire);
        if have < want {
            let lent = state.budget.lend(want - have);
            self.front.fetch_add(lent, Ordering::AcqRel);
        }
    }

    /// Tells the running task `task` to ask at its next safe point.
    fn tell(&self, state: &State, task: TaskId) {
        for slot in &self.slots[..state.high as usize] {
            if slot.used.load(Ordering::Acquire) && slot.task.load(Ordering::Relaxed) == task.0 {
                slot.told.store(true, Ordering::Release);
                return;
            }
        }
    }
}

/// A call the driver made that the Budget refused: a bug in this driver or its frame, named.
#[cold]
#[inline(never)]
#[track_caller]
fn misused(misuse: Misuse) -> ! {
    panic!("{misuse}")
}

impl TaskRef {
    fn slot(&self) -> &'static Slot {
        &self.gate.slots[self.slot as usize]
    }

    pub(crate) fn id(&self) -> TaskId {
        self.id
    }

    /// The most quanta one reservation asks: a worker's overdraft, so a frame steps its span
    /// by at most this between safe points.
    pub(crate) fn step_quanta(&self) -> u64 {
        self.gate.step.load(Ordering::Relaxed)
    }

    /// `quanta` (at most [`TaskRef::step_quanta`]) for this task's frame, before it commits
    /// them: from the front with no lock while it has them and the task was not told; else from
    /// the Budget under the lock. Never refused: a refusal marks the task, which unwinds or
    /// waits at its next safe point.
    pub(crate) fn reserve(&self, quanta: u64) -> Grant {
        let slot = self.slot();
        if !slot.told.load(Ordering::Acquire)
            && self
                .gate
                .front
                .try_update(Ordering::AcqRel, Ordering::Acquire, |front| front.checked_sub(quanta))
                .is_ok()
        {
            slot.taken.fetch_add(quanta, Ordering::AcqRel);
            return Grant { bytes: (quanta * QUANTUM) as usize, core: None };
        }
        let gate = self.gate;
        let mut state = gate.lock();
        gate.absorb_all(&mut state);
        gate.reclaim(&mut state);
        match state.budget.reserve(self.id, quanta) {
            Ok(Reserved::Granted) => {}
            Ok(Reserved::Overdrawn { .. } | Reserved::TooBig(_)) => slot.told.store(true, Ordering::Release),
            Ok(Reserved::Wound { victims, .. }) => {
                for victim in victims {
                    gate.tell(&state, victim);
                }
                // The asker waits at its safe point until its overdraft is paid.
                slot.told.store(true, Ordering::Release);
            }
            Err(misuse) => misused(misuse),
        }
        gate.refill(&mut state);
        Grant { bytes: (quanta * QUANTUM) as usize, core: None }
    }

    /// The task's frame gave `quanta` back to the kernel or to core.
    pub(crate) fn release(&self, quanta: u64) {
        if quanta == 0 {
            return;
        }
        self.slot().given.fetch_add(quanta, Ordering::AcqRel);
        if self.gate.waiting.load(Ordering::Acquire) > 0 {
            let mut state = self.gate.lock();
            self.gate.absorb(&mut state, self.slot());
            drop(state);
            self.gate.changed.notify_all();
        }
    }

    /// A safe point of the task: its frame at a chunk boundary or a span step. One load unless
    /// it was told; then, outside any lock and not already unwinding, it does what the Budget
    /// says: goes on, waits until its overdraft is paid, or unwinds (with
    /// [`crate::frame::Cancelled`] to give way, with [`crate::budget::TooBig`] to fail).
    #[inline]
    pub(crate) fn safe_point(&self) {
        if !self.slot().told.load(Ordering::Acquire) || in_lock() || std::thread::panicking() {
            return;
        }
        self.safe_point_slow();
    }

    #[cold]
    #[inline(never)]
    fn safe_point_slow(&self) {
        let (gate, slot) = (self.gate, self.slot());
        let mut state = gate.lock();
        let deadline = Instant::now() + WAIT_MOST;
        loop {
            gate.absorb_all(&mut state);
            gate.reclaim(&mut state);
            let answer = state.budget.safe_point(self.id);
            match answer {
                Ok(SafePoint::Go) => {
                    slot.told.store(false, Ordering::Release);
                    gate.refill(&mut state);
                    return;
                }
                Ok(SafePoint::Wait { victims }) => {
                    for victim in victims {
                        gate.tell(&state, victim);
                    }
                    state = gate.wait(state, deadline, Waited::SafePoint { task: self.id });
                }
                Ok(SafePoint::GiveWay) => {
                    slot.gave_way.store(true, Ordering::Release);
                    drop(state);
                    crate::frame::cancel()
                }
                Ok(SafePoint::Fail(too_big)) => {
                    drop(state);
                    // alloc: the typed failure's payload, once per task too big
                    std::panic::resume_unwind(Box::new(too_big))
                }
                Err(misuse) => misused(misuse),
            }
        }
    }

    /// The task ended, its frames' blocks all given back: parked when it unwound to give way
    /// (and posted again by its scheduler, if ranked), else done.
    pub(crate) fn end(self) {
        let (gate, slot) = (self.gate, self.slot());
        let mut state = gate.lock();
        gate.absorb(&mut state, slot);
        let gave_way = std::thread::panicking() && slot.gave_way.load(Ordering::Acquire);
        slot.used.store(false, Ordering::Release);
        state.free.push(self.slot); // alloc: within the room made for every slot
        let done = if gave_way {
            match state.budget.wounded_at_safe_point(self.id) {
                // An unranked task is started again under a new id: its parked row goes.
                Ok(unwound) if !self.ranked => {
                    gate.unheld(&mut state, unwound.unheld);
                    state.budget.end(self.id).map(|ended| ended.unheld)
                }
                other => other.map(|unwound| unwound.unheld),
            }
        } else {
            state.budget.end(self.id).map(|ended| ended.unheld)
        };
        match done {
            Ok(unheld) => gate.unheld(&mut state, unheld),
            Err(misuse) => misused(misuse),
        }
        gate.refill(&mut state);
        drop(state);
        gate.wake();
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests;
