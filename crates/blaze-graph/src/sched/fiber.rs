//! The fibers driver of the scheduler (docs/v2/plan/lanes/sched.md, phase 2): one worker thread per
//! core of the shape, each running jobs on stackful fibers (corosensei). A job that waits switches
//! back to its worker, which tells the scheduler and runs whatever it decides next; the waiting job
//! holds no thread, and resumes, on whichever core the scheduler says, when what it waits for is
//! done.
//!
//! Every call into the scheduler is made on a worker's own stack, after the fiber has switched
//! out: a job is never made runnable (and so never resumed by another core) while it is still
//! on its old core's stack.
//!
//! Every driver of the process is a member of one [`World`] behind one lock: one wait graph
//! for all their jobs, so a cycle of waits through two drivers' jobs is refused as one of a
//! single driver is, and each driver's own placement. A job's [`JobId`] here (its [`Cx::job`],
//! [`Handle`], a [`Cycle`]'s) is its world id, unique among the live jobs of every driver. Each
//! transition is a few dozen instructions and allocates nothing warm.
//!
//! Code that has no [`Cx`] to hand (the query engine) reaches the running job through
//! [`wait`], [`miss`] and [`current_job`], which read this thread's fiber.
//!
//! A fiber moves between threads only while suspended. What its stack holds across a wait must
//! be `Send`: no thread-local borrow, no lock guard, no `Rc` (phase 2's audit). A [`NoWait`]
//! guard marks a region that must not wait; a wait inside one panics, naming it. Code that
//! reads a `thread_local!` must not span a wait inside one function: the compiler may keep the
//! thread-local's address across the call, and after the wait the job can be on another
//! thread. This module's own thread-local reads sit in `#[inline(never)]` functions for that
//! reason. A thread-local whose value a job keeps across a wait is [`Carried`] with the job:
//! taken off the thread when the job parks and put on the thread that resumes it.

use std::{
    any::Any,
    cell::Cell,
    fmt, io,
    mem::{self, ManuallyDrop},
    ops::{Deref, DerefMut},
    panic::{self, AssertUnwindSafe},
    ptr,
    sync::{
        Arc, LazyLock, Once, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle, Thread},
    time::{Duration, Instant},
};

use corosensei::{
    Coroutine, CoroutineResult, Yielder,
    stack::{DefaultStack, Stack},
};
use parking_lot::{Mutex, MutexGuard};

use super::{CoreId, CounterId, Cores, Cycle, JobId, OWN, Spawn, Stats, Suspend, Wait, World, Yielded};
use crate::Key;

/// What the driver runs: each job's payload, and how to run one.
pub trait Work: Sized + Send + Sync + 'static {
    type Job: Send + 'static;

    /// Runs `job` on a fiber; `cx` spawns, claims and waits for it.
    fn run(&self, cx: &Cx<'_, Self>, job: Self::Job);
}

/// The driver's sizes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Fibers made at start; more are made (and kept, recycled) while more jobs are started and
    /// not done at once. Admission bounds that number.
    pub fibers: usize,
    /// Bytes of each fiber's stack, reserved (committed as touched), with a guard page below.
    pub stack: usize,
    /// Spins an idle worker makes before it parks.
    pub spin: u32,
}

impl Config {
    /// Four fibers per core, 16 MiB stacks (the front end's deepest recursion runs on them),
    /// and a few microseconds of spinning.
    pub fn for_cores(cores: usize) -> Config {
        Config { fibers: cores * 4, stack: 16 << 20, spin: 1 << 10 }
    }
}

/// A spawned job, for [`Cx::wait_job`]: its id is recycled once it ends, the generation tells
/// the old job from the new one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Handle {
    pub job: JobId,
    generation: u32,
}

/// The driver could not start or go on.
#[derive(Debug)]
pub enum DriverError {
    /// A fiber's stack could not be mapped.
    Stack { bytes: usize, why: io::Error },
    /// A worker's setup (binding it to its core, its QoS class) failed.
    Setup { core: CoreId, why: io::Error },
    /// A worker thread could not be started.
    Thread { core: CoreId, why: io::Error },
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverError::Stack { bytes, why } => write!(f, "mapping a fiber stack of {bytes} bytes: {why}"),
            DriverError::Setup { core, why } => write!(f, "setting up the worker of core {}: {why}", core.0),
            DriverError::Thread { core, why } => write!(f, "starting the worker of core {}: {why}", core.0),
        }
    }
}

impl std::error::Error for DriverError {}

// dyn: a job's panic payload, carried to `Driver::join` and raised there; cold.
pub(super) type Panic = Box<dyn Any + Send>;

/// What a worker passes into a fiber.
enum In<J> {
    Start(JobId, J),
    Go(Answer),
    Exit,
}

/// What a wait or move came to.
pub(super) enum Answer {
    Ready,
    Cycle(Cycle),
    /// A carried thread-local's take or put panicked as the job switched: the job unwinds
    /// with that panic where it waited, its own failure.
    Raise(Panic),
}

/// What a fiber passes back to its worker.
enum Out {
    Wait(Want),
    Move,
    Done(Option<Panic>),
}

enum Want {
    On(Wait),
    Job(Handle),
}

/// A fiber: a coroutine that runs job after job, never ending until told to, and its number
/// ([`Switch::fiber`]).
struct Fiber<J>(Stackful<In<J>, Out>, u32);

/// A coroutine that is never dropped suspended mid-job: such a one is leaked, stack and all.
/// Dropping it would force-unwind the job, whose own `catch_unwind` swallows corosensei's
/// unwind and goes on into code that panics, and corosensei aborts the process on that panic.
pub(super) struct Stackful<I, O>(ManuallyDrop<Coroutine<I, O, (), DefaultStack>>);

impl<I, O> Stackful<I, O> {
    pub(super) fn new(coroutine: Coroutine<I, O, (), DefaultStack>) -> Stackful<I, O> {
        Stackful(ManuallyDrop::new(coroutine))
    }

    pub(super) fn resume(&mut self, input: I) -> CoroutineResult<O, ()> {
        self.0.resume(input)
    }
}

impl<I, O> Drop for Stackful<I, O> {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if !self.0.started() || self.0.done() {
            // SAFETY: dropped once, here, and never used after.
            unsafe { ManuallyDrop::drop(&mut self.0) };
        }
    }
}

/// Fibers made in the process: the next one's number.
static FIBERS: AtomicU32 = AtomicU32::new(0);

/// A new fiber's number ([`Switch::fiber`]).
pub(super) fn next_fiber() -> u32 {
    FIBERS.fetch_add(1, Ordering::Relaxed)
}

// SAFETY: a fiber is moved to another thread only while suspended (in the idle pool, or parked
// in its job's slot), never while it runs. Its stack then holds the driver's frames (an
// `Arc<Shared>`, the `Yielder` and `Cx`, which only the fiber itself touches, and a payload
// `J: Send`) and the job's frames below a wait, which phase 2's audit
// (docs/v2/plan/lanes/sched.md) holds to `Send`: no thread-local borrow, lock guard or `Rc`
// across a wait; `NoWait` guards are checked at every switch (`Cx::switch`).
#[allow(unsafe_code)]
unsafe impl<J: Send> Send for Fiber<J> {}

enum Slot<J> {
    Free,
    /// Spawned, not started: its payload.
    Fresh(J),
    /// Started; on a core now.
    Running,
    /// Started; suspended, or moved and not resumed yet, with the thread-locals it carries.
    Parked(Fiber<J>, Box<Taken>, Option<Panic>),
}

/// A driver's own: its jobs' slots, by local id, and its fibers.
struct State<W: Work> {
    slots: Vec<Slot<W::Job>>,
    free: Vec<u32>,
    /// Spawned and not done.
    live: u32,
    idle: Vec<Fiber<W::Job>>,
    /// Boxes for parked jobs' carried thread-locals, kept for the next job to park.
    #[allow(clippy::vec_box, reason = "each box moves whole into a `Slot::Parked`, which holds it boxed to stay small")]
    spare_taken: Vec<Box<Taken>>,
    closing: bool,
    joiner: Option<Thread>,
    panic: Option<Panic>,
    failed: Option<DriverError>,
}

const NONE: u32 = u32::MAX;

/// One core's word: its move flag and its wakeup.
struct CoreSlot {
    /// Set by `Decision::Yield`, cleared at each `Run`: the job there reads it at a miss.
    moving: AtomicBool,
    /// A `Wake` (or the stop) since the worker last looked.
    woken: AtomicBool,
    thread: OnceLock<Thread>,
}

impl CoreSlot {
    fn wake(&self) {
        self.woken.store(true, Ordering::Release);
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
}

/// Every driver's scheduling, under one lock for the process: the [`World`] (one wait graph,
/// each driver's placement as a member) and each member's wakers. A job of one driver woken by
/// another's is woken through them. The per-core change (docs/v2/plan/lanes/sched.md) takes
/// spawns and ends off this lock.
pub(super) struct Global {
    pub(super) world: World,
    /// By member, as the world numbers them; `None` once the driver stopped.
    io: Vec<Option<Io>>,
    /// By member of its own placement (from [`OWN`]), how its woken jobs are handed back.
    pub(super) own: Vec<Option<OwnIo>>,
    dirty: Vec<u32>,
    woken: Vec<usize>,
}

/// How a member of its own placement takes back a woken job: `wake(ctx, token)`, called under
/// the scheduler's lock, so it must not call into the scheduler.
#[derive(Clone, Copy)]
pub(super) struct OwnIo {
    pub(super) wake: fn(usize, usize),
    pub(super) ctx: usize,
    /// Of a pool: whether all its workers sleep with nothing queued (`idle(ctx)`), under the
    /// scheduler's lock. A pool's job not yet in the graph runs only on a worker or waits in
    /// a queue, so an idle pool has none.
    pub(super) idle: Option<fn(usize) -> bool>,
}

struct Io {
    cores: Arc<[CoreSlot]>,
    /// A `Run` for a core (a local job id) made by another core's worker; read by its worker.
    pending: Vec<u32>,
}

static GLOBAL: LazyLock<Mutex<Global>> = LazyLock::new(|| {
    // alloc: the process's scheduler, once
    Mutex::new(Global {
        world: World::new(1024),
        io: Vec::with_capacity(8),
        own: Vec::with_capacity(8),
        dirty: Vec::with_capacity(8),
        woken: Vec::with_capacity(64),
    })
});

#[cfg(test)]
thread_local! {
    /// How many times this thread took the scheduler's lock.
    pub(crate) static GLOBAL_TAKEN: Cell<u64> = const { Cell::new(0) };
}

/// The process's scheduler, locked: always before a driver's own lock.
pub(super) fn global() -> MutexGuard<'static, Global> {
    #[cfg(test)]
    GLOBAL_TAKEN.with(|it| it.set(it.get() + 1));
    match GLOBAL.try_lock() {
        Some(held) => held,
        None => crate::wait::wait(crate::wait::WaitOn::Lock("the scheduler"), None, || GLOBAL.lock()),
    }
}

/// Does what the scheduler decided, for every member whose jobs it changed: a member of its
/// own placement is handed its woken jobs, a driver its `Run`s, wakes and moves. The `Run` of
/// `mine` (a driver's member and core), if any, is returned (a local job id).
pub(super) fn dispatch_global(global: &mut Global, mine: Option<(u32, u32)>) -> Option<u32> {
    let mut found = None;
    let Global { world, io, own, dirty, woken } = global;
    world.take_dirty(dirty);
    for &member in dirty.iter() {
        if member & OWN != 0 {
            world.take_woken(member, woken);
            if let Some(Some(hook)) = own.get((member & !OWN) as usize) {
                for &token in woken.iter() {
                    (hook.wake)(hook.ctx, token);
                }
            }
            continue;
        }
        let Some(Some(io)) = io.get_mut(member as usize) else { continue };
        for decision in world.take_decisions(member) {
            match decision {
                super::Decision::Run { core, job } => {
                    if mine == Some((member, core.0)) {
                        found = Some(job.0);
                    } else {
                        io.pending[core.0 as usize] = job.0;
                        io.cores[core.0 as usize].wake();
                    }
                }
                super::Decision::Wake { core, .. } => io.cores[core.0 as usize].wake(),
                super::Decision::Yield { core, .. } => {
                    io.cores[core.0 as usize].moving.store(true, Ordering::Relaxed);
                }
            }
        }
    }
    found
}

/// The process's [`Global`] and a driver's [`State`], locked in that order, always, so no two
/// locks are ever taken the other way round.
struct Locked<'a, W: Work> {
    global: MutexGuard<'static, Global>,
    state: MutexGuard<'a, State<W>>,
    member: u32,
}

impl<W: Work> Locked<'_, W> {
    fn world(&mut self) -> &mut World {
        &mut self.global.world
    }

    /// This member's pending `Run` of `core`, taken.
    fn take_pending(&mut self, core: u32) -> Option<u32> {
        let io = self.global.io[self.member as usize].as_mut().expect("a running driver's wakers");
        let job = mem::replace(&mut io.pending[core as usize], NONE);
        (job != NONE).then_some(job)
    }
}

impl<W: Work> Deref for Locked<'_, W> {
    type Target = State<W>;

    fn deref(&self) -> &State<W> {
        &self.state
    }
}

impl<W: Work> DerefMut for Locked<'_, W> {
    fn deref_mut(&mut self) -> &mut State<W> {
        &mut self.state
    }
}

struct Shared<W: Work> {
    work: W,
    /// This driver's number in the [`World`].
    member: u32,
    state: Mutex<State<W>>,
    cores: Arc<[CoreSlot]>,
    config: Config,
    start: Instant,
    go: AtomicBool,
    stop: AtomicBool,
}

impl<W: Work> Shared<W> {
    fn lock(&self) -> Locked<'_, W> {
        let global = global();
        let state = self.state.lock();
        Locked { global, state, member: self.member }
    }

    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Does what the scheduler decided, for every driver whose placement it changed; the
    /// `Run` of this driver's core `me`, if any, is returned (a local job id).
    fn dispatch(&self, st: &mut Locked<'_, W>, me: Option<u32>) -> Option<u32> {
        dispatch_global(&mut st.global, me.map(|me| (self.member, me)))
    }

    fn fail(&self, st: &mut State<W>, error: DriverError) {
        st.failed.get_or_insert(error);
        self.stop.store(true, Ordering::Release);
        for core in &self.cores[..] {
            core.wake();
        }
        if let Some(joiner) = &st.joiner {
            joiner.unpark();
        }
    }

    fn spawn_locked(&self, st: &mut Locked<'_, W>, job: W::Job, how: Spawn) -> Handle {
        let local = match st.free.pop() {
            Some(local) => local,
            None => {
                st.slots.push(Slot::Free);
                (st.slots.len() - 1) as u32
            }
        };
        st.slots[local as usize] = Slot::Fresh(job);
        st.live += 1;
        let world = st.world();
        let global = world.alloc();
        world.spawn(self.member, local, global, how);
        let generation = world.generation(global);
        let _ = self.dispatch(st, None);
        Handle { job: global, generation }
    }
}

fn new_fiber<W: Work>(shared: &Arc<Shared<W>>) -> Result<Fiber<W::Job>, DriverError> {
    let bytes = shared.config.stack;
    let stack = DefaultStack::new(bytes).map_err(|why| DriverError::Stack { bytes, why })?;
    // The stack has at least `bytes` above its guard page, which `Stack::limit` counts in.
    let limit = stack.base().get() - bytes;
    let shared = Arc::clone(shared); // clone: one per fiber made, held for its life
    let number = FIBERS.fetch_add(1, Ordering::Relaxed);
    Ok(Fiber(Stackful::new(Coroutine::with_stack(stack, move |yielder, first| body(&shared, limit, yielder, first))), number))
}

/// A fiber's life: run each job it is started with, report its end, and wait for the next.
/// `limit` is the lowest usable address of its stack, for [`stdx::ensure_sufficient_stack`].
fn body<W: Work>(
    shared: &Shared<W>,
    limit: usize,
    yielder: &Yielder<In<W::Job>, Out>,
    first: In<W::Job>,
) {
    let mut next = first;
    loop {
        let (job, payload) = match next {
            In::Start(job, payload) => (job, payload),
            In::Exit => return,
            In::Go(_) => unreachable!("an idle fiber is only started or ended"),
        };
        let cx = Cx { shared, job, yielder };
        enter(&cx);
        stdx::set_fiber_stack_limit(limit);
        let ran = panic::catch_unwind(AssertUnwindSafe(|| shared.work.run(&cx, payload)));
        stdx::set_fiber_stack_limit(0);
        next = yielder.suspend(Out::Done(ran.err()));
    }
}

/// A running job's handle on the driver.
pub struct Cx<'a, W: Work> {
    shared: &'a Shared<W>,
    job: JobId,
    yielder: &'a Yielder<In<W::Job>, Out>,
}

impl<W: Work> Cx<'_, W> {
    pub fn job(&self) -> JobId {
        self.job
    }

    pub fn work(&self) -> &W {
        &self.shared.work
    }

    /// The driver's clock, in nanoseconds since it started: what [`Wait::Until`] counts in.
    pub fn now(&self) -> u64 {
        self.shared.now()
    }

    /// A new job of this one's, placed as `how` says (its parent is this job, whatever `how`
    /// names).
    pub fn spawn(&self, job: W::Job, mut how: Spawn) -> Handle {
        how.parent = Some(self.job);
        let mut st = self.shared.lock();
        self.shared.spawn_locked(&mut st, job, how)
    }

    /// A counter for a parallel map's jobs ([`Spawn::counted`]).
    pub fn counter(&self) -> CounterId {
        self.shared.lock().world().counter()
    }

    pub fn drop_counter(&self, counter: CounterId) {
        self.shared.lock().world().drop_counter(counter);
    }

    /// Runs every job of `jobs` as a child counted towards one counter, and waits for them all
    /// with one suspend.
    ///
    /// # Errors
    ///
    /// The cycle the wait would close; the children run on regardless.
    pub fn map(&self, jobs: impl IntoIterator<Item = W::Job>, how: Spawn) -> Result<(), Cycle> {
        let counter = self.counter();
        for job in jobs {
            self.spawn(job, how.counted(counter));
        }
        let waited = self.wait(Wait::Counter(counter));
        self.drop_counter(counter);
        waited
    }

    /// This job makes `key`: `false` when another flight has it already (wait for it).
    pub fn claim(&self, key: Key) -> bool {
        self.shared.lock().world().claim(key, Some(self.job))
    }

    /// This job's flight of `key` ended: its waiters wake on this core.
    pub fn key_done(&self, key: Key) {
        let mut st = self.shared.lock();
        st.world().key_done(key);
        let _ = self.shared.dispatch(&mut st, None);
    }

    /// Whether this job is on the critical path, by the profiler.
    pub fn set_critical(&self, critical: bool) {
        let mut st = self.shared.lock();
        st.world().set_critical(self.job, critical);
        let _ = self.shared.dispatch(&mut st, None);
    }

    /// Waits for `on`, holding no thread: the fiber switches out and resumes when it is done.
    ///
    /// # Errors
    ///
    /// The cycle the wait would close; nothing waited.
    ///
    /// # Panics
    ///
    /// For `Wait::Job` (use [`Cx::wait_job`]: a bare id may be recycled), or inside a
    /// [`NoWait`].
    pub fn wait(&self, on: Wait) -> Result<(), Cycle> {
        assert!(!matches!(on, Wait::Job(_)), "job {} waits for a bare job id: wait for its `Handle`", self.job.0);
        self.switch(Out::Wait(Want::On(on)))
    }

    /// Waits for a job spawned before, holding no thread.
    ///
    /// # Errors
    ///
    /// The cycle the wait would close; nothing waited.
    pub fn wait_job(&self, job: Handle) -> Result<(), Cycle> {
        self.switch(Out::Wait(Want::Job(job)))
    }

    /// A query miss: if the scheduler asked this core to give up its job, the job moves to a
    /// faster idle core now. One relaxed load when it did not.
    pub fn miss(&self) {
        miss();
    }

    fn relocate(&self) {
        let _ = self.switch(Out::Move);
    }

    #[inline(never)]
    fn switch(&self, out: Out) -> Result<(), Cycle> {
        check_no_wait(self.job);
        let here = HERE.get();
        assert!(
            here.is_some_and(|here| ptr::eq(here.cx, ptr::from_ref(self).cast())),
            "job {}'s `Cx` waited off its own fiber (on job {:?}'s)",
            self.job.0,
            here.map(|here| here.job.0)
        );
        let back = self.yielder.suspend(out);
        enter(self);
        match back {
            In::Go(Answer::Ready) => Ok(()),
            In::Go(Answer::Cycle(cycle)) => Err(cycle),
            In::Go(Answer::Raise(panic)) => panic::resume_unwind(panic),
            In::Start(..) | In::Exit => unreachable!("a suspended job is only resumed"),
        }
    }
}

// SAFETY: the one part of a `Cx` that is not `Sync` is its `Yielder`, used only by `switch`,
// which first asserts that this thread runs the fiber of this very `Cx` (`HERE` holds its
// address, set by `enter` on that fiber and cleared as it switches out): another thread
// holding a `&Cx` panics there instead of touching the yielder. The rest goes through
// `Shared`, which is `Sync`.
#[allow(unsafe_code)]
unsafe impl<W: Work> Sync for Cx<'_, W> {}

/// This thread's running job, as the code without a [`Cx`] reaches it.
#[derive(Clone, Copy)]
pub(super) struct Here {
    pub(super) cx: *const (),
    /// [`UNREGISTERED`] for a job of a pool's until it first joins the wait graph.
    pub(super) job: JobId,
    pub(super) register: unsafe fn(*const ()) -> JobId,
    pub(super) wait: unsafe fn(*const (), Wait) -> Result<(), Cycle>,
    pub(super) relocate: unsafe fn(*const ()),
    pub(super) claim: unsafe fn(*const (), Key) -> bool,
    pub(super) key_done: unsafe fn(*const (), Key),
    pub(super) count: unsafe fn(*const (), CounterId),
}

/// The job id of a [`Here`] whose job is in no graph yet.
pub(super) const UNREGISTERED: JobId = JobId(u32::MAX);

thread_local! {
    pub(super) static HERE: Cell<Option<Here>> = const { Cell::new(None) };
    /// This worker's core's move flag; null off a worker.
    static FLAG: Cell<*const AtomicBool> = const { Cell::new(ptr::null()) };
    /// Open [`NoWait`] guards on this thread.
    static HELD: Cell<u32> = const { Cell::new(0) };
}

#[allow(unsafe_code)]
unsafe fn wait_thunk<W: Work>(cx: *const (), on: Wait) -> Result<(), Cycle> {
    // SAFETY: `cx` is the `Cx<W>` `enter` stored, on the running fiber's stack; it is read
    // only by code running on that fiber, while it is on this thread (`clear` runs as soon as
    // the fiber switches out).
    let cx = unsafe { &*cx.cast::<Cx<'_, W>>() };
    cx.wait(on)
}

#[allow(unsafe_code)]
unsafe fn register_thunk<W: Work>(cx: *const ()) -> JobId {
    // SAFETY: as in `wait_thunk`.
    unsafe { &*cx.cast::<Cx<'_, W>>() }.job
}

#[allow(unsafe_code)]
unsafe fn count_thunk<W: Work>(cx: *const (), counter: CounterId) {
    // SAFETY: as in `wait_thunk`.
    let cx = unsafe { &*cx.cast::<Cx<'_, W>>() };
    cx.shared.lock().world().count_towards(cx.job, counter);
}

#[allow(unsafe_code)]
unsafe fn relocate_thunk<W: Work>(cx: *const ()) {
    // SAFETY: as in `wait_thunk`.
    let cx = unsafe { &*cx.cast::<Cx<'_, W>>() };
    cx.relocate();
}

#[allow(unsafe_code)]
unsafe fn claim_thunk<W: Work>(cx: *const (), key: Key) -> bool {
    // SAFETY: as in `wait_thunk`.
    let cx = unsafe { &*cx.cast::<Cx<'_, W>>() };
    cx.claim(key)
}

#[allow(unsafe_code)]
unsafe fn key_done_thunk<W: Work>(cx: *const (), key: Key) {
    // SAFETY: as in `wait_thunk`.
    let cx = unsafe { &*cx.cast::<Cx<'_, W>>() };
    cx.key_done(key);
}

/// The fiber `cx` runs is on this thread now.
#[inline(never)]
fn enter<W: Work>(cx: &Cx<'_, W>) {
    let here = Here {
        cx: ptr::from_ref(cx).cast(),
        job: cx.job,
        register: register_thunk::<W>,
        wait: wait_thunk::<W>,
        relocate: relocate_thunk::<W>,
        claim: claim_thunk::<W>,
        key_done: key_done_thunk::<W>,
        count: count_thunk::<W>,
    };
    HERE.set(Some(here));
}

/// No fiber is on this thread now.
#[inline(never)]
pub(super) fn clear() {
    HERE.set(None);
}

#[inline(never)]
pub(super) fn check_no_wait(job: JobId) {
    let held = HELD.get();
    assert!(held == 0, "job {} waited inside {held} `NoWait` region(s): a lock or thread-local borrow would cross threads", job.0);
}

/// The job running on this thread's fiber, put in the wait graph if it is not yet (a pool's
/// job joins it at its first claim or wait); `None` off a fiber.
#[inline(never)]
#[allow(unsafe_code)]
pub fn current_job() -> Option<JobId> {
    let here = HERE.get()?;
    if here.job != UNREGISTERED {
        return Some(here.job);
    }
    // SAFETY: `here` was stored by the fiber running this code, on this thread.
    Some(unsafe { (here.register)(here.cx) })
}

/// Whether this thread runs a job on a fiber now.
#[inline(never)]
pub fn on_fiber() -> bool {
    HERE.get().is_some()
}

/// A query miss on this thread's fiber: when the scheduler asked its core to give up the job,
/// the job moves now. One relaxed load otherwise; nothing off a fiber.
#[inline(never)]
#[allow(unsafe_code)]
pub fn miss() {
    let flag = FLAG.get();
    if flag.is_null() {
        return;
    }
    // SAFETY: set once by this thread's worker to its core's flag in `Shared`, which the worker
    // holds an `Arc` of for as long as it runs; this thread is that worker.
    if !unsafe { &*flag }.load(Ordering::Relaxed) {
        return;
    }
    if let Some(here) = HERE.get() {
        // SAFETY: as in `wait`.
        unsafe { (here.relocate)(here.cx) };
    }
}

/// A thread-local that a job's code keeps across a wait (the query engine's active frames):
/// when the job parks (it waits, or moves, and its core runs something else), `take` moves its
/// value off the thread, leaving the thread's empty, and `put` moves it onto whichever thread
/// resumes the job. A wait that comes back at once moves nothing. Both run on the worker's
/// stack under the driver's lock: they must not call into the driver. Registered once, with
/// [`carry`], before any driver runs.
#[derive(Clone, Copy)]
pub struct Carried {
    pub take: fn() -> usize,
    pub put: fn(usize),
}

/// The most thread-locals carried with a job.
pub const MAX_CARRIED: usize = 32;

static CARRIED: [OnceLock<Carried>; MAX_CARRIED] = [const { OnceLock::new() }; MAX_CARRIED];
static CARRIED_LEN: AtomicUsize = AtomicUsize::new(0);

/// The fiber stack limit `stdx::ensure_sufficient_stack` checks against, carried with the job
/// (each job's limit is set at its start, in `body`), and the job's reclamation task
/// (`blaze_reclaim`: a pool's job is one), which keeps what the job read valid while it waits,
/// its task frame (`blaze_mem::frame`, which allocates into the job's own frame wherever it
/// resumes), its innermost open scope ([`super::innermost_scope`]) and its give-ways
/// (`crate::pump::gave_way`), which a loop compares across an item that may resume elsewhere.
pub(super) fn carry_stack_limit() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        carry(Carried { take: blaze_reclaim::park, put: blaze_reclaim::unpark });
        carry(Carried { take: super::host::take_scope, put: super::host::put_scope });
        carry(Carried { take: blaze_mem::frame::take_current, put: blaze_mem::frame::put_current });
        carry(Carried { take: blaze_mem::frame::take_below, put: blaze_mem::frame::put_below });
        carry(Carried { take: blaze_mem::frame::take_cancelling, put: blaze_mem::frame::put_cancelling });
        carry(Carried { take: crate::pump::take_gave_ways, put: crate::pump::put_gave_ways });
        carry(Carried {
            take: || {
                let limit = stdx::fiber_stack_limit();
                stdx::set_fiber_stack_limit(0);
                limit
            },
            put: stdx::set_fiber_stack_limit,
        });
    });
}

/// Carries a thread-local with each job across its waits.
///
/// # Panics
///
/// Past [`MAX_CARRIED`].
pub fn carry(carried: Carried) {
    let at = CARRIED_LEN.fetch_add(1, Ordering::AcqRel);
    assert!(at < MAX_CARRIED, "more than {MAX_CARRIED} thread-locals carried with jobs");
    let _ = CARRIED[at].set(carried);
}

/// What a switching job takes off its thread: each carried thread-local's value, `None` for
/// one not registered yet when it switched out (so it is not put back either).
pub(super) type Taken = [Option<usize>; MAX_CARRIED];

/// Takes each carried thread-local off this thread into `taken`, leaving each at its default:
/// every one, even past one whose take panics, so none is left behind; the first panic.
#[inline(never)]
fn take_carried(taken: &mut Taken) -> Option<Panic> {
    *taken = [None; MAX_CARRIED];
    let len = CARRIED_LEN.load(Ordering::Acquire).min(MAX_CARRIED);
    let mut failed = None;
    for (slot, carried) in taken.iter_mut().zip(&CARRIED[..len]) {
        if let Some(carried) = carried.get() {
            match panic::catch_unwind(carried.take) {
                Ok(value) => *slot = Some(value),
                Err(panic) => _ = failed.get_or_insert(panic),
            }
        }
    }
    failed
}

/// Puts what [`take_carried`] took onto this thread, whose carried thread-locals are each at
/// their default: every one, even past one whose put panics; the first panic.
#[inline(never)]
fn put_carried(taken: &Taken) -> Option<Panic> {
    let mut failed = None;
    for (value, carried) in taken.iter().zip(&CARRIED) {
        if let (Some(value), Some(carried)) = (value, carried.get()) {
            let value = *value;
            if let Err(panic) = panic::catch_unwind(move || (carried.put)(value)) {
                failed.get_or_insert(panic);
            }
        }
    }
    failed
}

// A job's carried thread-locals are swapped with its worker's, never put over them: as a job
// comes on a thread (starts or resumes) what the thread holds is set aside in `worker`, and as
// it goes off (parks or ends) the thread gets that back. So each job runs with its own values
// only, whatever another job or the worker itself left on the thread.

/// A job comes on this thread: what it holds is taken into `worker`, and the job's (`job`, of
/// a job that resumes; none for one that starts) put in its place. What the job is resumed
/// with: the first panic of the swap or `failed` (of its take as it parked), for it to raise.
#[inline(never)]
pub(super) fn come_on(worker: &mut Taken, job: Option<&Taken>, failed: Option<Panic>) -> Answer {
    let mut failed = failed;
    if let Some(panic) = take_carried(worker) {
        failed.get_or_insert(panic);
    }
    if let Some(panic) = job.and_then(put_carried) {
        failed.get_or_insert(panic);
    }
    failed.map_or(Answer::Ready, Answer::Raise)
}

/// A job parks: its values are taken into `job`, and the thread gets `worker` back. The first
/// panic, for the job to raise when it resumes.
#[inline(never)]
pub(super) fn go_off(job: &mut Taken, worker: &Taken) -> Option<Panic> {
    let mut failed = take_carried(job);
    if let Some(panic) = put_carried(worker) {
        failed.get_or_insert(panic);
    }
    failed
}

/// A job ended: the thread gets `worker` back. A value the job left set stays on the thread
/// where the worker held none, as the worker's own, which no job sees (each comes on with the
/// worker's set aside); where the worker held one too, the job's is leaked. The first panic.
#[inline(never)]
pub(super) fn go_off_ended(worker: &Taken) -> Option<Panic> {
    let mut left = [None; MAX_CARRIED];
    let mut failed = take_carried(&mut left);
    for (left, worker) in left.iter_mut().zip(worker) {
        if let Some(held) = *worker
            && held != 0
        {
            *left = Some(held);
        }
    }
    if let Some(panic) = put_carried(&left) {
        failed.get_or_insert(panic);
    }
    failed
}

/// A job's switch on its fiber, as the observer ([`observe`]) is told it: on the thread it
/// happens on, under the driver's lock, so the observer must not call into the driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Switch {
    pub event: SwitchEvent,
    pub job: JobId,
    /// The job's generation: with `job`, one job of the process's life ([`Handle`]).
    pub generation: u32,
    pub fiber: u32,
    pub core: CoreId,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SwitchEvent {
    /// The job's first run, on a fiber from the idle pool.
    Started,
    /// The job parked: waiting for `Some(on)`, or `None` moving to another core. Told before
    /// its carried thread-locals are taken, so they are still the job's.
    Parked(Option<Wait>),
    /// A parked job runs again, on this thread; told after its thread-locals are put back.
    Resumed,
    /// The job ended; its fiber goes back to the idle pool.
    Ended,
}

static OBSERVER: OnceLock<fn(Switch)> = OnceLock::new();

/// Has `observer` told of every job's switches (the profiler's). Once a process; a second is
/// refused.
pub fn observe(observer: fn(Switch)) -> Result<(), fn(Switch)> {
    OBSERVER.set(observer)
}

/// Whether an observer is told of switches: only then is a switch worth the lock.
pub(super) fn observing() -> bool {
    OBSERVER.get().is_some()
}

#[inline]
pub(super) fn switched(world: &World, event: SwitchEvent, job: JobId, fiber: u32, me: u32) {
    if let Some(observer) = OBSERVER.get() {
        let generation = world.generation(job);
        observer(Switch { event, job, generation, fiber, core: CoreId(me) });
    }
}

/// A worker going idle or waking, as the idle observer ([`observe_idle`]) is told it: on the
/// worker's thread, with no lock held. With the switches, it puts each worker's every moment in
/// one state: running a job, idle (with why), or the driver's own work between them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Idle {
    pub core: CoreId,
    pub reason: IdleReason,
    /// `true` as the worker goes idle, `false` as it wakes.
    pub began: bool,
}

/// Why a worker is idle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdleReason {
    /// Its core has nothing to run.
    Nothing,
    /// Nothing to run before the timer of a job waiting until this time (the driver's clock).
    Until(u64),
}

static IDLE_OBSERVER: OnceLock<fn(Idle)> = OnceLock::new();

/// Has `observer` told of every worker's idle stretches (the profiler's). Once a process; a
/// second is refused.
pub fn observe_idle(observer: fn(Idle)) -> Result<(), fn(Idle)> {
    IDLE_OBSERVER.set(observer)
}

#[inline]
fn idled(core: u32, reason: IdleReason, began: bool) {
    if let Some(observer) = IDLE_OBSERVER.get() {
        observer(Idle { core: CoreId(core), reason, began });
    }
}

/// A region that must not wait (a lock held, a thread-local borrowed): a wait inside it
/// panics, naming the job, instead of moving the region to another thread.
pub struct NoWait(());

impl NoWait {
    #[inline(never)]
    pub fn new() -> NoWait {
        HELD.set(HELD.get() + 1);
        NoWait(())
    }
}

impl Default for NoWait {
    fn default() -> NoWait {
        NoWait::new()
    }
}

impl Drop for NoWait {
    #[inline(never)]
    fn drop(&mut self) {
        HELD.set(HELD.get() - 1);
    }
}

/// The worker threads and the scheduler they drive.
pub struct Driver<W: Work> {
    shared: Arc<Shared<W>>,
    threads: Vec<JoinHandle<()>>,
}

impl<W: Work> Driver<W> {
    /// One worker per core of `cores`, each set up by `setup` (bound to its core, given its
    /// QoS class) before any job runs.
    ///
    /// # Errors
    ///
    /// A fiber stack that could not be mapped, a thread that could not start, or a failed
    /// setup; every worker started is stopped first.
    pub fn start(
        cores: Cores,
        work: W,
        config: Config,
        setup: impl Fn(CoreId) -> io::Result<()> + Send + Sync + 'static,
    ) -> Result<Driver<W>, DriverError> {
        carry_stack_limit();
        let n = cores.len();
        let slots: Arc<[CoreSlot]> = (0..n)
            .map(|_| CoreSlot { moving: AtomicBool::new(false), woken: AtomicBool::new(false), thread: OnceLock::new() })
            .collect(); // alloc: setup, once per driver
        let mut pending = Vec::with_capacity(n);
        pending.resize(n, NONE);
        let jobs = config.fibers.max(64);
        let member = {
            let mut global = global();
            let member = global.world.join(cores, jobs);
            assert_eq!(member as usize, global.io.len(), "the world's members and their wakers");
            global.io.push(Some(Io { cores: Arc::clone(&slots), pending }));
            member
        };
        let state = State {
            slots: Vec::with_capacity(jobs),
            free: Vec::with_capacity(jobs),
            live: 0,
            idle: Vec::with_capacity(config.fibers),
            spare_taken: Vec::with_capacity(config.fibers),
            closing: false,
            joiner: None,
            panic: None,
            failed: None,
        };
        // alloc: the driver's shared state, once per driver
        let shared = Arc::new(Shared {
            work,
            member,
            state: Mutex::new(state),
            cores: slots,
            config,
            start: Instant::now(),
            go: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let mut fibers = Vec::with_capacity(config.fibers);
        for _ in 0..config.fibers {
            match new_fiber(&shared) {
                Ok(fiber) => fibers.push(fiber),
                Err(error) => {
                    let mut st = shared.lock();
                    st.world().leave(member);
                    st.global.io[member as usize] = None;
                    return Err(error);
                }
            }
        }
        shared.lock().idle = fibers;
        let setup = Arc::new(setup); // alloc: once per driver
        let (sent, ready) = crossbeam_channel::bounded(n);
        let mut threads = Vec::with_capacity(n);
        let mut failed = None;
        for core in 0..n as u32 {
            let shared = Arc::clone(&shared); // clone: one per worker, held for its life
            let setup = Arc::clone(&setup); // clone: one per worker, at start
            let sent = sent.clone();
            // alloc: a worker's name, once per worker
            let started = thread::Builder::new().name(format!("blaze-fiber-{core}")).spawn(move || {
                let set = (*setup)(CoreId(core)).map_err(|why| DriverError::Setup { core: CoreId(core), why });
                let ok = set.is_ok();
                let _ = sent.send(set);
                drop(sent);
                if ok {
                    worker(&shared, core);
                }
            });
            match started {
                Ok(handle) => threads.push(handle),
                Err(why) => {
                    failed = Some(DriverError::Thread { core: CoreId(core), why });
                    break;
                }
            }
        }
        drop(sent);
        for _ in 0..threads.len() {
            match ready.recv() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    failed.get_or_insert(error);
                }
                // A setup panicked; `shutdown` raises its panic.
                Err(_) => {
                    failed.get_or_insert_with(|| DriverError::Setup {
                        core: CoreId(NONE),
                        why: io::Error::other("a worker's setup panicked"),
                    });
                    break;
                }
            }
        }
        let driver = Driver { shared, threads };
        if let Some(error) = failed {
            driver.shared.stop.store(true, Ordering::Release);
            driver.release();
            let _ = driver.shutdown();
            return Err(error);
        }
        driver.release();
        Ok(driver)
    }

    /// Lets every worker past its setup, each core's thread known first so no wake is lost.
    fn release(&self) {
        for (core, handle) in self.threads.iter().enumerate() {
            let _ = self.shared.cores[core].thread.set(handle.thread().clone()); // clone: a thread handle, once per worker
        }
        self.shared.go.store(true, Ordering::Release);
        for handle in &self.threads {
            handle.thread().unpark();
        }
    }

    /// A job of the driver's (no parent).
    ///
    /// # Panics
    ///
    /// Where `how` names a parent: a job's children are spawned through its [`Cx`].
    pub fn spawn(&self, job: W::Job, how: Spawn) -> Handle {
        assert!(how.parent.is_none(), "the driver spawns jobs with no parent");
        let mut st = self.shared.lock();
        self.shared.spawn_locked(&mut st, job, how)
    }

    /// A flight of the driver's (IO, a child process): `false` when the key has one already.
    pub fn claim(&self, key: Key) -> bool {
        self.shared.lock().world().claim(key, None)
    }

    /// The driver's flight of `key` ended: its waiters wake, each on the core it last ran on.
    pub fn key_done(&self, key: Key) {
        let mut st = self.shared.lock();
        st.world().key_done(key);
        let _ = self.shared.dispatch(&mut st, None);
    }

    /// Waits for every job to end, then stops the workers.
    ///
    /// # Errors
    ///
    /// The driver stopped early: a fiber stack could not be mapped.
    ///
    /// # Panics
    ///
    /// Raises the first panic of any job, once every job has ended.
    pub fn join(self) -> Result<Stats, DriverError> {
        {
            let mut st = self.shared.lock();
            st.closing = true;
            while st.live > 0 && st.failed.is_none() {
                st.joiner = Some(thread::current());
                drop(st);
                thread::park();
                st = self.shared.lock();
            }
            st.joiner = None;
        }
        self.shared.stop.store(true, Ordering::Release);
        let (stats, failed, panicked) = self.shutdown();
        if let Some(panicked) = panicked {
            panic::resume_unwind(panicked);
        }
        match failed {
            Some(error) => Err(error),
            None => Ok(stats),
        }
    }

    /// Stops and joins every worker and ends every idle fiber.
    fn shutdown(mut self) -> (Stats, Option<DriverError>, Option<Panic>) {
        for core in &self.shared.cores[..] {
            core.wake();
        }
        for handle in self.threads.drain(..) {
            if let Err(panicked) = handle.join() {
                panic::resume_unwind(panicked);
            }
        }
        let mut st = self.shared.lock();
        for mut fiber in st.idle.drain(..) {
            match fiber.0.resume(In::Exit) {
                CoroutineResult::Return(()) => {}
                CoroutineResult::Yield(_) => unreachable!("an idle fiber told to exit ends"),
            }
        }
        // Fibers of jobs that never ended (the driver failed): their stacks hold frames that
        // may hold locks, so they are not unwound; dropped, they stay mapped until the process
        // ends (`Stackful`).
        for slot in &mut st.slots {
            *slot = Slot::Free;
        }
        let member = self.shared.member;
        let stats = st.world().leave(member).map(|place| place.stats()).unwrap_or_default();
        st.global.io[member as usize] = None;
        (stats, st.failed.take(), st.panic.take())
    }
}

/// A worker: runs what the scheduler gives its core, parks when it gives nothing.
fn worker<W: Work>(shared: &Arc<Shared<W>>, me: u32) {
    while !shared.go.load(Ordering::Acquire) {
        thread::park();
    }
    if shared.stop.load(Ordering::Acquire) {
        return;
    }
    let slot = &shared.cores[me as usize];
    FLAG.set(ptr::from_ref(&slot.moving));
    let member = shared.member;
    let mut st = shared.lock();
    st.world().core_idle(member, CoreId(me));
    let mut next = shared.dispatch(&mut st, Some(me));
    loop {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let job = match next.take() {
            Some(job) => Some(job),
            None => st.take_pending(me),
        };
        if let Some(job) = job {
            let (back, then) = run(shared, st, me, job);
            st = back;
            next = then;
            continue;
        }
        let deadline = st.world().next_deadline(member);
        drop(st);
        let reason = deadline.map_or(IdleReason::Nothing, IdleReason::Until);
        idled(me, reason, true);
        idle(shared, slot, deadline);
        idled(me, reason, false);
        st = shared.lock();
        if deadline.is_some() {
            st.world().advance_clock(member, shared.now());
        }
        if st.world().place(member).running(CoreId(me)).is_none() {
            st.world().core_idle(member, CoreId(me));
        }
        next = shared.dispatch(&mut st, Some(me));
    }
    drop(st);
    FLAG.set(ptr::null());
}

/// Spins, then parks, until the core is woken, the stop is set or `deadline` passes.
fn idle<W: Work>(shared: &Shared<W>, slot: &CoreSlot, deadline: Option<u64>) {
    let (mut spins, mut spin) = (0, crate::spin::SpinWait::new());
    while !slot.woken.load(Ordering::Acquire) && !shared.stop.load(Ordering::Acquire) {
        if spins < shared.config.spin && spin.pause() {
            spins += 1;
            continue;
        }
        match deadline {
            Some(at) => {
                let now = shared.now();
                if now >= at {
                    break;
                }
                thread::park_timeout(Duration::from_nanos(at - now));
            }
            None => thread::park(),
        }
    }
    slot.woken.store(false, Ordering::Release);
}

/// Takes the carried thread-locals of the job that just switched out on this thread, as it
/// parks, and gives the thread `worker`'s back ([`go_off`]): before the job is visible as
/// parked, so no core resumes it first. Only a job that really waits or moves pays for it; a
/// wait that comes back at once leaves them where they are.
fn park_carried<W: Work, J>(st: &mut State<W>, fiber: Fiber<J>, worker: &Taken) -> Slot<J> {
    // alloc: a box once per job parked at once, then kept in `spare_taken`.
    let mut taken = st.spare_taken.pop().unwrap_or_else(|| Box::new([None; MAX_CARRIED]));
    let failed = go_off(&mut taken, worker);
    Slot::Parked(fiber, taken, failed)
}

/// Runs (or resumes) the job of local id `local` on core `me` until it waits, moves or ends;
/// returns the lock and the core's next `Run`.
fn run<'a, W: Work>(
    shared: &'a Arc<Shared<W>>,
    mut st: Locked<'a, W>,
    me: u32,
    local: u32,
) -> (Locked<'a, W>, Option<u32>) {
    let member = shared.member;
    let j = local as usize;
    let job = st.world().place(member).global(local);
    let mut worker = [None; MAX_CARRIED];
    let (mut fiber, mut input) = match mem::replace(&mut st.slots[j], Slot::Running) {
        Slot::Fresh(payload) => {
            let fiber = match st.idle.pop() {
                Some(fiber) => fiber,
                None => match new_fiber(shared) {
                    Ok(fiber) => fiber,
                    Err(error) => {
                        st.slots[j] = Slot::Free;
                        shared.fail(&mut st, error);
                        return (st, None);
                    }
                },
            };
            if let Answer::Raise(panic) = come_on(&mut worker, None, None) {
                st.panic.get_or_insert(panic);
            }
            switched(&st.global.world, SwitchEvent::Started, job, fiber.1, me);
            (fiber, In::Start(job, payload))
        }
        Slot::Parked(fiber, taken, failed) => {
            let answer = come_on(&mut worker, Some(&taken), failed);
            st.spare_taken.push(taken);
            switched(&st.global.world, SwitchEvent::Resumed, job, fiber.1, me);
            (fiber, In::Go(answer))
        }
        Slot::Running => panic!("job {} run on core {me} while it runs", job.0),
        Slot::Free => panic!("job {} run on core {me}, never spawned", job.0),
    };
    drop(st);
    shared.cores[me as usize].moving.store(false, Ordering::Relaxed);
    loop {
        let out = match fiber.0.resume(input) {
            CoroutineResult::Yield(out) => out,
            CoroutineResult::Return(()) => unreachable!("a fiber ends only when told to exit"),
        };
        clear();
        let mut st = shared.lock();
        input = match out {
            Out::Wait(want) => {
                let on = match want {
                    Want::On(on) => Some(on),
                    Want::Job(handle) => {
                        (st.world().generation(handle.job) == handle.generation).then_some(Wait::Job(handle.job))
                    }
                };
                if let Some(Wait::Until(_)) = on {
                    st.world().advance_clock(member, shared.now());
                }
                match on.map(|on| st.world().suspended(member, job, on)) {
                    None | Some(Ok(Suspend::Ready)) => In::Go(Answer::Ready),
                    Some(Err(cycle)) => In::Go(Answer::Cycle(cycle)),
                    Some(Ok(Suspend::Waiting)) => {
                        switched(&st.global.world, SwitchEvent::Parked(on), job, fiber.1, me);
                        st.slots[j] = park_carried(&mut st, fiber, &worker);
                        let next = shared.dispatch(&mut st, Some(me));
                        return (st, next);
                    }
                }
            }
            Out::Move => match st.world().yielded(member, job) {
                Yielded::Stay => In::Go(Answer::Ready),
                Yielded::Moved => {
                    switched(&st.global.world, SwitchEvent::Parked(None), job, fiber.1, me);
                    st.slots[j] = park_carried(&mut st, fiber, &worker);
                    let next = shared.dispatch(&mut st, Some(me));
                    return (st, next);
                }
            },
            Out::Done(panicked) => {
                let left = go_off_ended(&worker);
                switched(&st.global.world, SwitchEvent::Ended, job, fiber.1, me);
                st.world().done(member, job);
                st.slots[j] = Slot::Free;
                st.free.push(local);
                st.live -= 1;
                st.idle.push(fiber);
                if let Some(panicked) = panicked.or(left) {
                    st.panic.get_or_insert(panicked);
                }
                if st.live == 0
                    && st.closing
                    && let Some(joiner) = &st.joiner
                {
                    joiner.unpark();
                }
                let next = shared.dispatch(&mut st, Some(me));
                return (st, next);
            }
        };
        drop(st);
    }
}
