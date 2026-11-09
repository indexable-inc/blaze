//! The fibers driver of the scheduler (docs/v2/plan/design/fibers.md, phase 2): one worker thread per
//! core of the shape, each running jobs on stackful fibers ([`super::stack`]). A job that waits switches
//! back to its worker, which tells the scheduler and runs whatever it decides next; the waiting job
//! holds no thread, and resumes, on whichever core the scheduler says, when what it waits for is
//! done.
//!
//! Every call into the scheduler is made on a worker's own stack, after the fiber has switched
//! out: a job is never made runnable (and so never resumed by another core) while it is still
//! on its old core's stack.
//!
//! Each driver holds a scheduler of its own ([`Scheduler`]), its placement a member of its
//! [`World`]; a wait of one driver's job on another's, or on a pool's, joins their schedulers
//! into one, so a cycle of waits through two drivers' jobs is refused as one of a single driver
//! is. A job's [`JobId`] here (its [`Cx::job`], [`Handle`], a [`Cycle`]'s) is its process id,
//! unique among the live jobs of every driver and pool. Each transition is a few dozen
//! instructions and allocates nothing warm.
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
    fmt, io, mem,
    num::NonZeroUsize,
    ops::{Deref, DerefMut},
    panic::{self, AssertUnwindSafe},
    ptr,
    sync::{
        Arc, Once,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle, Thread},
    time::{Duration, Instant},
};

use blaze_core::lock::LockGuard;

use super::{
    CoreId, Cores, CounterId, Cycle, JobId, OWN, Spawn, Stats, Suspend, Wait, World, Yielded,
    domain::{Domain, Guard, Scheduler, lock_joined, wait_cell},
    stack::{FIBER_STACK, Resumed, Stack, Stackful, Yielder},
};
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
    /// Four fibers per core, stacks of the target's [`FIBER_STACK`] (the front end's deepest
    /// recursion runs on them), and a few microseconds of spinning.
    pub fn for_cores(cores: usize) -> Config {
        Config { fibers: cores * 4, stack: FIBER_STACK, spin: 1 << 10 }
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
            DriverError::Stack { bytes, why } => {
                write!(f, "mapping a fiber stack of {bytes} bytes: {why}")
            }
            DriverError::Setup { core, why } => {
                write!(f, "setting up the worker of core {}: {why}", core.0)
            }
            DriverError::Thread { core, why } => {
                write!(f, "starting the worker of core {}: {why}", core.0)
            }
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
/// ([`Switch::fiber`]). Never dropped suspended mid-job ([`Stackful`]).
struct Fiber<J>(Stackful<In<J>, Out>, u32);

/// Fibers made in the process: the next one's number.
static FIBERS: AtomicU32 = AtomicU32::new(0);

/// A new fiber's number ([`Switch::fiber`]).
pub(super) fn next_fiber() -> u32 {
    FIBERS.fetch_add(1, Ordering::Relaxed)
}

#[allow(unsafe_code)]
// SAFETY: a fiber is moved to another thread only while suspended (in the idle pool, or parked
// in its job's slot), never while it runs. Its stack then holds the driver's frames (an
// `Arc<Shared>`, the `Yielder` and `Cx`, which only the fiber itself touches, and a payload
// `J: Send`) and the job's frames below a wait, which phase 2's audit
// (docs/v2/plan/design/fibers.md) holds to `Send`: no thread-local borrow, lock guard or `Rc`
// across a wait; `NoWait` guards are checked at every switch (`Cx::switch`).
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
    #[allow(
        clippy::vec_box,
        reason = "each box moves whole into a `Slot::Parked`, which holds it boxed to stay small"
    )]
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
    #[expect(
        clippy::disallowed_types,
        reason = "no global state: its worker's handle, set once as it is spawned, read with no lock by each wake"
    )]
    thread: std::sync::OnceLock<Thread>,
}

impl CoreSlot {
    fn wake(&self) {
        self.woken.store(true, Ordering::Release);
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
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

/// A fiber driver's wakers, in its scheduler's [`Domain`].
pub(super) struct Io {
    cores: Arc<[CoreSlot]>,
    /// A `Run` for a core (a local job id) made by another core's worker; read by its worker.
    pending: Vec<u32>,
}

/// Does what the scheduler decided, for every member whose jobs it changed: a member of its
/// own placement is handed its woken jobs, a driver its `Run`s, wakes and moves. The `Run` of
/// `mine` (a driver's member and core), if any, is returned (a local job id).
pub(super) fn dispatch(domain: &mut Domain, mine: Option<(u32, u32)>) -> Option<u32> {
    let mut found = None;
    let Domain { world, io, own, dirty, woken, .. } = domain;
    world.take_dirty(dirty);
    for &member in dirty.iter() {
        if member & OWN != 0 {
            world.take_woken(member, woken);
            if let Some((_, hook)) = own.iter().find(|it| it.0 == member) {
                for &token in woken.iter() {
                    (hook.wake)(hook.ctx, token);
                }
            }
            continue;
        }
        let Some((_, io)) = io.iter_mut().find(|it| it.0 == member) else { continue };
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

/// A driver's scheduler and its [`State`], locked in that order, always, so no two locks are
/// ever taken the other way round.
struct Locked<'a, W: Work> {
    global: Guard<'a>,
    state: LockGuard<'a, State<W>>,
    member: u32,
}

impl<W: Work> Locked<'_, W> {
    fn world(&mut self) -> &mut World {
        &mut self.global.world
    }

    /// This member's pending `Run` of `core`, taken.
    fn take_pending(&mut self, core: u32) -> Option<u32> {
        let member = self.member;
        let (_, io) =
            self.global.io.iter_mut().find(|it| it.0 == member).expect("a running driver's wakers");
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
    /// Its scheduler: its own, or the one it joined.
    sched: Scheduler,
    /// This driver's number in the [`World`].
    member: u32,
    #[expect(
        clippy::disallowed_types,
        reason = "a driver's own state, taken after its scheduler's lock, never across a call into work"
    )]
    state: blaze_core::lock::Lock<State<W>>,
    cores: Arc<[CoreSlot]>,
    config: Config,
    start: Instant,
    go: AtomicBool,
    stop: AtomicBool,
    /// Set by [`Driver::fail_new_fibers`]: the next fiber made fails as an unmapped stack
    /// would, so a test reaches the failed-driver path.
    #[cfg(test)]
    fail_fibers: AtomicBool,
}

impl<W: Work> Shared<W> {
    fn lock(&self) -> Locked<'_, W> {
        let global = self.sched.lock();
        let state = self.state.lock();
        Locked { global, state, member: self.member }
    }

    /// As [`Shared::lock`], the scheduler joined first with the one that has what `on` waits
    /// for, if another has it.
    fn lock_for(&self, on: Option<Wait>) -> Locked<'_, W> {
        let global = lock_joined(self.sched.cell(), || wait_cell(on?));
        let state = self.state.lock();
        Locked { global, state, member: self.member }
    }

    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Does what the scheduler decided, for every driver whose placement it changed; the
    /// `Run` of this driver's core `me`, if any, is returned (a local job id).
    fn dispatch(&self, st: &mut Locked<'_, W>, me: Option<u32>) -> Option<u32> {
        dispatch(&mut st.global, me.map(|me| (self.member, me)))
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
    #[cfg(test)]
    if shared.fail_fibers.load(Ordering::Acquire) {
        return Err(DriverError::Stack {
            bytes,
            why: io::Error::other("a test failed the driver's next fiber"),
        });
    }
    let stack = Stack::new(bytes).map_err(|why| DriverError::Stack { bytes, why })?;
    let limit = stack.limit();
    let shared = Arc::clone(shared); // clone: one per fiber made, held for its life
    let number = FIBERS.fetch_add(1, Ordering::Relaxed);
    Ok(Fiber(
        Stackful::new(stack, number, move |yielder, first| body(&shared, limit, yielder, first)),
        number,
    ))
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
        self.shared.lock().global.claim(key, Some(self.job))
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
        assert!(
            !matches!(on, Wait::Job(_)),
            "job {} waits for a bare job id: wait for its `Handle`",
            self.job.0
        );
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

#[allow(unsafe_code)]
// SAFETY: the one part of a `Cx` that is not `Sync` is its `Yielder`, used only by `switch`,
// which first asserts that this thread runs the fiber of this very `Cx` (`HERE` holds its
// address, set by `enter` on that fiber and cleared as it switches out): another thread
// holding a `&Cx` panics there instead of touching the yielder. The rest goes through
// `Shared`, which is `Sync`.
unsafe impl<W: Work> Sync for Cx<'_, W> {}

/// This thread's running job, as the code without a [`Cx`] reaches it.
#[derive(Clone, Copy)]
pub(super) struct Here {
    pub(super) cx: *const (),
    /// The scheduler of the job's pool or driver, which outlives the job's run on its fiber.
    pub(super) sched: *const Scheduler,
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
    cx.shared.lock_for(Some(Wait::Counter(counter))).world().count_towards(cx.job, counter);
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
        sched: &raw const cx.shared.sched,
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
    assert!(
        held == 0,
        "job {} waited inside {held} `NoWait` region(s): a lock or thread-local borrow would cross threads",
        job.0
    );
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
/// resumes the job. A wait that comes back at once moves nothing. `take` and `put` run on the
/// worker's stack, under the fiber driver's lock; `end` runs as a job ends (on its worker,
/// under that lock too, or outside the global lock on a host's thread) or, for a failed
/// driver's parked jobs, on the thread that joins it, after its lock is let go. None of them
/// may call into the driver. Registered once, with [`carry`], before any driver runs.
///
/// A value `take` gave is owned by exactly one place until it is put back or ended: a parked
/// job's [`Taken`], or the thread it was put on. Nothing drops one silently: as a job ends, each
/// value it still holds is ended through `end`, and the thread gets back exactly what it held
/// before the job came on ([`go_off_ended`]); a parked job's record dropped without resuming
/// ends its values too ([`Taken`]'s `Drop`). So a carried value cannot leak by construction:
/// one whose `take` gives a value for an empty state (a box made for nothing, as the query
/// stack's once did, 5.2 GiB building blaze's tree with one worker) costs a box per switch,
/// freed by its `end`, never one per job lost.
#[derive(Clone, Copy)]
pub struct Carried {
    /// Moves this thread's value off it, leaving the default; `None` when the thread holds the
    /// default, so there is nothing to carry and nothing moves.
    pub take: fn() -> Option<NonZeroUsize>,
    /// Puts a value `take` gave onto this thread, which holds the default.
    pub put: fn(NonZeroUsize),
    /// Ends a value `take` gave that no thread takes back: its job ended holding it, or its
    /// parked record was dropped. Frees what the value owns (a box, a slot); a word that owns
    /// nothing ([`Carried::word`]) is forgotten.
    pub end: fn(NonZeroUsize),
}

impl Carried {
    /// A carried word that owns nothing (a count, a flag, an id, a borrowed address): ending it
    /// is forgetting it.
    pub const fn word(take: fn() -> Option<NonZeroUsize>, put: fn(NonZeroUsize)) -> Carried {
        Carried { take, put, end: forget }
    }
}

/// The `end` of a [`Carried::word`].
fn forget(_: NonZeroUsize) {}

/// The most thread-locals carried with a job.
pub const MAX_CARRIED: usize = 32;

#[expect(
    clippy::disallowed_types,
    reason = "the process's carried thread-locals: a job carries every one registered, on any thread"
)]
static CARRIED: [std::sync::OnceLock<Carried>; MAX_CARRIED] =
    [const { std::sync::OnceLock::new() }; MAX_CARRIED];
static CARRIED_LEN: AtomicUsize = AtomicUsize::new(0);

/// The fiber stack limit `stdx::ensure_sufficient_stack` checks against, carried with the job
/// (each job's limit is set at its start, in `body`), and the job's reclamation task
/// (`blaze_reclaim`: a pool's job is one), which keeps what the job read valid while it waits,
/// its task frame (`blaze_mem::frame`, which allocates into the job's own frame wherever it
/// resumes) and the most its frames held since its innermost frame measure began, the memory
/// account its global-heap bytes are charged to (`crate::arena::account`, the job's task's, not
/// the thread's: a peak read on whatever thread a job ended on was once a daemon's whole heap
/// drifted between threads, 121.59 GB for one build script, nftest2), its
/// innermost open scope ([`super::innermost_scope`]), its give-ways
/// (`crate::pump::gave_way`), which a loop compares across an item that may resume elsewhere,
/// and whether it has asked since it last gave way ([`super::before_claim`]).
pub(super) fn carry_stack_limit() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        carry(Carried {
            take: blaze_reclaim::park,
            put: blaze_reclaim::unpark,
            end: blaze_reclaim::end_parked,
        });
        carry(Carried::word(super::host::take_scope, super::host::put_scope));
        carry(Carried::word(blaze_mem::frame::take_current, blaze_mem::frame::put_current));
        carry(Carried::word(blaze_mem::frame::take_below, blaze_mem::frame::put_below));
        carry(Carried::word(blaze_mem::frame::take_peak, blaze_mem::frame::put_peak));
        carry(Carried::word(crate::arena::take_account, crate::arena::put_account));
        carry(Carried::word(blaze_mem::frame::take_cancelling, blaze_mem::frame::put_cancelling));
        carry(Carried::word(crate::pump::take_gave_ways, crate::pump::put_gave_ways));
        carry(Carried::word(super::host::take_retry, super::host::put_retry));
        carry(Carried::word(
            || {
                let limit = stdx::fiber_stack_limit();
                stdx::set_fiber_stack_limit(0);
                NonZeroUsize::new(limit)
            },
            |limit| stdx::set_fiber_stack_limit(limit.get()),
        ));
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

/// What a switching job took off its thread: each carried thread-local's value, `None` where
/// the thread held the default (or the thread-local was not registered yet). It owns what it
/// holds until [`put_carried`] moves it onto a thread or [`end_carried`] ends it; dropped
/// holding any, it ends them, so no path loses one.
pub(super) struct Taken([Option<NonZeroUsize>; MAX_CARRIED]);

impl Taken {
    pub(super) const EMPTY: Taken = Taken([None; MAX_CARRIED]);

    fn is_empty(&self) -> bool {
        self.0.iter().all(Option::is_none)
    }
}

impl Drop for Taken {
    /// A parked job dropped without resuming (its driver or pool gone first): its values are
    /// ended, as at a job's end. A panic of an `end` goes on unwinding unless the thread
    /// already does, which keeps its first panic.
    fn drop(&mut self) {
        if let Some(panic) = end_carried(self)
            && !thread::panicking()
        {
            panic::resume_unwind(panic);
        }
    }
}

/// Takes each carried thread-local off this thread into `taken`, which holds nothing, leaving
/// each at its default: every one, even past one whose take panics, so none is left behind;
/// the first panic.
///
/// # Panics
///
/// Where `taken` holds a value: writing over it would lose it.
#[inline(never)]
fn take_carried(taken: &mut Taken) -> Option<Panic> {
    assert!(taken.is_empty(), "carried thread-locals taken into a record that holds another job's");
    let len = CARRIED_LEN.load(Ordering::Acquire).min(MAX_CARRIED);
    let mut failed = None;
    for (slot, carried) in taken.0.iter_mut().zip(&CARRIED[..len]) {
        if let Some(carried) = carried.get() {
            match panic::catch_unwind(carried.take) {
                Ok(value) => *slot = value,
                Err(panic) => _ = failed.get_or_insert(panic),
            }
        }
    }
    failed
}

/// Moves what [`take_carried`] took onto this thread, whose carried thread-locals are each at
/// their default, leaving `taken` empty: every one, even past one whose put panics; the first
/// panic. A value whose put panicked is the put's: it was handed over.
#[inline(never)]
fn put_carried(taken: &mut Taken) -> Option<Panic> {
    let mut failed = None;
    for (value, carried) in taken.0.iter_mut().zip(&CARRIED) {
        if let Some(value) = value.take() {
            let carried =
                carried.get().expect("a carried value of a thread-local that was never registered");
            if let Err(panic) = panic::catch_unwind(move || (carried.put)(value)) {
                failed.get_or_insert(panic);
            }
        }
    }
    failed
}

/// Ends each value `taken` holds through its thread-local's `end`, leaving `taken` empty:
/// every one, even past one whose end panics; the first panic.
#[inline(never)]
fn end_carried(taken: &mut Taken) -> Option<Panic> {
    let mut failed = None;
    for (value, carried) in taken.0.iter_mut().zip(&CARRIED) {
        if let Some(value) = value.take() {
            let carried =
                carried.get().expect("a carried value of a thread-local that was never registered");
            if let Err(panic) = panic::catch_unwind(move || (carried.end)(value)) {
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

/// A job comes on this thread: what it holds is taken into `worker` (empty), and the job's
/// (`job`, of a job that resumes, left empty; none for one that starts) put in its place. What
/// the job is resumed with: the first panic of the swap or `failed` (of its take as it parked),
/// for it to raise.
#[inline(never)]
pub(super) fn come_on(
    worker: &mut Taken,
    job: Option<&mut Taken>,
    failed: Option<Panic>,
) -> Answer {
    let mut failed = failed;
    if let Some(panic) = take_carried(worker) {
        failed.get_or_insert(panic);
    }
    if let Some(panic) = job.and_then(put_carried) {
        failed.get_or_insert(panic);
    }
    failed.map_or(Answer::Ready, Answer::Raise)
}

/// A job parks: its values are taken into `job` (empty), and the thread gets `worker` back,
/// which is left empty. The first panic, for the job to raise when it resumes.
#[inline(never)]
pub(super) fn go_off(job: &mut Taken, worker: &mut Taken) -> Option<Panic> {
    let mut failed = take_carried(job);
    if let Some(panic) = put_carried(worker) {
        failed.get_or_insert(panic);
    }
    failed
}

/// A job ended: each value it still holds is ended through its `end`, and the thread gets
/// exactly `worker`'s back (left empty). No value of the job outlives it, and none is dropped
/// unended. The first panic.
#[inline(never)]
pub(super) fn go_off_ended(worker: &mut Taken) -> Option<Panic> {
    let mut left = Taken::EMPTY;
    let mut failed = take_carried(&mut left);
    if let Some(panic) = end_carried(&mut left) {
        failed.get_or_insert(panic);
    }
    if let Some(panic) = put_carried(worker) {
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
    /// For a park on a job ([`Wait::Job`]): that job's generation, so the wait names one job of
    /// the process's life. 0 otherwise.
    pub awaited: u32,
    pub fiber: u32,
    pub core: CoreId,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SwitchEvent {
    /// A pool job begins on its fiber, before anything joins it to the graph: its `job` is
    /// [`UNREGISTERED`] and its `fiber` 0. `given` is what [`observe_spawner`]'s reader said
    /// as the job was given to the pool. Told with no lock held.
    Began { given: Given },
    /// The job joined the graph (a pool job at its first claim or wait, after its `Began`), or
    /// its first run on the fiber driver.
    Started,
    /// The job parked: waiting for `Some(on)`, or `None` moving to another core. Told before
    /// its carried thread-locals are taken, so they are still the job's.
    Parked(Option<Wait>),
    /// A parked job was made runnable: told on the waker's thread, under the scheduler's lock,
    /// before it is queued. `core` is the waker's.
    Ready,
    /// A parked job runs again, on this thread; told after its thread-locals are put back.
    Resumed,
    /// The job ended; its fiber goes back to the idle pool.
    Ended,
    /// A pool job that never joined the graph ended: the one switch after its `Began`, its
    /// `job` [`UNREGISTERED`]. Told with no lock held.
    Ran,
}

#[expect(
    clippy::disallowed_types,
    reason = "the process's one profiler hook, set once, read with no lock at every switch"
)]
static OBSERVER: std::sync::OnceLock<fn(Switch)> = std::sync::OnceLock::new();

/// Has `observer` told of every job's switches (the profiler's). Once a process; a second is
/// refused.
pub fn observe(observer: fn(Switch)) -> Result<(), fn(Switch)> {
    OBSERVER.set(observer)
}

/// Whether an observer is told of switches: only then is a switch worth the lock.
pub(super) fn observing() -> bool {
    OBSERVER.get().is_some()
}

/// A switch of `job`, a live job of a driver or pool, its generation read from the process's
/// table ([`super::ids`]): no lock.
#[inline]
pub(super) fn switched(event: SwitchEvent, job: JobId, fiber: u32, me: u32) {
    if let Some(observer) = OBSERVER.get() {
        let generation = super::ids::generation(job);
        let awaited = match event {
            SwitchEvent::Parked(Some(Wait::Job(on))) => super::ids::generation(on),
            _ => 0,
        };
        observer(Switch { event, job, generation, awaited, fiber, core: CoreId(me) });
    }
}

/// A switch of a job not in the graph ([`SwitchEvent::Began`], [`SwitchEvent::Ran`]): no
/// world to read, so it needs no lock.
#[inline]
pub(super) fn switched_unregistered(event: SwitchEvent, fiber: u32, me: u32) {
    switched_known(event, UNREGISTERED, 0, fiber, me);
}

/// A switch of `job` whose generation the caller kept ([`SwitchEvent::Ready`] from a parked
/// record), told without reading the world.
#[inline]
pub(super) fn switched_known(event: SwitchEvent, job: JobId, generation: u32, fiber: u32, me: u32) {
    if let Some(observer) = OBSERVER.get() {
        observer(Switch { event, job, generation, awaited: 0, fiber, core: CoreId(me) });
    }
}

/// How a pool job was given to the pool, as the profiler says it ([`observe_spawner`]): told in
/// its [`SwitchEvent::Began`], so its wait for a core is in the record.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Given {
    /// The job running where it was given, as the observer numbers it; 0 for none.
    pub spawner: u64,
    /// When, on the observer's clock; 0 when not said.
    pub at: u64,
    /// The build the giver worked for, as the observer numbers builds; 0 for none. The job
    /// works for it too, so its spans and switches are the build's (a job given by a build's
    /// task is that build's work, on whichever thread it runs).
    pub build: u32,
    /// The span open where it was given, as the observer numbers spans; 0 for none: what gave
    /// it when no job did (a thread on no fiber, such as the daemon's loop), so the critical
    /// path crosses from the job's start into the giver's work.
    pub creator: u64,
}

#[expect(
    clippy::disallowed_types,
    reason = "the process's one profiler hook, set once, read with no lock at every pool submit"
)]
static SPAWNER: std::sync::OnceLock<fn() -> Given> = std::sync::OnceLock::new();

/// Has `given` say, as a pool job is given to the pool, the observer's number for the job
/// running on this thread (the new job's spawner) and its clock: told in the job's
/// [`SwitchEvent::Began`]. Once a process; a second is refused.
pub fn observe_spawner(given: fn() -> Given) -> Result<(), fn() -> Given> {
    SPAWNER.set(given)
}

/// The job running on this thread and the time, as [`observe_spawner`]'s reader says them;
/// zeros when none is set.
#[inline]
pub(crate) fn spawner() -> Given {
    SPAWNER.get().map_or(Given::default(), |given| given())
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

#[expect(
    clippy::disallowed_types,
    reason = "the process's one profiler hook, set once, read with no lock as a worker idles"
)]
static IDLE_OBSERVER: std::sync::OnceLock<fn(Idle)> = std::sync::OnceLock::new();

/// Has `observer` told of every worker's idle stretches (the profiler's). Once a process; a
/// second is refused.
pub fn observe_idle(observer: fn(Idle)) -> Result<(), fn(Idle)> {
    IDLE_OBSERVER.set(observer)
}

#[inline]
pub(super) fn idled(core: u32, reason: IdleReason, began: bool) {
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
        #[allow(
            clippy::disallowed_types,
            reason = "no global state: each core's worker handle, set as it is spawned"
        )]
        let slots: Arc<[CoreSlot]> = (0..n)
            .map(|_| CoreSlot {
                moving: AtomicBool::new(false),
                woken: AtomicBool::new(false),
                thread: std::sync::OnceLock::new(),
            })
            .collect(); // alloc: setup, once per driver
        let mut pending = Vec::with_capacity(n);
        pending.resize(n, NONE);
        let jobs = config.fibers.max(64);
        let sched = Scheduler::new(jobs);
        let member = {
            let mut global = sched.lock();
            let member = global.world.join(cores, jobs);
            global.io.push((member, Io { cores: Arc::clone(&slots), pending }));
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
        #[expect(clippy::disallowed_types, reason = "makes `Shared::state`, which allows it")]
        let shared = Arc::new(Shared {
            work,
            sched,
            member,
            state: blaze_core::lock::Lock::new("sched.fiber.driver", state),
            cores: slots,
            config,
            start: Instant::now(),
            go: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            #[cfg(test)]
            fail_fibers: AtomicBool::new(false),
        });
        let mut fibers = Vec::with_capacity(config.fibers);
        for _ in 0..config.fibers {
            match new_fiber(&shared) {
                Ok(fiber) => fibers.push(fiber),
                Err(error) => {
                    let mut st = shared.lock();
                    st.world().leave(member);
                    st.global.io.retain(|it| it.0 != member);
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
            let started =
                thread::Builder::new().name(format!("blaze-fiber-{core}")).spawn(move || {
                    let set = (*setup)(CoreId(core))
                        .map_err(|why| DriverError::Setup { core: CoreId(core), why });
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

    /// Makes every fiber this driver makes from now on fail as an unmapped stack would: the
    /// failed-driver path, for a test.
    #[cfg(test)]
    pub(super) fn fail_new_fibers(&self) {
        self.shared.fail_fibers.store(true, Ordering::Release);
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
        self.shared.lock().global.claim(key, None)
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
                Resumed::Return => {}
                Resumed::Yield(_) => unreachable!("an idle fiber told to exit ends"),
            }
        }
        // Jobs that never ended (the driver failed): taken out under the lock, ended after it.
        let slots = mem::take(&mut st.slots);
        let member = self.shared.member;
        let stats = st.world().leave(member).map(|place| place.stats()).unwrap_or_default();
        st.global.io.retain(|it| it.0 != member);
        let (failed, mut panicked) = (st.failed.take(), st.panic.take());
        drop(st);
        // Outside the driver's lock, and after the member left the world, so a panicking `end`
        // leaves no member behind and runs no foreign code under the lock. Their fibers' stacks
        // hold frames that may hold locks, so they are not unwound; dropped, they stay mapped
        // until the process ends (`Stackful`). The values the jobs carried are ended here,
        // every one even past a panicking `end`, whose panic the joiner raises.
        for slot in slots {
            if let Slot::Parked(fiber, mut taken, _) = slot {
                drop(fiber);
                if let Some(panic) = end_carried(&mut taken) {
                    panicked.get_or_insert(panic);
                }
            }
        }
        (stats, failed, panicked)
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
fn park_carried<W: Work, J>(st: &mut State<W>, fiber: Fiber<J>, worker: &mut Taken) -> Slot<J> {
    // alloc: a box once per job parked at once, then kept in `spare_taken`.
    let mut taken = st.spare_taken.pop().unwrap_or_else(|| Box::new(Taken::EMPTY));
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
    let mut worker = Taken::EMPTY;
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
            switched(SwitchEvent::Started, job, fiber.1, me);
            (fiber, In::Start(job, payload))
        }
        Slot::Parked(fiber, mut taken, failed) => {
            let answer = come_on(&mut worker, Some(&mut *taken), failed);
            st.spare_taken.push(taken);
            switched(SwitchEvent::Resumed, job, fiber.1, me);
            (fiber, In::Go(answer))
        }
        Slot::Running => panic!("job {} run on core {me} while it runs", job.0),
        Slot::Free => panic!("job {} run on core {me}, never spawned", job.0),
    };
    drop(st);
    shared.cores[me as usize].moving.store(false, Ordering::Relaxed);
    loop {
        let out = match fiber.0.resume(input) {
            Resumed::Yield(out) => out,
            Resumed::Return => unreachable!("a fiber ends only when told to exit"),
        };
        clear();
        // The scheduler of what the job waits for joins its own first: a job of another driver
        // or pool, a key or a counter of theirs.
        let joins = match &out {
            Out::Wait(Want::On(on)) => Some(*on),
            Out::Wait(Want::Job(handle)) => Some(Wait::Job(handle.job)),
            _ => None,
        };
        let mut st = shared.lock_for(joins);
        input = match out {
            Out::Wait(want) => {
                let on = match want {
                    Want::On(on) => Some(on),
                    Want::Job(handle) => (st.world().generation(handle.job) == handle.generation)
                        .then_some(Wait::Job(handle.job)),
                };
                if let Some(Wait::Until(_)) = on {
                    st.world().advance_clock(member, shared.now());
                }
                match on.map(|on| st.world().suspended(member, job, on)) {
                    None | Some(Ok(Suspend::Ready)) => In::Go(Answer::Ready),
                    Some(Err(cycle)) => In::Go(Answer::Cycle(cycle)),
                    Some(Ok(Suspend::Waiting)) => {
                        switched(SwitchEvent::Parked(on), job, fiber.1, me);
                        st.slots[j] = park_carried(&mut st, fiber, &mut worker);
                        let next = shared.dispatch(&mut st, Some(me));
                        return (st, next);
                    }
                }
            }
            Out::Move => match st.world().yielded(member, job) {
                Yielded::Stay => In::Go(Answer::Ready),
                Yielded::Moved => {
                    switched(SwitchEvent::Parked(None), job, fiber.1, me);
                    st.slots[j] = park_carried(&mut st, fiber, &mut worker);
                    let next = shared.dispatch(&mut st, Some(me));
                    return (st, next);
                }
            },
            Out::Done(panicked) => {
                // The observer is told first, while the thread still holds the job's own
                // values (its build in the profiler's state), which `go_off_ended` ends.
                switched(SwitchEvent::Ended, job, fiber.1, me);
                let left = go_off_ended(&mut worker);
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
