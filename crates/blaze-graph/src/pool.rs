//! The threads the plane's tasks run on (docs/v2/plan.md section 4, docs/v2/plan.md "The
//! executor"): one worker per CPU of a [`Topology`], stealing work nearest first.
//!
//! Each worker has a deque of its own (crossbeam's Chase-Lev deque, as Rayon's) and a LIFO
//! slot, and each group (the CPUs sharing a last-level cache or a cluster) an injector. A job
//! submitted near a group goes to that group's injector; one near no group goes to the least
//! loaded group, the fast ones first on a tie (Apple's performance cores, where the plane's
//! first ready nodes, its critical path, go). A job a worker gives itself ([`Pool::submit_next`],
//! or a [`Scope`]'s job submitted on one of the pool's workers) goes to its LIFO slot and runs
//! next, hot in its cache, as in Tokio; after three such jobs in a row the slot's job goes
//! behind the deque's, so the slot never starves it.
//!
//! An idle worker takes from its slot and its deque, then searches: its group's injector and
//! workers (half a deque at a time), then each group of its [`Topology::steal_order`] the same
//! way, four times over, then parks. Inside a group it picks deques as its pool's [`Victims`]
//! says: by default twice the longer of two random ones, then each from a random one. At most half the
//! workers search at once, and a job wakes a parked worker (of its group first) only when none
//! searches; a searcher that finds work and was the last wakes another for the rest (Go's and
//! Tokio's rules), so a burst of work wakes no stampede and none is left unseen. A parked worker
//! sleeps on its own futex word (a [`blaze_core::wait::Parker`]: `IORING_OP_FUTEX_WAIT` on its
//! thread's io_uring on Linux), not a shared condvar; nothing signals it.
//!
//! A worker that cannot park (the kernel refused its wait) or whose loop panicked stops the
//! pool for that error ([`Broken`], [`Pool::broken`]): it is said once on standard error, the
//! pool's workers run what is queued and end without parking again, and every scope on the
//! pool fails with the error, a thread blocked on one woken to do so. No worker ends telling no
//! one, and no thread waits on a pool none of whose workers is left.
//!
//! On a machine of fast and slow groups, a job on its request's critical path
//! ([`Pool::submit_critical`]) goes to its `near` group if that is fast, else to the least loaded
//! fast group; stealing is unchanged, so a slow worker with nothing else still takes it. Where
//! every group is fast a critical job is placed as any other. A background job
//! ([`Class::Background`], work ahead of demand) waits in its group's queue of its own, taken one
//! at a time by a worker that found nothing else while no group has a job queued. [`node`] names the NUMA node of
//! the calling worker, for memory it should keep local.
//!
//! Bound to its CPU, a worker is pinned to it on Linux (`sched_setaffinity`); on macOS, which
//! pins nothing, every worker takes the QoS class `user-initiated`, the class of work a person
//! waits for. No worker runs at `utility` or `background`: a loaded host gives those about 2%
//! of a core, and any worker may run critical work, by stealing it or by a job others come to
//! wait on, so a worker at such a class stalls the build ([`Topology::from_perflevels`]).
//!
//! Every job runs as a task of [`crate::pump`]'s stack, so a wait inside it pumps this pool.
//!
//! On a target without threads (the wasm64 seed, docs/future/bootstrap.md "One thread") a pool
//! starts no thread: each worker's loop, the same `work` as every worker's, runs on a fiber that
//! the thread that made the pool drives ([`Pool::driven`], [`block_until`]). That thread, as it
//! waits on no fiber (a client's [`crate::sched::wait`]), gives each worker a turn of one job at
//! a time until its wait ends; where a worker of its own thread would park, a driven one gives
//! its turn back, and its thread looks at the clock's next due wait, then for a stall.
//!
//! A job allocates nothing once its thread is warm (docs/v2/plan/design/memory.md). Its
//! closure goes in a cell of 128, 512 or 2048 bytes from one of three process-wide
//! [`blaze_mem::Pool`]s, through a cache of the thread that submits it, and its cell goes back
//! to the cache of the thread that runs it before the closure runs. A closure no cell holds is
//! a build error (`Placed::CLASS`), never a box. A group's injector is a list through the
//! jobs' own headers behind a lock (Tokio's inject queue), so queueing a job adds no node.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    io,
    marker::PhantomData,
    mem::MaybeUninit,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    ptr::{self, NonNull},
    sync::Arc,
};

use blaze_mem::{Cache, MemError, Pool as CellPool, Pooled, Tag};
use blaze_radix as radix;
use crossbeam_deque::{Steal, Stealer, Worker};

use crate::{
    pump::Lock,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence},
        thread::{self, JoinHandle, Thread},
        thread_local,
    },
    topology::{Cpu, Topology},
};

/// A worker's stack. Its jobs run on fibers ([`crate::sched::Host`]), but under the model
/// checker, which runs them on the worker's own stack.
const STACK: usize = 64 << 20;
/// How many jobs a worker takes from an injector at once: few, so the plane's order (its
/// critical path first) holds across workers.
const BATCH: usize = 4;
/// How many jobs in a row a worker runs from its LIFO slot before its deque's oldest (Tokio's
/// `MAX_LIFO_POLLS_PER_TICK`).
const LIFO_RUNS: u8 = 3;
/// How many times a searching worker looks at every queue before it parks.
const ROUNDS: u32 = 4;
/// How many pairs a [`Victims::TwoChoices`] thief draws in a group before it walks the group.
const CHOICES: u32 = 2;

/// Whether the target starts threads: a pool on one without ([`Pool::new`] there) is driven by
/// the thread that made it ([`Pool::driven`]). wasm without the atomics feature is such a target
/// (the seed); once the seed has threads, its pools start them as native ones do.
const THREADS: bool = !cfg!(all(target_family = "wasm", not(target_feature = "atomics")));

/// How long a pool sleeps, idle with jobs parked, before it asks the scheduler whether they
/// are stalled ([`crate::sched::check_stall`]).
#[cfg(not(blaze_check))]
const STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a pool's stop waits with jobs parked and none ending before it fails what is still
/// parked, and as long again, with no job ending after that, before it gives up: what a drop
/// waits at most past the pool's last progress ([`Pool::stop`]).
#[cfg(not(blaze_check))]
pub const STOP_BOUND: std::time::Duration = std::time::Duration::from_secs(60);

/// The pools whose workers run threads of their own, while they live: what a heap give-back
/// wakes ([`wake_for_give_back`]). Pushed as such a pool starts, pruned of dropped ones then.
#[cfg(not(blaze_check))]
static LIVE: Lock<Vec<std::sync::Weak<Shared>>> = Lock::new("graph.pool.live", Vec::new());

/// Wakes every idle worker of every live pool, so it gives back the free pages its heaps keep
/// as it goes idle again ([`blaze_mem::tag::settle_heap`] in [`Local::park`]) rather than at its
/// next job: what [`blaze_mem::tag::ask_heap_give_back`] calls, and a purge then waits for
/// every worker's give-back (docs/memory-footprint.md, step 2). A worker woken with no job
/// parks again; nothing is signalled.
#[cfg(not(blaze_check))]
fn wake_for_give_back() {
    // Taken out of the lock before any wake: a wake is a system call (rule
    // standard-structures: a lock is held for a field update alone).
    // alloc: a few pools, once a purge
    let live: Vec<Arc<Shared>> = LIVE.lock().iter().filter_map(std::sync::Weak::upgrade).collect();
    for shared in live {
        shared.unpark_workers();
    }
}

/// What the queues hold: a pointer to a job's [`Header`], which says how to run it. One pointer
/// (8 bytes on 64-bit targets), so a line of a deque's buffer holds eight jobs there.
pub(crate) struct Job(NonNull<Header>);

const _: () = assert!(size_of::<Job>() == size_of::<usize>());

// SAFETY: a job is made only of work that may move to another thread (`Job::erase`'s bound, or
// a scheduler's run, whose fields are `Send`), and is run once, by the thread that takes it.
unsafe impl Send for Job {}

/// The first field of every job: the function that runs it, given the job, and its link in
/// the [`Inject`] queue it waits in. Two pointers (16 bytes on 64-bit targets).
#[repr(C)]
pub(crate) struct Header {
    run: unsafe fn(NonNull<Header>),
    /// The job after this one in its group's [`Inject`] list: read and written only under the
    /// list's lock, while the job waits there.
    next: AtomicPtr<Header>,
}

#[cfg(not(blaze_check))]
const _: () = assert!(size_of::<Header>() == 2 * size_of::<usize>());

impl Header {
    pub(crate) const fn new(run: unsafe fn(NonNull<Header>)) -> Header {
        Header { run, next: AtomicPtr::new(ptr::null_mut()) }
    }

    /// Whether this header's job runs by `run`: for a `run` that is neither generic nor
    /// inlined, which has one address.
    pub(crate) fn runs(&self, run: unsafe fn(NonNull<Header>)) -> bool {
        ptr::fn_addr_eq(self.run, run)
    }
}

/// A job's cell: `N` bytes, 64-byte aligned, holding a [`Placed`].
#[repr(C, align(64))]
struct JobCell<const N: usize>(MaybeUninit<[u8; N]>);

impl<const N: usize> JobCell<N> {
    const EMPTY: JobCell<N> = JobCell(MaybeUninit::uninit());
}

type Small = JobCell<128>;
type Medium = JobCell<512>;
type Large = JobCell<2048>;

/// The most cells of each size live at once, each a job submitted and not yet run: far past
/// what a process queues. Past it a submit stops, naming the pool ([`MemError::Exhausted`]).
const SMALL_CELLS: u32 = 1 << 22;
const MEDIUM_CELLS: u32 = 1 << 18;
const LARGE_CELLS: u32 = 1 << 16;

/// The cells of every pool of the process: a job may be run by another pool's worker that
/// pumps it, so they are not one pool's.
struct Cells {
    small: CellPool<Small>,
    medium: CellPool<Medium>,
    large: CellPool<Large>,
}

impl Cells {
    fn new() -> Result<Cells, MemError> {
        Ok(Cells {
            small: CellPool::new(SMALL_CELLS, Tag::Plane)?,
            medium: CellPool::new(MEDIUM_CELLS, Tag::Plane)?,
            large: CellPool::new(LARGE_CELLS, Tag::Plane)?,
        })
    }
}

/// Reserved by the first [`Pool::new`], which says why it could not be, or by the first job
/// given with no pool made. Under `blaze_check` a set `static` cell stays set across
/// executions; the pools' atomics start each execution afresh, and so do the model threads'
/// caches, so no execution sees another's cells.
static CELLS: OnceLock<Cells> = OnceLock::new();

fn cells() -> &'static Cells {
    CELLS.get_or_init(|| Cells::new().unwrap_or_else(|why| panic!("the pool's job cells: {why}")))
}

/// A thread's caches into [`CELLS`], made on its first job; dropped with the thread, which
/// gives back the cells they hold.
struct Caches {
    small: Cache<'static, Small>,
    medium: Cache<'static, Medium>,
    large: Cache<'static, Large>,
    /// The thread whose caches these are: every use checks it is that thread's.
    #[cfg(all(test, not(blaze_check)))]
    owner: std::thread::ThreadId,
}

impl Caches {
    fn new() -> Caches {
        let cells = cells();
        Caches {
            small: cells.small.cache(),
            medium: cells.medium.cache(),
            large: cells.large.cache(),
            #[cfg(all(test, not(blaze_check)))]
            owner: this_thread(),
        }
    }
}

thread_local! {
    static CACHES: RefCell<Option<Caches>> = const { RefCell::new(None) };
}

/// `f` with this thread's caches. Not inlined, and so are its callers ([`take_room`],
/// [`give_back`]): the compiler keeps a thread-local's address across calls inside one
/// function, so a caller inlined into a job's code that waits between two submits (checkpoint
/// 36's `blaze_codegen::driver::Build::walk`, two `par::map`s in a loop) borrowed the cache of
/// the thread it ran on before the wait, racing that thread's own jobs on a `RefCell` that is
/// not `Sync`: "RefCell already borrowed" at every job of the victim thread (rule 27).
#[inline(never)]
fn with_caches<R>(f: impl FnOnce(&mut Caches) -> R) -> Option<R> {
    CACHES
        .try_with(|caches| {
            let mut caches = caches
                .try_borrow_mut()
                .expect("this thread's job-cell caches, borrowed twice: a thread-local's address kept across a fiber switch (rule 27)");
            let caches = caches.get_or_insert_with(Caches::new);
            #[cfg(all(test, not(blaze_check)))]
            assert_eq!(caches.owner, this_thread(), "a job used another thread's job-cell caches: a thread-local's address kept across a wait (rule 27)");
            f(caches)
        })
        .ok()
}

/// The calling thread, looked up anew at each call (never a kept thread-local's address).
#[cfg(all(test, not(blaze_check)))]
#[inline(never)]
fn this_thread() -> std::thread::ThreadId {
    std::thread::current().id()
}

/// A cell of a job, owned by the job it holds.
enum Room {
    Small(Pooled<'static, Small>),
    Medium(Pooled<'static, Medium>),
    Large(Pooled<'static, Large>),
}

/// A job in its cell.
#[repr(C)]
struct Placed<F> {
    header: Header,
    cell: Room,
    /// The job running where this one was given to the pool and when, as a profiler says them
    /// ([`crate::sched::fiber::observe_spawner`]).
    given: crate::sched::Given,
    f: F,
}

impl<F> Placed<F> {
    /// The smallest cell that holds a job of `F` (0, 1 or 2): evaluated as `F` is compiled, so
    /// a closure no cell holds fails the build.
    const CLASS: usize = {
        let (size, align) = (size_of::<Placed<F>>(), align_of::<Placed<F>>());
        assert!(
            align <= align_of::<Small>(),
            "a job's closure is aligned past 64 bytes: no job cell holds it"
        );
        if size <= size_of::<Small>() {
            0
        } else if size <= size_of::<Medium>() {
            1
        } else if size <= size_of::<Large>() {
            2
        } else {
            panic!(
                "a job's closure is past 2 KiB with its header: no job cell holds it; capture less"
            )
        }
    };
}

/// A cell of `cache`'s pool, and where its room begins.
fn take_cell<const N: usize>(
    cache: &mut Cache<'static, JobCell<N>>,
    room: fn(Pooled<'static, JobCell<N>>) -> Room,
) -> (Room, NonNull<u8>) {
    let mut cell = cache.alloc(JobCell::EMPTY);
    let at = NonNull::from(&mut *cell).cast::<u8>();
    (room(cell), at)
}

/// A cell of class `class` (0, 1 or 2, [`Placed::CLASS`]) from this thread's cache, and where
/// its room begins.
fn take_room(class: usize) -> (Room, NonNull<u8>) {
    with_caches(|caches| match class {
        0 => take_cell(&mut caches.small, Room::Small),
        1 => take_cell(&mut caches.medium, Room::Medium),
        _ => take_cell(&mut caches.large, Room::Large),
    })
    .expect("a job submitted by a thread whose locals are gone: its job-cell caches went with them")
}

/// Gives `cell` back to this thread's cache, its job's closure already read out of it. A
/// thread whose locals are gone drops it, which gives it to its pool's strays.
fn give_back(cell: Room) {
    with_caches(move |caches| match cell {
        Room::Small(cell) => caches.small.recycle(cell),
        Room::Medium(cell) => caches.medium.recycle(cell),
        Room::Large(cell) => caches.large.recycle(cell),
    });
}

/// How the pool's own work before a job's closure went (giving back its cell, beginning its
/// span): `Err` with what that panicked with, which the job reports as its failure rather than
/// run. A job is never dropped unrun for it, so whoever waits on the job hears of it (rule 20).
// dyn: a panic's payload, as `catch_unwind` gives it
pub(crate) type Started = Result<(), Box<dyn Any + Send>>;

/// What a job of `kind` fails with when its start panicked with `panic`: a message naming the
/// job and the panic.
// dyn: a panic's payload, as `catch_unwind` gives it
#[cold]
fn failed_start(kind: &'static str, panic: &(dyn Any + Send)) -> Box<dyn Any + Send> {
    let said = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("a panic of no message");
    // alloc: a failed start's message, once per failed job
    Box::new(format!("the pool could not start job `{kind}`: {said}"))
}

/// What a profiler is told of each job a pool runs, so a worker's time is always inside a span
/// of the job it runs: `began` opens and enters the span of a job of `kind` (the type of its
/// closure, or of the plane's runner) on this thread, giving a number for `ended`, which closes
/// it on whatever thread the job ended on. A job that parks on its fiber closes its span as it
/// parks and opens another as it resumes: the profiler's `take` and `put` of
/// [`crate::sched::fiber::carry`] do that. `driving` opens and enters the span of a worker's
/// own work between jobs ([`Driving`], its name), giving a number for `drove`, which closes it
/// on the same thread: with the jobs' spans, every moment of a worker is inside one.
#[derive(Clone, Copy)]
pub struct JobSpans {
    pub began: fn(kind: &'static str) -> u64,
    pub ended: fn(u64),
    pub driving: fn(doing: &'static str) -> u64,
    pub drove: fn(u64),
}

static JOB_SPANS: OnceLock<JobSpans> = OnceLock::new();

/// Spans every job from now on with `spans`; `Err` with them when spans were given before.
pub fn span_jobs(spans: JobSpans) -> Result<(), JobSpans> {
    JOB_SPANS.set(spans)
}

/// The span of the job running, open while this lives (an unwind closes it too); nothing while
/// no profiler spans jobs.
pub(crate) struct JobSpan(Option<(fn(u64), u64)>);

impl JobSpan {
    #[inline]
    pub(crate) fn begin(kind: &'static str) -> JobSpan {
        JobSpan(JOB_SPANS.get().map(|spans| (spans.ended, (spans.began)(kind))))
    }
}

impl Drop for JobSpan {
    #[inline]
    fn drop(&mut self) {
        if let Some((ended, span)) = self.0 {
            ended(span);
        }
    }
}

/// What a pool worker does when it runs no job, each under a span of its own
/// ([`JobSpans::driving`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Driving {
    /// Looking for a job: its own queues, the injectors, stealing, the timers due.
    Search,
    /// Parked with nothing to run, until a job is queued or a timed wait is due.
    Idle,
    /// The fiber driver between two jobs: a fiber made or ended, a parked or ended job's
    /// record and the scheduler's dispatch of what it changed.
    Switch,
}

impl Driving {
    pub const fn name(self) -> &'static str {
        match self {
            Driving::Search => "search",
            Driving::Idle => "idle",
            Driving::Switch => "switch",
        }
    }
}

/// The span of a worker's own work ([`Driving`]), open while this lives on the thread that
/// opened it; nothing while no profiler spans jobs. Never held across a job's run: a job's
/// fiber takes its thread's open spans with it.
pub(crate) struct DriverSpan(Option<(fn(u64), u64)>);

impl DriverSpan {
    #[inline]
    pub(crate) fn open(doing: Driving) -> DriverSpan {
        DriverSpan(JOB_SPANS.get().map(|spans| (spans.drove, (spans.driving)(doing.name()))))
    }
}

impl Drop for DriverSpan {
    #[inline]
    fn drop(&mut self) {
        if let Some((drove, span)) = self.0 {
            drove(span);
        }
    }
}

impl Job {
    /// `f` in a cell, given back as it runs; `f` is told how its start went ([`Started`]).
    fn new<F: FnOnce(Started) + Send + 'static>(f: F) -> Job {
        // SAFETY: `f` borrows nothing.
        unsafe { Job::erase(f) }
    }

    /// # Safety
    ///
    /// What `f` borrows outlives its run.
    unsafe fn erase<F: FnOnce(Started) + Send>(f: F) -> Job {
        /// # Safety
        ///
        /// `header` is the header of the `Placed<F>` that `erase` wrote into its cell, and
        /// this is its one run (it gives the cell back).
        unsafe fn run<F: FnOnce(Started)>(header: NonNull<Header>) {
            let at = header.cast::<Placed<F>>().as_ptr();
            // SAFETY: `at` is the live `Placed<F>` that `erase` wrote.
            let cell = unsafe { &raw const (*at).cell };
            // SAFETY: its cell is read out once here, and the rest of it needs no drop.
            // Read twice, the cell would be given back twice.
            let cell = unsafe { cell.read() };
            // SAFETY: `at` is the live `Placed<F>` that `erase` wrote.
            let f = unsafe { &raw const (*at).f };
            // SAFETY: the closure is read out once, so it runs and drops once.
            let f = unsafe { f.read() };
            // SAFETY: `at` is the live `Placed<F>` that `erase` wrote; a `Given` is `Copy`.
            let given = unsafe { (*at).given };
            let kind = std::any::type_name::<F>();
            // A panic here is the job's failure, given to `f`, never a job dropped unrun.
            let started = catch_unwind(AssertUnwindSafe(move || {
                give_back(cell);
                crate::sched::job_begins(given);
                JobSpan::begin(kind)
            }));
            match started {
                Ok(_span) => f(Ok(())),
                Err(panic) => f(Err(failed_start(kind, &*panic))),
            }
        }
        let class = const { Placed::<F>::CLASS };
        let header = Header::new(run::<F>);
        let given = crate::sched::spawner();
        let (cell, at) = take_room(class);
        let at = at.cast::<Placed<F>>();
        // SAFETY: the cell is this job's alone, committed, 64-byte aligned and at least
        // `size_of::<Placed<F>>()` bytes (`CLASS` chose its size), and holds nothing that
        // needs a drop.
        unsafe { at.write(Placed { header, cell, given, f }) };
        Job(at.cast())
    }

    /// # Safety
    ///
    /// `header` is the first field of a job that lives until it has run, and that no other
    /// [`Job`] names until then.
    pub(crate) unsafe fn from_header(header: NonNull<Header>) -> Job {
        Job(header)
    }

    pub(crate) fn header(&self) -> NonNull<Header> {
        self.0
    }

    pub(crate) fn run(self) {
        // SAFETY: `self.0` points at the live `Header` of a job not yet run (in the cell `erase`
        // took, or kept by `from_header`'s caller until it has run), and running moves the
        // `Job`, so no other `Job` names it; if not, this reads a cell given back.
        let run = unsafe { self.0.as_ref().run };
        // SAFETY: `run` is the function made for this header's job (`erase`'s, or the one its
        // `from_header` caller put there), called once, as its own `# Safety` asks.
        unsafe { run(self.0) }
    }
}

/// A group's queue of the jobs given to it but not to a worker's own: a list through each
/// job's [`Header::next`] behind a lock, its length readable without it (Tokio's inject
/// queue). The jobs are its nodes, so a push allocates nothing, where a queue of blocks makes
/// one every few dozen.
struct Inject {
    len: AtomicUsize,
    list: Lock<List>,
}

/// Oldest first; both null when empty.
struct List {
    head: *mut Header,
    tail: *mut Header,
}

// SAFETY: the list's nodes are `Job`s, which may move to another thread, and it is reached only
// under `Inject::list`'s lock.
unsafe impl Send for List {}

impl Inject {
    fn new() -> Inject {
        let list = List { head: ptr::null_mut(), tail: ptr::null_mut() };
        Inject { len: AtomicUsize::new(0), list: Lock::new("graph.pool.inject", list) }
    }

    /// Jobs queued, as last seen: for placing a job, not for deciding there is none.
    fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// Whether no job is queued, ordered with a push as a submit's `announce` and a sleeper's
    /// look are.
    fn is_empty(&self) -> bool {
        self.len.load(Ordering::SeqCst) == 0
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "the queue takes sole ownership of the job until it is popped"
    )]
    fn push(&self, job: Job) {
        let at = job.0.as_ptr();
        // SAFETY: the job's header lives until it has run, and from here until a worker takes
        // it back out this queue is the one place that names it.
        unsafe { (*at).next.store(ptr::null_mut(), Ordering::Relaxed) };
        let mut list = self.list.lock();
        match NonNull::new(list.tail) {
            // SAFETY: the tail is a job of this list, alive until it is taken out, under this
            // lock.
            Some(tail) => unsafe { tail.as_ref() }.next.store(at, Ordering::Relaxed),
            None => list.head = at,
        }
        list.tail = at;
        self.len.fetch_add(1, Ordering::SeqCst);
    }

    /// The oldest job, taken.
    fn pop(&self) -> Option<Job> {
        if self.is_empty() {
            return None;
        }
        let mut list = self.list.lock();
        let head = NonNull::new(list.head)?;
        // SAFETY: as in `push`: the head is a job of this list, under its lock.
        list.head = unsafe { head.as_ref() }.next.load(Ordering::Relaxed);
        if list.head.is_null() {
            list.tail = ptr::null_mut();
        }
        self.len.fetch_sub(1, Ordering::SeqCst);
        Some(Job(head))
    }

    /// Every job of `self`, oldest first, to the back of `to`, one lock at a time.
    fn move_to(&self, to: &Inject) {
        if self.is_empty() {
            return;
        }
        let (head, tail, count) = {
            let mut list = self.list.lock();
            let taken = (list.head, list.tail);
            list.head = ptr::null_mut();
            list.tail = ptr::null_mut();
            let count = self.len.swap(0, Ordering::SeqCst);
            (taken.0, taken.1, count)
        };
        let Some(head) = NonNull::new(head) else { return };
        let mut list = to.list.lock();
        match NonNull::new(list.tail) {
            // SAFETY: as in `push`: the tail is a job of `to`'s list, under its lock.
            Some(old) => unsafe { old.as_ref() }.next.store(head.as_ptr(), Ordering::Relaxed),
            None => list.head = head.as_ptr(),
        }
        list.tail = tail;
        to.len.fetch_add(count, Ordering::SeqCst);
    }

    /// Up to [`BATCH`] jobs, oldest first: the first to run now, the rest to `deque`.
    fn steal_into(&self, deque: &Worker<Job>) -> Option<Job> {
        if self.is_empty() {
            return None;
        }
        let mut taken: [Option<NonNull<Header>>; BATCH] = [None; BATCH];
        {
            let mut list = self.list.lock();
            let mut count = 0;
            for slot in &mut taken {
                let Some(head) = NonNull::new(list.head) else { break };
                // SAFETY: as in `push`: the head is a job of this list, under its lock.
                list.head = unsafe { head.as_ref() }.next.load(Ordering::Relaxed);
                *slot = Some(head);
                count += 1;
            }
            if list.head.is_null() {
                list.tail = ptr::null_mut();
            }
            self.len.fetch_sub(count, Ordering::SeqCst);
        }
        let [first, rest @ ..] = taken;
        for job in rest.into_iter().flatten() {
            deque.push(Job(job));
        }
        first.map(Job)
    }
}

/// A group's own: 128-byte aligned, since every submitter writes its queue's lock and length.
#[repr(align(128))]
struct Group {
    inject: Inject,
    /// Its [`Class::Background`] jobs: taken one at a time, by a worker that found nothing
    /// else, never into a deque, so no thief takes one as other work.
    later: Inject,
    workers: Box<[u32]>,
    order: Box<[u32]>,
    fast: bool,
    /// Its workers parked or about to park, as their [`Slot::sleeping`] says.
    sleepers: AtomicU32,
}

/// A worker as the others see it: 128-byte aligned, since thieves read it while it writes.
#[repr(align(128))]
struct Slot {
    stealer: Stealer<Job>,
    /// The LIFO slot: the job to run next, which thieves take only when there is nothing else.
    next: AtomicPtr<Header>,
    sleeping: AtomicBool,
    group: u32,
    thread: OnceLock<Thread>,
    /// What its worker parks on when idle: a futex wait on its thread's io_uring on Linux
    /// (rule deadline-waits), woken by [`Shared::wake`] and [`Shared::unpark_workers`].
    parker: blaze_core::wait::Parker,
    /// How many jobs ended on this worker (ran to their end, not parked): written by its
    /// worker alone, summed by [`Pool::ended`].
    ended: AtomicU64,
    /// Whether its worker runs a job's code now, from a fiber's resume to its yield or end:
    /// written by its worker alone, counted by [`Pool::working`].
    inside: AtomicBool,
}

impl Slot {
    /// Its worker parks for up to `wait` (no limit for `None`), until a wake: on its own
    /// [`blaze_core::wait::Parker`], whose deadline is absolute, so an interruption waits only
    /// for what is left. The caller's span names the wait (`Driving::Idle`).
    ///
    /// # Errors
    ///
    /// The wait's, when the kernel refused it: its worker stops the pool with it
    /// ([`Shared::broke`]).
    #[cfg(not(blaze_check))]
    fn park_for(
        &self,
        wait: Option<std::time::Duration>,
    ) -> Result<(), blaze_core::wait::WaitError> {
        use blaze_core::wait::{Clock as _, Deadline, Monotonic};
        let clock = Monotonic::new();
        let deadline = wait.map(|it| Deadline::after(clock.now(), it));
        self.parker.park(IDLE_PARK, deadline, &clock).map(|_| ())
    }
}

/// What an idle worker's park waits for, as its wait and its error name it.
#[cfg(not(blaze_check))]
const IDLE_PARK: &str = "a job for an idle worker, or its pool's next due wait";

/// Why a pool stopped for an error ([`Pool::broken`]): one of its workers could not go on. Its
/// message names the worker, its group and the wait or the panic, and is what every scope on
/// the pool fails with from then on.
#[cfg(not(blaze_check))]
#[derive(Debug)]
pub struct Broken {
    /// The worker, by its index in the pool.
    pub worker: usize,
    /// The worker's group.
    pub group: u32,
    /// How many workers the pool has.
    pub workers: usize,
    pub why: Unable,
}

/// What a pool's worker could not do ([`Broken`]).
#[cfg(not(blaze_check))]
#[derive(Debug)]
pub enum Unable {
    /// Its idle park failed: the kernel refused the wait.
    Park(blaze_core::wait::WaitError),
    /// Its loop panicked, outside any job, with this message.
    Loop(String),
}

#[cfg(not(blaze_check))]
impl std::fmt::Display for Broken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "worker {} (group {}) of a pool of {} workers ",
            self.worker, self.group, self.workers
        )?;
        match &self.why {
            Unable::Park(error) => write!(f, "could not park: {error}")?,
            Unable::Loop(panic) => write!(f, "panicked outside any job: {panic}")?,
        }
        f.write_str(
            "; a pool one of whose workers cannot go on is stopped: its workers end, and every scope on it fails with \
             this rather than wait for jobs no worker will run (rules bounded-progress, no-fallback)",
        )
    }
}

#[cfg(not(blaze_check))]
impl std::error::Error for Broken {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.why {
            Unable::Park(error) => Some(error),
            Unable::Loop(_) => None,
        }
    }
}

/// What a test makes a pool's idle parks do ([`Shared::park_plant`]).
#[cfg(all(test, not(blaze_check)))]
mod park_plant {
    /// Park, as outside a test.
    pub(super) const PARKS: u32 = 0;
    /// Every park fails, as where the kernel refuses the wait.
    pub(super) const FAILS: u32 = 1;
    /// Every park fails and its worker ends telling no one: the bug of 0.20261105.0 on, where
    /// a worker whose park failed panicked out of its loop, unheard with standard error
    /// closed, and whoever waited on the pool waited forever.
    pub(super) const FAILS_AND_ENDS_SILENTLY: u32 = 2;
}

/// How many workers search and how many sleep, in one word so a submit reads both at once.
#[repr(align(128))]
struct Idle(AtomicU64);

const SEARCHING: u64 = 1;
const SLEEPING: u64 = 1 << 32;

fn searching(word: u64) -> u64 {
    word & (SLEEPING - 1)
}

fn sleeping(word: u64) -> u64 {
    word >> 32
}

struct Shared {
    groups: Box<[Group]>,
    slots: Box<[Slot]>,
    idle: Idle,
    stop: AtomicBool,
    /// Of fast and slow groups both: only then does a job's being critical place it.
    mixed: bool,
    /// The first panic of a job's `done`, raised when the pool is dropped.
    // dyn: a panic's payload, as `catch_unwind` gives it
    stray: Lock<Option<Box<dyn Any + Send>>>,
    /// The pool's scheduler: its own, or the one it joined as its jobs met another's.
    #[cfg(not(blaze_check))]
    sched: crate::sched::Scheduler,
    /// The pool's member of its scheduler ([`crate::sched::join_pool`]).
    #[cfg(not(blaze_check))]
    member: AtomicU32,
    /// Its jobs parked on a wait now: its workers stop only once there are none. Under
    /// `blaze_check` jobs run on their worker's stack and never park, so it stays 0.
    parked: AtomicUsize,
    /// The thread that stops the pool ([`Pool::stop`]), set before `stop`: its workers wake it
    /// as they sleep on jobs parked, and as they end.
    #[cfg(not(blaze_check))]
    stopper: OnceLock<Thread>,
    /// Its stop gave up ([`Unstopped`]): its workers end though jobs are parked.
    #[cfg(not(blaze_check))]
    abandoned: AtomicBool,
    /// Its workers whose loop returned: what its stopper waits for.
    left: AtomicUsize,
    /// Why it stopped for an error, set once, by the first worker that could not go on
    /// ([`Shared::broke`]).
    #[cfg(not(blaze_check))]
    broken: OnceLock<Broken>,
    /// The threads on no fiber blocked on a scope of this pool now ([`block`]): woken as the
    /// pool breaks and as each worker of a broken pool ends.
    #[cfg(not(blaze_check))]
    waiters: Lock<Vec<Thread>>,
    /// What a test makes its idle parks do ([`park_plant`]).
    #[cfg(all(test, not(blaze_check)))]
    park_plant: AtomicU32,
    #[cfg(test)]
    critical: AtomicUsize,
}

/// How soon a job is taken, by what waits on it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Class {
    /// Work ahead of demand: taken only by a worker that found no other job while none is
    /// queued, never stolen as other work. It is background only until it starts: once it ran,
    /// a wait parks and resumes it as a normal job, so a job that waits for a key it holds
    /// never waits behind the queue.
    Background,
    Normal,
    /// On its request's critical path ([`Pool::submit_critical`]).
    Critical,
}

/// How a searching worker picks the deques it steals from inside one group. Either way it
/// visits the groups in its [`Topology::steal_order`], each group's injector first, so which
/// groups' workers take which jobs (critical ones on fast groups) does not change.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Victims {
    /// Each other worker of the group in turn, from a random one.
    Walk,
    /// Up to twice (`CHOICES`), two other workers of the group drawn at random and half of the
    /// longer deque of the two (Mitzenmacher's power of two choices), so thieves going idle at
    /// once spread over the fullest deques; then [`Victims::Walk`] of the group, for what the
    /// picks missed.
    TwoChoices,
}

/// How a pool's workers steal: what [`Pool::with_stealing`] takes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Stealing {
    pub victims: Victims,
    /// Seeds every worker's draws, mixed with its index: the same seed and the same deque
    /// lengths draw the same victims.
    pub seed: u64,
}

impl Stealing {
    /// What [`Pool::new`] makes a pool with.
    pub const DEFAULT: Stealing = Stealing { victims: Victims::Walk, seed: 0x5EED_B1A2E };
}

pub struct Pool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    /// Whether it was stopped ([`Pool::stop`]), so its drop stops it no more.
    stopped: bool,
    /// Its workers when it has no thread of its own ([`Pool::driven`]).
    #[cfg(not(blaze_check))]
    driven: Option<Box<Drivers>>,
    topology: Topology,
}

/// The workers of a pool with no thread of its own, each its loop on a fiber
/// ([`crate::sched::Driven`]): given turns by the thread that made the pool, the one thread that
/// lists it in [`DRIVEN`], from the pool's making to its drop.
#[cfg(not(blaze_check))]
struct Drivers {
    workers: RefCell<Box<[crate::sched::Driven]>>,
    /// The thread that made the pool, the one that drives it.
    owner: std::thread::ThreadId,
}

#[cfg(not(blaze_check))]
// SAFETY: a pool is shared and sent as any (a process's pool is a static), but its `Drivers` are
// touched only on the thread that made it: through that thread's own `DRIVEN` list, and by the
// pool's drop, which refuses any other thread before it reads them (`Drivers::end`).
unsafe impl Send for Drivers {}
#[cfg(not(blaze_check))]
// SAFETY: a `&Drivers` reaches the drivers only on the thread that made the pool (its `DRIVEN`
// list, and the pool's drop, which refuses any other thread first), so sharing one across
// threads lets no other thread touch them.
unsafe impl Sync for Drivers {}

#[cfg(not(blaze_check))]
thread_local! {
    /// The pools this thread drives ([`Drivers`]), in the order they were made.
    static DRIVEN: RefCell<Vec<*const Drivers>> = const { RefCell::new(Vec::new()) };
}

/// A worker's own: on its thread's stack for as long as the thread works.
struct Local {
    shared: Arc<Shared>,
    index: usize,
    group: usize,
    node: u32,
    deque: Worker<Job>,
    /// How many jobs in a row came from the LIFO slot.
    lifo: Cell<u8>,
    /// The state of the draws of a search (xorshift): where a walk of a group's workers begins,
    /// and a [`Victims::TwoChoices`] pair.
    seed: Cell<u32>,
    victims: Victims,
}

thread_local! {
    static LOCAL: Cell<*const Local> = const { Cell::new(ptr::null()) };
}

/// The worker this thread is, or null. Not inlined, as [`with_caches`] is not: a job that reads
/// it, waits and reads it again must find the worker it resumed on (rule 27).
#[inline(never)]
fn local() -> *const Local {
    LOCAL.get()
}

fn with_local<R>(f: impl FnOnce(&Local) -> R) -> Option<R> {
    let at = local();
    // SAFETY: `work` points it at its own `Local`, which lives until `work` clears it on its
    // way out, on this thread.
    (!at.is_null()).then(|| f(unsafe { &*at }))
}

/// The group of the worker this thread is, or `None` for a thread of no pool.
pub fn group() -> Option<u32> {
    with_local(|local| u32::try_from(local.group).ok()).flatten()
}

/// The NUMA node of the worker this thread is (its CPU's [`Cpu::node`]), or `None` for a
/// thread of no pool.
pub fn node() -> Option<u32> {
    with_local(|local| local.node)
}

impl Pool {
    /// A worker per CPU of `topology`, bound to it when `bind` (pinned on Linux, a QoS class on
    /// macOS; an error elsewhere). An error when a thread cannot be started or bound: the pool
    /// is not made smaller. Its workers steal as [`Stealing::DEFAULT`] says.
    ///
    /// On a target without threads it is [`Pool::driven`]: it starts none, and `bind` is an
    /// error, there being no thread of a worker's own to bind.
    pub fn new(topology: &Topology, bind: bool) -> io::Result<Pool> {
        Pool::with_stealing(topology, bind, Stealing::DEFAULT)
    }

    /// As [`Pool::new`], its workers stealing as `stealing` says: for a bench that compares
    /// [`Victims`], or a run that replays a seed.
    pub fn with_stealing(topology: &Topology, bind: bool, stealing: Stealing) -> io::Result<Pool> {
        Pool::make(topology, bind, stealing, THREADS)
    }

    /// A pool that starts no thread: a worker per CPU of `topology`, each running the loop every
    /// worker runs, on a fiber the calling thread drives. The calling thread runs its jobs
    /// whenever it waits on no fiber ([`crate::sched::wait`], so [`Pool::scope`],
    /// [`Pool::install`] and every query wait of it), a job at a time until its wait ends, and
    /// as the pool is dropped, until every job given it has run. What [`Pool::new`] makes on a
    /// target without threads, the wasm64 seed (docs/future/bootstrap.md, "One thread"); on any
    /// other, the same pool as the seed's, for a test.
    ///
    /// Only the thread that made it drives it: a job queued from another thread waits for that
    /// thread's next wait, which the queueing unparks.
    #[cfg(not(blaze_check))]
    pub fn driven(topology: &Topology) -> io::Result<Pool> {
        Pool::make(topology, false, Stealing::DEFAULT, false)
    }

    /// A pool whose workers have threads of their own when `threaded`, else are driven by the
    /// calling thread ([`Pool::driven`]).
    fn make(
        topology: &Topology,
        bind: bool,
        stealing: Stealing,
        threaded: bool,
    ) -> io::Result<Pool> {
        radix::set_ambient(ambient);
        if !threaded && bind {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "binding a driven pool's workers to CPUs: they have no thread of their own (a target without threads)",
            ));
        }
        if topology.cpus.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a worker pool of no CPU"));
        }
        if CELLS.get().is_none() {
            let made = Cells::new().map_err(|why| {
                // alloc: the error of a pool not made, once
                io::Error::new(io::ErrorKind::OutOfMemory, format!("the pool's job cells: {why}"))
            })?;
            // Another pool made them first: these are unreserved as they drop.
            _ = CELLS.set(made);
        }
        let count = topology.groups() as usize;
        // alloc: here and below, the pool's own tables, made once with it
        let mut members: Vec<Vec<u32>> = vec![Vec::new(); count];
        for (index, cpu) in topology.cpus.iter().enumerate() {
            let index = u32::try_from(index).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "a worker pool of more than 2^32 CPUs")
            })?;
            members[cpu.group as usize].push(index);
        }
        if let Some(empty) = members.iter().position(Vec::is_empty) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                // alloc: the error of a pool not made, once
                format!("group {empty} of the topology has no CPU: groups are numbered densely"),
            ));
        }
        // alloc: made once with the pool
        let deques: Vec<Worker<Job>> = topology.cpus.iter().map(|_| Worker::new_fifo()).collect();
        let slots = deques
            .iter()
            .zip(&topology.cpus)
            .map(|(deque, cpu)| Slot {
                stealer: deque.stealer(),
                next: AtomicPtr::new(ptr::null_mut()),
                sleeping: AtomicBool::new(false),
                group: cpu.group,
                thread: OnceLock::new(),
                parker: blaze_core::wait::Parker::new(),
                ended: AtomicU64::new(0),
                inside: AtomicBool::new(false),
            })
            // alloc: made once with the pool
            .collect();
        let groups = members
            .into_iter()
            .enumerate()
            .map(|(at, workers)| Group {
                inject: Inject::new(),
                later: Inject::new(),
                fast: workers.iter().any(|&it| topology.cpus[it as usize].fast),
                order: topology.steal_order(at as u32).into_boxed_slice(),
                workers: workers.into_boxed_slice(),
                sleepers: AtomicU32::new(0),
            })
            .collect::<Box<[Group]>>();
        let mixed = groups.iter().any(|it| it.fast) && groups.iter().any(|it| !it.fast);
        // alloc: made once with the pool
        let shared = Arc::new(Shared {
            groups,
            slots,
            idle: Idle(AtomicU64::new(0)),
            stop: AtomicBool::new(false),
            mixed,
            stray: Lock::new("graph.pool.stray_panic", None),
            #[cfg(not(blaze_check))]
            sched: crate::sched::Scheduler::new(64),
            #[cfg(not(blaze_check))]
            member: AtomicU32::new(0),
            parked: AtomicUsize::new(0),
            #[cfg(not(blaze_check))]
            stopper: OnceLock::new(),
            #[cfg(not(blaze_check))]
            abandoned: AtomicBool::new(false),
            left: AtomicUsize::new(0),
            #[cfg(not(blaze_check))]
            broken: OnceLock::new(),
            #[cfg(not(blaze_check))]
            waiters: Lock::new("graph.pool.scope_waiters", Vec::new()),
            #[cfg(all(test, not(blaze_check)))]
            park_plant: AtomicU32::new(park_plant::PARKS),
            #[cfg(test)]
            critical: AtomicUsize::new(0),
        });
        #[cfg(not(blaze_check))]
        shared.member.store(
            crate::sched::join_pool(
                &shared.sched,
                wake_parked,
                pool_idle,
                Arc::as_ptr(&shared).expose_provenance(),
            ),
            Ordering::Relaxed,
        );
        if !threaded {
            #[cfg(not(blaze_check))]
            return Ok(Pool::drive(shared, deques, topology, stealing));
            #[cfg(blaze_check)]
            unreachable!("the model checker's targets have threads");
        }
        #[cfg(not(blaze_check))]
        {
            let mut live = LIVE.lock();
            live.retain(|it| it.strong_count() > 0);
            live.push(Arc::downgrade(&shared)); // alloc: a pool's entry, once a pool
            drop(live);
            blaze_mem::tag::heap_wakes_by(wake_for_give_back);
        }
        let (ready_tx, ready_rx) = crossbeam_channel::unbounded::<io::Result<()>>();
        let mut threads = Vec::with_capacity(deques.len());
        let mut failed = None;
        for (index, deque) in deques.into_iter().enumerate() {
            // clone: setup, once per worker thread.
            let (cpu, shared, ready) =
                (topology.cpus[index], Arc::clone(&shared), ready_tx.clone());
            let spawned = thread::Builder::new()
                // alloc: setup, once per worker thread.
                .name(format!("blaze-work-{index}"))
                .stack_size(STACK)
                .spawn(move || {
                    let bound = if bind { bind_to(&cpu) } else { Ok(()) };
                    _ = shared.slots[index].thread.set(thread::current());
                    let ok = bound.is_ok();
                    _ = ready.send(bound);
                    // clone: a reference count, once per worker thread
                    let after = Arc::clone(&shared);
                    if ok {
                        // A worker counts for every heap give-back while its loop runs: a purge
                        // waits for its give-back before it reads the footprint.
                        let _settles = blaze_mem::tag::count_heap_settles();
                        let group = shared.slots[index].group as usize;
                        let local = Local {
                            shared,
                            index,
                            group,
                            node: cpu.node,
                            deque,
                            lifo: Cell::new(0),
                            seed: Cell::new(worker_seed(stealing.seed, index)),
                            victims: stealing.victims,
                        };
                        #[cfg(blaze_check)]
                        work(&local, None);
                        // A panic of the loop itself (a job's is the job's failure, caught where
                        // it runs) ends this worker: the pool stops for it, so no one waits on
                        // jobs it would have run (rule 20).
                        #[cfg(not(blaze_check))]
                        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| work(&local, None))) {
                            let said = panic
                                .downcast_ref::<String>()
                                .map(String::as_str)
                                .or_else(|| panic.downcast_ref::<&str>().copied())
                                .unwrap_or("a panic of no message");
                            // alloc: the error of a pool that stops, once
                            after.broke(index, Unable::Loop(said.to_owned()));
                        }
                    }
                    // Its loop returned, or never began: the stopper waits for each ([`Pool::stop`]).
                    after.left.fetch_add(1, Ordering::SeqCst);
                    #[cfg(not(blaze_check))]
                    {
                        after.unpark_stopper();
                        // A thread blocked on a scope of a broken pool fails once no worker is
                        // left to run the scope's jobs: each that ends has it look.
                        if after.broken.get().is_some() {
                            after.wake_waiters();
                        }
                    }
                });
            match spawned {
                Ok(handle) => threads.push(handle),
                Err(why) => {
                    failed = Some(why);
                    break;
                }
            }
        }
        drop(ready_tx);
        for _ in 0..threads.len() {
            match crate::wait::wait(
                crate::wait::WaitOn::Channel("the pool's workers started"),
                None,
                || ready_rx.recv(),
            ) {
                Ok(Ok(())) => {}
                Ok(Err(why)) => _ = failed.get_or_insert(why),
                Err(_) => break,
            }
        }
        // clone: the pool keeps the topology it was made of, once
        let pool = Pool {
            shared,
            threads,
            stopped: false,
            #[cfg(not(blaze_check))]
            driven: None,
            topology: topology.clone(),
        };
        match failed {
            Some(why) => {
                drop(pool);
                Err(why)
            }
            None => Ok(pool),
        }
    }

    /// The pool of `shared`, its workers on fibers this thread drives ([`Pool::driven`]).
    #[cfg(not(blaze_check))]
    fn drive(
        shared: Arc<Shared>,
        deques: Vec<Worker<Job>>,
        topology: &Topology,
        stealing: Stealing,
    ) -> Pool {
        let me = thread::current();
        let workers = deques
            .into_iter()
            .enumerate()
            .map(|(index, deque)| {
                // clone: setup, once per worker
                let (cpu, shared) = (topology.cpus[index], Arc::clone(&shared));
                // A job queued from another thread unparks the driving one, as a parked worker
                // of its own thread would be.
                // clone: a reference count, once per worker
                _ = shared.slots[index].thread.set(me.clone());
                crate::sched::Driven::new(move |turns| {
                    let group = shared.slots[index].group as usize;
                    work(
                        &Local {
                            shared,
                            index,
                            group,
                            node: cpu.node,
                            deque,
                            lifo: Cell::new(0),
                            seed: Cell::new(worker_seed(stealing.seed, index)),
                            victims: stealing.victims,
                        },
                        Some(turns),
                    );
                })
            })
            // alloc: made once with the pool
            .collect::<Box<[_]>>();
        // alloc: made once with the pool, its address listed until it drops
        let drivers = Box::new(Drivers { workers: RefCell::new(workers), owner: me.id() });
        // alloc: once per driven pool
        DRIVEN.with_borrow_mut(|it| it.push(ptr::from_ref(&*drivers)));
        // clone: the pool keeps the topology it was made of, once
        Pool {
            shared,
            threads: Vec::new(),
            stopped: false,
            driven: Some(drivers),
            topology: topology.clone(),
        }
    }

    pub fn topology(&self) -> &Topology {
        &self.topology
    }

    /// Why the pool stopped for an error, if it did: one of its workers could not park, or
    /// its loop panicked. Its workers end, and every scope on it fails with this.
    #[cfg(not(blaze_check))]
    pub fn broken(&self) -> Option<&Broken> {
        self.shared.broken.get()
    }

    pub fn workers(&self) -> usize {
        self.shared.slots.len()
    }

    /// This thread's index among this pool's workers, if it is one of them: for tables kept
    /// by worker, read and written without a lock. A job that waits may resume on another
    /// worker, and another job runs on this one meanwhile, so such a table's entry must not be
    /// held across a wait.
    pub fn worker(&self) -> Option<u32> {
        with_local(|local| {
            Arc::ptr_eq(&local.shared, &self.shared)
                .then(|| u32::try_from(local.index).ok())
                .flatten()
        })
        .flatten()
    }

    /// Whether every worker sleeps with nothing queued: no job of the pool runs or is
    /// runnable, though some may be parked on a wait ([`Pool::parked`]).
    pub fn idle(&self) -> bool {
        let shared = &*self.shared;
        shared.idle.0.load(Ordering::SeqCst) / SLEEPING == shared.slots.len() as u64
            && !shared.any_work()
    }

    /// How many of the pool's jobs are parked on a wait now.
    pub fn parked(&self) -> usize {
        self.shared.parked.load(Ordering::Acquire)
    }

    /// How many of the pool's jobs ran to their end so far: it grows only as jobs finish, never
    /// as a worker searches, parks, checks for a stall or switches fibers, nor as a job parks
    /// or resumes. What a stall watchdog counts as the pool's progress. 0 under the model
    /// checker, whose jobs run on their worker's stack with no host to count them.
    pub fn ended(&self) -> u64 {
        self.shared.slots.iter().map(|slot| slot.ended.load(Ordering::Relaxed)).sum()
    }

    /// How many workers run a job's code now (a job resumed and not yet parked or ended): work
    /// under way though no job ends, as one long compile is, which a stall watchdog counts as
    /// moving. A job blocked outside the scheduler (a lock, a syscall) counts too; a worker
    /// that searches, sleeps or switches fibers never does. 0 under the model checker.
    pub fn working(&self) -> usize {
        self.shared.slots.iter().filter(|slot| slot.inside.load(Ordering::Relaxed)).count()
    }

    /// Runs `run` on a worker near `group` (a group this pool does not have is no group), then
    /// `done` on the same thread with what came of it: its value, or its panic, caught. The
    /// pool goes on after a panic; a panic of `done` is kept and raised when the pool is
    /// dropped. It goes behind what is queued for its group.
    pub fn submit<R: 'static>(
        &self,
        near: Option<u32>,
        run: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(thread::Result<R>) + Send + 'static,
    ) {
        self.shared.push(
            near,
            Job::new(move |started: Started| {
                done(started.and_then(|()| catch_unwind(AssertUnwindSafe(run))))
            }),
        );
    }

    /// As [`Pool::submit`], but called on one of this pool's workers the job runs next on it
    /// (its LIFO slot), hot in its cache; elsewhere it is [`Pool::submit`] near no group.
    pub fn submit_next<R: 'static>(
        &self,
        run: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(thread::Result<R>) + Send + 'static,
    ) {
        let job = Job::new(move |started: Started| {
            done(started.and_then(|()| catch_unwind(AssertUnwindSafe(run))))
        });
        if let Some(job) = push_next(&self.shared, None, job) {
            self.shared.push(None, job);
        }
    }

    /// As [`Pool::submit`], for a job on its request's critical path: on a pool of fast and
    /// slow groups it goes to `near` if that group is fast, else to the least loaded fast
    /// group. Where every group is fast it is placed as [`Pool::submit`] places it.
    pub fn submit_critical<R: 'static>(
        &self,
        near: Option<u32>,
        run: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(thread::Result<R>) + Send + 'static,
    ) {
        self.push_placed(
            near,
            true,
            Job::new(move |started: Started| {
                done(started.and_then(|()| catch_unwind(AssertUnwindSafe(run))))
            }),
        );
    }

    /// As [`Pool::submit`], for a job of `class`: a critical one placed as
    /// [`Pool::submit_critical`] places it, a background one queued apart ([`Class::Background`]).
    pub fn submit_as<R: 'static>(
        &self,
        near: Option<u32>,
        class: Class,
        run: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(thread::Result<R>) + Send + 'static,
    ) {
        #[cfg(test)]
        if class == Class::Critical {
            self.shared.critical.fetch_add(1, Ordering::Relaxed);
        }
        self.shared.push_as(
            near,
            class,
            Job::new(move |started: Started| {
                done(started.and_then(|()| catch_unwind(AssertUnwindSafe(run))))
            }),
        );
    }

    /// Every background job queued now becomes a normal one, behind those queued: what is
    /// ahead of demand is wanted now (a write waits on the readers whose jobs they are).
    pub fn promote_background(&self) {
        self.shared.promote();
    }

    /// `job` near `near`, placed as [`Pool::submit_critical`] places a job when `critical`,
    /// else as [`Pool::submit`] does.
    pub(crate) fn push_placed(&self, near: Option<u32>, critical: bool, job: Job) {
        #[cfg(test)]
        if critical {
            self.shared.critical.fetch_add(1, Ordering::Relaxed);
        }
        self.shared.push_to(self.shared.place(near, critical), job);
    }

    /// How many jobs were given as critical.
    #[cfg(test)]
    pub(crate) fn critical_submitted(&self) -> usize {
        self.shared.critical.load(Ordering::Relaxed)
    }

    /// Runs `f`, which may submit jobs that borrow what outlives this call, and returns once
    /// every job it submitted has run. The wait holds no worker: a pool's job switches out, a
    /// thread of no pool parks ([`crate::pump`]).
    pub fn scope<'env, R>(&self, f: impl FnOnce(&Scope<'env>) -> R) -> R {
        scope_in(&self.shared, f)
    }

    /// Runs `f` on one of this pool's workers and gives back what it made, its panic raised
    /// here: the loops of [`crate::par`] it runs split across this pool.
    pub fn install<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        let mut f = Some(f);
        let f = &mut f;
        let mut out = None;
        self.scope(|scope| {
            scope.submit(
                None,
                move || f.take().expect("a scope's job runs once")(),
                |it| out = Some(it),
            );
        });
        match out.expect("a scope runs its job before it returns") {
            Ok(it) => it,
            Err(panic) => resume_unwind(panic),
        }
    }
}

/// [`Pool::scope`] on the pool this thread is a worker of, or on the process's
/// ([`crate::exec::shared`]) from a thread of no pool.
pub(crate) fn scope_here<'env, R>(f: impl FnOnce(&Scope<'env>) -> R) -> R {
    let at = local();
    // SAFETY: as in `with_local`: `work` points it at its own `Local`, which lives until `work`
    // returns, and this call is on that thread, inside `work`.
    match unsafe { at.as_ref() } {
        Some(local) => scope_in(&local.shared, f),
        None => crate::exec::shared().scope(f),
    }
}

/// The workers of the pool [`scope_here`] runs on.
pub(crate) fn threads_here() -> usize {
    with_local(|local| local.shared.slots.len()).unwrap_or_else(|| crate::exec::shared().workers())
}

/// [`Pool::scope`] on the pool `shared` is of, which outlives the call. The caller's job opens
/// a scope in the scheduler, joined with the pool's ([`crate::sched::open_scope_on`]), so a job
/// of it that waits for what the caller makes gives way rather than wait for a caller that
/// waits for it, and a stall check of the scope's wait sees whether the pool is idle.
///
/// # Panics
///
/// With the pool's [`Broken`] where the pool stopped for an error: before `f` runs when it had
/// by then, else once the scope's jobs have run, or, where no worker is left to run them, once
/// the last worker has ended (those jobs are never run, and what they borrow is never used).
fn scope_in<'env, R>(shared: &Shared, f: impl FnOnce(&Scope<'env>) -> R) -> R {
    // A pool stopped for an error runs nothing more: the scope fails before it gives it a job.
    #[cfg(not(blaze_check))]
    if let Some(broken) = shared.broken.get() {
        panic!("{broken}");
    }
    let pending = Pending {
        count: AtomicUsize::new(1),
        finished: AtomicBool::new(false),
        background: AtomicBool::new(false),
        #[cfg(not(blaze_check))]
        counter: crate::sched::open_scope_on(&shared.sched),
        #[cfg(blaze_check)]
        owner: thread::current(),
    };
    let scope = Scope { shared: ptr::from_ref(shared), pending: &pending, _env: PhantomData };
    let result = catch_unwind(AssertUnwindSafe(|| f(&scope)));
    if pending.background.load(Ordering::Relaxed) {
        shared.promote();
    }
    drop(Left(&raw const pending));
    #[cfg(not(blaze_check))]
    {
        // A thread on no fiber parks for the wait ([`block`]), which reads what it waits on
        // here: the pool's break wakes it, and it ends a wait no worker is left to end. A job
        // switches out instead, and may resume on another thread: it names nothing here.
        let waiting = ScopeWait {
            shared: ptr::from_ref(shared),
            counter: pending.counter,
            let_go: Cell::new(false),
        };
        let named = crate::sched::fiber::current_job().is_none().then(|| Named::new(&waiting));
        let waited = crate::wait::wait(crate::wait::WaitOn::Scope(pending.counter.0), None, || {
            crate::sched::wait(crate::sched::Wait::Counter(pending.counter))
        });
        drop(named);
        if let Err(cycle) = waited {
            panic!(
                "a scope's caller closed a cycle of waits, which its scope's jobs would have: {cycle:?}"
            );
        }
        // Whether the pool broke and its last worker ended with jobs of this scope never run:
        // the wait ended as `block` let go of the scope's count, and `finished` is never set.
        // Read in this order: a worker has ended only after every job it ran said so.
        let lost = shared.ended_broken().is_some() && !pending.finished.load(Ordering::Acquire);
        if !lost {
            // The last job released the counter just before it says so, as the last thing it
            // does.
            crate::spin::SpinWait::until("a scope's last job to let go of it", || {
                pending.finished.load(Ordering::Acquire)
            });
        }
        crate::sched::close_scope(pending.counter);
        // A scope on a pool that stopped for an error fails with it, whatever its jobs made:
        // the process does not go on as if its pool were whole.
        if let Some(broken) = shared.broken.get() {
            drop(result);
            panic!("{broken}");
        }
    }
    #[cfg(blaze_check)]
    while !pending.finished.load(Ordering::Acquire) {
        thread::park();
    }
    result.unwrap_or_else(|panic| resume_unwind(panic))
}

/// `job(0..jobs)` as jobs of a scope on `shared`'s pool; the first panic of one is raised once
/// all have run.
// dyn: `radix::Workers`'s signature: a sort's pass, one call per job
fn run_jobs(shared: &Shared, jobs: usize, job: &(dyn Fn(usize) + Sync)) {
    // dyn: a panic's payload, as `catch_unwind` gives it
    let failed: Lock<Option<Box<dyn Any + Send>>> = Lock::new("graph.pool.scope_panic", None);
    scope_in(shared, |scope| {
        for at in 0..jobs {
            scope.submit(
                None,
                move || job(at),
                |it| {
                    if let Err(panic) = it {
                        failed.lock().get_or_insert(panic);
                    }
                },
            );
        }
    });
    if let Some(panic) = failed.into_inner() {
        resume_unwind(panic);
    }
}

// SAFETY: `run_jobs` submits each job once to a scope, which returns only once every job of it
// has run (a job's panic is caught and given to its `done`).
unsafe impl radix::Workers for Pool {
    fn threads(&self) -> usize {
        self.workers()
    }

    // dyn: the trait's signature, as for `run_jobs`
    fn run(&self, jobs: usize, job: &(dyn Fn(usize) + Sync)) {
        run_jobs(&self.shared, jobs, job);
    }
}

/// The pool of the worker a sort runs on, for [`radix`]'s parallel passes.
struct Here<'a>(&'a Shared);

// SAFETY: `run_jobs` submits each job once to a scope of the worker's pool, which returns only once
// every job of it has run; the worker's `Local` keeps the pool's `Shared` for as long as it works.
unsafe impl radix::Workers for Here<'_> {
    fn threads(&self) -> usize {
        self.0.slots.len()
    }

    // dyn: the trait's signature, as for `run_jobs`
    fn run(&self, jobs: usize, job: &(dyn Fn(usize) + Sync)) {
        run_jobs(self.0, jobs, job);
    }
}

/// [`radix::Ambient`]: the pool this thread is a worker of.
// dyn: `radix::Ambient`'s signature, called once per parallel sort
fn ambient(with: &mut dyn FnMut(&dyn radix::Workers)) -> bool {
    with_local(|local| with(&Here(&*local.shared))).is_some()
}

impl Pool {
    /// Stops the pool: its workers run every job given it and every parked one once woken,
    /// then end, and are joined. Bounded (rule 18), as its drop is with [`STOP_BOUND`]:
    ///
    /// - a parked job whose waits lead to the calling thread, which holds what it claimed until
    ///   this returns (a key its panic left held), can never wake: it wakes at once, to fail with
    ///   a [`crate::sched::Stopped`] naming its wait and the cycle;
    /// - once the pool has gone `bound` with jobs parked and none ending, a parked job resumed
    ///   or a worker running a job's code, every job still parked in the wait graph fails so;
    /// - if as long again goes by with no job ending after that, the stop gives up.
    ///
    /// A job whose code runs holds its worker, and the stop waits for it as for any job: the
    /// bound is on waits, which only the scheduler can end.
    ///
    /// # Errors
    ///
    /// [`Unstopped`] when it gave up, naming every wait still parked. Its workers end, though
    /// jobs are parked, and the pool's queues are leaked: a parked job may still be woken into
    /// them.
    pub fn stop(mut self, bound: std::time::Duration) -> Result<(), Unstopped> {
        self.end(bound)
    }

    /// Stops the pool, once: [`Pool::stop`], and its drop.
    fn end(&mut self, bound: std::time::Duration) -> Result<(), Unstopped> {
        if std::mem::replace(&mut self.stopped, true) {
            return Ok(());
        }
        let shared = &*self.shared;
        #[cfg(not(blaze_check))]
        {
            _ = shared.stopper.set(thread::current());
        }
        shared.stop.store(true, Ordering::SeqCst);
        shared.unpark_workers();
        #[cfg(not(blaze_check))]
        if let Some(drivers) = self.driven.take() {
            let ended = drivers.end(shared, bound);
            if ended.is_err() {
                // Its workers end at their next turn, which its thread still gives them: their
                // fibers and queues live on.
                shared.abandoned.store(true, Ordering::SeqCst);
                std::mem::forget(drivers);
                std::mem::forget(Arc::clone(&self.shared));
            } else {
                crate::sched::leave_pool(&shared.sched, shared.member.load(Ordering::Relaxed));
            }
            return ended;
        }
        let me = thread::current().id();
        // A pool stopped by a job of its own cannot wait for the worker that runs the job.
        let own = self.threads.iter().any(|it| it.thread().id() == me);
        #[cfg(not(blaze_check))]
        {
            // Each worker's loop returns before its thread ends, and it wakes this one as it does.
            let others = self.threads.len() - usize::from(own);
            let ended = || shared.left.load(Ordering::SeqCst) >= others;
            let waited = stop_bounded(shared, bound, &ended, &|recheck, until| {
                if recheck() {
                    return;
                }
                let left = until.saturating_sub(crate::sched::now());
                crate::wait::wait(
                    crate::wait::WaitOn::Channel("a stopping pool's workers"),
                    None,
                    || {
                        thread::park_timeout(std::time::Duration::from_nanos(left));
                    },
                );
            });
            if let Err(unstopped) = waited {
                shared.abandoned.store(true, Ordering::SeqCst);
                shared.unpark_workers();
                // Its member stays in the scheduler, which may wake a parked job into its queues
                // after this: they live on, as do its workers until they see the stop gave up.
                std::mem::forget(Arc::clone(&self.shared));
                self.threads.clear();
                return Err(unstopped);
            }
        }
        for handle in self.threads.drain(..) {
            if handle.thread().id() != me {
                _ = handle.join();
            }
        }
        #[cfg(not(blaze_check))]
        if !own {
            crate::sched::leave_pool(&shared.sched, shared.member.load(Ordering::Relaxed));
        }
        #[cfg(blaze_check)]
        let _ = (own, bound);
        Ok(())
    }
}

/// A pool's stop that did not end within its bound ([`Pool::stop`]): what is still parked, and
/// what it waits for.
#[derive(Clone, Debug)] // clone: an error, off every hot path
pub struct Unstopped {
    /// How long it waited with no job ending, after it had failed every job parked then.
    pub waited_ns: u64,
    /// The pool's jobs still parked.
    pub parked: usize,
    /// Its workers that run a job's code.
    pub working: usize,
    /// What each of its jobs that waits in the wait graph waits for (a timed wait is in none).
    #[cfg(not(blaze_check))]
    pub waits: crate::sched::Waits,
}

impl std::fmt::Display for Unstopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a pool's stop gave up: {:.1} s went by with no job ending after it failed every job parked, and {} of its jobs \
             are still parked and {} of its workers run a job's code; its workers end and its queues are leaked",
            self.waited_ns as f64 / 1e9,
            self.parked,
            self.working
        )?;
        #[cfg(not(blaze_check))]
        write!(f, ": {}", self.waits)?;
        Ok(())
    }
}

impl std::error::Error for Unstopped {}

/// What a pool's stop does next ([`StopClock::advance_clock`]).
#[cfg(not(blaze_check))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StopStep {
    /// Wait for the pool to move, until this time at the latest.
    Wait { until: u64 },
    /// It went its bound with jobs parked and none moving: every job still parked fails.
    FailParked { waited_ns: u64 },
    /// Failing them moved nothing within the bound: the stop gives up.
    GiveUp { waited_ns: u64 },
}

/// The bound of a pool's stop (rule 18), a sans-IO core (rule 14): told the time on the
/// injected clock and whether the pool moved since it was last told, it says what the stop does
/// next. The bound runs from the pool's last progress.
#[cfg(not(blaze_check))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct StopClock {
    bound: u64,
    since: u64,
    /// It failed what was parked, and nothing moved since.
    failed: bool,
}

#[cfg(not(blaze_check))]
impl StopClock {
    pub(crate) fn new(now: u64, bound: std::time::Duration) -> StopClock {
        StopClock {
            bound: u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX),
            since: now,
            failed: false,
        }
    }

    pub(crate) fn advance_clock(&mut self, now: u64, moved: bool) -> StopStep {
        if moved {
            self.since = now;
            self.failed = false;
        }
        let due = self.since.saturating_add(self.bound);
        if now < due {
            return StopStep::Wait { until: due };
        }
        let waited_ns = now - self.since;
        self.since = now;
        if std::mem::replace(&mut self.failed, true) {
            StopStep::GiveUp { waited_ns }
        } else {
            StopStep::FailParked { waited_ns }
        }
    }
}

/// The stopping pool of `shared`'s wait for `ended` (its workers' end), bounded as
/// [`Pool::stop`] says: `wait(recheck, until)` waits for the pool to move, until `until` on
/// [`crate::sched::now`]'s clock at the latest, calling `recheck` (true once `ended`) as it
/// can; `recheck` wakes the parked jobs whose waits lead to this thread.
///
/// The thread's job, if it is in the wait graph and of another pool or a client, waits in the
/// graph for the pool's jobs meanwhile ([`crate::sched::stop_begins`]): a wait that would close
/// a cycle through the stop is refused, and a stall of what waits on this thread (a job of
/// another pool waiting on a key it holds) is found, as it is waiting, not running.
#[cfg(not(blaze_check))]
fn stop_bounded(
    shared: &Shared,
    bound: std::time::Duration,
    ended: &dyn Fn() -> bool,
    wait: &dyn Fn(&dyn Fn() -> bool, u64),
) -> Result<(), Unstopped> {
    use crate::sched::StopReason;
    /// Ends the stop's wait in the graph, as the stop returns or unwinds.
    struct Linked<'a>(&'a crate::sched::Scheduler, crate::sched::JobId);
    impl Drop for Linked<'_> {
        fn drop(&mut self) {
            crate::sched::stop_ended(self.0, self.1);
        }
    }
    let member = shared.member.load(Ordering::Relaxed);
    let in_graph = crate::sched::job_in_graph();
    let _linked = in_graph
        .as_ref()
        .filter(|(job, theirs)| crate::sched::stop_begins(&shared.sched, member, *job, theirs))
        .map(|&(job, _)| Linked(&shared.sched, job));
    let dropper = in_graph.map(|(job, _)| job);
    let panicking = thread::panicking();
    let recheck = || {
        if ended() {
            return true;
        }
        // Looked for as the pool goes idle with jobs parked: a job parks only as its worker
        // goes on to the next, and a worker with none wakes this thread.
        if let Some(dropper) = dropper
            && shared.parked.load(Ordering::Acquire) != 0
            && !shared.any_work()
        {
            crate::sched::stop_pool(
                &shared.sched,
                member,
                StopReason::Dropper { dropper, panicking },
            );
        }
        false
    };
    let mut clock = StopClock::new(crate::sched::now(), bound);
    let mut seen = shared.progress();
    loop {
        if recheck() {
            return Ok(());
        }
        let progress = shared.progress();
        let moved = progress != seen || shared.busy();
        seen = progress;
        match clock.advance_clock(crate::sched::now(), moved) {
            StopStep::Wait { until } => wait(&recheck, until),
            StopStep::FailParked { waited_ns } => {
                crate::sched::stop_pool(&shared.sched, member, StopReason::Bound { waited_ns });
            }
            StopStep::GiveUp { waited_ns } => {
                return Err(Unstopped {
                    waited_ns,
                    parked: shared.parked.load(Ordering::Acquire),
                    working: shared
                        .slots
                        .iter()
                        .filter(|slot| slot.inside.load(Ordering::Relaxed))
                        .count(),
                    waits: crate::sched::waits_of(&shared.sched, member),
                });
            }
        }
    }
}

impl Drop for Pool {
    /// Stops the pool as [`Pool::stop`] does with [`STOP_BOUND`]; a stop that gave up panics
    /// with its [`Unstopped`], unless the thread is unwinding already.
    fn drop(&mut self) {
        #[cfg(not(blaze_check))]
        let ended = self.end(STOP_BOUND);
        #[cfg(blaze_check)]
        let ended = self.end(std::time::Duration::MAX);
        if let Err(unstopped) = ended {
            if thread::panicking() {
                // A second panic would abort: the first goes on up, and this says what was left.
                eprintln!("{unstopped}");
            } else {
                panic!("{unstopped}");
            }
        }
        if let Some(panic) = self.shared.stray.lock().take()
            && !thread::panicking()
        {
            resume_unwind(panic);
        }
    }
}

/// Jobs that borrow what lives for `'env`, of [`Pool::scope`].
pub struct Scope<'env> {
    shared: *const Shared,
    pending: *const Pending,
    _env: PhantomData<&'env mut &'env ()>,
}

// SAFETY: a scope only pushes jobs (lock-free, from any thread) and counts them in `Pending`'s
// atomics; the pool and `Pending` outlive it, as `Pool::scope` keeps them.
unsafe impl Send for Scope<'_> {}
// SAFETY: every method of a `&Scope` only pushes a job (lock-free) or counts in `Pending`'s
// atomics, both safe from many threads at once; the pool and `Pending` outlive the scope.
unsafe impl Sync for Scope<'_> {}

/// A scope's count, on the stack of its call: 128-byte aligned, since every job of it writes
/// it once.
#[repr(align(128))]
struct Pending {
    /// Jobs submitted and not done, and one for the call while it runs `f`.
    count: AtomicUsize,
    /// Set by whoever brought `count` to 0, as the last thing it does with `Pending`: the call
    /// returns only once it is.
    finished: AtomicBool,
    /// A job of the scope was given as [`Class::Background`].
    background: AtomicBool,
    /// The scope's counter in the scheduler, held until `count` is 0; the jobs of it that wait
    /// count towards it.
    #[cfg(not(blaze_check))]
    counter: crate::sched::CounterId,
    #[cfg(blaze_check)]
    owner: Thread,
}

/// Counts a job of a scope as run, once everything it borrowed is no longer used.
struct Left(*const Pending);

// SAFETY: `Pending` is atomics and a counter's id (or a `Thread`), shared by design.
unsafe impl Send for Left {}

impl Drop for Left {
    fn drop(&mut self) {
        // SAFETY: the scope's call waits for `finished`, which only the last of its jobs sets,
        // here, and does nothing with `Pending` after.
        let pending = unsafe { &*self.0 };
        if pending.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            #[cfg(not(blaze_check))]
            {
                crate::sched::release(pending.counter);
                pending.finished.store(true, Ordering::Release);
            }
            #[cfg(blaze_check)]
            {
                // clone: a reference count, so the owner is woken after `Pending` may be gone
                let owner = pending.owner.clone();
                pending.finished.store(true, Ordering::Release);
                owner.unpark();
            }
        }
    }
}

impl<'env> Scope<'env> {
    /// As [`Pool::submit`], for a job that borrows what lives for `'env`. Submitted on one of
    /// the pool's workers near its own group (or none), it runs next on that worker (its LIFO
    /// slot), unless the worker's other workers take it first.
    pub fn submit<R: 'env>(
        &self,
        near: Option<u32>,
        run: impl FnOnce() -> R + Send + 'env,
        done: impl FnOnce(thread::Result<R>) + Send + 'env,
    ) {
        self.submit_as(near, Class::Normal, run, done);
    }

    /// As [`Scope::submit`], for a job of `class`: a normal one to this worker's LIFO slot as
    /// there, a critical or background one queued as [`Pool::submit_as`] queues it. A scope
    /// with a background job promotes the pool's ([`Pool::promote_background`]) once its call
    /// waits on its jobs, so the call never waits behind the queue.
    pub fn submit_as<R: 'env>(
        &self,
        near: Option<u32>,
        class: Class,
        run: impl FnOnce() -> R + Send + 'env,
        done: impl FnOnce(thread::Result<R>) + Send + 'env,
    ) {
        if class == Class::Background {
            // SAFETY: as below.
            unsafe { &*self.pending }.background.store(true, Ordering::Relaxed);
        }
        // SAFETY: `Pool::scope` keeps `Pending` until every job counted in it is done.
        unsafe { &*self.pending }.count.fetch_add(1, Ordering::Relaxed);
        let left = Left(self.pending);
        #[cfg(not(blaze_check))]
        // SAFETY: as above.
        let counter = unsafe { &*self.pending }.counter;
        // The scope's work is the work of whoever opened it: its bytes are charged to that
        // task's memory account wherever the job runs (`crate::arena::account`), so a task's
        // peak is its fan-out's too. The account outlives the job: the scope's call waits for
        // every job, and the task closes its account only after the call returns.
        let account = crate::arena::current();
        let job = move |started: Started| {
            let _left = left;
            #[cfg(not(blaze_check))]
            crate::sched::count_towards(counter);
            done(started.and_then(|()| {
                catch_unwind(AssertUnwindSafe(|| crate::arena::charged_to(account, run)))
            }));
        };
        // SAFETY: the job borrows only what lives for `'env`, which outlives the call of
        // `Pool::scope` this scope is of. That call returns (or unwinds) only once `finished`
        // is set, which `Left` does after `run` and `done` were consumed, so nothing of `'env`
        // is used after it. The pool runs every job it was given before its workers stop, and
        // it outlives the call, which borrows it; a job never run is never counted out, and
        // the call waits rather than return.
        let job = unsafe { Job::erase(job) };
        // SAFETY: the pool outlives the call of `Pool::scope`, which borrows it.
        let shared = unsafe { &*self.shared };
        match class {
            Class::Normal => {
                if let Some(job) = push_next(shared, near, job) {
                    shared.push(near, job);
                }
            }
            Class::Critical | Class::Background => shared.push_as(near, class, job),
        }
    }
}

/// Gives `job` to this thread's LIFO slot when it is a worker of `shared`'s pool and `near`
/// is its group or none; otherwise gives it back.
fn push_next(shared: &Shared, near: Option<u32>, job: Job) -> Option<Job> {
    let at = local();
    // SAFETY: as in `with_local`.
    match unsafe { at.as_ref() } {
        Some(local)
            if ptr::eq(Arc::as_ptr(&local.shared), shared)
                && near.is_none_or(|it| it as usize == local.group) =>
        {
            local.push_next(job);
            None
        }
        _ => Some(job),
    }
}

/// The job in a LIFO slot, taken; nothing written when it is empty.
fn take(next: &AtomicPtr<Header>) -> Option<Job> {
    if next.load(Ordering::Relaxed).is_null() {
        return None;
    }
    NonNull::new(next.swap(ptr::null_mut(), Ordering::Acquire)).map(Job)
}

impl Shared {
    fn unpark_workers(&self) {
        for slot in &self.slots {
            slot.parker.unpark();
            if let Some(thread) = slot.thread.get() {
                thread.unpark();
            }
        }
    }

    #[cfg(not(blaze_check))]
    fn unpark_stopper(&self) {
        if let Some(thread) = self.stopper.get() {
            thread.unpark();
        }
    }

    /// Stops the pool for an error: its worker `worker` could not go on, as `why` says. The
    /// first such is kept ([`Pool::broken`]) and said on standard error, once; every worker is
    /// woken, to run what is queued and end without parking again ([`work`]), and every thread
    /// blocked on a scope of the pool, to fail with it once it can ([`block`]).
    #[cfg(not(blaze_check))]
    #[cold]
    fn broke(&self, worker: usize, why: Unable) {
        let broken =
            Broken { worker, group: self.slots[worker].group, workers: self.slots.len(), why };
        if self.broken.set(broken).is_ok()
            && let Some(broken) = self.broken.get()
        {
            eprintln!("error: {broken}");
        }
        self.unpark_workers();
        self.wake_waiters();
    }

    /// Wakes every thread blocked on a scope of the pool, to look at the pool again.
    #[cfg(not(blaze_check))]
    fn wake_waiters(&self) {
        // Taken out of the lock before any wake: a wake is a system call.
        // alloc: a few threads' handles, as a pool breaks and as its workers end
        let waiters: Vec<Thread> = self.waiters.lock().clone();
        for thread in waiters {
            thread.unpark();
        }
    }

    /// Why the pool stopped for an error, once every worker of it has ended: nothing queued
    /// in it or parked on it runs again. Only a pool with threads of its own breaks.
    #[cfg(not(blaze_check))]
    fn ended_broken(&self) -> Option<&Broken> {
        let broken = self.broken.get()?;
        (self.left.load(Ordering::SeqCst) >= self.slots.len()).then_some(broken)
    }

    /// What a stop counts as the pool's progress: the jobs ended, and those parked.
    #[cfg(not(blaze_check))]
    fn progress(&self) -> (u64, usize) {
        let ended = self.slots.iter().map(|slot| slot.ended.load(Ordering::Relaxed)).sum();
        (ended, self.parked.load(Ordering::Acquire))
    }

    /// Whether a worker runs a job's code or a job is queued: the pool moves.
    #[cfg(not(blaze_check))]
    fn busy(&self) -> bool {
        self.slots.iter().any(|slot| slot.inside.load(Ordering::Relaxed)) || self.any_work()
    }

    fn push(&self, near: Option<u32>, job: Job) {
        self.push_to(self.place(near, false), job);
    }

    fn push_to(&self, group: usize, job: Job) {
        self.groups[group].inject.push(job);
        self.announce(group);
    }

    /// The group a job near `near` (a group this pool does not have is none) is queued on: a
    /// critical one, on a pool of fast and slow groups, only on a fast group.
    fn place(&self, near: Option<u32>, critical: bool) -> usize {
        let only_fast = critical && self.mixed;
        near.map(|it| it as usize)
            .filter(|&it| it < self.groups.len() && (!only_fast || self.groups[it].fast))
            .unwrap_or_else(|| self.least_loaded(only_fast))
    }

    /// After a job is queued: wakes a parked worker, of `group` first, when none searches. A
    /// worker about to park says so, then looks for work once more; one of the two sees the
    /// other.
    fn announce(&self, group: usize) {
        fence(Ordering::SeqCst);
        let word = self.idle.0.load(Ordering::SeqCst);
        if searching(word) == 0 && sleeping(word) > 0 {
            self.wake(group);
        }
    }

    /// The group with the least queued and awake per worker (of the fast ones alone when
    /// `only_fast`), the fast ones first on a tie: read from what idling and queueing already
    /// keep, so a job adds no count of its own.
    fn least_loaded(&self, only_fast: bool) -> usize {
        if self.groups.len() == 1 {
            return 0;
        }
        least(self.groups.len(), only_fast, |at| {
            let group = &self.groups[at];
            let workers = group.workers.len() as u64;
            let asleep = u64::from(group.sleepers.load(Ordering::Relaxed)).min(workers);
            (group.inject.len() as u64 + workers - asleep, workers, group.fast)
        })
    }

    /// Wakes one parked worker, of `group` first, then of the groups that steal from it first,
    /// as a searcher: the next job queued wakes no other while it looks.
    fn wake(&self, group: usize) {
        for &at in self.groups[group].order.iter() {
            let group = &self.groups[at as usize];
            if group.sleepers.load(Ordering::SeqCst) == 0 {
                continue;
            }
            for &worker in group.workers.iter() {
                let slot = &self.slots[worker as usize];
                if slot.sleeping.load(Ordering::Relaxed)
                    && slot
                        .sleeping
                        .compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed)
                        .is_ok()
                {
                    group.sleepers.fetch_sub(1, Ordering::SeqCst);
                    self.idle.0.fetch_add(SEARCHING.wrapping_sub(SLEEPING), Ordering::SeqCst);
                    #[cfg(not(blaze_check))]
                    slot.parker.unpark();
                    #[cfg(blaze_check)]
                    if let Some(thread) = slot.thread.get() {
                        thread.unpark();
                    }
                    return;
                }
            }
        }
    }

    /// `job` of `class` near `near`: as [`Pool::submit_critical`] places a critical one, as
    /// [`Pool::submit`] a normal one, and a background one to its group's [`Group::later`].
    fn push_as(&self, near: Option<u32>, class: Class, job: Job) {
        match class {
            Class::Critical => self.push_to(self.place(near, true), job),
            Class::Normal => self.push(near, job),
            Class::Background => {
                let group = self.place(near, false);
                self.groups[group].later.push(job);
                self.announce(group);
            }
        }
    }

    /// Every background job queued becomes a normal one, behind those queued.
    fn promote(&self) {
        for (at, group) in self.groups.iter().enumerate() {
            if !group.later.is_empty() {
                group.later.move_to(&group.inject);
                self.announce(at);
            }
        }
    }

    fn any_work(&self) -> bool {
        self.groups.iter().any(|it| !it.inject.is_empty() || !it.later.is_empty())
            || self
                .slots
                .iter()
                .any(|it| !it.stealer.is_empty() || !it.next.load(Ordering::SeqCst).is_null())
    }
}

/// Of `count` groups, where `of(at)` is a group's load, its workers and whether it is fast:
/// the one of least load per worker, the fast ones first on a tie, then the lower index; of
/// the fast ones alone when `only_fast` and there is one. Compared without division.
fn least(count: usize, only_fast: bool, of: impl Fn(usize) -> (u64, u64, bool)) -> usize {
    let mut best: Option<(usize, (u64, u64, bool))> = None;
    for at in 0..count {
        let this = of(at);
        if only_fast && !this.2 {
            continue;
        }
        let better = best.is_none_or(|(_, (load, workers, fast))| {
            let (mine, theirs) =
                (u128::from(this.0) * u128::from(workers), u128::from(load) * u128::from(this.1));
            mine < theirs || (mine == theirs && this.2 && !fast)
        });
        if better {
            best = Some((at, this));
        }
    }
    best.map_or(0, |(at, _)| at)
}

/// The next state of a worker's draws (Marsaglia's xorshift32): never 0 from a state not 0.
fn xorshift(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

/// The first state of worker `index`'s draws under the pool's `seed`: SplitMix64's finalizer
/// over both, so neighbouring workers and seeds draw apart; odd, so never xorshift's dead 0.
fn worker_seed(seed: u64, index: usize) -> u32 {
    let z = blaze_rng::mix(seed ^ (index as u64).wrapping_add(1).wrapping_mul(blaze_rng::GAMMA));
    (z as u32) | 1
}

/// Two of `workers` but the one at `me` (its position there, if it is one), each drawn by
/// `random` uniformly (the two may be the same), and of them the one `len` says has the longer
/// deque, the first on a tie: a worker's index. `None` when `len` says both are empty, or
/// `workers` holds no other.
fn pick_two(
    workers: &[u32],
    me: Option<usize>,
    mut random: impl FnMut() -> u32,
    len: impl Fn(usize) -> usize,
) -> Option<usize> {
    let others = workers.len().saturating_sub(usize::from(me.is_some()));
    if others == 0 {
        return None;
    }
    let mut draw = || {
        let at = random() as usize % others;
        workers[if me.is_some_and(|me| at >= me) { at + 1 } else { at }] as usize
    };
    let (first, second) = (draw(), draw());
    let (of_first, of_second) = (len(first), if second == first { 0 } else { len(second) });
    match (of_first, of_second) {
        (0, 0) => None,
        (a, b) if b > a => Some(second),
        _ => Some(first),
    }
}

impl Local {
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the LIFO slot takes sole ownership of the job until it is taken"
    )]
    fn push_next(&self, job: Job) {
        let slot = &self.shared.slots[self.index];
        #[cfg(all(test, not(blaze_check)))]
        if let Some(thread) = slot.thread.get() {
            assert_eq!(
                thread.id(),
                this_thread(),
                "a job gave a job to another worker's own slot and deque: a thread-local's address kept across a wait (rule 27)"
            );
        }
        if let Some(old) = NonNull::new(slot.next.swap(job.0.as_ptr(), Ordering::AcqRel)) {
            self.deque.push(Job(old));
        }
        self.shared.announce(self.group);
    }

    /// The next job of this worker's own: its LIFO slot's, but not more than [`LIFO_RUNS`] in a
    /// row, then its deque's oldest.
    fn next(&self) -> Option<Job> {
        let slot = &self.shared.slots[self.index].next;
        if self.lifo.get() < LIFO_RUNS {
            if let Some(job) = take(slot) {
                self.lifo.set(self.lifo.get() + 1);
                return Some(job);
            }
        } else if let Some(job) = take(slot) {
            self.deque.push(job);
        }
        self.lifo.set(0);
        self.deque.pop()
    }

    fn random(&self) -> u32 {
        let x = xorshift(self.seed.get());
        self.seed.set(x);
        x
    }

    /// Up to [`CHOICES`] pairs of `group`'s other workers ([`pick_two`]), each a steal of half
    /// the longer deque; `retry` set when a steal lost a race. `None` when every pick found
    /// nothing.
    fn two_choices(&self, group: &Group, retry: &mut bool) -> Option<Job> {
        let shared = &*self.shared;
        let me = group.workers.iter().position(|&it| it as usize == self.index);
        for _ in 0..CHOICES {
            // A deque's length is a hint: its owner and other thieves move it as it is read.
            let Some(worker) =
                pick_two(&group.workers, me, || self.random(), |it| shared.slots[it].stealer.len())
            else {
                continue;
            };
            match shared.slots[worker].stealer.steal_batch_and_pop(&self.deque) {
                Steal::Success(job) => return Some(job),
                Steal::Retry => *retry = true,
                Steal::Empty => {}
            }
        }
        None
    }

    /// One look at every queue but this worker's own, nearest first: each group's injector (a
    /// few jobs), its workers' deques (half of one, picked as [`Local::victims`] says), then
    /// their LIFO slots.
    fn steal(&self) -> Option<Job> {
        let shared = &*self.shared;
        loop {
            let mut retry = false;
            for &at in shared.groups[self.group].order.iter() {
                let group = &shared.groups[at as usize];
                if let Some(job) = group.inject.steal_into(&self.deque) {
                    return Some(job);
                }
                if self.victims == Victims::TwoChoices
                    && let Some(job) = self.two_choices(group, &mut retry)
                {
                    return Some(job);
                }
                // The walk of every other deque of the group: all of `Victims::Walk`, and under
                // `TwoChoices` the documented second step for deques its picks missed or read
                // as empty, so no queued job is left unseen before the worker parks.
                let count = group.workers.len();
                let start = self.random() as usize % count;
                for step in 0..count {
                    let worker = group.workers[(start + step) % count] as usize;
                    if worker == self.index {
                        continue;
                    }
                    match shared.slots[worker].stealer.steal_batch_and_pop(&self.deque) {
                        Steal::Success(job) => return Some(job),
                        Steal::Retry => retry = true,
                        Steal::Empty => {}
                    }
                }
                for &worker in group.workers.iter() {
                    if worker as usize != self.index
                        && let Some(job) = take(&shared.slots[worker as usize].next)
                    {
                        return Some(job);
                    }
                }
            }
            if !retry {
                return None;
            }
        }
    }

    /// A background job, nearest group first, for a worker that found no other while no group
    /// has one queued.
    fn later(&self) -> Option<Job> {
        let shared = &*self.shared;
        if !shared.groups.iter().all(|it| it.inject.is_empty()) {
            return None;
        }
        shared.groups[self.group]
            .order
            .iter()
            .find_map(|&at| shared.groups[at as usize].later.pop())
    }

    /// Whether this worker may search: while fewer than half the workers do.
    fn begin_search(&self) -> bool {
        let idle = &self.shared.idle.0;
        if 2 * searching(idle.load(Ordering::Relaxed)) >= self.shared.slots.len() as u64 {
            return false;
        }
        idle.fetch_add(SEARCHING, Ordering::SeqCst);
        true
    }

    /// Stops searching; the last searcher, having found work, wakes another for the rest.
    fn end_search(&self, found: bool) {
        let before = self.shared.idle.0.fetch_sub(SEARCHING, Ordering::SeqCst);
        if found && searching(before) == 1 {
            self.shared.announce(self.group);
        }
    }

    /// Searches, as a searcher, up to `rounds` times; the search ends either way.
    fn search(&self, rounds: u32) -> Option<Job> {
        let mut back_off = crate::spin::SpinWait::new();
        for round in 0..rounds {
            if let Some(job) = self.steal() {
                self.end_search(true);
                return Some(job);
            }
            if round + 1 < rounds {
                back_off.pause();
            }
        }
        self.end_search(false);
        None
    }

    /// `job` catches its own panic and gives it to `done`; only `done`'s comes here.
    fn run(&self, runner: &mut Runner<'_>, job: Job) {
        if let Some(panic) = runner.run(job) {
            self.shared.stray.lock().get_or_insert(panic);
        }
    }

    /// Parks until woken, unless `ended` or there is work: a job queued after this says it
    /// sleeps wakes it. Whether it was woken to search, as its waker counted it. `park` is how:
    /// [`Local::park`] for a worker of its own thread, a turn given back for a driven one.
    fn sleep(&self, ended: &impl Fn() -> bool, park: impl FnOnce()) -> bool {
        let shared = &*self.shared;
        let (slot, group) = (&shared.slots[self.index], &shared.groups[self.group]);
        slot.sleeping.store(true, Ordering::SeqCst);
        group.sleepers.fetch_add(1, Ordering::SeqCst);
        shared.idle.0.fetch_add(SLEEPING, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if !ended() && !shared.any_work() {
            park();
        }
        if slot.sleeping.compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed).is_ok()
        {
            group.sleepers.fetch_sub(1, Ordering::SeqCst);
            shared.idle.0.fetch_sub(SLEEPING, Ordering::SeqCst);
            false
        } else {
            true
        }
    }

    /// A worker of its own thread parks, marked asleep ([`Local::sleep`]), until a job queued
    /// wakes it or a timed wait is due; the last to sleep with jobs parked checks for a stall.
    ///
    /// # Errors
    ///
    /// The park's, when the kernel refused the wait: the worker did not park, and stops the
    /// pool with it ([`work`]).
    fn park(&self, ended: &impl Fn() -> bool) -> Result<(), blaze_core::wait::WaitError> {
        let shared = &*self.shared;
        let slot = &shared.slots[self.index];
        #[cfg(not(blaze_check))]
        {
            #[cfg(test)]
            if shared.park_plant.load(Ordering::SeqCst) != park_plant::PARKS {
                return Err(blaze_core::wait::WaitError::io(
                    IDLE_PARK,
                    io::Error::new(io::ErrorKind::Unsupported, "a test's kernel refuses the wait"),
                ));
            }
            // A timed wait of a pool's job ends only as a worker of its scheduler looks at
            // the clock.
            let due = crate::sched::until_due(&shared.sched);
            // Outside any job: the free pages this thread's heaps keep go back if a release
            // asked since it last gave them (memcap5); nothing otherwise.
            blaze_mem::tag::settle_heap();
            crate::sched::idle(&shared.sched, true);
            let parked = DriverSpan::open(Driving::Idle);
            let waited = if shared.parked.load(Ordering::Acquire) != 0
                && pool_idle(ptr::from_ref(shared).expose_provenance())
            {
                // The last to sleep with jobs parked: none of them wakes unless a job ends a
                // wait, so after a while with none, the scheduler checks for a stall.
                let waited =
                    slot.park_for(Some(due.map_or(STALL_AFTER, |due| due.min(STALL_AFTER))));
                if waited.is_ok()
                    && slot.sleeping.load(Ordering::SeqCst)
                    && !ended()
                    && !shared.any_work()
                {
                    _ = crate::sched::check_stall(&shared.sched);
                }
                waited
            } else {
                slot.park_for(due)
            };
            drop(parked);
            crate::sched::idle(&shared.sched, false);
            crate::sched::fire_due(&shared.sched);
            waited
        }
        #[cfg(blaze_check)]
        {
            let _ = (slot, ended);
            thread::park();
            Ok(())
        }
    }
}

/// How a worker runs the jobs it takes: each on a fiber of its own set, which a wait switches
/// out of.
#[cfg(not(blaze_check))]
type Runner<'a> = crate::sched::Host<'a>;

/// Under the model checker, whose threads cannot switch stacks: each job on the worker's own.
#[cfg(blaze_check)]
struct Runner<'a>(PhantomData<&'a ()>);

#[cfg(blaze_check)]
impl Runner<'_> {
    // dyn: a panic's payload, as `catch_unwind` gives it
    fn run(&mut self, job: Job) -> Option<Box<dyn Any + Send>> {
        let _task = blaze_reclaim::enter();
        catch_unwind(AssertUnwindSafe(|| job.run())).err()
    }
}

/// A woken job of the pool at `shared` (an exposed `Shared` address), `token` its header's:
/// to the waker's LIFO slot when the waker is a worker of the pool, else to the pool's
/// queues. Called under the scheduler's lock, as [`crate::sched::join_pool`] says; it takes
/// none but an injector's.
#[cfg(not(blaze_check))]
fn wake_parked(shared: usize, token: usize) {
    // SAFETY: the pool gave its `Shared`'s address when it joined, and leaves the scheduler
    // only once no job of it is parked, so none is woken after; an `Arc` keeps it until then.
    let shared = unsafe { &*ptr::with_exposed_provenance::<Shared>(shared) };
    let header = NonNull::new(ptr::with_exposed_provenance_mut::<Header>(token))
        .expect("a parked job's header");
    crate::sched::ready(token);
    if let Some(job) = push_next(shared, None, Job(header)) {
        shared.push(None, job);
    }
    if shared.stop.load(Ordering::SeqCst) {
        // A stopping pool's workers may have ended but the one a wake would go to: each awake
        // one takes it.
        shared.unpark_workers();
    }
}

/// Whether every worker of the pool at `shared` (an exposed `Shared` address) sleeps with
/// nothing queued: called under the scheduler's lock ([`crate::sched::check_stall`]).
#[cfg(not(blaze_check))]
fn pool_idle(shared: usize) -> bool {
    // SAFETY: as in `wake_parked`: the pool leaves the scheduler before its `Shared` goes.
    let shared = unsafe { &*ptr::with_exposed_provenance::<Shared>(shared) };
    shared.idle.0.load(Ordering::SeqCst) / SLEEPING == shared.slots.len() as u64
        && !shared.any_work()
}

/// What the thread that drives a driven worker had of a worker's thread-locals as it gave the
/// worker its turn: put back as the worker gives the turn back, so that thread is the worker only
/// during the turn (it may be another pool's worker, or a client of this pool).
struct Outer {
    local: Cell<*const Local>,
    core: Cell<u32>,
}

/// Gives a driven worker's turn back to its thread with `turn`, that thread's own thread-locals
/// put back for it, and takes them again at the next turn.
#[cfg(not(blaze_check))]
fn hand_back(
    local: &Local,
    outer: &Outer,
    turns: &crate::sched::Turns<'_>,
    turn: crate::sched::Turn,
) {
    LOCAL.set(outer.local.get());
    let mine = crate::sched::swap_core(outer.core.get());
    turns.give(turn);
    outer.local.set(LOCAL.replace(local));
    outer.core.set(crate::sched::swap_core(mine));
}

/// A worker's loop, on its own thread or (`turns`) on a fiber its pool's thread drives
/// ([`Pool::driven`]): the same loop either way, but that a driven worker gives the turn back
/// after each job it ran and where a worker of its own thread would park.
fn work(local: &Local, turns: Option<&crate::sched::Turns<'_>>) {
    #[cfg(not(blaze_check))]
    let outer = Outer {
        local: Cell::new(LOCAL.replace(local)),
        core: Cell::new(crate::sched::swap_core(u32::MAX)),
    };
    #[cfg(blaze_check)]
    let outer = Outer { local: Cell::new(LOCAL.replace(local)), core: Cell::new(u32::MAX) };
    // A driven worker gives its thread the turn after a job, and where it would park.
    #[cfg(not(blaze_check))]
    let ran = || {
        if let Some(turns) = turns {
            hand_back(local, &outer, turns, crate::sched::Turn::Ran);
        }
    };
    #[cfg(blaze_check)]
    let ran = || debug_assert!(turns.is_none(), "the model checker's pools have threads");
    // The error of a park the kernel refused, until the loop stops the pool with it.
    let refused: Cell<Option<blaze_core::wait::WaitError>> = Cell::new(None);
    let park = |ended: &dyn Fn() -> bool| match turns {
        #[cfg(not(blaze_check))]
        Some(turns) => {
            crate::sched::idle(&local.shared.sched, true);
            hand_back(local, &outer, turns, crate::sched::Turn::Idle);
            crate::sched::idle(&local.shared.sched, false);
            crate::sched::fire_due(&local.shared.sched);
        }
        _ => {
            if let Err(error) = local.park(&ended) {
                refused.set(Some(error));
            }
        }
    };
    let shared = &*local.shared;
    #[cfg(not(blaze_check))]
    let mut runner = crate::sched::Host::new(
        &shared.sched,
        shared.member.load(Ordering::Relaxed),
        u32::try_from(local.index).expect("a worker's index is a u32"),
        &shared.parked,
        &shared.slots[local.index].ended,
        &shared.slots[local.index].inside,
    );
    #[cfg(blaze_check)]
    let mut runner = Runner(PhantomData);
    let mut searching = false;
    // What the stop's drain backs off with while another worker takes what is left.
    let mut drain = crate::spin::SpinWait::new();
    // The span of this worker's search, open from a job's end (or a wake) to the next job's
    // start: closed before a job runs, as the job's fiber would take it along.
    let mut search: Option<DriverSpan> = None;
    loop {
        // Its park was refused: it cannot wait, so the pool stops for it, and no thread is
        // left waiting on jobs a worker that ended unheard would have run.
        #[cfg(not(blaze_check))]
        if let Some(error) = refused.take() {
            #[cfg(test)]
            if shared.park_plant.load(Ordering::SeqCst) == park_plant::FAILS_AND_ENDS_SILENTLY {
                break;
            }
            shared.broke(local.index, Unable::Park(error));
        }
        #[cfg(blaze_check)]
        let _ = refused.take();
        search.get_or_insert_with(|| DriverSpan::open(Driving::Search));
        #[cfg(not(blaze_check))]
        crate::sched::fire_due(&shared.sched);
        if let Some(job) = local.next() {
            if searching {
                local.end_search(true);
                searching = false;
            }
            search = None;
            local.run(&mut runner, job);
            ran();
            continue;
        }
        if searching || local.begin_search() {
            searching = false;
            if let Some(job) = local.search(ROUNDS) {
                search = None;
                local.run(&mut runner, job);
                ran();
                continue;
            }
        }
        if let Some(job) = local.later() {
            search = None;
            local.run(&mut runner, job);
            ran();
            continue;
        }
        // A pool stopped for an error: its workers park no more. Each runs what it finds
        // queued, as above, and ends once nothing is; a job queued after the last has ended is
        // never run, and its scope's caller fails rather than wait for it ([`block`]).
        #[cfg(not(blaze_check))]
        if shared.broken.get().is_some() {
            if shared.any_work() {
                if !drain.spin() {
                    drain = crate::spin::SpinWait::new();
                }
                continue;
            }
            break;
        }
        if shared.stop.load(Ordering::SeqCst) {
            // Every job given before the stop is run, and every parked one resumed and run:
            // what is left was found by none.
            if shared.any_work() {
                if !drain.spin() {
                    drain = crate::spin::SpinWait::new();
                }
                continue;
            }
            // It sleeps until no job is parked: a parked job's wake queues it and wakes the
            // workers ([`wake_parked`]), and a worker that ends wakes the others. Its stopper
            // looks, as it is woken, for parked jobs that can never wake, and fails them, or
            // gives up past its bound ([`Pool::stop`]), so this sleep ends.
            #[cfg(not(blaze_check))]
            if shared.parked.load(Ordering::Acquire) != 0
                && !shared.abandoned.load(Ordering::SeqCst)
            {
                shared.unpark_stopper();
                search = None;
                let drained = || {
                    shared.parked.load(Ordering::Acquire) == 0
                        || shared.abandoned.load(Ordering::SeqCst)
                };
                searching = local.sleep(&drained, || park(&drained));
                continue;
            }
            break;
        }
        search = None;
        let ended = || shared.stop.load(Ordering::SeqCst);
        searching = local.sleep(&ended, || park(&ended));
    }
    drop(search);
    drop(runner);
    // The others may sleep until the last parked job resumed, which none is told of but by
    // this.
    shared.unpark_workers();
    LOCAL.set(outer.local.get());
    #[cfg(not(blaze_check))]
    crate::sched::swap_core(outer.core.get());
}

/// Blocks this thread, on no fiber, until `until` holds: a client's wait
/// ([`crate::sched::wait`]), or a driven pool's drop. A thread that drives pools
/// ([`Pool::driven`]) gives their workers turns meanwhile, a job each at a time, looking at
/// `until` after each; when none has a job, it sleeps until the next timed wait of a job is due,
/// else asks the scheduler for a stall, which wakes every stuck job to fail naming its wait. A
/// thread that drives none parks, for its waker to unpark.
///
/// # Panics
///
/// On a target without threads, when no pool has a job, no wait is due and no stall was found:
/// nothing that could make `until` hold is left to run, and no other thread can. The panic
/// names every wait of the graph.
pub(crate) fn block_until(until: &dyn Fn() -> bool) {
    #[cfg(not(blaze_check))]
    block(until, None);
    #[cfg(blaze_check)]
    while !until() {
        thread::park();
    }
}

/// [`block_until`], and returns by `deadline` (on [`crate::sched::now`]'s clock) at the latest,
/// whether `until` holds or not: a stopping pool's bounded wait ([`stop_bounded`]).
///
/// A thread whose wait is on a scope of a pool with threads of its own ([`scope_in`] names it)
/// is listed with that pool while it blocks, and woken as the pool stops for an error
/// ([`Broken`]) and as its workers end. Once the last has ended and the wait has not, nothing
/// is left to end it: the thread lets go of the scope's own count, as the scope's last job
/// would have, which ends the wait where every job still counted never began; where jobs of
/// the scope are parked, which hold counts of their own and are never resumed, the wait can
/// never end and the scheduler has no way to fail the wait of a thread on no fiber, so the
/// process ends, saying so ([`cannot_go_on`]).
#[cfg(not(blaze_check))]
fn block(until: &dyn Fn() -> bool, deadline: Option<u64>) {
    // Taken for this call alone: a `block` further down this thread's stack (a pool dropped
    // by a job this thread drives) waits on something else.
    let scope = Blocked::take();
    // How long until the deadline, if one: `Some(ZERO)` once it is past.
    let left = || {
        deadline.map(|at| std::time::Duration::from_nanos(at.saturating_sub(crate::sched::now())))
    };
    let rest = |wait: std::time::Duration| {
        if THREADS {
            thread::park_timeout(wait);
        } else {
            // Nothing but the clock can wake the one thread of a target without them.
            thread::sleep(wait);
        }
    };
    loop {
        if until() || left().is_some_and(|it| it.is_zero()) {
            return;
        }
        if let Some(waiting) = scope.waiting()
            && let Some(broken) = waiting.pool().ended_broken()
        {
            // Looked at again: the last worker's last job may have ended the wait as it ended.
            if until() {
                return;
            }
            if !waiting.let_go.replace(true) {
                crate::sched::release(waiting.counter);
                continue;
            }
            cannot_go_on(broken);
        }
        match drive_round() {
            // A client of pools with threads of their own: its waker unparks it.
            Round::None if THREADS => match left() {
                Some(left) => thread::park_timeout(left),
                // wakes: the client's waker as its wait ends, and the pool this thread is listed with (`Blocked`) as it stops for an error and as each worker ends, which the loop then reads as `ended_broken`
                None => thread::park(),
            },
            Round::Ran => {}
            Round::None | Round::Idle => {
                if until() {
                    return;
                }
                // The thread may drive pools of any scheduler, and wait on any.
                if let Some(due) = crate::sched::until_due_any() {
                    let _idle = DriverSpan::open(Driving::Idle);
                    rest(left().map_or(due, |left| due.min(left)));
                    crate::sched::fire_due_any();
                    continue;
                }
                if crate::sched::check_stall_any() {
                    continue;
                }
                if let Some(left) = left() {
                    let _idle = DriverSpan::open(Driving::Idle);
                    rest(left);
                    continue;
                }
                assert!(
                    THREADS,
                    "the one thread of a target without threads waits, and nothing can end its wait: no driven pool has a \
                     job, no job's wait is due and no stall was found ({})",
                    crate::sched::waits_now()
                );
                // Another thread may queue a job (which unparks this one) or end the wait.
                let _idle = DriverSpan::open(Driving::Idle);
                thread::park();
            }
        }
    }
}

/// What a thread on no fiber waits on in [`scope_in`]: the scope's pool and its counter, on the
/// stack of that call for as long as the wait.
#[cfg(not(blaze_check))]
struct ScopeWait {
    shared: *const Shared,
    counter: crate::sched::CounterId,
    /// Whether [`block`] let go of the scope's own count, which it does once.
    let_go: Cell<bool>,
}

#[cfg(not(blaze_check))]
impl ScopeWait {
    fn pool(&self) -> &Shared {
        // SAFETY: `scope_in` made this of the `&Shared` it holds until its wait is over, and
        // it is read only during that wait, on the same thread.
        unsafe { &*self.shared }
    }
}

#[cfg(not(blaze_check))]
thread_local! {
    /// The scope this thread, on no fiber, is about to wait on or waits on ([`scope_in`]), or
    /// null. Never set by a job: a job that waits switches out and may resume elsewhere.
    static SCOPE_WAIT: Cell<*const ScopeWait> = const { Cell::new(ptr::null()) };
}

/// [`SCOPE_WAIT`] naming a [`ScopeWait`] while this lives, and what it named before after.
#[cfg(not(blaze_check))]
struct Named(*const ScopeWait);

#[cfg(not(blaze_check))]
impl Named {
    fn new(waiting: &ScopeWait) -> Named {
        Named(SCOPE_WAIT.replace(ptr::from_ref(waiting)))
    }
}

#[cfg(not(blaze_check))]
impl Drop for Named {
    fn drop(&mut self) {
        SCOPE_WAIT.set(self.0);
    }
}

/// The scope a [`block`] waits on, taken from [`SCOPE_WAIT`] and its thread listed with the
/// scope's pool, for as long as this lives.
#[cfg(not(blaze_check))]
struct Blocked(*const ScopeWait);

#[cfg(not(blaze_check))]
impl Blocked {
    fn take() -> Blocked {
        let blocked = Blocked(SCOPE_WAIT.replace(ptr::null()));
        if let Some(waiting) = blocked.waiting() {
            // alloc: grows to the most threads blocked on the pool's scopes at once
            waiting.pool().waiters.lock().push(thread::current());
        }
        blocked
    }

    fn waiting(&self) -> Option<&ScopeWait> {
        // SAFETY: null, or the `ScopeWait` on the stack of the `scope_in` whose wait this
        // `block` is, on this thread: it outlives the wait, which this is inside.
        unsafe { self.0.as_ref() }
    }
}

#[cfg(not(blaze_check))]
impl Drop for Blocked {
    fn drop(&mut self) {
        if let Some(waiting) = self.waiting() {
            let me = thread::current().id();
            let mut waiters = waiting.pool().waiters.lock();
            if let Some(at) = waiters.iter().position(|it| it.id() == me) {
                waiters.swap_remove(at);
            }
        }
        SCOPE_WAIT.set(self.0);
    }
}

/// A thread waits on a scope of a pool that stopped for `broken`, whose workers have all ended
/// with jobs of the scope parked: nothing is left to resume them, so the wait can never end,
/// and it cannot be failed (the scheduler ends the wait of a thread on no fiber only as what
/// it waits for ends). The process ends, saying so: the hang this was is the alternative.
#[cfg(not(blaze_check))]
#[cold]
fn cannot_go_on(broken: &Broken) -> ! {
    eprintln!(
        "error: {broken}; a thread waits on a scope of that pool whose jobs are parked with no worker left to resume \
         them, so its wait can never end and the process ends here (rule bounded-progress)"
    );
    std::process::abort()
}

/// `rx.recv()` on a thread on no fiber, for the channel `name`. Where the target has threads,
/// the receive itself. On one without (the seed), only a job of a pool this thread drives can
/// send what it waits for, so the thread gives those pools turns until a message, or the end of
/// every sender, is there ([`block_until`]); a receive that blocked would stop the one thread.
///
/// # Panics
///
/// On a target without threads, called on a fiber (a job): its thread is already a pool's
/// worker, whose turn can't drive the pools; the panic names the channel. And as
/// [`block_until`] does, when nothing left could send.
pub(crate) fn recv<T>(
    rx: &crossbeam_channel::Receiver<T>,
    name: &'static str,
) -> Result<T, crossbeam_channel::RecvError> {
    if THREADS {
        return rx.recv();
    }
    assert!(
        crate::sched::fiber::current_job().is_none(),
        "a job on the one thread of a target without threads receives from the channel `{name}`: only the \
         thread that drives the pools may wait on a channel there"
    );
    let got = Cell::new(None);
    block_until(&|| match rx.try_recv() {
        Ok(it) => {
            got.set(Some(Ok(it)));
            true
        }
        Err(crossbeam_channel::TryRecvError::Disconnected) => {
            got.set(Some(Err(crossbeam_channel::RecvError)));
            true
        }
        Err(crossbeam_channel::TryRecvError::Empty) => false,
    });
    got.into_inner()
        .unwrap_or_else(|| unreachable!("`block_until` returns once the channel `{name}` answered"))
}

/// What a round of turns of the pools a thread drives came to.
#[cfg(not(blaze_check))]
enum Round {
    /// The thread drives no pool.
    None,
    /// A worker ran a job.
    Ran,
    /// Every worker found nothing to run, or had ended.
    Idle,
}

/// A turn of every worker not ended of every pool this thread drives, but of a pool whose
/// workers are in a turn already, further down this thread's stack: a pool dropped by a job of
/// another driven pool drives its own and the others while that job waits for its end.
#[cfg(not(blaze_check))]
fn drive_round() -> Round {
    let mut round = Round::None;
    let mut at = 0;
    while let Some(drivers) = DRIVEN.with_borrow(|it| it.get(at).copied()) {
        at += 1;
        // SAFETY: a pool's `Drivers` are listed from its making until its drop has ended them,
        // on this thread, which is the only one that reads the list.
        let drivers = unsafe { &*drivers };
        let Ok(mut workers) = drivers.workers.try_borrow_mut() else { continue };
        if matches!(round, Round::None) {
            round = Round::Idle;
        }
        for worker in workers.iter_mut().filter(|it| !it.ended()) {
            if worker.turn() == crate::sched::Turn::Ran {
                round = Round::Ran;
            }
        }
    }
    round
}

#[cfg(not(blaze_check))]
impl Drivers {
    /// The pool of `shared` stopped: runs its workers until every job given it has run and
    /// each loop returned, then this thread drives it no more; bounded by `bound` as
    /// [`Pool::stop`] says.
    ///
    /// # Errors
    ///
    /// [`Unstopped`] when the stop gave up: this thread still lists the pool, to give its
    /// workers the turns they end at.
    ///
    /// # Panics
    ///
    /// On a thread other than the one that made the pool, naming both: only that one drives it.
    /// Inside one of its own jobs, which the drop would wait for.
    fn end(&self, shared: &Shared, bound: std::time::Duration) -> Result<(), Unstopped> {
        let me = std::thread::current().id();
        assert!(
            me == self.owner,
            "a driven pool made on thread {:?} was dropped on thread {me:?}: only the thread that made it drives it",
            self.owner
        );
        assert!(
            self.workers.try_borrow_mut().is_ok(),
            "a driven pool was dropped by one of its own jobs: its drop waits for every job given it, that one too, so drop \
             it outside its jobs"
        );
        let ended = || self.workers.borrow().iter().all(crate::sched::Driven::ended);
        stop_bounded(shared, bound, &ended, &|recheck, until| {
            crate::wait::wait(
                crate::wait::WaitOn::Channel("a driven pool's last jobs"),
                None,
                || {
                    block(recheck, Some(until));
                },
            );
        })?;
        DRIVEN.with_borrow_mut(|it| it.retain(|&at| !ptr::eq(at, self)));
        Ok(())
    }
}

/// Binds the calling worker to `cpu`, and its frames' chunks to the CPU's NUMA node
/// ([`blaze_mem::chunks::set_thread_node`]): Linux's topology names the real node.
#[cfg(target_os = "linux")]
fn bind_to(cpu: &Cpu) -> io::Result<()> {
    crate::topology::linux::bind(cpu.id)?;
    blaze_mem::chunks::set_thread_node(Some(cpu.node)).map_err(io::Error::other)
}

/// The QoS class of every worker on macOS. `user-initiated`, what a build someone waits for
/// is, when the process runs at normal priority (`BLAZE_CORES=all`); `default` when it runs at
/// background priority (every other setting, [`blaze_core::cores::lower`]): below the GUI's
/// interactive and user-initiated work, so a build at its budget leaves the desktop
/// responsive, and still above `utility`, which a loaded host starves (the self-build profile
/// of 2026-11-02: workers at `utility` got about 2% of a core each). A pool made where the
/// budget can't be read (it panics at once in [`crate::exec::shared`]) is not lowered.
#[cfg(target_os = "macos")]
fn worker_qos() -> libc::qos_class_t {
    match blaze_core::cores::get() {
        Ok(cores) if cores.lowered() => libc::qos_class_t::QOS_CLASS_DEFAULT,
        _ => libc::qos_class_t::QOS_CLASS_USER_INITIATED,
    }
}

#[cfg(target_os = "macos")]
fn bind_to(cpu: &Cpu) -> io::Result<()> {
    // SAFETY: sets the calling thread's own QoS class; no memory is passed.
    let got = unsafe { libc::pthread_set_qos_class_self_np(worker_qos(), 0) };
    if got == 0 {
        Ok(())
    } else {
        let why = io::Error::from_raw_os_error(got);
        // alloc: the error of a worker not bound, once
        Err(io::Error::new(
            why.kind(),
            format!("the QoS class of the worker of CPU {}: {why}", cpu.id),
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn bind_to(cpu: &Cpu) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        // alloc: the error of a worker not bound, once
        format!("binding the worker of CPU {} to it: not on this OS", cpu.id),
    ))
}

#[cfg(all(test, not(blaze_check)))]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU32, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use blaze_alloc_count::measure;
    use blaze_ids::rng::SplitMix64 as Rng;
    use crossbeam_channel::{bounded, unbounded};
    use blaze_core::lock::Lock;

    use super::{
        Class, Group, Header, Job, Pending, Placed, Pool, STALL_AFTER, STOP_BOUND, Slot, StopClock,
        StopStep, group, searching, sleeping,
    };
    use crate::{
        Cancelled, Key,
        pump::Flights,
        topology::{Cpu, Topology},
    };

    const LONG: Duration = Duration::from_secs(60);

    fn groups_of(count: u32, each: u32) -> Topology {
        Topology {
            cpus: (0..count * each)
                .map(|id| Cpu { id, group: id / each, socket: 0, node: 0, fast: true })
                .collect(),
        }
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        crate::spin::SpinWait::within(what, LONG, done);
    }

    #[test]
    fn per_worker_state_is_padded_and_a_job_is_a_pointer() {
        assert_eq!(align_of::<Slot>(), 128);
        assert_eq!(align_of::<Group>(), 128);
        assert_eq!(align_of::<Pending>(), 128);
        assert_eq!(size_of::<Job>(), 8);
        assert_eq!(size_of::<Header>(), 16);
    }

    /// The words of a closure's capture, and what the closure makes of them.
    fn closure<const N: usize>(
        fill: u64,
        dropped: Dropped,
    ) -> impl FnOnce() -> u64 + Send + 'static {
        let mut words = [0u64; N];
        for (at, it) in words.iter_mut().enumerate() {
            *it = fill.wrapping_mul(at as u64 + 1);
        }
        move || {
            let _dropped = dropped;
            words.iter().fold(0u64, |sum, &it| sum.rotate_left(5) ^ it)
        }
    }

    /// Counts its drop.
    struct Dropped(Arc<AtomicUsize>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn class<F>(_: &F) -> usize {
        Placed::<F>::CLASS
    }

    /// A job takes the smallest cell its closure fits, its header and cell's handle with it.
    #[test]
    fn a_job_takes_the_smallest_cell_that_holds_it() {
        let drops = Arc::new(AtomicUsize::new(0));
        let small = closure::<2>(1, Dropped(Arc::clone(&drops)));
        let medium = closure::<40>(1, Dropped(Arc::clone(&drops)));
        let large = closure::<200>(1, Dropped(drops));
        assert_eq!((class(&small), class(&medium), class(&large)), (0, 1, 2));
        assert_eq!(class(&|| ()), 0);
        let largest = [0u8; 2048 - size_of::<Placed<()>>()];
        assert_eq!(
            (size_of::<Placed<[u8; 2048 - size_of::<Placed<()>>()]>>(), class(&largest)),
            (2048, 2)
        );
    }

    /// Jobs of every cell size, against the same closures called in place: each runs once
    /// with what it captured intact, gives what the one called in place gives, and drops its
    /// capture once.
    #[test]
    fn a_job_in_a_cell_runs_as_the_same_closure_called_in_place() {
        fn one<const N: usize>(
            pool: &Pool,
            fill: u64,
            drops: &Arc<AtomicUsize>,
            tx: crossbeam_channel::Sender<(usize, u64)>,
            job: usize,
        ) -> u64 {
            let in_place = closure::<N>(fill, Dropped(Arc::clone(drops)));
            pool.submit(None, closure::<N>(fill, Dropped(Arc::clone(drops))), move |it| {
                _ = tx.send((job, it.unwrap()));
            });
            in_place()
        }
        let pool = Pool::new(&groups_of(2, 2), false).unwrap();
        for seed in 0..20u64 {
            let mut rng = Rng::new(seed);
            let drops = Arc::new(AtomicUsize::new(0));
            let (tx, rx) = unbounded::<(usize, u64)>();
            let want: Vec<u64> = (0..300)
                .map(|job| {
                    let (fill, tx) = (rng.next_u64(), tx.clone());
                    match rng.below(3) {
                        0 => one::<2>(&pool, fill, &drops, tx, job),
                        1 => one::<40>(&pool, fill, &drops, tx, job),
                        _ => one::<200>(&pool, fill, &drops, tx, job),
                    }
                })
                .collect();
            let mut got = vec![None; want.len()];
            for _ in 0..want.len() {
                let (job, value) = rx.recv_timeout(LONG).unwrap();
                assert!(got[job].replace(value).is_none(), "seed {seed}: job {job} ran twice");
            }
            let got: Vec<u64> = got.into_iter().map(Option::unwrap).collect();
            assert_eq!(got, want, "seed {seed}");
            until("every capture to drop", || drops.load(Ordering::SeqCst) == 2 * want.len());
        }
    }

    /// On a warm worker, a scope of jobs submitted, run and waited for allocates nothing.
    #[test]
    fn a_warm_job_allocates_nothing() {
        let pool = Arc::new(Pool::new(&Topology::flat(1), false).unwrap());
        let (tx, rx) = bounded::<(u64, usize)>(1);
        let inner = Arc::clone(&pool);
        pool.submit(
            None,
            move || {
                let count = AtomicUsize::new(0);
                let round = || {
                    inner.scope(|scope| {
                        for _ in 0..8 {
                            scope.submit(
                                None,
                                || count.fetch_add(1, Ordering::Relaxed),
                                |it| {
                                    it.unwrap();
                                },
                            );
                        }
                    });
                };
                for _ in 0..4 {
                    round();
                }
                let ((), used) = measure(round);
                (used.allocations, count.load(Ordering::Relaxed))
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        assert_eq!(
            rx.recv_timeout(LONG).unwrap(),
            (0, 40),
            "allocations of a warm scope of 8 jobs"
        );
    }

    /// From a thread of no pool, a warm submit allocates nothing: its cell comes from the
    /// thread's cache and its queue's node is the job.
    #[test]
    fn a_warm_submit_from_outside_allocates_nothing() {
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let (tx, rx) = bounded::<usize>(64);
        let round = |senders: Vec<crossbeam_channel::Sender<usize>>| {
            for (job, tx) in senders.into_iter().enumerate() {
                pool.submit(None, move || job, move |it| _ = tx.send(it.unwrap()));
            }
        };
        for _ in 0..8 {
            round((0..64).map(|_| tx.clone()).collect());
            for _ in 0..64 {
                rx.recv_timeout(LONG).unwrap();
            }
        }
        let senders = (0..64).map(|_| tx.clone()).collect();
        let ((), used) = measure(|| round(senders));
        assert_eq!(used.allocations, 0, "a warm submit of 64 jobs: {used:?}");
        let mut seen: Vec<usize> = (0..64).map(|_| rx.recv_timeout(LONG).unwrap()).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..64).collect::<Vec<_>>());
    }

    #[test]
    fn every_job_runs_once_and_says_its_group() {
        let pool = Pool::new(&groups_of(3, 2), false).unwrap();
        assert_eq!(pool.workers(), 6);
        assert_eq!(group(), None);
        let (tx, rx) = unbounded::<(usize, Option<u32>)>();
        for job in 0..1000usize {
            let tx = tx.clone();
            let near = (job % 4 != 3).then_some((job % 4) as u32);
            pool.submit(near, group, move |it| _ = tx.send((job, it.unwrap())));
        }
        let mut seen = vec![0u32; 1000];
        for _ in 0..1000 {
            let (job, group) = rx.recv_timeout(LONG).unwrap();
            seen[job] += 1;
            assert!(group.is_some_and(|it| it < 3), "{group:?}");
        }
        assert!(seen.iter().all(|&it| it == 1));
    }

    /// Group 0's one worker is held; a job queued near it is stolen by group 1's.
    #[test]
    fn an_idle_group_steals_from_a_busy_one() {
        let pool = Pool::new(&groups_of(2, 1), false).unwrap();
        let (hold_tx, hold_rx) = bounded::<()>(0);
        let (held_tx, held_rx) = bounded::<Option<u32>>(1);
        pool.submit(
            Some(0),
            move || {
                _ = held_tx.send(group());
                hold_rx.recv_timeout(LONG).unwrap();
            },
            |it| it.unwrap(),
        );
        let first = held_rx.recv_timeout(LONG).unwrap();
        let (tx, rx) = bounded::<Option<u32>>(1);
        let busy = first.unwrap();
        pool.submit(Some(busy), group, move |it| _ = tx.send(it.unwrap()));
        assert_eq!(rx.recv_timeout(LONG).unwrap(), Some(1 - busy));
        hold_tx.send(()).unwrap();
    }

    #[test]
    fn a_panic_is_the_jobs_failure_and_the_pool_goes_on() {
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let (tx, rx) = unbounded::<Result<u32, String>>();
        for job in 0..10u32 {
            let tx = tx.clone();
            pool.submit(
                None,
                move || {
                    assert!(job % 3 != 0, "job {job}");
                    job
                },
                move |it| {
                    let it = it.map_err(|panic| {
                        panic.downcast_ref::<String>().cloned().unwrap_or_default()
                    });
                    _ = tx.send(it);
                },
            );
        }
        let mut failed: Vec<String> =
            (0..10).filter_map(|_| rx.recv_timeout(LONG).unwrap().err()).collect();
        failed.sort();
        assert_eq!(failed, ["job 0", "job 3", "job 6", "job 9"]);
    }

    #[test]
    fn dropping_the_pool_runs_what_it_was_given_and_joins() {
        let ran = Arc::new(AtomicUsize::new(0));
        let pool = Pool::new(&Topology::flat(3), false).unwrap();
        for _ in 0..200 {
            let ran = Arc::clone(&ran);
            pool.submit(
                None,
                move || {
                    crate::spin::busy(Duration::from_micros(50));
                    ran.fetch_add(1, Ordering::SeqCst);
                },
                |it| it.unwrap(),
            );
        }
        drop(pool);
        assert_eq!(ran.load(Ordering::SeqCst), 200);
    }

    #[test]
    fn a_scope_borrows_and_waits_for_its_jobs() {
        let pool = Pool::new(&groups_of(2, 2), false).unwrap();
        let sums = Lock::new("graph.test.pool.sums", Vec::new());
        let numbers: Vec<u64> = (0..100).collect();
        let total = pool.scope(|scope| {
            for chunk in numbers.chunks(10) {
                scope.submit(
                    None,
                    move || chunk.iter().sum::<u64>(),
                    |it| {
                        sums.lock().push(it.unwrap());
                    },
                );
            }
            7
        });
        assert_eq!(total, 7);
        let sums = sums.into_inner();
        assert_eq!(sums.len(), 10);
        assert_eq!(sums.iter().sum::<u64>(), 4950);
        assert_eq!(pool.scope(|_| 3), 3, "a scope of no job returns at once");
    }

    /// A scope waited on inside a job of a one-worker pool: the job switches out, and the worker
    /// runs the scope's jobs, which went to its LIFO slot and deque.
    #[test]
    fn a_scope_inside_a_job_of_one_worker_ends() {
        let pool = Arc::new(Pool::new(&Topology::flat(1), false).unwrap());
        let (tx, rx) = bounded::<u64>(1);
        let inner = Arc::clone(&pool);
        pool.submit(
            None,
            move || {
                let count = AtomicUsize::new(0);
                inner.scope(|scope| {
                    for _ in 0..5 {
                        scope.submit(
                            None,
                            || count.fetch_add(1, Ordering::SeqCst),
                            |it| {
                                it.unwrap();
                            },
                        );
                    }
                });
                count.load(Ordering::SeqCst) as u64
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        assert_eq!(rx.recv_timeout(LONG).unwrap(), 5);
    }

    fn key(n: u32) -> Key {
        let mut key = [0u8; 32];
        key[..4].copy_from_slice(&n.to_le_bytes());
        Key(key)
    }

    /// Key `at`'s value, `JOBS - at`: each key asks the next, and the last is the client's.
    fn chain(flights: &Flights<u32>, at: u32, jobs: u32) -> Result<u32, Cancelled> {
        if at == jobs {
            return flights.get_or_run(key(jobs), || panic!("the client runs the last key"));
        }
        flights.get_or_run(key(at), || chain(flights, at + 1, jobs).map(|it| it + 1))
    }

    /// A waiting job frees its worker: two workers, 48 jobs each waiting on a later one, the
    /// last on a key a client runs until every job waits, parked, which no two workers could
    /// hold at once; then all finish, each with its key's value.
    ///
    /// The client ends its key only once all 48 are parked, never once they have only started:
    /// a job that has started may still be on its way down the chain, and would reach the
    /// client's key after it ended and find no one running it (the pre-2026-11-02 flake under
    /// load, "the client runs the last key").
    #[test]
    fn a_waiting_job_frees_its_worker() {
        const JOBS: u32 = 48;
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        let (tx, rx) = unbounded::<(u32, Result<u32, Cancelled>)>();
        let last = flights.get_or_run(key(JOBS), || {
            for at in 0..JOBS {
                let (flights, tx) = (Arc::clone(&flights), tx.clone());
                pool.submit(
                    None,
                    move || chain(&flights, at, JOBS),
                    move |it| _ = tx.send((at, it.unwrap())),
                );
            }
            // Each job waits on a key another claimed, or the job holding key 47 on this one's:
            // all of them park, and none can end before this key does.
            until("every job to wait, parked", || pool.parked() == JOBS as usize);
            Ok(0)
        });
        assert_eq!(last, Ok(0));
        let mut got: Vec<_> = (0..JOBS).map(|_| rx.recv_timeout(LONG).unwrap()).collect();
        got.sort_unstable_by_key(|it| it.0);
        assert_eq!(got, (0..JOBS).map(|at| (at, Ok(JOBS - at))).collect::<Vec<_>>());
        assert_eq!(flights.running(), 0);
    }

    std::thread_local! {
        static MARK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn take_mark() -> Option<std::num::NonZeroUsize> {
        std::num::NonZeroUsize::new(MARK.replace(0))
    }

    fn put_mark(taken: std::num::NonZeroUsize) {
        MARK.set(taken.get());
    }

    /// A job inside a frame task that waits resumes in its own frame, on whichever worker takes
    /// it (another one, in some of the rounds), and allocates there; a job a worker runs
    /// meanwhile starts in no task. The frame moves with the job, not with the thread, as a
    /// thread-local registered to be carried does ([`crate::sched::carry`]).
    #[test]
    fn a_waiting_job_keeps_its_frame_on_any_worker() {
        use blaze_mem::{Tag, frame};
        static CARRIED: std::sync::Once = std::sync::Once::new();
        CARRIED.call_once(|| crate::sched::carry(crate::sched::Carried::word(take_mark, put_mark)));
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        let mut moved = 0;
        for round in 0..50u32 {
            let at = key(1_000 + round);
            let (tx, rx) = unbounded::<Result<Option<(usize, usize)>, String>>();
            let waiting = Arc::new(AtomicUsize::new(0));
            let held = flights.get_or_run(at, || {
                {
                    let (flights, waiting, tx) =
                        (Arc::clone(&flights), Arc::clone(&waiting), tx.clone());
                    pool.submit(
                        None,
                        move || {
                            frame::task(Tag::Plane, |mine| {
                                let before = super::with_local(|local| local.index).unwrap();
                                *mine.alloc(1u64) += 1;
                                let used = mine.used();
                                MARK.set(round as usize + 1);
                                waiting.store(1, Ordering::SeqCst);
                                assert_eq!(
                                    flights.get_or_run(at, || panic!("the client runs the key")),
                                    Ok(round)
                                );
                                frame::current(|now| {
                                    assert!(
                                        std::ptr::eq(now, mine),
                                        "the job resumed in another frame"
                                    );
                                    *now.alloc(2u64) += 1;
                                });
                                assert!(
                                    mine.used() > used,
                                    "the job allocated outside its own frame"
                                );
                                assert_eq!(
                                    MARK.get(),
                                    round as usize + 1,
                                    "a carried thread-local stayed behind"
                                );
                                MARK.set(0);
                                let after = super::with_local(|local| local.index).unwrap();
                                Some((before, after))
                            })
                        },
                        move |it| {
                            _ = tx.send(it.map_err(|_| "the waiting job panicked".to_owned()))
                        },
                    );
                }
                until("the job to park", || {
                    waiting.load(Ordering::SeqCst) == 1
                        && pool.shared.parked.load(Ordering::SeqCst) == 1
                });
                for _ in 0..2 {
                    let tx = tx.clone();
                    pool.submit(
                        None,
                        || {
                            assert!(
                                !frame::in_task(),
                                "a job began inside a parked job's frame task"
                            );
                            assert_eq!(
                                MARK.get(),
                                0,
                                "a job began with a parked job's carried thread-local"
                            );
                            frame::task(Tag::Plane, |_| {
                                crate::spin::busy(Duration::from_millis(5))
                            });
                            None
                        },
                        move |it| {
                            _ = tx.send(it.map_err(|_| "a job meanwhile panicked".to_owned()))
                        },
                    );
                }
                crate::spin::busy(Duration::from_millis(2));
                Ok(round)
            });
            assert_eq!(held, Ok(round));
            for _ in 0..3 {
                if let Some((before, after)) = rx.recv_timeout(LONG).unwrap().unwrap() {
                    moved += usize::from(before != after);
                }
            }
        }
        assert!(moved > 0, "in 50 rounds the waiting job never resumed on the other worker");
    }

    /// A job that waits in a task inside a task resumes in the inner task's frame, on whichever
    /// worker takes it, and a task it opens after the switch starts in the rest of its own outer
    /// frame's chunk (`frame::task` lends it), while the jobs the other worker runs meanwhile
    /// open and fill tasks of their own: a query that waits for another's flight, then opens the
    /// next query's task.
    #[test]
    fn a_task_opened_after_a_switch_starts_in_the_jobs_own_frame() {
        use blaze_mem::{Tag, frame};
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        let mut moved = 0;
        for round in 0..50u32 {
            let at = key(2_000 + round);
            let (tx, rx) = unbounded::<Result<Option<(usize, usize)>, String>>();
            let waiting = Arc::new(AtomicUsize::new(0));
            let held = flights.get_or_run(at, || {
                {
                    let (flights, waiting, tx) = (Arc::clone(&flights), Arc::clone(&waiting), tx.clone());
                    pool.submit(
                        None,
                        move || {
                            frame::task(Tag::Plane, |outer| {
                                *outer.alloc(1u64) += 1;
                                let before = super::with_local(|local| local.index).unwrap();
                                frame::task(Tag::Plane, |inner| {
                                    *inner.alloc(2u64) += 1;
                                    waiting.store(1, Ordering::SeqCst);
                                    assert_eq!(flights.get_or_run(at, || panic!("the client runs the key")), Ok(round));
                                    frame::current(|now| {
                                        assert!(std::ptr::eq(now, inner), "the job resumed in another frame");
                                        let value: *const u64 = now.alloc(3u64);
                                        assert!(inner.contains(value.cast()), "the job allocated outside its own frame");
                                    });
                                });
                                frame::task(Tag::Plane, |after| {
                                    frame::current(|now| {
                                        assert!(std::ptr::eq(now, after), "a task opened after the switch is not current");
                                        let values = now.alloc_slice_fill_with(64, |at| at as u64 + u64::from(round));
                                        assert!(
                                            outer.contains(values.as_ptr().cast()),
                                            "a task opened after the switch did not start in its own job's frame"
                                        );
                                        assert_eq!(values[63], 63 + u64::from(round));
                                    });
                                });
                                assert_eq!(*outer.alloc(4u64), 4, "the outer frame took its chunk back");
                                let after = super::with_local(|local| local.index).unwrap();
                                Some((before, after))
                            })
                        },
                        move |it| _ = tx.send(it.map_err(|_| "the waiting job panicked".to_owned())),
                    );
                }
                until("the job to park", || {
                    waiting.load(Ordering::SeqCst) == 1 && pool.shared.parked.load(Ordering::SeqCst) == 1
                });
                for _ in 0..2 {
                    let tx = tx.clone();
                    pool.submit(
                        None,
                        || {
                            assert!(!frame::in_task(), "a job began inside a parked job's frame task");
                            frame::task(Tag::Plane, |mine| {
                                let filled = mine.alloc_slice_fill_with(1024, |at| at as u64);
                                frame::task(Tag::Plane, |_| crate::spin::busy(Duration::from_millis(5)));
                                assert_eq!(filled[1023], 1023, "another job wrote into this job's frame");
                            });
                            None
                        },
                        move |it| _ = tx.send(it.map_err(|_| "a job meanwhile panicked".to_owned())),
                    );
                }
                crate::spin::busy(Duration::from_millis(2));
                Ok(round)
            });
            assert_eq!(held, Ok(round));
            for _ in 0..3 {
                if let Some((before, after)) = rx.recv_timeout(LONG).unwrap().unwrap() {
                    moved += usize::from(before != after);
                }
            }
        }
        assert!(moved > 0, "in 50 rounds the waiting job never resumed on the other worker");
    }

    /// One function, so the compiler may keep a thread-local's address from its first submit to
    /// its last, across the wait between them, as it did in checkpoint 36's `Build::walk`.
    #[inline(never)]
    fn submit_wait_submit(
        flights: &Flights<u32>,
        at: Key,
        round: u32,
        waiting: &AtomicUsize,
    ) -> (bool, usize) {
        let ran = AtomicUsize::new(0);
        let mut resumed_elsewhere = false;
        super::scope_here(|scope| {
            let before = super::this_thread();
            for _ in 0..2 {
                scope.submit(None, || ran.fetch_add(1, Ordering::SeqCst), |it| _ = it.unwrap());
            }
            waiting.store(1, Ordering::SeqCst);
            assert_eq!(flights.get_or_run(at, || panic!("the client runs the key")), Ok(round));
            resumed_elsewhere = super::this_thread() != before;
            for _ in 0..2 {
                scope.submit(None, || ran.fetch_add(1, Ordering::SeqCst), |it| _ = it.unwrap());
            }
        });
        (resumed_elsewhere, ran.load(Ordering::SeqCst))
    }

    /// Checkpoint 36's "RefCell already borrowed" at pool.rs:247 and :381 (hyperfine and the
    /// self-build, 2026-11-02): `Build::walk` ran two `par::map`s in a loop, each a scope's
    /// submits and a wait, and the compiler kept the job-cell caches' and the worker's
    /// thread-local addresses from the function's entry, so a job resumed on another worker took
    /// cells from, and gave jobs to, the worker it left, racing that worker's own jobs. Here a job
    /// submits to a scope, waits until it may resume on the other worker, and submits again in
    /// the same function, in rounds until it moved: every submit uses the thread it runs on
    /// (`Caches::owner`, the LIFO slot's thread), and every job of the scope runs.
    #[test]
    fn a_job_resumed_on_another_worker_submits_through_that_workers_own_thread_locals() {
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        let mut moved = 0;
        for round in 0..200u32 {
            if moved >= 3 {
                break;
            }
            let at = key(3_000 + round);
            let (tx, rx) = unbounded::<std::thread::Result<(bool, usize)>>();
            let waiting = Arc::new(AtomicUsize::new(0));
            let held = flights.get_or_run(at, || {
                {
                    let (flights, waiting) = (Arc::clone(&flights), Arc::clone(&waiting));
                    pool.submit(
                        None,
                        move || submit_wait_submit(&flights, at, round, &waiting),
                        move |it| _ = tx.send(it),
                    );
                }
                until("the job to park", || {
                    waiting.load(Ordering::SeqCst) == 1
                        && pool.shared.parked.load(Ordering::SeqCst) == 1
                });
                // Both workers busy a while, so the parked job may resume on either.
                for _ in 0..2 {
                    pool.submit(
                        None,
                        || crate::spin::busy(Duration::from_millis(5)),
                        |it| it.unwrap(),
                    );
                }
                crate::spin::busy(Duration::from_millis(2));
                Ok(round)
            });
            assert_eq!(held, Ok(round));
            let got = rx.recv_timeout(LONG).expect("the job ends");
            let (resumed_elsewhere, ran) =
                got.unwrap_or_else(|panic| panic!("round {round}: {}", message(&*panic)));
            assert_eq!(ran, 4, "round {round}: a job of the scope did not run");
            moved += usize::from(resumed_elsewhere);
        }
        assert!(moved > 0, "in 200 rounds the job never resumed on the other worker");
    }

    /// The other half of checkpoint 36's stall: a job whose start panicked (its cell's give-back,
    /// pool.rs:247, on a worker whose caches were left borrowed) was dropped unrun, so its `done`
    /// never ran and the unit waiting on it waited for good. Here a job leaves its worker's caches
    /// borrowed for good; each job that worker starts after fails through its own `done`, with a
    /// message naming the job and the panic, a scope's job as well, and the scope's call returns.
    #[test]
    fn a_job_whose_start_panics_fails_through_its_done_and_is_never_lost() {
        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        let (tx, rx) = unbounded::<()>();
        pool.submit(
            None,
            || super::CACHES.with(|caches| std::mem::forget(caches.borrow_mut())),
            move |it| {
                it.unwrap();
                _ = tx.send(());
            },
        );
        rx.recv_timeout(LONG).expect("the job that leaves its caches borrowed ends");
        let (tx, rx) = unbounded::<String>();
        pool.submit(
            None,
            || 7,
            move |it| _ = tx.send(message(&*it.expect_err("a job whose start panicked ran"))),
        );
        let said =
            rx.recv_timeout(Duration::from_secs(10)).expect("the job was lost: its done never ran");
        assert!(said.contains("could not start job") && said.contains("borrowed twice"), "{said}");
        let mut got = None;
        pool.scope(|scope| {
            scope.submit(None, || 7, |it| got = Some(it.map_err(|panic| message(&*panic))))
        });
        let said = got
            .expect("the scope's job told its done")
            .expect_err("a scope's job whose start panicked ran");
        assert!(said.contains("could not start job"), "{said}");
    }

    /// The poolborrow stall (2026-11-02) went unseen for ten minutes as the daemon's watchdog
    /// counted every span closed as progress, and a pool with a job parked goes on closing its
    /// workers' `search`, `idle` and `switch` spans, its last sleeper checking for a stall every
    /// [`STALL_AFTER`]. The pool's own count of progress, its jobs ended, stands still through
    /// all that and moves by one for each job that ends.
    #[test]
    fn the_jobs_ended_count_moves_only_as_jobs_end_never_while_workers_idle() {
        use crate::sched::{self, Wait};
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let at = Key([0x5a; 32]);
        assert!(sched::claim(at), "the key is new");
        // Ends the key however the test ends, before the pool's drop waits for the parked job.
        struct Done(Option<Key>);
        impl Drop for Done {
            fn drop(&mut self) {
                if let Some(at) = self.0.take() {
                    sched::key_done(at);
                }
            }
        }
        let mut done = Done(Some(at));
        let (tx, rx) = unbounded::<bool>();
        {
            let tx = tx.clone();
            pool.submit(
                None,
                move || sched::wait(Wait::Key(at)).is_ok(),
                move |it| _ = tx.send(it.unwrap_or(false)),
            );
        }
        until("the job to park", || pool.parked() == 1);
        let before = pool.ended();
        // Past the pool's stall check: its workers search, park, check and park again.
        assert!(
            rx.recv_timeout(STALL_AFTER * 2).is_err(),
            "the parked job ended while its key was held"
        );
        assert_eq!(pool.ended(), before, "the pool's idle workers moved its count of jobs ended");
        if let Some(at) = done.0.take() {
            sched::key_done(at);
        }
        assert_eq!(rx.recv_timeout(LONG), Ok(true), "the job ends once its key is done");
        until("the job's end to be counted", || pool.ended() == before + 1);
        for _ in 0..10 {
            let tx = tx.clone();
            pool.submit(None, || true, move |it| _ = tx.send(it.unwrap_or(false)));
        }
        for _ in 0..10 {
            assert_eq!(rx.recv_timeout(LONG), Ok(true));
        }
        until("ten more jobs' ends to be counted", || pool.ended() == before + 11);
    }

    /// A panic's message, as a job's `done` gets it.
    fn message(panic: &(dyn std::any::Any + Send)) -> String {
        panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|it| (*it).to_owned()))
            .unwrap_or_default()
    }

    /// The hung test of 2026-11-03 (hung-9554, 80 minutes): a thread claims a key, a job of its
    /// pool parks on it, and the thread panics, dropping the pool as it unwinds with the key
    /// still held. The drop waits for the job, the job for the key, and the thread ends the key
    /// only after the drop. The job must wake at once to fail naming the key and its holder,
    /// and the drop end.
    #[test]
    fn a_panic_while_holding_a_key_ends_the_pool_promptly_with_an_error_naming_the_key() {
        use crate::sched::{self, Wait};
        let key = Key([0x5e; 32]);
        let (tx, rx) = unbounded::<Result<(), String>>();
        let began = std::time::Instant::now();
        let holder = std::thread::spawn(move || {
            let pool = Pool::new(&groups_of(1, 2), false).unwrap();
            assert!(sched::claim(key), "the key is new");
            pool.submit(
                None,
                move || _ = sched::wait(Wait::Key(key)),
                move |it| _ = tx.send(it.map_err(|panic| message(&*panic))),
            );
            until("the job to park on the key", || pool.parked() == 1);
            panic!("the key's holder panics, and its pool drops as it unwinds");
        });
        let joined = holder.join();
        let took = began.elapsed();
        assert!(joined.is_err(), "the holder's own panic goes on up");
        let failed =
            rx.recv_timeout(LONG).expect("the parked job ended").expect_err("the parked job fails");
        let hex: String = key.0.iter().map(|byte| format!("{byte:02x}")).collect();
        for name in ["pool stopped", &hex, "the thread dropping its pool", "unwinds from a panic"] {
            assert!(failed.contains(name), "the failure names `{name}`: {failed}");
        }
        assert!(
            took < Duration::from_secs(10),
            "the drop ended in {took:?}, not at its bound ({STOP_BOUND:?})"
        );
    }

    /// A thread holds a key and stops pool P, whose one job waits (through a scope of pool Q) for
    /// a job of Q that waits for the key: the stop waits for P's job, it for Q's, and Q's for the
    /// stopping thread. The stop's wait is in the graph, so Q's stall check sees the cycle (the
    /// stopper is no running client that may still end its key) and fails Q's job naming the
    /// stop; P's job ends with it, and the stop ends well inside its bound rather than failing
    /// what is parked at it and giving up a bound later.
    #[test]
    fn a_stop_by_the_holder_of_a_key_another_pools_job_waits_on_fails_that_job_naming_the_stop() {
        use crate::sched::{self, Wait};
        let key = Key([0x7c; 32]);
        let bound = Duration::from_secs(5);
        let (tx, rx) = unbounded::<Result<(), String>>();
        let q = Arc::new(Pool::new(&groups_of(1, 1), false).unwrap());
        let p = Pool::new(&groups_of(1, 1), false).unwrap();
        assert!(sched::claim(key), "the key is new");
        let inner = Arc::clone(&q);
        p.submit(
            None,
            move || {
                inner.scope(|scope| {
                    scope.submit(
                        None,
                        move || _ = sched::wait(Wait::Key(key)),
                        move |it| _ = tx.send(it.map_err(|panic| message(&*panic))),
                    );
                });
            },
            |_| {},
        );
        until("Q's job to park on the key and P's on Q's scope", || {
            q.parked() == 1 && p.parked() == 1
        });
        let began = std::time::Instant::now();
        let stopped = p.stop(bound);
        let took = began.elapsed();
        sched::key_done(key);
        assert!(stopped.is_ok(), "the stop ended: {stopped:?}");
        assert!(took < bound, "the stop ended in {took:?}, not at its bound ({bound:?})");
        let failed = rx.recv_timeout(LONG).expect("Q's job ended").expect_err("Q's job fails");
        for name in ["stalled", "which it stops"] {
            assert!(failed.contains(name), "the failure names `{name}`: {failed}");
        }
        drop(q);
    }

    /// The stop's bound as a transcript on its injected clock: progress moves it; at the bound
    /// what is parked fails; it gives up only when a bound goes by with nothing moving after that.
    #[test]
    fn a_stop_fails_what_is_parked_at_its_bound_and_gives_up_only_when_that_moved_nothing() {
        let mut clock = StopClock::new(100, Duration::from_nanos(50));
        assert_eq!(clock.advance_clock(120, false), StopStep::Wait { until: 150 });
        assert_eq!(
            clock.advance_clock(140, true),
            StopStep::Wait { until: 190 },
            "progress moves the bound"
        );
        assert_eq!(clock.advance_clock(190, false), StopStep::FailParked { waited_ns: 50 });
        assert_eq!(
            clock.advance_clock(200, true),
            StopStep::Wait { until: 250 },
            "the failed jobs ending is progress"
        );
        assert_eq!(
            clock.advance_clock(250, false),
            StopStep::FailParked { waited_ns: 50 },
            "what parked since fails too"
        );
        assert_eq!(clock.advance_clock(299, false), StopStep::Wait { until: 300 });
        assert_eq!(clock.advance_clock(300, false), StopStep::GiveUp { waited_ns: 50 });
    }

    /// A job that waits inside a scope, resumes and panics there, a job of the scope still
    /// running, fails alone: its `done` gets its panic's message, the scope's job ends, and
    /// the pool runs the next job. No worker and no process goes down with it.
    #[test]
    fn a_job_that_panics_inside_a_scope_fails_alone_and_the_pool_runs_the_next() {
        let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
        let flights = Arc::new(Flights::<u32>::new());
        for round in 0..20u32 {
            let at = key(3_000 + round);
            let (tx, rx) = unbounded::<Result<u32, String>>();
            let waiting = Arc::new(AtomicUsize::new(0));
            let scoped = Arc::new(AtomicUsize::new(0));
            let held = flights.get_or_run(at, || {
                {
                    let (inner, flights, waiting, scoped, tx) = (
                        Arc::clone(&pool),
                        Arc::clone(&flights),
                        Arc::clone(&waiting),
                        Arc::clone(&scoped),
                        tx.clone(),
                    );
                    pool.submit(
                        None,
                        move || {
                            inner.scope(|scope| {
                                scope.submit(
                                    None,
                                    || crate::spin::busy(Duration::from_millis(3)),
                                    |_| _ = scoped.fetch_add(1, Ordering::SeqCst),
                                );
                                waiting.store(1, Ordering::SeqCst);
                                assert_eq!(
                                    flights.get_or_run(at, || panic!("the client runs the key")),
                                    Ok(round)
                                );
                                panic!("round {round} failed inside its scope");
                            })
                        },
                        move |it: std::thread::Result<()>| {
                            _ = tx.send(it.map(|()| round).map_err(|it| message(&*it)))
                        },
                    );
                }
                until("the job to park", || {
                    waiting.load(Ordering::SeqCst) == 1
                        && pool.shared.parked.load(Ordering::SeqCst) == 1
                });
                Ok(round)
            });
            assert_eq!(held, Ok(round));
            assert_eq!(
                rx.recv_timeout(LONG).unwrap(),
                Err(format!("round {round} failed inside its scope"))
            );
            assert_eq!(
                scoped.load(Ordering::SeqCst),
                1,
                "the scope's job did not end before its owner failed"
            );
            pool.submit(
                None,
                move || round + 1,
                move |it| _ = tx.send(it.map_err(|it| message(&*it))),
            );
            assert_eq!(
                rx.recv_timeout(LONG).unwrap(),
                Ok(round + 1),
                "the pool ran no job after the failure"
            );
        }
        assert_eq!(flights.running(), 0);
    }

    std::thread_local! {
        static REFUSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// The value of [`REFUSED`] whose put panics.
    const REFUSE: usize = usize::MAX;

    fn take_refused() -> Option<std::num::NonZeroUsize> {
        std::num::NonZeroUsize::new(REFUSED.replace(0))
    }

    fn put_refused(taken: std::num::NonZeroUsize) {
        assert_ne!(taken.get(), REFUSE, "a carried thread-local refused to move");
        REFUSED.set(taken.get());
    }

    /// A job whose carried thread-local panics as it is put back on the thread the job resumes
    /// on fails with that panic, raised where it waited: its own unwind runs, the pool goes on
    /// running jobs, and the parked fiber is never dropped suspended (which would abort).
    #[test]
    fn a_carried_put_that_panics_fails_its_job_and_the_pool_goes_on() {
        static CARRIED: std::sync::Once = std::sync::Once::new();
        CARRIED.call_once(|| {
            crate::sched::carry(crate::sched::Carried::word(take_refused, put_refused))
        });
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        for round in 0..10u32 {
            let at = key(4_000 + round);
            let (tx, rx) = unbounded::<Result<u32, String>>();
            let waiting = Arc::new(AtomicUsize::new(0));
            let unwound = Arc::new(AtomicUsize::new(0));
            let held = flights.get_or_run(at, || {
                {
                    let (flights, waiting, unwound, tx) = (
                        Arc::clone(&flights),
                        Arc::clone(&waiting),
                        Arc::clone(&unwound),
                        tx.clone(),
                    );
                    pool.submit(
                        None,
                        move || {
                            struct Unwound(Arc<AtomicUsize>);
                            impl Drop for Unwound {
                                fn drop(&mut self) {
                                    self.0.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            let _unwound = Unwound(unwound);
                            REFUSED.set(REFUSE);
                            waiting.store(1, Ordering::SeqCst);
                            let _ = flights.get_or_run(at, || panic!("the client runs the key"));
                            REFUSED.set(0);
                            round
                        },
                        move |it| _ = tx.send(it.map_err(|it| message(&*it))),
                    );
                }
                until("the job to park", || {
                    waiting.load(Ordering::SeqCst) == 1
                        && pool.shared.parked.load(Ordering::SeqCst) == 1
                });
                Ok(round)
            });
            assert_eq!(held, Ok(round));
            let failed = rx.recv_timeout(LONG).unwrap().unwrap_err();
            assert!(failed.contains("a carried thread-local refused to move"), "{failed}");
            assert_eq!(unwound.load(Ordering::SeqCst), 1, "the job's own unwind did not run");
            pool.submit(
                None,
                move || round + 1,
                move |it| _ = tx.send(it.map_err(|it| message(&*it))),
            );
            assert_eq!(
                rx.recv_timeout(LONG).unwrap(),
                Ok(round + 1),
                "the pool ran no job after the failure"
            );
        }
    }

    std::thread_local! {
        static LENT: std::cell::RefCell<Option<u32>> = const { std::cell::RefCell::new(None) };
    }

    /// Carries [`LENT`] with each job, once for every test of it: carried twice, the second
    /// carry's put of nothing finds the first's value on the thread.
    fn carry_lent() {
        static CARRIED: std::sync::Once = std::sync::Once::new();
        CARRIED.call_once(|| {
            crate::sched::carry(crate::sched::Carried {
                take: || stdx::lend::take_carried(&LENT, Option::is_none),
                put: |it| stdx::lend::put_carried(&LENT, Option::is_none, it),
                end: |it| stdx::lend::end_carried(&LENT, it),
            });
        });
    }

    /// Two jobs each lend a value to a carried thread-local ([`stdx::lend`]), park on a key and
    /// resume, often on each other's worker, after a job on each worker ended holding a value
    /// in the slot (ended with it, `go_off_ended`, so the worker's slot is empty again). A job
    /// always comes on to an empty slot, finds its own value after the wait, and the jobs after
    /// them find it empty: a value a job ends holding is ended, never left for the next job,
    /// and none outlives its job. (A worker that holds a value of its own across jobs is
    /// `sched::fiber_tests::a_workers_own_carried_value_is_set_aside_for_each_job_and_back_when_it_ends`.)
    #[test]
    fn jobs_holding_lends_resume_crosswise_after_jobs_that_ended_holding_one() {
        carry_lent();
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        let mut crossed = 0;
        for round in 0..50u32 {
            let (tx, rx) = unbounded::<Result<Option<(usize, usize)>, String>>();
            let leaving = Arc::new(AtomicUsize::new(0));
            for at in 0..2u32 {
                let (tx, leaving) = (tx.clone(), Arc::clone(&leaving));
                pool.submit(
                    None,
                    move || {
                        assert_eq!(
                            LENT.with_borrow(|it| *it),
                            None,
                            "a job came on to its worker's value"
                        );
                        LENT.set(Some(900 + at));
                        leaving.fetch_add(1, Ordering::SeqCst);
                        until("both workers to hold a value", || {
                            leaving.load(Ordering::SeqCst) == 2
                        });
                        None
                    },
                    move |it| {
                        _ = tx.send(it.map_err(|_| "a job leaving its value panicked".to_owned()))
                    },
                );
            }
            for _ in 0..2 {
                rx.recv_timeout(LONG).unwrap().unwrap();
            }
            let at = key(5_000 + round);
            let waiting = Arc::new(AtomicUsize::new(0));
            let held = flights.get_or_run(at, || {
                for mine in 0..2u32 {
                    let (flights, waiting, tx) =
                        (Arc::clone(&flights), Arc::clone(&waiting), tx.clone());
                    pool.submit(
                        None,
                        move || {
                            let before = super::with_local(|local| local.index).unwrap();
                            let mut value = 100 + mine;
                            stdx::lend(&LENT, &mut value, || {
                                waiting.fetch_add(1, Ordering::SeqCst);
                                assert_eq!(
                                    flights.get_or_run(at, || panic!("the client runs the key")),
                                    Ok(round)
                                );
                                assert_eq!(
                                    LENT.with_borrow(|it| *it),
                                    Some(100 + mine),
                                    "a job resumed with another's value"
                                );
                            });
                            assert_eq!(value, 100 + mine, "the lend came back changed");
                            let after = super::with_local(|local| local.index).unwrap();
                            Some((before, after))
                        },
                        move |it| _ = tx.send(it.map_err(|it| message(&*it))),
                    );
                }
                until("both jobs to park", || {
                    waiting.load(Ordering::SeqCst) == 2
                        && pool.shared.parked.load(Ordering::SeqCst) == 2
                });
                Ok(round)
            });
            assert_eq!(held, Ok(round));
            for _ in 0..2 {
                if let Some((before, after)) = rx.recv_timeout(LONG).unwrap().unwrap() {
                    crossed += usize::from(before != after);
                }
            }
            for _ in 0..4 {
                let tx = tx.clone();
                pool.submit(
                    None,
                    || {
                        assert_eq!(
                            LENT.with_borrow(|it| *it),
                            None,
                            "a job came on to another's value"
                        );
                        None
                    },
                    move |it| _ = tx.send(it.map_err(|_| "a job after them panicked".to_owned())),
                );
            }
            for _ in 0..4 {
                rx.recv_timeout(LONG).unwrap().unwrap();
            }
        }
        assert!(crossed > 0, "in 50 rounds no job resumed on the other worker");
    }

    /// On a pool of one worker, after a job ended holding a value in the carried slot (ended
    /// with it), two jobs each lend the slot a value and park, the first on one key and the
    /// second on another; the keys end in the other order, so they resume on the one thread
    /// swapped. Each sees its own value after its wait, and the jobs after them an empty slot:
    /// between jobs the slot is the worker's, set aside while a job runs and given back as it
    /// goes off.
    #[test]
    fn jobs_holding_lends_resume_on_one_thread_in_swapped_order() {
        carry_lent();
        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        for round in 0..20u32 {
            let (tx, rx) = unbounded::<Result<u32, String>>();
            {
                let tx = tx.clone();
                pool.submit(
                    None,
                    move || {
                        LENT.set(Some(900 + round));
                        0
                    },
                    move |it| _ = tx.send(it.map_err(|it| message(&*it))),
                );
            }
            assert_eq!(rx.recv_timeout(LONG).unwrap(), Ok(0));
            let (first, second) = (key(6_000 + 2 * round), key(6_001 + 2 * round));
            let order = Arc::new(Lock::new("graph.test.pool.order", Vec::new()));
            let parked = |count: usize| {
                until("the jobs to park", || pool.shared.parked.load(Ordering::SeqCst) == count)
            };
            let held = flights.get_or_run(first, || {
                let job = |mine: u32, on: Key| {
                    let (flights, order, tx) =
                        (Arc::clone(&flights), Arc::clone(&order), tx.clone());
                    pool.submit(
                        None,
                        move || {
                            let mut value = 100 + mine;
                            stdx::lend(&LENT, &mut value, || {
                                assert_eq!(
                                    flights.get_or_run(on, || panic!("the client runs the key")),
                                    Ok(round)
                                );
                                assert_eq!(
                                    LENT.with_borrow(|it| *it),
                                    Some(100 + mine),
                                    "a job resumed with another's value"
                                );
                            });
                            order.lock().push(mine);
                            mine
                        },
                        move |it| _ = tx.send(it.map_err(|it| message(&*it))),
                    );
                };
                job(0, first);
                parked(1);
                let held = flights.get_or_run(second, || {
                    job(1, second);
                    parked(2);
                    Ok(round)
                });
                assert_eq!(held, Ok(round));
                until("the second job to end", || order.lock().len() == 1);
                Ok(round)
            });
            assert_eq!(held, Ok(round));
            let mut ended = [rx.recv_timeout(LONG).unwrap(), rx.recv_timeout(LONG).unwrap()];
            ended.sort();
            assert_eq!(ended, [Ok(0), Ok(1)]);
            assert_eq!(*order.lock(), [1, 0], "the jobs did not resume in the swapped order");
            {
                pool.submit(
                    None,
                    || {
                        assert_eq!(
                            LENT.with_borrow(|it| *it),
                            None,
                            "a job came on to another's value"
                        );
                        2
                    },
                    move |it| _ = tx.send(it.map_err(|_| "a job after them panicked".to_owned())),
                );
            }
            assert_eq!(rx.recv_timeout(LONG).unwrap(), Ok(2));
        }
    }

    /// On a pool of one worker, a job waits on an event and holds no thread: the worker runs
    /// another job meanwhile. A thread of no pool fires it; the job goes on, and a wait after
    /// the fire, on a worker or on no pool, ends at once.
    #[test]
    fn a_job_waiting_on_an_event_holds_no_thread_until_another_thread_fires_it() {
        use crate::sched::{Event, EventWait};
        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        let event = Event::new();
        let on: EventWait = event.waiter();
        let (tx, rx) = unbounded::<&'static str>();
        {
            let tx = tx.clone();
            pool.submit(
                None,
                move || {
                    on.wait().unwrap();
                    "the waiter"
                },
                move |it| _ = tx.send(it.unwrap()),
            );
        }
        until("the waiter to park", || pool.shared.parked.load(Ordering::SeqCst) == 1);
        {
            let tx = tx.clone();
            pool.submit(None, || "a job meanwhile", move |it| _ = tx.send(it.unwrap()));
        }
        assert_eq!(rx.recv_timeout(LONG).unwrap(), "a job meanwhile");
        std::thread::spawn(move || event.fire()).join().unwrap();
        assert_eq!(rx.recv_timeout(LONG).unwrap(), "the waiter");
        on.wait().unwrap();
        pool.submit(
            None,
            move || {
                on.wait().unwrap();
                "after the fire"
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        assert_eq!(rx.recv_timeout(LONG).unwrap(), "after the fire");
    }

    /// An event rearmed ends the waits on it before, as a fire does, and is armed again: a
    /// waiter taken after waits for the next fire, a job on a worker parked meanwhile, and its
    /// last fire (the drop) ends that wait too.
    #[test]
    fn a_rearmed_event_ends_the_waits_before_and_holds_the_ones_after() {
        use crate::sched::{Event, EventWait};
        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        let mut event = Event::new();
        let first: EventWait = event.waiter();
        event.rearm();
        first.wait().unwrap();
        let second: EventWait = event.waiter();
        assert_ne!(first, second, "a rearmed event waits under a new key");
        let (tx, rx) = unbounded::<&'static str>();
        pool.submit(
            None,
            move || {
                second.wait().unwrap();
                "the second waiter"
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        until("the second waiter to park", || pool.shared.parked.load(Ordering::SeqCst) == 1);
        assert!(rx.try_recv().is_err(), "the second waiter waits for the next fire");
        event.rearm();
        assert_eq!(rx.recv_timeout(LONG).unwrap(), "the second waiter");
        let third = event.waiter();
        std::thread::spawn(move || event.fire()).join().unwrap();
        third.wait().unwrap();
    }

    /// On a pool of one worker, a job waits on the pool's clock and holds no thread: it parks,
    /// the worker runs another job meanwhile, then sleeps with nothing queued, and wakes for the
    /// deadline. The job goes on no earlier than it; a wait for a time gone by ends at once.
    ///
    /// The park is seen only while the wait lasts, and a test thread kept off its core longer
    /// than that (a loaded machine) misses it: then the wait ended first, and the round is
    /// tried again with a wait twice as long, never left polling a park that is over (the
    /// pre-2026-11-02 flake, "timed out after 60s waiting for the timed job to park"). A job
    /// that held its thread would never park in any round, and fails naming every wait tried.
    #[test]
    fn a_job_waiting_for_a_time_holds_no_thread_and_goes_on_once_it_is_due() {
        use crate::sched::{self, Wait};
        const ROUNDS: u32 = 8;
        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        for round in 0..ROUNDS {
            let wait_ns = 20_000_000u64 << round;
            let (tx, rx) = unbounded::<(&'static str, Result<u64, String>)>();
            {
                let tx = tx.clone();
                pool.submit(
                    None,
                    move || {
                        let at = sched::now() + wait_ns;
                        sched::wait(Wait::Until(at)).unwrap();
                        let late = sched::now()
                            .checked_sub(at)
                            .expect("the job went on before its deadline");
                        sched::wait(Wait::Until(at)).unwrap();
                        late
                    },
                    move |it| _ = tx.send(("timed", it.map_err(|it| message(&*it)))),
                );
            }
            let seen = std::cell::Cell::new(false);
            until("the timed job to park or end", || {
                seen.set(pool.parked() == 1);
                seen.get() || !rx.is_empty()
            });
            let parked = seen.get();
            if parked {
                pool.submit(
                    None,
                    || u64::MAX,
                    move |it| _ = tx.send(("meanwhile", it.map_err(|it| message(&*it)))),
                );
            }
            let first = rx.recv_timeout(LONG).expect("the pool's first job's end");
            if first.0 == "timed" {
                // The wait was over before this thread saw the park, or before the job given
                // meanwhile ran: that one ends too, and the round goes again.
                assert!(first.1.is_ok(), "round {round}: the timed job failed: {first:?}");
                if parked {
                    assert_eq!(
                        rx.recv_timeout(LONG),
                        Ok(("meanwhile", Ok(u64::MAX))),
                        "round {round}: the job meanwhile"
                    );
                }
                continue;
            }
            assert_eq!(first, ("meanwhile", Ok(u64::MAX)), "round {round}: the job meanwhile");
            let (_, late) = rx.recv_timeout(LONG).expect("the timed job's end");
            let late =
                late.unwrap_or_else(|it| panic!("round {round}: the timed job failed: {it}"));
            assert!(
                late < u64::try_from(LONG.as_nanos()).unwrap(),
                "the timed job went on {late} ns late"
            );
            assert_eq!(pool.parked(), 0);
            return;
        }
        panic!(
            "in {ROUNDS} rounds, waits of 20 ms to {} ms, the timed job never parked while its wait lasted",
            20u64 << (ROUNDS - 1)
        );
    }

    /// A job's wait for a time past the pool's stall check, with every pool idle meanwhile: no
    /// stall, as the clock ends it, so the job goes on with its value.
    #[test]
    fn a_wait_for_a_time_is_never_a_stall() {
        use crate::sched::{self, Wait};
        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        let (tx, rx) = unbounded::<Result<(), String>>();
        pool.submit(
            None,
            || {
                sched::wait(Wait::Until(
                    sched::now() + u64::try_from(STALL_AFTER.as_nanos()).unwrap() * 5 / 4,
                ))
                .unwrap()
            },
            move |it| _ = tx.send(it.map_err(|it| message(&*it))),
        );
        assert_eq!(rx.recv_timeout(LONG).unwrap(), Ok(()));
    }

    /// On a pool of one worker, busy while a background job and then two normal ones are
    /// given: the normal ones run first, then the background one.
    #[test]
    fn a_background_job_runs_after_the_normal_ones_queued_with_it() {
        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        let (go_tx, go_rx) = bounded::<()>(0);
        let (tx, rx) = unbounded::<&'static str>();
        let started = Arc::new(AtomicUsize::new(0));
        {
            let started = Arc::clone(&started);
            pool.submit(
                None,
                move || {
                    started.store(1, Ordering::SeqCst);
                    go_rx.recv().unwrap();
                },
                |it| it.unwrap(),
            );
        }
        until("the worker to be busy", || started.load(Ordering::SeqCst) == 1);
        for (class, name) in
            [(Class::Background, "background"), (Class::Normal, "first"), (Class::Normal, "second")]
        {
            let tx = tx.clone();
            pool.submit_as(None, class, move || name, move |it| _ = tx.send(it.unwrap()));
        }
        go_tx.send(()).unwrap();
        let ran: Vec<_> = (0..3).map(|_| rx.recv_timeout(LONG).unwrap()).collect();
        assert_eq!(ran, ["first", "second", "background"]);
    }

    /// A scope's background job, on a pool of one worker that always has a normal job queued
    /// (each gives the next): never taken as background, it runs once the scope's call waits
    /// on it, promoted behind the queued one.
    #[test]
    fn a_scope_waiting_on_its_background_job_runs_it_ahead_of_normal_work() {
        fn again(pool: &Arc<Pool>, stop: Arc<AtomicUsize>) {
            if stop.load(Ordering::SeqCst) == 0 {
                let next = Arc::clone(pool);
                pool.submit(None, move || again(&next, stop), |it| it.unwrap());
            }
        }
        let pool = Arc::new(Pool::new(&Topology::flat(1), false).unwrap());
        let stop = Arc::new(AtomicUsize::new(0));
        again(&pool, Arc::clone(&stop));
        let ran = AtomicUsize::new(0);
        pool.scope(|scope| {
            scope.submit_as(
                None,
                Class::Background,
                || ran.store(1, Ordering::SeqCst),
                |it| it.unwrap(),
            );
        });
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        stop.store(1, Ordering::SeqCst);
        until("the last of the chain to end", || Arc::strong_count(&pool) == 1);
    }

    /// A scope's job given to a worker's LIFO slot is taken by another worker while the first
    /// blocks its thread: the slot is stealable.
    #[test]
    fn a_job_in_a_blocked_workers_slot_is_stolen() {
        let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
        let (tx, rx) = bounded::<Option<u32>>(1);
        let inner = Arc::clone(&pool);
        pool.submit(
            None,
            move || {
                let mine = inner.worker();
                let (ran_tx, ran_rx) = bounded::<Option<u32>>(1);
                let asked = Arc::clone(&inner);
                inner.scope(|scope| {
                    scope.submit(
                        None,
                        move || asked.worker(),
                        move |it| _ = ran_tx.send(it.unwrap()),
                    );
                    // Blocks this worker's thread: only the other can run the job.
                    let ran = ran_rx.recv_timeout(LONG).expect("the other worker took it");
                    assert_ne!(ran, mine);
                    ran
                })
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        assert!(rx.recv_timeout(LONG).unwrap().is_some());
    }

    /// One worker: a job gives itself `a` then `b` to run next, so `a` goes to the deque; `b`
    /// starts a chain of jobs each giving itself the next. The chain runs from the LIFO slot,
    /// but `a` runs after at most three of it.
    #[test]
    fn the_lifo_slot_runs_the_job_just_given_but_never_starves_the_deque() {
        fn chain(pool: &Arc<Pool>, log: Arc<Lock<Vec<String>>>, n: u32) {
            log.lock().push(format!("b{n}"));
            if n < 20 {
                let next = Arc::clone(pool);
                pool.submit_next(move || chain(&next, log, n + 1), |it| it.unwrap());
            }
        }
        let pool = Arc::new(Pool::new(&Topology::flat(1), false).unwrap());
        let log = Arc::new(Lock::new("graph.test.pool.log", Vec::new()));
        {
            let (inner, log) = (Arc::clone(&pool), Arc::clone(&log));
            pool.submit(
                None,
                move || {
                    let a = Arc::clone(&log);
                    inner.submit_next(move || a.lock().push("a".to_owned()), |it| it.unwrap());
                    let next = Arc::clone(&inner);
                    inner.submit_next(move || chain(&next, log, 0), |it| it.unwrap());
                },
                |it| it.unwrap(),
            );
        }
        {
            until("the chain to end", || log.lock().len() == 22);
        }
        let log = log.lock().clone();
        assert_eq!(&log[..3], ["b0", "b1", "b2"], "the job just given runs next: {log:?}");
        assert_eq!(log[3], "a", "after three from the slot, the deque's job: {log:?}");
        assert_eq!(log.iter().filter(|it| it.starts_with('b')).count(), 21);
    }

    /// Off the pool, `submit_next` is `submit`; and each worker says its own index.
    #[test]
    fn workers_say_their_index_and_submit_next_works_off_the_pool() {
        let pool = Arc::new(Pool::new(&groups_of(2, 2), false).unwrap());
        assert_eq!(pool.worker(), None);
        let other = Arc::new(Pool::new(&Topology::flat(1), false).unwrap());
        let (tx, rx) = unbounded::<(Option<u32>, Option<u32>)>();
        for _ in 0..64 {
            let (tx, mine, theirs) = (tx.clone(), Arc::clone(&pool), Arc::clone(&other));
            pool.submit_next(
                move || (mine.worker(), theirs.worker()),
                move |it| _ = tx.send(it.unwrap()),
            );
        }
        for _ in 0..64 {
            let (mine, theirs) = rx.recv_timeout(LONG).unwrap();
            assert!(mine.is_some_and(|it| it < 4), "{mine:?}");
            assert_eq!(theirs, None, "a worker of one pool is none of another's");
        }
    }

    /// Once the work is done, every worker parks and none is left counted as searching.
    #[test]
    fn idle_workers_park_and_leave_no_searcher() {
        let pool = Pool::new(&groups_of(2, 3), false).unwrap();
        let (tx, rx) = unbounded::<()>();
        for job in 0..500u32 {
            let tx = tx.clone();
            pool.submit(
                (job % 3 != 0).then_some(job % 2),
                || std::hint::black_box(()),
                move |_| {
                    _ = tx.send(());
                },
            );
        }
        for _ in 0..500 {
            rx.recv_timeout(LONG).unwrap();
        }
        until("every worker to park", || {
            let word = pool.shared.idle.0.load(Ordering::SeqCst);
            searching(word) == 0 && sleeping(word) == 6
        });
        let asleep: u32 =
            pool.shared.groups.iter().map(|it| it.sleepers.load(Ordering::SeqCst)).sum();
        assert_eq!(asleep, 6);
    }

    #[derive(Clone, Copy)]
    enum How {
        Submit(Option<u32>),
        Next,
        Scope,
    }

    struct Tree {
        children: Vec<Vec<(How, usize)>>,
        ran: Vec<AtomicU32>,
        ended: AtomicUsize,
    }

    fn run_node(pool: &Arc<Pool>, tree: &Arc<Tree>, node: usize) {
        tree.ran[node].fetch_add(1, Ordering::SeqCst);
        let mut scoped = Vec::new();
        for &(how, child) in &tree.children[node] {
            let (pool2, tree2) = (Arc::clone(pool), Arc::clone(tree));
            let job = move || run_node(&pool2, &tree2, child);
            match how {
                How::Submit(near) => pool.submit(near, job, |it| it.unwrap()),
                How::Next => pool.submit_next(job, |it| it.unwrap()),
                How::Scope => scoped.push(child),
            }
        }
        if !scoped.is_empty() {
            pool.scope(|scope| {
                for &child in &scoped {
                    scope.submit(None, move || run_node(pool, tree, child), |it| it.unwrap());
                }
            });
        }
        tree.ended.fetch_add(1, Ordering::SeqCst);
    }

    /// Random trees of jobs that give their children by `submit` (near a random group or
    /// none), `submit_next` and a scope, on random topologies: every node of the model's tree
    /// runs exactly once, and then every worker parks with no searcher left. Scopes nest as
    /// deep as the tree: each waiting caller holds no worker.
    #[test]
    fn a_seeded_stress_runs_every_job_of_the_model_once() {
        for seed in 0..10u64 {
            let mut rng = Rng::new(seed);
            let (count, each) = (1 + rng.below(3) as u32, 1 + rng.below(3) as u32);
            let mut children: Vec<Vec<(How, usize)>> = vec![Vec::new()];
            let mut at = 0;
            while at < children.len() && children.len() < 600 {
                let mut scopes = false;
                for _ in 0..rng.below(4) {
                    let mut how = match rng.below(4) {
                        0 => How::Submit(None),
                        1 => How::Submit(Some(rng.below(u64::from(count) + 1) as u32)),
                        2 => How::Next,
                        _ => How::Scope,
                    };
                    if matches!(how, How::Scope) {
                        if scopes {
                            how = How::Next;
                        }
                        scopes = true;
                    }
                    let child = children.len();
                    children.push(Vec::new());
                    children[at].push((how, child));
                }
                at += 1;
            }
            let nodes = children.len();
            let tree = Arc::new(Tree {
                children,
                ran: (0..nodes).map(|_| AtomicU32::new(0)).collect(),
                ended: AtomicUsize::new(0),
            });
            let pool = Arc::new(Pool::new(&groups_of(count, each), false).unwrap());
            {
                let (inner, tree) = (Arc::clone(&pool), Arc::clone(&tree));
                pool.submit(None, move || run_node(&inner, &tree, 0), |it| it.unwrap());
            }
            until("the tree to run", || tree.ended.load(Ordering::SeqCst) == nodes);
            assert!(
                tree.ran.iter().all(|it| it.load(Ordering::SeqCst) == 1),
                "seed {seed}: a job ran other than once"
            );
            let workers = u64::from(count * each);
            until("every worker to park", || {
                let word = pool.shared.idle.0.load(Ordering::SeqCst);
                searching(word) == 0 && sleeping(word) == workers
            });
        }
    }

    /// `least` against the model: the group first in the order (load per worker, slow after
    /// fast, index), over seeded loads, of every group or the fast ones alone.
    #[test]
    fn the_least_loaded_group_is_the_models() {
        let mut rng = Rng::new(7);
        for _ in 0..2000 {
            let count = 1 + rng.below(6) as usize;
            let groups: Vec<(u64, u64, bool)> =
                (0..count).map(|_| (rng.below(9), 1 + rng.below(4), rng.below(2) == 0)).collect();
            for only_fast in [false, true] {
                let model = (0..count)
                    .filter(|&at| !only_fast || groups[at].2)
                    .min_by(|&a, &b| {
                        let ((la, wa, fa), (lb, wb, fb)) = (groups[a], groups[b]);
                        (u128::from(la) * u128::from(wb))
                            .cmp(&(u128::from(lb) * u128::from(wa)))
                            .then(fb.cmp(&fa))
                            .then(a.cmp(&b))
                    })
                    .unwrap_or(0);
                assert_eq!(super::least(count, only_fast, |at| groups[at]), model, "{groups:?}");
            }
        }
    }

    /// Two fast groups of two and a slow group of four (an Arm big.LITTLE machine's shape; a
    /// Mac's are all fast): a critical job goes to its `near` group only if that is fast, else
    /// to a fast group; any other job goes to `near`, and one near no group to a least loaded
    /// one (fast first on a tie, so group 0 when idle).
    #[test]
    fn a_critical_job_is_placed_on_a_fast_group() {
        let topology = Topology {
            cpus: (0..8u32)
                .map(|id| Cpu { id, group: (id / 2).min(2), socket: 0, node: 0, fast: id < 4 })
                .collect(),
        };
        let pool = Pool::new(&topology, false).unwrap();
        let shared = &*pool.shared;
        assert!(shared.mixed);
        assert_eq!(shared.groups.iter().map(|it| it.fast).collect::<Vec<_>>(), [true, true, false]);
        until("every worker to park", || sleeping(shared.idle.0.load(Ordering::SeqCst)) == 8);
        for near in [None, Some(0), Some(1), Some(2), Some(9)] {
            let at = shared.place(near, true);
            assert!(shared.groups[at].fast, "critical near {near:?} went to slow group {at}");
            if let Some(near @ (0 | 1)) = near {
                assert_eq!(at, near as usize);
            }
        }
        assert_eq!(shared.place(Some(2), false), 2);
        assert_eq!(shared.place(Some(1), false), 1);
        assert_eq!(shared.place(None, false), 0);
        assert_eq!(shared.place(Some(9), false), 0, "a group the pool lacks is none");
        let (tx, rx) = unbounded::<Option<u32>>();
        for _ in 0..50 {
            let tx = tx.clone();
            pool.submit_critical(Some(2), group, move |it| _ = tx.send(it.unwrap()));
        }
        for _ in 0..50 {
            assert!(rx.recv_timeout(LONG).unwrap().is_some_and(|it| it < 3));
        }
        assert_eq!(pool.critical_submitted(), 50);
    }

    /// Where every group is fast, a critical job is placed as any other.
    #[test]
    fn on_an_all_fast_machine_critical_changes_nothing() {
        let pool = Pool::new(&groups_of(3, 2), false).unwrap();
        let shared = &*pool.shared;
        assert!(!shared.mixed);
        until("every worker to park", || sleeping(shared.idle.0.load(Ordering::SeqCst)) == 6);
        for near in [None, Some(0), Some(1), Some(2), Some(5)] {
            assert_eq!(shared.place(near, true), shared.place(near, false), "near {near:?}");
        }
    }

    /// Each worker names its CPU's node: on two nodes of a group each, a job's node is that of
    /// the group it ran on, wherever it was stolen to; off the pool there is none.
    #[test]
    fn a_worker_says_its_numa_node() {
        let cpus = (0..4).map(|id| Cpu { id, group: id / 2, socket: 0, node: id / 2, fast: true });
        let pool = Pool::new(&Topology { cpus: cpus.collect() }, false).unwrap();
        assert_eq!(super::node(), None);
        let (tx, rx) = unbounded::<(Option<u32>, Option<u32>)>();
        for job in 0..200u32 {
            let tx = tx.clone();
            let ran = || (group(), super::node());
            pool.submit(Some(job % 2), ran, move |it| _ = tx.send(it.unwrap()));
        }
        for _ in 0..200 {
            let (group, node) = rx.recv_timeout(LONG).unwrap();
            assert!(group.is_some());
            assert_eq!(node, group);
        }
    }

    #[test]
    fn no_cpu_is_refused() {
        assert!(Pool::new(&Topology { cpus: Vec::new() }, false).is_err());
        let gap = Topology { cpus: vec![Cpu { id: 0, group: 1, socket: 0, node: 0, fast: true }] };
        assert!(Pool::new(&gap, false).is_err());
    }

    /// A radix sort of `PARALLEL` items or more runs its passes on the pool: given it, or
    /// called on one of its workers. Either way it sorts as the standard stable sort.
    #[test]
    fn a_large_radix_sort_runs_on_the_pool() {
        use blaze_radix as radix;
        fn input() -> (Vec<(u64, u32)>, Vec<(u64, u32)>) {
            let mut rng = Rng::new(11);
            let pairs: Vec<(u64, u32)> =
                (0..radix::PARALLEL as u32 + 7).map(|at| (rng.below(1000) << 20, at)).collect();
            let mut want = pairs.clone();
            want.sort_by_key(|it| it.0);
            (pairs, want)
        }
        let pool = Arc::new(Pool::new(&Topology::flat(4), false).unwrap());
        let (mut pairs, want) = input();
        radix::on(&*pool).sort_pairs(&mut pairs);
        assert_eq!(pairs, want);
        let (tx, rx) = bounded::<bool>(1);
        pool.submit(
            None,
            || {
                let (mut pairs, want) = input();
                radix::sort_pairs(&mut pairs);
                pairs == want
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        assert!(rx.recv_timeout(LONG).unwrap());
    }

    /// Every worker of a bound pool runs at one class ([`super::worker_qos`]: `user-initiated`,
    /// or `default` at background priority), whatever the performance level of its CPU: none
    /// at `utility` or `background`, which a loaded host starves while the
    /// worker steals critical jobs (the self-build profile of 2026-11-02: 12 of 18 workers at
    /// `utility` got about 2% of a core each and did 78-92% of the build's job time).
    #[cfg(target_os = "macos")]
    #[test]
    #[allow(unsafe_code)]
    fn no_worker_runs_at_a_qos_class_a_loaded_host_starves() {
        let topology = Topology::detect().unwrap();
        let pool = Pool::new(&topology, true).unwrap();
        let workers = pool.workers();
        let arrived = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = unbounded::<(Option<usize>, u32)>();
        for _ in 0..workers {
            let (tx, arrived) = (tx.clone(), Arc::clone(&arrived));
            pool.submit(
                None,
                move || {
                    let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
                    let mut relative = 0;
                    // SAFETY: the calling thread's own class and priority, written into the two
                    // locals; the class is one of `qos_class_t`'s, as the kernel only has those.
                    let got = unsafe {
                        libc::pthread_get_qos_class_np(
                            libc::pthread_self(),
                            &mut class,
                            &mut relative,
                        )
                    };
                    assert_eq!(got, 0, "pthread_get_qos_class_np");
                    let index = super::with_local(|local| local.index);
                    // Each job is held until every worker holds one, so each worker runs one.
                    arrived.fetch_add(1, Ordering::SeqCst);
                    until("every worker holds a job", || arrived.load(Ordering::SeqCst) == workers);
                    (index, class as u32)
                },
                move |it| _ = tx.send(it.unwrap()),
            );
        }
        drop(tx);
        let mut ran = vec![false; workers];
        for (index, class) in rx.iter() {
            let index = index.expect("a job runs on a worker");
            assert!(!std::mem::replace(&mut ran[index], true), "worker {index} ran two jobs");
            assert_eq!(
                class,
                super::worker_qos() as u32,
                "the QoS class of worker {index} of {:?}",
                topology.cpus[index]
            );
        }
        assert!(ran.iter().all(|it| *it), "not every worker ran a job: {ran:?}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn workers_bound_to_their_cpus_run_jobs() {
        let topology = Topology::detect().unwrap();
        let pool = Pool::new(&topology, true).unwrap();
        let (tx, rx) = unbounded::<Option<u32>>();
        for _ in 0..pool.workers() {
            let tx = tx.clone();
            pool.submit(None, group, move |it| _ = tx.send(it.unwrap()));
        }
        for _ in 0..pool.workers() {
            assert!(rx.recv_timeout(LONG).unwrap().is_some());
        }
    }
}

/// A pool one of whose workers cannot wait ([`Broken`]): it stops for the wait's error, and a
/// thread that waits on a scope of it fails with that error rather than wait, where once the
/// worker ended unheard and the thread waited forever (a build script's `rustc`, blaze itself,
/// under the sandbox's filter: `blaze build` of anything that depends on anyhow hung). And the
/// cause of that on Linux, with the cause removed: a process under a seccomp filter that
/// refuses io_uring, as the sandbox's does, parks and wakes its workers on the direct driver
/// (`blaze_core::wait::choose_from_env`) and never asks for a ring.
#[cfg(all(test, not(blaze_check)))]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod broken {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use crossbeam_channel::bounded;

    use super::{Pool, Unable, park_plant};
    use crate::topology::{Cpu, Topology};

    const LONG: Duration = Duration::from_secs(60);
    /// How long the control gives a waiter that will never end before it says so.
    const NEVER: Duration = Duration::from_secs(3);

    fn two_groups_of_two() -> Topology {
        let cpu = |id| Cpu { id, group: id / 2, socket: 0, node: 0, fast: true };
        Topology { cpus: (0..4).map(cpu).collect() }
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        crate::spin::SpinWait::within(what, LONG, done);
    }

    fn message(panic: &(dyn std::any::Any + Send)) -> String {
        panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|it| (*it).to_owned()))
            .unwrap_or_default()
    }

    /// Makes `pool`'s idle parks do as `plant` says from now on, each worker woken to meet it.
    fn plant(pool: &Pool, plant: u32) {
        pool.shared.park_plant.store(plant, Ordering::SeqCst);
        pool.shared.unpark_workers();
    }

    fn ended(pool: &Pool) -> usize {
        pool.shared.left.load(Ordering::SeqCst)
    }

    /// A thread of its own opens a scope on `pool` and gives it `job`: how the scope ended
    /// within `bound` (its panic's message when it failed), or `None` while it still waits.
    fn scoped_waiter(
        pool: &Arc<Pool>,
        job: impl FnOnce() + Send + 'static,
        bound: Duration,
    ) -> Option<Result<(), String>> {
        let (tx, rx) = bounded(1);
        let pool = Arc::clone(pool);
        std::thread::Builder::new()
            .name("scoped-waiter".to_owned())
            .spawn(move || {
                let scoped = catch_unwind(AssertUnwindSafe(|| {
                    pool.scope(|scope| scope.submit(None, job, |_| ()));
                }));
                _ = tx.send(scoped.map_err(|panic| message(&*panic)));
            })
            .unwrap();
        rx.recv_timeout(bound).ok()
    }

    /// A pool of four whose every worker met a park that fails as `planted` says and ended,
    /// and what a thread that then waits on a scope of it got within `bound`.
    fn waiter_on_a_pool_whose_parks_fail(
        planted: u32,
        bound: Duration,
    ) -> (Arc<Pool>, Option<Result<(), String>>) {
        let pool = Arc::new(Pool::new(&two_groups_of_two(), false).unwrap());
        plant(&pool, planted);
        until("every worker to end at its refused park", || ended(&pool) == pool.workers());
        let waited = scoped_waiter(&pool, || (), bound);
        (pool, waited)
    }

    /// The invariant (tested): a pool whose worker cannot park stops with the park's error,
    /// and a thread that waits on a scope of it fails with that error, naming the worker and
    /// the wait, within a bound: none waits for jobs no worker is left to run.
    #[test]
    fn a_worker_that_cannot_park_stops_the_pool_and_its_scoped_waiter_fails_with_the_wait() {
        let (pool, waited) = waiter_on_a_pool_whose_parks_fail(park_plant::FAILS, LONG);
        let broken = pool.broken().expect("the pool stopped for the park's error");
        assert!(broken.worker < 4 && broken.group < 2 && broken.workers == 4, "{broken:?}");
        let Unable::Park(error) = &broken.why else { panic!("{broken:?}") };
        assert_eq!(error.what(), super::IDLE_PARK);
        let said = match waited {
            Some(Err(said)) => said,
            Some(Ok(())) => panic!("the scope of a stopped pool returned as if its job had run"),
            None => panic!("the waiter still waited after 60 s on a pool with no worker left"),
        };
        assert_eq!(said, broken.to_string());
        for names in ["could not park", super::IDLE_PARK, "bounded-progress"] {
            assert!(said.contains(names), "the failure does not say `{names}`: {said}");
        }
        // The pool's drop ends: its workers are counted as ended.
        drop(pool);
    }

    /// The control: a worker whose park fails and that ends telling no one (the bug) leaves
    /// the waiter parked, which the bound of the test above would have caught.
    #[test]
    fn the_oracle_catches_a_worker_that_ends_silently() {
        let planted = park_plant::FAILS_AND_ENDS_SILENTLY;
        let (pool, waited) = waiter_on_a_pool_whose_parks_fail(planted, NEVER);
        assert!(pool.broken().is_none(), "the planted worker told the pool");
        assert_eq!(
            waited, None,
            "a waiter on a pool whose workers ended unheard came back: the plant plants nothing"
        );
        // The waiter's thread keeps the pool, parked for good: the bug, kept until the test
        // binary ends.
    }

    /// A scope whose job runs as the pool breaks is waited for to its end, so nothing it
    /// borrows is used after, and then fails with the pool's error.
    #[test]
    fn a_scope_whose_job_runs_as_the_pool_breaks_ends_and_then_fails() {
        let pool = Arc::new(Pool::new(&two_groups_of_two(), false).unwrap());
        let (began_tx, began_rx) = bounded::<()>(1);
        let (go_tx, go_rx) = bounded::<()>(1);
        let ran = Arc::new(AtomicBool::new(false));
        let (tx, rx) = bounded(1);
        {
            let (pool, ran) = (Arc::clone(&pool), Arc::clone(&ran));
            std::thread::Builder::new()
                .name("scoped-waiter".to_owned())
                .spawn(move || {
                    let scoped = catch_unwind(AssertUnwindSafe(|| {
                        pool.scope(|scope| {
                            scope.submit(
                                None,
                                || {
                                    began_tx.send(()).unwrap();
                                    go_rx.recv_timeout(LONG).unwrap();
                                    ran.store(true, Ordering::SeqCst);
                                },
                                |it| it.unwrap(),
                            );
                        });
                    }));
                    _ = tx.send(scoped.map_err(|panic| message(&*panic)));
                })
                .unwrap();
        }
        began_rx.recv_timeout(LONG).unwrap();
        plant(&pool, park_plant::FAILS);
        // The three idle workers end at their park; the fourth is inside the job.
        until("the idle workers to end at their refused park", || ended(&pool) == 3);
        assert!(pool.broken().is_some());
        assert!(rx.try_recv().is_err(), "the scope ended while its job ran");
        go_tx.send(()).unwrap();
        let said = rx.recv_timeout(LONG).unwrap().expect_err("a scope of a stopped pool returned");
        assert!(ran.load(Ordering::SeqCst), "the scope failed before its job had ended");
        assert_eq!(said, pool.broken().unwrap().to_string());
        until("the last worker to end", || ended(&pool) == 4);
    }

    /// The failing scenario, in a process of its own: under the filter, the driver chosen as
    /// blaze's `main` chooses it, a pool's workers park idle and are woken by jobs eight
    /// times over, and a loop of [`crate::par`] runs on the process's own pool from this
    /// thread, as the probe of a build script's `rustc` does. Which driver it chose.
    #[cfg(target_os = "linux")]
    fn park_and_wake_under_the_filter(waits: blaze_core::wait::Waits) -> String {
        use std::sync::atomic::AtomicUsize;
        let pool = Pool::new(&two_groups_of_two(), false).unwrap();
        let sum = AtomicUsize::new(0);
        for _ in 0..8 {
            // A pool that broke never goes idle: its scope, next, says why.
            until("every worker to park idle", || pool.idle() || pool.broken().is_some());
            pool.scope(|scope| {
                for n in 0..64usize {
                    let sum = &sum;
                    let add = move || sum.fetch_add(n, Ordering::SeqCst);
                    scope.submit(None, add, |it| _ = it.unwrap());
                }
            });
        }
        assert_eq!(sum.load(Ordering::SeqCst), 8 * (0..64).sum::<usize>());
        assert!(pool.broken().is_none(), "{:?}", pool.broken());
        drop(pool);
        let seen = AtomicUsize::new(0);
        let again = crate::par::for_each_giving_way(
            256,
            |_| _ = seen.fetch_add(1, Ordering::SeqCst),
            |_| false,
        );
        assert!(again.is_empty() && seen.load(Ordering::SeqCst) == 256);
        assert!(crate::exec::shared().broken().is_none());
        format!("parked and woke its workers on {waits:?}")
    }

    /// The cases this test binary runs in a process of its own under the filter that refuses
    /// io_uring (`blaze_core::wait::filtered`, the one helper for it).
    #[cfg(target_os = "linux")]
    const UNDER_FILTER: &[blaze_core::wait::filtered::Case] = &[blaze_core::wait::filtered::Case {
        name: "park and wake the pool's workers",
        run: park_and_wake_under_the_filter,
    }];

    /// This test binary run again with a case named: runs it before libtest's `main`, as the
    /// process's one thread, and ends (`filtered::take_over`).
    #[cfg(target_os = "linux")]
    extern "C" fn take_over() {
        blaze_core::wait::filtered::take_over(UNDER_FILTER);
    }

    // SAFETY: the loader calls each entry of this section as a function as the binary loads,
    // before `main`, when the process is one thread. The entry is `take_over`, an
    // `extern "C" fn()`: the loader passes it `argc`, `argv` and `envp`, which the C calling
    // convention lets it ignore. Anything but a function pointer there would be called as code.
    #[cfg(target_os = "linux")]
    #[used]
    #[unsafe(link_section = ".init_array")]
    static TAKE_OVER: extern "C" fn() = take_over;

    /// The invariant (tested): a process the sandbox launcher marked, under a filter that
    /// refuses io_uring as the sandbox's does, parks and wakes its pool's workers and runs a
    /// parallel loop to its end on the direct driver, and makes no ring.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sandboxed_process_parks_its_workers_with_no_io_uring() {
        use blaze_core::wait::filtered::{Ended, run};
        match run("park and wake the pool's workers", true, LONG) {
            Ended::Passed { said } => {
                assert!(said.contains("parked and woke its workers on Direct"), "{said}");
                assert!(!said.contains("io_uring"), "it asked for a ring: {said}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// The control (the cause, put back): the same process not marked waits on its ring,
    /// which the filter refuses, and fails naming `io_uring_setup` and the worker that could
    /// not park. It ends, where before it hung; a ring that worked under the filter would
    /// mean the test above proves nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_oracle_catches_a_sandboxed_process_on_the_ring() {
        use blaze_core::wait::filtered::{Ended, run};
        match run("park and wake the pool's workers", false, LONG) {
            Ended::Failed { code, said } => {
                assert_eq!(code, Some(3), "{said}");
                for names in ["could not park", "io_uring_setup", "__BLAZE_SANDBOXED"] {
                    assert!(said.contains(names), "the failure does not say `{names}`: {said}");
                }
            }
            other => panic!("{other:?}"),
        }
    }
}

/// [`Victims`]: a two-choices pick is the longer of two other workers' deques and replays for
/// a seed, and under either way of picking every job of a tree of scopes runs once.
#[cfg(all(test, not(blaze_check)))]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod victims {
    use std::sync::atomic::{AtomicU32, Ordering};

    use hegel::{TestCase, generators as gs};

    use super::{Pool, Stealing, Victims, pick_two, worker_seed, xorshift};
    use crate::topology::{Cpu, Topology};

    /// Workers' pool indices start here, so a position in a group's list is never mistaken
    /// for a worker's index.
    const BASE: usize = 7;

    /// Over generated groups, deque lengths and seeds: each pick draws two workers other than
    /// `me`, is the one of the longer deque (the first on a tie), is none only when both are
    /// empty, and the same seed draws the same picks again.
    #[hegel::test(test_cases = 200)]
    fn a_two_choices_pick_is_the_longer_of_two_others_and_replays_for_a_seed(tc: TestCase) {
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
        let me =
            tc.draw(gs::booleans()).then(|| tc.draw(gs::integers::<usize>().max_value(count - 1)));
        let lens: Vec<usize> =
            (0..count).map(|_| tc.draw(gs::integers::<usize>().max_value(4))).collect();
        let (seed, index) =
            (tc.draw(gs::integers::<u64>()), tc.draw(gs::integers::<usize>().max_value(255)));
        let workers: Vec<u32> = (0..count).map(|it| (BASE + it) as u32).collect();
        let len = |worker: usize| lens[worker - BASE];
        let run = || {
            let mut state = worker_seed(seed, index);
            (0..16)
                .map(|_| {
                    let mut drawn = Vec::new();
                    let random = || {
                        state = xorshift(state);
                        drawn.push(state);
                        state
                    };
                    let pick = pick_two(&workers, me, random, len);
                    (pick, drawn)
                })
                .collect::<Vec<_>>()
        };
        let picks = run();
        assert_eq!(picks, run(), "seed {seed:#x} worker {index}: the same seed drew other victims");
        let others = count - usize::from(me.is_some());
        let mine = me.map(|it| BASE + it);
        for (pick, drawn) in picks {
            if others == 0 {
                assert_eq!((pick, drawn.len()), (None, 0), "a group of only `me` draws nothing");
                continue;
            }
            let worker = |r: u32| {
                let at = r as usize % others;
                BASE + if me.is_some_and(|me| at >= me) { at + 1 } else { at }
            };
            let [a, b] = [worker(drawn[0]), worker(drawn[1])];
            assert!(Some(a) != mine && Some(b) != mine, "drew `me` {mine:?}: {a} {b}");
            let want = match (len(a), len(b)) {
                (0, 0) => None,
                (of_a, of_b) if of_b > of_a => Some(b),
                _ => Some(a),
            };
            assert_eq!(pick, want, "lens {lens:?}, drew {a} and {b}");
        }
    }

    /// With every deque as long, the picks reach every other worker of the group: two-choices
    /// keeps the walk's spread rather than pinning a thief to one victim.
    #[test]
    fn equal_deques_spread_the_picks_over_every_other_worker() {
        let workers: Vec<u32> = (0..8).map(|it| (BASE + it) as u32).collect();
        let mut state = worker_seed(Stealing::DEFAULT.seed, 3);
        let mut seen = [0u32; 8];
        for _ in 0..400 {
            let random = || {
                state = xorshift(state);
                state
            };
            let pick = pick_two(&workers, Some(3), random, |_| 1).unwrap();
            seen[pick - BASE] += 1;
        }
        assert_eq!(seen[3], 0, "picked itself: {seen:?}");
        assert!(
            seen.iter().enumerate().all(|(at, &it)| at == 3 || it > 0),
            "a worker never picked: {seen:?}"
        );
    }

    /// A worker per generated topology's CPU, one job per node of a generated tree, each
    /// node's job a scope of its children's (so jobs pile up in deques and are stolen), under
    /// either [`Victims`]: every job runs exactly once and the scope at the root ends.
    #[hegel::test(test_cases = 32)]
    fn every_job_of_a_tree_runs_once_under_either_victims(tc: TestCase) {
        fn run(pool: &Pool, children: &[Vec<usize>], ran: &[AtomicU32], at: usize) {
            ran[at].fetch_add(1, Ordering::Relaxed);
            let mut spin = 0u64;
            for it in 0..2000u64 {
                spin = std::hint::black_box(spin.wrapping_add(it));
            }
            pool.scope(|scope| {
                for &child in &children[at] {
                    scope.submit(None, move || run(pool, children, ran, child), |it| it.unwrap());
                }
            });
        }
        let groups = tc.draw(gs::integers::<u32>().min_value(1).max_value(2));
        let each = tc.draw(gs::integers::<u32>().min_value(1).max_value(3));
        let victims = if tc.draw(gs::booleans()) { Victims::TwoChoices } else { Victims::Walk };
        let seed = tc.draw(gs::integers::<u64>());
        let nodes = tc.draw(gs::integers::<usize>().min_value(1).max_value(40));
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes];
        for node in 1..nodes {
            let parent = tc.draw(gs::integers::<usize>().max_value(node - 1));
            children[parent].push(node);
        }
        let topology = Topology {
            cpus: (0..groups * each)
                .map(|id| Cpu { id, group: id / each, socket: 0, node: 0, fast: true })
                .collect(),
        };
        let pool = Pool::with_stealing(&topology, false, Stealing { victims, seed }).unwrap();
        let ran: Vec<AtomicU32> = (0..nodes).map(|_| AtomicU32::new(0)).collect();
        pool.install(|| run(&pool, &children, &ran, 0));
        let counts: Vec<u32> = ran.iter().map(|it| it.load(Ordering::Relaxed)).collect();
        assert!(
            counts.iter().all(|&it| it == 1),
            "{victims:?} on {groups}x{each}: runs per node {counts:?}"
        );
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-graph --release
/// model_`), against a bag of jobs (each runs once, after it was given, and none is left):
/// pools built by hand, without `Pool::new`'s start-up channel and binding, which the checker
/// does not see, their workers model threads in the real [`work`] loop. The deques are
/// crossbeam's, atomic to the checker; what is checked is this module's slots, counts, inject
/// lists, job cells and parking around them.
#[cfg(all(test, blaze_check))]
mod model {
    use std::{cell::Cell, ptr, sync::Arc};

    use blaze_check::{
        linear::{History, Spec},
        model,
    };
    use crossbeam_deque::Worker;

    use super::{
        Group, Idle, Inject, Job, Local, Pool, Shared, Slot, Stealing, scope_in, searching,
        sleeping, work, worker_seed,
    };
    use crate::{
        pump::Lock,
        sync::{
            OnceLock,
            atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
            thread::{self, JoinHandle},
        },
        topology::Topology,
    };

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Give(u8),
        Ran(u8),
        /// Once every job should have run.
        Drained,
    }

    /// The sequential pool: bit `j` is set while job `j` is given and has not run.
    #[derive(Clone, PartialEq, Eq, Hash, Default)]
    struct Bag(u8);

    impl Spec for Bag {
        type Op = Op;
        /// `Give`: true. `Ran`: whether the job was given and had not run. `Drained`: whether
        /// none is left.
        type Ret = bool;

        fn apply(&mut self, op: &Op) -> bool {
            match *op {
                Op::Give(job) => {
                    self.0 |= 1 << job;
                    true
                }
                Op::Ran(job) => {
                    let given = self.0 & (1 << job) != 0;
                    self.0 &= !(1 << job);
                    given
                }
                Op::Drained => self.0 == 0,
            }
        }
    }

    /// A pool of `workers` in one group, with no thread yet: its shared part and each
    /// worker's deque, by index.
    fn shared(workers: usize) -> (Arc<Shared>, Vec<Worker<Job>>) {
        let deques: Vec<Worker<Job>> = (0..workers).map(|_| Worker::new_fifo()).collect();
        let slots = deques
            .iter()
            .map(|deque| Slot {
                stealer: deque.stealer(),
                next: AtomicPtr::new(ptr::null_mut()),
                sleeping: AtomicBool::new(false),
                group: 0,
                thread: OnceLock::new(),
                parker: blaze_core::wait::Parker::new(),
                ended: AtomicU64::new(0),
                inside: AtomicBool::new(false),
            })
            .collect();
        let group = Group {
            inject: Inject::new(),
            later: Inject::new(),
            workers: (0..workers as u32).collect(),
            order: Topology::flat(workers).steal_order(0).into_boxed_slice(),
            fast: true,
            sleepers: AtomicU32::new(0),
        };
        let shared = Arc::new(Shared {
            groups: vec![group].into_boxed_slice(),
            slots,
            idle: Idle(AtomicU64::new(0)),
            stop: AtomicBool::new(false),
            mixed: false,
            stray: Lock::new("graph.pool.stray_panic", None),
            parked: AtomicUsize::new(0),
            critical: AtomicUsize::new(0),
        });
        (shared, deques)
    }

    fn local(shared: &Arc<Shared>, index: usize, deque: Worker<Job>) -> Local {
        Local {
            shared: shared.clone(),
            index,
            group: 0,
            node: 0,
            deque,
            lifo: Cell::new(0),
            seed: Cell::new(worker_seed(Stealing::DEFAULT.seed, index)),
            victims: Stealing::DEFAULT.victims,
        }
    }

    /// Worker `index` of `shared` as a model thread, started as `Pool::new` starts one.
    fn start(shared: &Arc<Shared>, index: usize, deque: Worker<Job>) -> JoinHandle<()> {
        let shared = shared.clone();
        thread::spawn(move || {
            _ = shared.slots[index].thread.set(thread::current());
            work(&local(&shared, index, deque), None);
        })
    }

    fn job(history: &Arc<History<Bag>>, id: u8) -> Job {
        let history = history.clone();
        Job::new(move |_started| {
            history.record(Op::Ran(id), |_| true);
        })
    }

    /// One worker, and a client that gives two jobs in a scope and waits for them while the
    /// worker searches, goes to sleep or sleeps: a job given as the worker says it sleeps still
    /// wakes it (a lost wake-up leaves both parked, a deadlock), each job runs once, and the
    /// dropped pool leaves no worker counted as searching or asleep.
    #[test]
    fn model_a_job_given_as_the_worker_sleeps_wakes_it() {
        model(|| {
            let (shared, mut deques) = shared(1);
            let worker = start(&shared, 0, deques.pop().expect("one deque"));
            let history = Arc::new(History::<Bag>::new());
            scope_in(&shared, |scope| {
                for id in 0..2 {
                    let ran = history.clone();
                    history.record(Op::Give(id), |_| {
                        let run = move || {
                            ran.record(Op::Ran(id), |_| true);
                        };
                        scope.submit(None, run, |it| it.unwrap());
                        true
                    });
                }
            });
            history.record(Op::Drained, |_| true);
            history.check(Bag::default());
            let pool =
                Pool { shared: shared.clone(), threads: vec![worker], topology: Topology::flat(1) };
            drop(pool);
            let word = shared.idle.0.load(Ordering::SeqCst);
            assert_eq!((searching(word), sleeping(word)), (0, 0), "a stopped worker is counted");
            assert_eq!(shared.groups[0].sleepers.load(Ordering::SeqCst), 0);
        });
    }

    /// A worker gives itself two jobs to run next (the first goes behind, to its deque) and
    /// takes its own twice, while another steals: from the deque, or from the LIFO slot when the
    /// deque is empty. Each job runs once, by whoever took it, and none is left.
    #[test]
    fn model_a_lifo_slot_and_its_deque_hand_each_job_out_once() {
        model(|| {
            let (shared, mut deques) = shared(2);
            let history = Arc::new(History::<Bag>::new());
            let thief = {
                let (shared, deque) = (shared.clone(), deques.pop().expect("two deques"));
                thread::spawn(move || {
                    let thief = local(&shared, 1, deque);
                    let mut took = thief.steal();
                    while let Some(job) = took {
                        job.run();
                        took = thief.next();
                    }
                })
            };
            let owner = local(&shared, 0, deques.pop().expect("two deques"));
            for id in 0..2 {
                history.record(Op::Give(id), |_| {
                    owner.push_next(job(&history, id));
                    true
                });
            }
            for _ in 0..2 {
                if let Some(job) = owner.next() {
                    job.run();
                }
            }
            thief.join().unwrap();
            while let Some(job) = owner.next() {
                job.run();
            }
            history.record(Op::Drained, |_| true);
            history.check(Bag::default());
            assert!(!shared.any_work(), "a job is left in a queue");
        });
    }
}
