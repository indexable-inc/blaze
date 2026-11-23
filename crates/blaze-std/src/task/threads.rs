//! The `Threads` layout (std.md, "`Threads`: the runtime of every bstd program"): a
//! thread-per-core pool that every bstd program, and the compiler's driver, runs its tasks on.
//!
//! - **Workers.** At most one OS thread per core the [`Topology`] names, started and stopped
//!   by `crate::rt` (the only module that names std's threads); this module is the pool's
//!   state and each worker's loop ([`Pool::work`]), with no thread, clock or kernel of its own
//!   but the `Parker` each worker sleeps on and the clock passed in. A worker's thread starts
//!   when a wake first reaches it ([`Pool::rouse`]), not with the pool: a wake tries the
//!   started workers first ([`Pool::wake_order`]), so a command with k tasks runnable at once
//!   starts about k threads, not one a core (bottom.md section 9, "Workers start in
//!   proportion to queued work").
//! - **Queues** (all bounded, never grown: std.md's backpressure rule). Each worker has a
//!   one-task LIFO slot that only it reads, a Chase-Lev deque of [`DEQUE`] slots
//!   (task/deque.rs) that the others steal from, a mailbox `must_overlap` hands a reserved
//!   worker its task through, and a small pinned queue for a task that must run on that worker
//!   ([`crate::task::PerWorker::fold`]'s visits). Each group of workers sharing a last-level
//!   cache has two injection queues of [`INJECT`] each, foreground and background, for tasks
//!   spawned from outside the pool and for every background task.
//! - **Only unstarted tasks move.** A task is taken whole by the worker that starts it and
//!   runs there to its end; thieves take only what is queued. A spawn that finds its queue full
//!   runs its task inline, at once (never waits for room, never drops).
//! - **Search order** of a foreground worker: its mailbox, its pinned queue, its LIFO slot (at
//!   most [`LIFO_RUN`] times in a row), its deque, its group's foreground queue; then, counted as
//!   a searcher (task/idle.rs), half of each victim's deque in its own group, then in the
//!   others, the other groups' foreground queues, and only then the background queues, its own
//!   group's first. A background worker looks at the background queues before the foreground
//!   ones. So a background task never starts on a foreground worker while foreground work is
//!   queued where that worker looks.
//! - **Idle.** A worker that finds nothing spins [`SPIN_ROUNDS`] looks, then parks on its
//!   `Parker` for at most [`PARK_SLICE`] (a heartbeat: rule 18), by the protocol of
//!   task/idle.rs that loses no wake and wakes at most one worker per spawn.
//! - **Per-worker state** is padded to [`LINE`], the target's cache line.

use alloc::{alloc::Global, boxed::Box};
use core::{
    cell::UnsafeCell,
    fmt,
    hint,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

// The pool's own atomics are std's even under `cfg(blaze_check)`: the model covers the deque
// and the idle protocol (their modules), each with its own model test; the pool is tested
// under real threads, and is not used under that cfg.
use blaze_core::{
    lock::Lock,
    wait::{Clock, Deadline, Monotonic, Parker, WaitError, Waited},
};
use blaze_own::{Arc, Vec, VecDeque};

use super::{
    TaskError,
    claim::{Claim, ClaimPlant, Start},
    deque::{Deque, DequePlant, Steal},
    idle::{Idle, IdlePlant, Mark, Woke},
    ring::{self, PoolPlant, RingEnd},
    seat::{Current, Named, Seats},
};
use crate::cx::{Canceller, Here};

/// Cache-line isolation stride: 128 bytes unconditionally across Apple Silicon, EPYC, and Graviton 5
/// so adjacent-line spatial prefetchers never false-share hot atomics or per-group queues at 128 cores.
pub(crate) const LINE: usize = 128;

/// A value alone on its 128-byte cache-line pair ([`LINE`]).
#[repr(align(128))]
pub(crate) struct Padded<T>(pub(crate) T);

const _: () = assert!(align_of::<Padded<u8>>() == LINE, "Padded's alignment is the target's line");

/// How many times in a row a worker runs the task in its LIFO slot before it looks at its
/// deque: a spawn-then-join pair ping-pongs through the slot without touching the deque, and
/// the bound keeps two such tasks from starving the deque (Tokio's value).
pub(crate) const LIFO_RUN: u32 = 3;

/// The slots of each group's injection queue, each class apart.
pub(crate) const INJECT: usize = 1024;

/// The slots of a worker's pinned queue.
pub(crate) const PINNED: usize = 64;

/// How many times an idle worker looks again, spinning between, before it parks: about a few
/// microseconds, the cost of one park and wake.
pub(crate) const SPIN_ROUNDS: u32 = 64;

/// The longest one park lasts before the worker looks again: the heartbeat that bounds a lost
/// wake to a delay (none is lost by the protocol, so this is a guard, not a mechanism).
pub(crate) const PARK_SLICE: Duration = Duration::from_secs(1);

/// The class of a scope's tasks and of a worker (std.md, "`Threads`", point 5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// Work someone waits on: the build that was asked for.
    Foreground,
    /// Work nobody waits on now: tier-up, analyses after the answer. It runs on a foreground
    /// worker only when that worker sees no foreground work.
    Background,
}

/// A group of cores that share a last-level cache: on Linux one `cache/index3`'s
/// `shared_cpu_list`, on macOS one performance level.
pub(crate) struct Group {
    pub(crate) class: Class,
    /// The cores, by the kernel's number: one worker each, pinned there where pinning is
    /// asked ([`Topology::pin`]).
    pub(crate) cpus: Vec<u32, Global>,
}

/// The machine as the pool sees it: passed in, read by `crate::rt` (rule 75).
pub(crate) struct Topology {
    pub(crate) groups: Vec<Group, Global>,
    /// Pin each worker to its core (Linux), or set its class's QoS (macOS).
    pub(crate) pin: bool,
}

impl Topology {
    /// `workers` foreground workers in one group, unpinned: a test's pool, or a machine whose
    /// caches were not named.
    pub(crate) fn flat(workers: u32) -> Topology {
        Topology::groups(&[(Class::Foreground, workers)])
    }

    /// Groups of the given class and size, unpinned, numbered from core 0.
    pub(crate) fn groups(sizes: &[(Class, u32)]) -> Topology {
        let mut groups = Vec::with_capacity_in(sizes.len(), Global);
        let mut next = 0;
        for &(class, size) in sizes {
            let mut cpus = Vec::with_capacity_in(size as usize, Global);
            for _ in 0..size {
                cpus.push(next);
                next += 1;
            }
            groups.push(Group { class, cpus });
        }
        Topology { groups, pin: false }
    }

    /// Every worker.
    pub(crate) fn workers(&self) -> u32 {
        let total: usize = self.groups.iter().map(|group| group.cpus.len()).sum();
        u32::try_from(total).unwrap_or(u32::MAX)
    }
}

/// A planted bug of the pool's, for a control (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Plants {
    pub(crate) deque: DequePlant,
    pub(crate) idle: IdlePlant,
    pub(crate) find: FindPlant,
}

impl Plants {
    pub(crate) const NONE: Plants = Plants { deque: DequePlant::None, idle: IdlePlant::None, find: FindPlant::None };
}

/// A planted bug in [`Pool::find`]'s order (threads/find_tests.rs's controls).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FindPlant {
    None,
    /// A foreground worker looks at its group's background queue before the foreground one.
    #[cfg_attr(not(test), expect(dead_code, reason = "the test's control"))]
    BackgroundFirst,
    /// A thief tries the next group's deques before its own group's.
    #[cfg_attr(not(test), expect(dead_code, reason = "the test's control"))]
    FarFirst,
}

#[cfg(all(test, not(blaze_check)))]
mod find_tests;

/// The type-erased head of every queued task (a [`Job`]): what a queue holds a pointer to.
#[repr(C)]
pub(crate) struct Header {
    /// Who may start it, and whether it ended (task/claim.rs): a worker, or for an open task
    /// its joiner running it inline; never the joiner of a reserved (`must_overlap`) one.
    claim: Claim,
    /// Runs the claimed task's body on `Here` and marks it done.
    run: unsafe fn(NonNull<Header>, Here),
    /// Drops the claimed task's body unrun and marks it done.
    discard: unsafe fn(NonNull<Header>),
    /// Gives up one reference to the job.
    release: unsafe fn(NonNull<Header>),
    pub(crate) name: &'static str,
    /// The ring of the batch whose spawn made it (task/ring.rs): it ends into it. Written at
    /// the making, before the job is published.
    ring: Option<RingEnd<Header>>,
    /// Its index in its batch, set by the batch on the scope's thread after the spawn and read
    /// there when the ring gives it back (`u32::MAX`: not set).
    tag: AtomicU32,
}

/// The batch index set on the job `header` heads ([`Queued::set_tag`]).
///
/// # Safety
///
/// `header` heads a live job (its batch still holds the handle).
pub(crate) unsafe fn tag_of(header: NonNull<Header>) -> u32 {
    // SAFETY: the caller's contract.
    unsafe { header.as_ref() }.tag.load(Ordering::Relaxed)
}

/// Whether the job `header` heads is done (`Acquire`: its result is seen after).
///
/// # Safety
///
/// `header` heads a live job (its batch still holds the handle).
pub(crate) unsafe fn done_of(header: NonNull<Header>) -> bool {
    // SAFETY: the caller's contract.
    unsafe { header.as_ref() }.done()
}

impl Header {
    /// Whether it ended: `Acquire`, ordered after the body's writes of its result.
    fn done(&self) -> bool {
        self.claim.done()
    }
}

/// A queued task's pointer, as a queue holds it: one reference to its job.
pub(crate) struct JobPtr(NonNull<Header>);

// SAFETY: a job is shared by its queue and its handle through `Arc` references, and every
// field a second thread touches is atomic or written before the `Release` that publishes it;
// the body (`F: Send`) and the result (`T: Send`) move to the thread that claims it.
unsafe impl Send for JobPtr {}

impl JobPtr {
    /// Gives up the queue's reference to a job that was never queued (a refused reservation):
    /// its handle drops the task unrun.
    pub(crate) fn release_unrun(self) {
        // SAFETY: the queue's reference, never put in a queue, given up once.
        let head = unsafe { self.0.as_ref() };
        // SAFETY: as above.
        unsafe { (head.release)(self.0) };
    }
}

/// What a scope shares with the threads that run its tasks.
pub(crate) struct Tally {
    /// Its tasks queued or running away from the scope's thread, not yet done.
    running: Padded<AtomicUsize>,
    /// The failure of the lowest spawn index: the one `Inline` would have kept, since it runs
    /// tasks in spawn order (so a scope fails the same under every layout).
    failed: Lock<Option<(u32, TaskError)>>,
    cancel: Canceller,
    /// The scope's thread parks here while it waits for a task; only that thread parks on it.
    owner: Parker,
    /// What the scope's joins did, and how many of its tasks ended (task/help.rs).
    joins: super::help::Joins,
}

impl Tally {
    pub(crate) fn new(cancel: Canceller) -> Arc<Tally, Global> {
        Arc::new_in(
            Tally {
                running: Padded(AtomicUsize::new(0)),
                failed: Lock::new("bstd.task.failed", None),
                cancel,
                owner: Parker::new(),
                joins: super::help::Joins::new(),
            },
            Global,
        )
    }

    /// What the scope's joins did so far, and its count of ended tasks (task/help.rs).
    pub(crate) fn joins(&self) -> &super::help::Joins {
        &self.joins
    }

    /// Task `index` failed: the scope is cancelled, and the failure kept if it is the lowest.
    pub(crate) fn fail(&self, index: u32, error: TaskError) {
        self.cancel.cancel();
        let mut failed = self.failed.lock();
        let keep = match &*failed {
            Some((kept, _)) => index < *kept,
            None => true,
        };
        if keep {
            *failed = Some((index, error));
        }
    }

    /// Cancels the scope: a join past its deadline does, before its last wait.
    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
    }

    pub(crate) fn take_failed(&self) -> Option<(u32, TaskError)> {
        self.failed.lock().take()
    }

    /// Its tasks not yet done away from its thread.
    pub(crate) fn running(&self) -> usize {
        self.running.0.load(Ordering::Acquire)
    }

    fn started(&self) {
        self.running.0.fetch_add(1, Ordering::AcqRel);
    }

    /// A task ended: counted, and the scope's thread woken (its unpark is a token, so a wake
    /// before its park is kept).
    fn ended(&self) {
        self.running.0.fetch_sub(1, Ordering::AcqRel);
        // After the task's `DONE` (its runner's store before this call) and before the wake: a
        // batch join that reads this count and then sweeps misses no task (task/help.rs).
        self.joins.ended.fetch_add(1, Ordering::AcqRel);
        self.owner.unpark();
    }
}

/// How a task's body ended.
pub(crate) enum Ran<T> {
    Made(T),
    /// It failed (an error, a panic): already recorded in its tally.
    Failed(TaskError),
    /// The process exits: passed on at the join, on the scope's thread.
    Exit(alloc::boxed::Box<dyn core::any::Any + Send>),
}

/// The part of a job that does not depend on its body's type: a pointer to a `Job<F, T>` is a
/// pointer to this (both `repr(C)`, this the prefix).
#[repr(C)]
pub(crate) struct JobHead<T> {
    header: Header,
    tally: Arc<Tally, Global>,
    out: UnsafeCell<Option<Ran<T>>>,
}

#[repr(C)]
struct Job<F, T> {
    head: JobHead<T>,
    body: UnsafeCell<Option<F>>,
}

/// The scope's handle on one queued task: one reference to its job.
pub(crate) struct Queued<T> {
    head: NonNull<JobHead<T>>,
}

impl<T> Queued<T> {
    fn header(&self) -> &Header {
        // SAFETY: the handle holds a reference, so the job is alive.
        unsafe { &self.head.as_ref().header }
    }

    /// Takes the task to run it on this thread, if no worker started it and it is not reserved
    /// for one (task/claim.rs): `Some` with what it made.
    pub(crate) fn run_here(&self, here: Here) -> Option<Ran<T>> {
        let header = self.header();
        if !header.claim.by_joiner() {
            return None;
        }
        // SAFETY: claimed by this thread, so the body is this thread's to run, once.
        unsafe { (header.run)(NonNull::from(header), here) };
        Some(self.take())
    }

    /// Whether it ended.
    pub(crate) fn done(&self) -> bool {
        self.header().done()
    }

    pub(crate) fn name(&self) -> &'static str {
        self.header().name
    }

    /// Its index in its batch: set by the batch on the scope's thread, read there
    /// ([`tag_of`]), so `Relaxed`.
    pub(crate) fn set_tag(&self, tag: u32) {
        self.header().tag.store(tag, Ordering::Relaxed);
    }

    /// The scope's shared state.
    pub(crate) fn tally(&self) -> &Tally {
        // SAFETY: the handle holds a reference, so the job is alive.
        unsafe { &self.head.as_ref().tally }
    }

    /// Drops the task unrun if nobody started it and it is not reserved for a worker: whether
    /// it did. A reserved one is left to its worker, which runs it.
    pub(crate) fn drop_unstarted(&self) -> bool {
        let header = self.header();
        if !header.claim.by_joiner() {
            return false;
        }
        // SAFETY: claimed by this thread just now.
        unsafe { (header.discard)(NonNull::from(header)) };
        true
    }

    /// Drops unrun a reserved task whose reservation was refused, so no worker ever saw it.
    pub(crate) fn withdraw(&self) {
        let header = self.header();
        header.claim.withdraw();
        // SAFETY: claimed by this thread just now.
        unsafe { (header.discard)(NonNull::from(header)) };
    }

    /// Drops what it made, if it is still held: once [`Queued::done`].
    pub(crate) fn drop_out(&self) {
        assert!(self.done(), "bstd::task::threads: a task's result dropped before it ended");
        // SAFETY: as `take`'s.
        drop(unsafe { (*self.head.as_ref().out.get()).take() });
    }

    /// What it made, once [`Queued::done`].
    pub(crate) fn take(&self) -> Ran<T> {
        assert!(self.done(), "bstd::task::threads: a task's result taken before it ended");
        // SAFETY: done, so the runner wrote the result before its `Release` store of `DONE`,
        // which `done`'s `Acquire` saw; nothing writes it after, and only this handle reads it.
        let out = unsafe { (*self.head.as_ref().out.get()).take() };
        match out {
            Some(ran) => ran,
            None => unreachable!("bstd::task::threads: a task's result was taken twice"),
        }
    }
}

impl<T> Drop for Queued<T> {
    fn drop(&mut self) {
        let header = self.header();
        // SAFETY: this handle's own reference, given up once.
        unsafe { (header.release)(NonNull::from(header)) };
    }
}

/// A job for `body`, its handle and the queue's pointer: two references.
///
/// # Safety
///
/// `body` may borrow what lives only as long as its scope: the caller makes sure the job has
/// run or been dropped unrun before that ends (a scope never returns while a task of it is
/// queued or running: its join waits, and a task past its deadline stops the process).
pub(crate) unsafe fn job<F, T>(name: &'static str, tally: &Arc<Tally, Global>, start: Start, body: F) -> (Queued<T>, JobPtr)
where
    F: FnOnce(Here) -> Ran<T> + Send,
    T: Send,
{
    let job = Job {
        head: JobHead::<T> {
            header: Header {
                claim: Claim::new(start, ClaimPlant::None),
                run: run::<F, T>,
                discard: discard::<F, T>,
                release: release::<F, T>,
                name,
                ring: ring::take_install(),
                tag: AtomicU32::new(u32::MAX),
            },
            tally: Arc::clone(tally),
            out: UnsafeCell::new(None),
        },
        body: UnsafeCell::new(Some(body)),
    };
    let job = Arc::new_in(job, Global);
    let queue = Arc::clone(&job);
    let (handle, _) = Arc::into_raw_with_allocator(job);
    let (queue, _) = Arc::into_raw_with_allocator(queue);
    tally.started();
    // Both pointers are non-null: `Arc::into_raw` of a live `Arc`.
    let head = NonNull::new(handle.cast_mut().cast::<JobHead<T>>()).expect("an Arc's pointer is not null");
    let queue = NonNull::new(queue.cast_mut().cast::<Header>()).expect("an Arc's pointer is not null");
    (Queued { head }, JobPtr(queue))
}

/// Runs a claimed job's body and marks it done.
///
/// # Safety
///
/// `header` is the head of a live `Job<F, T>` this thread claimed.
unsafe fn run<F, T>(header: NonNull<Header>, here: Here)
where
    F: FnOnce(Here) -> Ran<T>,
{
    // SAFETY: the caller's contract; `repr(C)` puts the header first.
    let job = unsafe { header.cast::<Job<F, T>>().as_ref() };
    // SAFETY: claimed, so this thread alone touches the body, once.
    let body = unsafe { (*job.body.get()).take() };
    let Some(body) = body else {
        unreachable!("bstd::task::threads {}: a claimed task had no body", job.head.header.name)
    };
    let ran = body(here);
    // SAFETY: claimed and not yet done: this thread alone touches the result until the store
    // of `DONE` below publishes it.
    unsafe { *job.head.out.get() = Some(ran) };
    // SAFETY: a ring is its batch's, which holds this job's handle (and so finishes this task)
    // before the ring drops.
    unsafe { ring::end_task(job.head.header.ring, header, PoolPlant::None, || job.head.header.claim.finish(), || job.head.tally.ended()) };
}

/// Drops a claimed job's body unrun and marks it done, with no result.
///
/// # Safety
///
/// `header` is the head of a live `Job<F, T>` this thread claimed.
unsafe fn discard<F, T>(header: NonNull<Header>) {
    // SAFETY: the caller's contract; `repr(C)` puts the header first.
    let job = unsafe { header.cast::<Job<F, T>>().as_ref() };
    // SAFETY: claimed, so this thread alone touches the body.
    drop(unsafe { (*job.body.get()).take() });
    // SAFETY: as in `run`.
    unsafe { ring::end_task(job.head.header.ring, header, PoolPlant::None, || job.head.header.claim.finish(), || job.head.tally.ended()) };
}

/// Gives up one reference to a job.
///
/// # Safety
///
/// `header` is the head of a live `Job<F, T>`, and the caller owns one of its references.
unsafe fn release<F, T>(header: NonNull<Header>) {
    // SAFETY: the caller's contract: the pointer came from `Arc::into_raw_with_allocator` of an
    // `Arc<Job<F, T>, Global>`.
    drop(unsafe { Arc::from_raw_in(header.cast::<Job<F, T>>().as_ptr().cast_const(), Global) });
}

/// One worker's state: its queues and its place in the idle protocol.
struct Worker {
    /// Its LIFO slot: read and written by its own thread only (`Relaxed`, no read-modify-write
    /// but the swap), so it is not stolen.
    lifo: AtomicPtr<Header>,
    /// How many times in a row it ran its LIFO slot: its own.
    lifo_run: AtomicU32,
    deque: Deque<Header>,
    /// `must_overlap`'s task for it: written by the reserver before its reservation, read by
    /// the worker after it sees `Reserved`.
    mailbox: AtomicPtr<Header>,
    pinned: Lock<VecDeque<JobPtr, Global>>,
    /// The pinned queue's length, moved under its lock after each push and pop: its own worker
    /// looks here before it takes the lock, so an empty queue costs no lock.
    pinned_count: AtomicU32,
    mark: Mark,
    parker: Parker,
    group: u32,
    class: Class,
    /// Its core, by the kernel's number: where its thread pins itself when the topology asks.
    cpu: u32,
    /// The task it runs now, for a stop's report: written by this worker alone at each task's
    /// start and end, with no lock (task/seat.rs `Current`).
    current: Current,
    /// Its thread was started: set once, by the compare-and-swap of [`Pool::rouse`].
    thread: AtomicBool,
    /// Its thread left its loop.
    exited: AtomicBool,
    /// Why its loop stopped early (a park the kernel refused): reported at the pool's stop.
    broken: Lock<Option<WaitError>>,
    /// Steals from its own group and from the others: its own counters, read by a test.
    stole_near: AtomicU64,
    stole_far: AtomicU64,
}

/// One group's shared state, padded to 128 bytes so adjacent CCX/L3 groups never false-share.
#[repr(align(128))]
struct GroupState {
    first: u32,
    len: u32,
    class: Class,
    /// Lock-free mirrored queue lengths (updated under each queue's lock) so `take` fast-paths
    /// empty queues and `Pool::visible` checks all groups with zero mutex acquisitions at 128 cores.
    fg_len: Padded<AtomicUsize>,
    bg_len: Padded<AtomicUsize>,
    foreground: Lock<VecDeque<JobPtr, Global>>,
    background: Lock<VecDeque<JobPtr, Global>>,
}

/// The pool's shared state, made from the machine's topology when the pool is first asked
/// anything about its workers ([`Pool::shared`]).
struct Shared {
    workers: Vec<Padded<Worker>, Global>,
    groups: Vec<GroupState, Global>,
    idle: Idle,
    stop: Padded<AtomicBool>,
    /// The group an outside spawn goes to next (round robin).
    next_group: Padded<AtomicU32>,
    /// Whether each thread pins itself to its core (Linux) or sets its class's QoS (macOS).
    pin: bool,
    /// The seats of joiners outside the pool that steal (task/seat.rs).
    seats: Padded<Seats>,
}

/// A joiner outside the pool's seat ([`Pool::adopt`]): while held, it steals from the workers'
/// deques ([`Pool::steal_seated`]). Given back when dropped.
pub(crate) struct Seat<'p> {
    seats: &'p Seats,
    at: u32,
}

impl Drop for Seat<'_> {
    fn drop(&mut self) {
        self.seats.give(self.at);
    }
}

/// What the pool is before it is asked about its workers, and what is the same after: no
/// topology read, no table made, no thread started, until a program asks for one.
struct Lazy {
    /// The workers' tables once made: null until then, set once by a compare-and-swap (a
    /// second maker's are freed), and freed with this.
    made: AtomicPtr<Shared>,
    /// What reads the machine: the driver's (rt.rs), called by the first that asks; `None`
    /// for a pool made from a topology given ([`Pool::new`]), whose tables are made at once.
    read: Option<Reader>,
    plants: Plants,
    clock: Monotonic,
    /// What starts a worker's thread: the driver's (rt.rs), called once a worker, by the first
    /// wake that reaches it ([`Pool::rouse`]).
    starter: Starter,
    /// How many workers' threads were started: what a stop waits to see leave.
    started: AtomicU32,
    /// How many workers have left their loops: the last started one wakes [`Lazy::stopper`].
    left: AtomicU32,
    /// Where a stop waits for the last worker to leave.
    stopper: Parker,
    /// How many workers' threads have reached their loops: a test waits for all, since an
    /// unstarted worker is counted parked too (task/idle.rs).
    #[cfg(test)]
    arrived: AtomicU32,
}

impl Drop for Lazy {
    fn drop(&mut self) {
        let made = *self.made.get_mut();
        if !made.is_null() {
            // SAFETY: set once from `Box::into_raw_with_allocator` of a `Box<Shared, Global>`
            // ([`Pool::shared`]), and this is its last owner.
            drop(unsafe { Box::from_raw_in(made, Global) });
        }
    }
}

/// Starts worker `at`'s thread, from the first wake that reaches it (the waker's thread): the
/// driver's, since threads are Rust std's (rt.rs). The thread runs [`Pool::work`] for `at`.
pub(crate) type Starter = fn(&Pool, u32);

/// Reads the machine's topology for a pool, when it is first asked about its workers: the
/// driver's (rt.rs). It returns the topology or stops the process naming why.
pub(crate) type Reader = fn() -> Topology;

/// A handle on the pool: cloned into every context that spawns on it.
#[derive(Clone)] // clone: an `Arc` handle
pub(crate) struct Pool(Arc<Lazy, Global>);

const _: () = {
    const fn crosses<T: Send + Sync>() {}
    crosses::<Pool>();
};

/// Workers that did not stop by the pool's deadline (rule 18): each named with the task it
/// was running, and workers whose park the kernel refused.
#[derive(Debug)]
pub struct Hung {
    pub stuck: Vec<Stuck, Global>,
}

/// A worker left running at the pool's stop, or broken before it.
#[derive(Debug)]
pub struct Stuck {
    pub worker: u32,
    /// The task it was running (empty: between tasks).
    pub task: &'static str,
    /// Why its loop stopped, if the kernel refused its park.
    pub broken: Option<WaitError>,
}

impl fmt::Display for Hung {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::task::threads: the pool stopped with workers that did not (rule bounded-progress):")?;
        for stuck in &self.stuck {
            match &stuck.broken {
                Some(why) => write!(f, " worker {} broke: {why};", stuck.worker)?,
                None if stuck.task.is_empty() => write!(f, " worker {} between tasks;", stuck.worker)?,
                None => write!(f, " worker {} still running task {};", stuck.worker, stuck.task)?,
            }
        }
        Ok(())
    }
}

impl core::error::Error for Hung {}

/// Where a spawn could not put its task: the caller runs it inline (rule: a full queue never
/// grows and never drops).
pub(crate) struct Full(pub(crate) JobPtr);

impl Pool {
    /// The pool of `topology`, its tables made now and no thread of it started: `starter`
    /// starts each worker's at the first wake that reaches it, and until then the worker is
    /// parked (task/idle.rs, "A worker whose thread has not started is parked"). A test's pool.
    pub(crate) fn new(topology: &Topology, clock: Monotonic, plants: Plants, starter: Starter) -> Pool {
        let made = Box::into_raw_with_allocator(Box::new_in(Pool::make(topology, plants), Global)).0;
        Pool::with(AtomicPtr::new(made), None, clock, plants, starter)
    }

    /// The pool of the machine `read` reads, nothing of it made: the topology is read and the
    /// tables made when it is first asked about its workers (a scope's layout, a spawn), and
    /// its threads start at the first spawn. A program that does neither reads no topology,
    /// makes no table and starts no thread, and one that spawns k tasks at once starts about k.
    pub(crate) fn lazy(read: Reader, clock: Monotonic, plants: Plants, starter: Starter) -> Pool {
        Pool::with(AtomicPtr::new(ptr::null_mut()), Some(read), clock, plants, starter)
    }

    fn with(made: AtomicPtr<Shared>, read: Option<Reader>, clock: Monotonic, plants: Plants, starter: Starter) -> Pool {
        let lazy = Lazy {
            made,
            read,
            plants,
            clock,
            starter,
            started: AtomicU32::new(0),
            left: AtomicU32::new(0),
            stopper: Parker::new(),
            #[cfg(test)]
            arrived: AtomicU32::new(0),
        };
        Pool(Arc::new_in(lazy, Global))
    }

    /// The workers' tables, made now if they were not: the first caller reads the topology and
    /// makes them; two at once each make theirs and the compare-and-swap keeps one (the other
    /// is freed), so every caller sees the same tables. `Acquire` pairs with the `AcqRel`
    /// that published them.
    #[inline]
    fn shared(&self) -> &Shared {
        let made = self.0.made.load(Ordering::Acquire);
        if !made.is_null() {
            // SAFETY: set once to a live `Box<Shared, Global>` that lives as long as `self.0`.
            return unsafe { &*made };
        }
        self.make_now()
    }

    #[cold]
    fn make_now(&self) -> &Shared {
        let read = self.0.read.expect("a pool without a reader was made with its tables");
        let mine = Box::into_raw_with_allocator(Box::new_in(Pool::make(&read(), self.0.plants), Global)).0;
        let made = match self.0.made.compare_exchange(ptr::null_mut(), mine, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => mine,
            Err(theirs) => {
                // SAFETY: `mine` is the box made above, never published.
                drop(unsafe { Box::from_raw_in(mine, Global) });
                theirs
            }
        };
        // SAFETY: as in `shared`.
        unsafe { &*made }
    }

    /// Whether the tables were made: a stop of a pool never asked about its workers has none
    /// to stop.
    fn made(&self) -> Option<&Shared> {
        let made = self.0.made.load(Ordering::Acquire);
        // SAFETY: as in `shared`.
        (!made.is_null()).then(|| unsafe { &*made })
    }

    fn make(topology: &Topology, plants: Plants) -> Shared {
        let count = topology.workers();
        let mut workers = Vec::with_capacity_in(count as usize, Global);
        let mut groups = Vec::with_capacity_in(topology.groups.len(), Global);
        let mut first = 0;
        for (at, group) in topology.groups.iter().enumerate() {
            let len = u32::try_from(group.cpus.len()).unwrap_or(u32::MAX);
            let at = u32::try_from(at).unwrap_or(u32::MAX);
            for &cpu in group.cpus.iter() {
                workers.push(Padded(Worker {
                    cpu,
                    lifo: AtomicPtr::new(ptr::null_mut()),
                    lifo_run: AtomicU32::new(0),
                    deque: Deque::new(plants.deque),
                    mailbox: AtomicPtr::new(ptr::null_mut()),
                    pinned: Lock::new("bstd.task.pinned", VecDeque::with_capacity_in(PINNED, Global)),
                    pinned_count: AtomicU32::new(0),
                    mark: Mark::unstarted(),
                    parker: Parker::new(),
                    group: at,
                    class: group.class,
                    current: Current::new(),
                    thread: AtomicBool::new(false),
                    exited: AtomicBool::new(false),
                    broken: Lock::new("bstd.task.broken", None),
                    stole_near: AtomicU64::new(0),
                    stole_far: AtomicU64::new(0),
                }));
            }
            groups.push(GroupState {
                first,
                len,
                class: group.class,
                fg_len: Padded(AtomicUsize::new(0)),
                bg_len: Padded(AtomicUsize::new(0)),
                foreground: Lock::new("bstd.task.inject", VecDeque::with_capacity_in(INJECT, Global)),
                background: Lock::new("bstd.task.inject.background", VecDeque::with_capacity_in(INJECT, Global)),
            });
            first += len;
        }
        Shared {
            workers,
            groups,
            idle: Idle::unstarted(count, plants.idle),
            stop: Padded(AtomicBool::new(false)),
            next_group: Padded(AtomicU32::new(0)),
            pin: topology.pin,
            seats: Padded(Seats::new()),
        }
    }

    /// Unparks worker `at`, whose mark a wake has just moved out of `Parked` (to `Searching` or
    /// `Reserved`), and starts its thread if it has none: the first caller for that worker, of
    /// any thread, calls the starter, once (the compare-and-swap); every other only unparks.
    /// The unpark comes first and is a token, so a thread started after it, or still starting,
    /// finds its wake: its first act reads the mark as after a park (task/idle.rs).
    fn rouse(&self, at: usize) {
        let worker = &self.shared().workers[at].0;
        worker.parker.unpark();
        if worker.thread.load(Ordering::Acquire) {
            return;
        }
        if worker.thread.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.0.started.fetch_add(1, Ordering::AcqRel);
            (self.0.starter)(self, u32::try_from(at).unwrap_or(u32::MAX));
        }
    }

    /// Starts every worker's thread that has none, now: a test's pool, which then waits for
    /// all of them to park. Each is still parked until a wake reaches it.
    pub(crate) fn start_all(&self) {
        for at in 0..self.shared().workers.len() {
            let worker = &self.shared().workers[at].0;
            if worker.thread.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                self.0.started.fetch_add(1, Ordering::AcqRel);
                (self.0.starter)(self, u32::try_from(at).unwrap_or(u32::MAX));
            }
        }
    }

    /// Where worker `at`'s thread places itself: its core and class, and `None` when the
    /// topology asks for no placement.
    pub(crate) fn placement(&self, at: u32) -> Option<(u32, Class)> {
        let worker = &self.shared().workers[at as usize].0;
        self.shared().pin.then_some((worker.cpu, worker.class))
    }

    /// How many workers' threads were started: for a test.
    #[cfg(test)]
    pub(crate) fn started(&self) -> u32 {
        self.0.started.load(Ordering::Acquire)
    }

    /// Waits until every started worker has left its loop, or `until` on the pool's clock:
    /// whether all have. The last to leave unparks this wait; a pool that started no thread has
    /// none to wait for. One thread waits at a time (the stop's).
    pub(crate) fn wait_left(&self, until: Deadline) -> Result<bool, WaitError> {
        let lazy = &*self.0;
        while lazy.left.load(Ordering::Acquire) != lazy.started.load(Ordering::Acquire) {
            if until.passed(lazy.clock.now()) {
                return Ok(false);
            }
            lazy.stopper.park("bstd.rt.pool.stop", Some(until), &lazy.clock)?;
        }
        Ok(true)
    }

    /// How many workers it has.
    pub(crate) fn workers(&self) -> u32 {
        u32::try_from(self.shared().workers.len()).unwrap_or(u32::MAX)
    }

    /// The clock its parks are timed on.
    pub(crate) fn clock(&self) -> &Monotonic {
        &self.0.clock
    }

    /// How many L3-cache / core-class worker groups this pool has (at least 1).
    pub(crate) fn groups(&self) -> u32 {
        u32::try_from(self.shared().groups.len()).unwrap_or(1).max(1)
    }

    /// The class of worker `at`.
    pub(crate) fn class_of(&self, at: u32) -> Class {
        self.shared().workers[at as usize].0.class
    }

    /// Spawns `job` onto a specific L3-cache `group` (modulo `self.groups()`), waking a worker
    /// inside that exact group first so all sub-tasks spawned by `job` stay L3-cache-warm on the
    /// same CCX / cluster.
    pub(crate) fn spawn_in_group(&self, group: u32, class: Class, job: JobPtr) -> Result<(), Full> {
        let count = self.groups();
        let target = group % count;
        let target = match class {
            Class::Foreground => target,
            Class::Background => self.background_group(target),
        };
        self.inject(target, class, job)?;
        self.notify(target);
        Ok(())
    }

    /// A task spawned from `here` in `class`: onto the spawner's own LIFO slot and deque when it
    /// is a worker and the task is foreground, else onto an injection queue; then at most one
    /// worker woken. `Err` when the queue is full: the caller runs it inline.
    pub(crate) fn spawn(&self, here: Here, class: Class, job: JobPtr) -> Result<(), Full> {
        let shared = self.shared();
        let near = match (here, class) {
            (Here::Worker(at), Class::Foreground) => {
                let worker = &shared.workers[at as usize].0;
                // The LIFO slot is the worker's own: the newest task runs next, warm in cache.
                let old = worker.lifo.swap(job.0.as_ptr(), Ordering::Relaxed);
                let Some(old) = NonNull::new(old) else {
                    // Nothing stealable was added: nobody to wake.
                    return Ok(());
                };
                if let Err(old) = worker.deque.push(old) {
                    // Full: the older task runs inline now, the new one stays next.
                    return Err(Full(JobPtr(old)));
                }
                worker.group
            }
            (Here::Worker(at), Class::Background) => {
                let group = self.background_group(shared.workers[at as usize].0.group);
                self.inject(group, class, job)?;
                group
            }
            (Here::Outside, _) => {
                let next = shared.next_group.0.fetch_add(1, Ordering::Relaxed);
                let group = next % u32::try_from(shared.groups.len()).unwrap_or(1).max(1);
                let group = match class {
                    Class::Foreground => group,
                    Class::Background => self.background_group(group),
                };
                self.inject(group, class, job)?;
                group
            }
        };
        self.notify(near);
        Ok(())
    }

    /// The group a background task from `group` goes to: `group` if it is a background group,
    /// else the first that is, else `group` (a machine with one kind of core).
    fn background_group(&self, group: u32) -> u32 {
        let groups = &self.shared().groups;
        if groups[group as usize].class == Class::Background {
            return group;
        }
        let first = groups.iter().position(|it| it.class == Class::Background);
        first.and_then(|at| u32::try_from(at).ok()).unwrap_or(group)
    }

    fn inject(&self, group: u32, class: Class, job: JobPtr) -> Result<(), Full> {
        let group = &self.shared().groups[group as usize];
        let (queue, count) = match class {
            Class::Foreground => (&group.foreground, &group.fg_len.0),
            Class::Background => (&group.background, &group.bg_len.0),
        };
        let mut guard = queue.lock();
        let res = bounded_push(&mut guard, INJECT, job);
        if res.is_ok() {
            count.store(guard.len(), Ordering::Release);
        }
        res
    }

    /// Runs a task a full queue gave back, on the spawner's thread `here`, now.
    pub(crate) fn run_now(&self, here: Here, job: JobPtr) {
        match here {
            Here::Worker(at) => self.run_job(at, job),
            Here::Outside => {
                // SAFETY: the queue's reference, which the full queue handed back.
                let head = unsafe { job.0.as_ref() };
                // A full queue hands back only open tasks: a reserved one goes to a mailbox.
                if head.claim.by_joiner() {
                    // SAFETY: claimed by this thread just now.
                    unsafe { (head.run)(job.0, Here::Outside) };
                }
                // SAFETY: that reference, given up once.
                unsafe { (head.release)(job.0) };
            }
        }
    }

    /// After a task is published: wakes at most one parked worker, its group `near` first,
    /// when no worker is searching (task/idle.rs), and starts its thread if it has none.
    fn notify(&self, near: u32) {
        let shared = self.shared();
        let order = self.wake_order(near).map(|at| (at as usize, &shared.workers[at as usize].0.mark));
        if let Some(at) = shared.idle.wake_one(order) {
            self.rouse(at);
        }
    }

    /// The workers a wake from group `near` tries, in order: those whose threads are started,
    /// the groups from `near` on, then those not started, in the same order. So a wake starts a
    /// thread only when no started worker is parked, and the threads started stay about as
    /// many as the tasks runnable at once, however many groups the machine has (a machine
    /// whose sysfs names only per-core caches is a group a core).
    fn wake_order(&self, near: u32) -> impl Iterator<Item = u32> + '_ {
        let shared = self.shared();
        let groups = shared.groups.len();
        let all = move || {
            (0..groups).flat_map(move |step| {
                let group = &shared.groups[(near as usize + step) % groups];
                group.first..group.first + group.len
            })
        };
        let started = move |at: &u32| shared.workers[*at as usize].0.thread.load(Ordering::Acquire);
        all().filter(started).chain(all().filter(move |at| !started(at)))
    }

    /// `must_overlap`: hands `job` to a worker that is idle now (searching or parked), not
    /// `here`, in [`Pool::wake_order`] (started ones, its own group first), which starts it
    /// next. `Err` when none is.
    pub(crate) fn reserve(&self, here: Here, job: JobPtr) -> Result<u32, JobPtr> {
        let shared = self.shared();
        let (near, me) = match here {
            Here::Worker(at) => (shared.workers[at as usize].0.group, Some(at)),
            Here::Outside => (0, None),
        };
        for at in self.wake_order(near) {
            let worker = &shared.workers[at as usize].0;
            if Some(at) == me || !shared.idle.idle(&worker.mark) {
                continue;
            }
            // The mailbox first, so no two reservers fill one; the reservation's
            // compare-and-swap then publishes it.
            if worker.mailbox.compare_exchange(ptr::null_mut(), job.0.as_ptr(), Ordering::Relaxed, Ordering::Relaxed).is_err() {
                continue;
            }
            if shared.idle.reserve(&worker.mark) {
                self.rouse(at as usize);
                return Ok(at);
            }
            worker.mailbox.store(ptr::null_mut(), Ordering::Relaxed);
        }
        Err(job)
    }

    /// A task that must run on worker `at` (a fold's visit): onto its pinned queue, and it
    /// woken if parked. `Err` when the queue is full.
    pub(crate) fn spawn_on(&self, at: u32, job: JobPtr) -> Result<(), Full> {
        let shared = self.shared();
        let worker = &shared.workers[at as usize].0;
        {
            let mut pinned = worker.pinned.lock();
            bounded_push(&mut pinned, PINNED, job)?;
            // Under the lock, after the push; `Release`, before the wake below: a worker that
            // reads the count after its wake (or in its idle re-check) sees it.
            worker.pinned_count.fetch_add(1, Ordering::Release);
        }
        if shared.idle.wake_this(&worker.mark) {
            self.rouse(at as usize);
        }
        Ok(())
    }

    /// Waits on the scope's thread, `here`, until `ready`, or `passed` says its deadline is
    /// gone (`false`). A worker runs its own queued tasks meanwhile (they are its descendants
    /// or its siblings, each correct to run inline); any thread then spins and parks on the
    /// scope's `Parker`, which every task of it unparks when it ends.
    pub(crate) fn wait(
        &self,
        here: Here,
        tally: &Tally,
        what: &'static str,
        mut ready: impl FnMut() -> bool,
        mut passed: impl FnMut() -> bool,
    ) -> Result<bool, WaitError> {
        let mut spins = 0;
        loop {
            if ready() {
                return Ok(true);
            }
            if passed() {
                return Ok(false);
            }
            if let Here::Worker(at) = here
                && let Some(job) = self.own(at)
            {
                self.run_job(at, job);
                continue;
            }
            if spins < SPIN_ROUNDS {
                spins += 1;
                hint::spin_loop();
                continue;
            }
            let clock = &self.0.clock;
            match tally.owner.park(what, Some(Deadline::after(clock.now(), PARK_SLICE)), clock)? {
                Waited::Ready(()) | Waited::Deadline => {}
            }
        }
    }

    /// A queued task for a joining thread that is not a worker (task/help.rs): one off an
    /// injection queue of `class`, the groups in order. Those queues hold what threads outside
    /// the pool spawned, which such a thread may run (`run_now` does). Never a worker's deque,
    /// LIFO slot, pinned queue or mailbox: a task spawned on a worker stays with workers.
    /// `None` when the pool has not started or those queues are empty.
    pub(crate) fn lend(&self, class: Class) -> Option<JobPtr> {
        let shared = self.made()?;
        shared.groups.iter().find_map(|group| take(group, class))
    }

    /// Runs a task [`Pool::lend`] gave, on the joining thread `here`, if nobody started it
    /// since (the joiner's claim: task/claim.rs), then gives up the queue's reference: whether
    /// it ran here. Its body catches its own error or panic and records it in its own scope, so
    /// a failure is that task's, never this thread's.
    pub(crate) fn run_lent(&self, here: Here, job: JobPtr) -> bool {
        // SAFETY: the queue's reference, which `lend` took out of its queue, keeps the job
        // alive until the release below.
        let head = unsafe { job.0.as_ref() };
        let took = super::help::take(&head.claim, super::help::HelpPlant::None);
        if took {
            // SAFETY: claimed by this thread just now, so the body is this thread's to run, once.
            unsafe { (head.run)(job.0, here) };
        }
        // SAFETY: that reference, given up once.
        unsafe { (head.release)(job.0) };
        took
    }

    /// A seat for a joiner outside the pool, if one is free and the pool's tables are made
    /// (task/seat.rs): held for the join's length, given back before it parks.
    pub(crate) fn adopt(&self) -> Option<Seat<'_>> {
        let shared = self.made()?;
        let at = shared.seats.0.take()?;
        Some(Seat { seats: &shared.seats.0, at })
    }

    /// One queued task off a deque of a worker of `class`, for a seated joiner: as a thief
    /// takes it (one, not half: the joiner has no deque of its own). Run it with
    /// [`Pool::run_lent`]: the joiner's claim decides whether it is still unstarted. `None` when
    /// every such deque looked empty or was contended.
    pub(crate) fn steal_seated(&self, _seat: &Seat<'_>, class: Class) -> Option<JobPtr> {
        let shared = self.made()?;
        for worker in shared.workers.iter() {
            if worker.0.class != class {
                continue;
            }
            match worker.0.deque.steal() {
                Steal::Taken(job) => return Some(JobPtr(job)),
                Steal::Empty | Steal::Lost => {}
            }
        }
        None
    }

    /// A task of worker `at`'s own: its pinned queue, its LIFO slot, its deque.
    fn own(&self, at: u32) -> Option<JobPtr> {
        let worker = &self.shared().workers[at as usize].0;
        // `Acquire`: pairs with `spawn_on`'s `Release`; a zero skips the lock.
        if worker.pinned_count.load(Ordering::Acquire) != 0 {
            let mut pinned = worker.pinned.lock();
            if let Some(job) = pinned.pop_front() {
                worker.pinned_count.fetch_sub(1, Ordering::Relaxed);
                return Some(job);
            }
        }
        if worker.lifo_run.load(Ordering::Relaxed) < LIFO_RUN {
            let job = worker.lifo.swap(ptr::null_mut(), Ordering::Relaxed);
            if let Some(job) = NonNull::new(job) {
                worker.lifo_run.store(worker.lifo_run.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
                return Some(JobPtr(job));
            }
        }
        worker.lifo_run.store(0, Ordering::Relaxed);
        if let Some(job) = worker.deque.pop() {
            return Some(JobPtr(job));
        }
        let job = worker.lifo.swap(ptr::null_mut(), Ordering::Relaxed);
        NonNull::new(job).map(JobPtr)
    }

    /// Runs `job` on worker `at` if it is still unstarted, then gives up the queue's reference.
    fn run_job(&self, at: u32, job: JobPtr) {
        let worker = &self.shared().workers[at as usize].0;
        let header = job.0;
        // SAFETY: the queue's reference keeps the job alive until the release below.
        let head = unsafe { header.as_ref() };
        if head.claim.by_worker() {
            let outer = worker.current.set(head.name);
            // SAFETY: claimed by this thread just now.
            unsafe { (head.run)(header, Here::Worker(at)) };
            worker.current.set(outer);
        }
        // SAFETY: the queue's reference, which this worker took out of its queue.
        unsafe { (head.release)(header) };
    }

    /// Worker `at`'s loop, on its own thread, until the pool stops and nothing is left where it
    /// looks.
    pub(crate) fn work(&self, at: u32) {
        let shared = self.shared();
        let worker = &shared.workers[at as usize].0;
        // The thread was parked before it existed (task/idle.rs): it comes back as from a
        // park, a searcher if a spawn woke it, its mailbox first if it was reserved.
        let mut searching = shared.idle.back(&worker.mark) == Woke::Searching;
        #[cfg(test)]
        self.0.arrived.fetch_add(1, Ordering::SeqCst);
        let mut looks = 0;
        loop {
            if worker.mark.reserved() {
                if searching {
                    searching = false;
                    shared.idle.leave_search();
                }
                self.take_mailbox(at);
                continue;
            }
            if let Some(job) = self.find(at, &mut searching) {
                looks = 0;
                if searching {
                    searching = false;
                    let (busy, last) = shared.idle.found(&worker.mark);
                    if last {
                        self.notify(worker.group);
                    }
                    if !busy {
                        // Reserved meanwhile: the mailbox goes first; the found task waits
                        // on this worker's deque, or runs after it when the deque is full.
                        let job = match worker.deque.push(job.0) {
                            Ok(()) => None,
                            Err(job) => Some(JobPtr(job)),
                        };
                        self.take_mailbox(at);
                        if let Some(job) = job {
                            self.run_job(at, job);
                        }
                        continue;
                    }
                }
                self.run_job(at, job);
                continue;
            }
            if shared.stop.0.load(Ordering::Acquire) {
                if searching {
                    shared.idle.leave_search();
                }
                break;
            }
            if looks < SPIN_ROUNDS {
                looks += 1;
                hint::spin_loop();
                continue;
            }
            looks = 0;
            if !shared.idle.to_park(&worker.mark, searching) {
                // Reserved: the mailbox, at the loop's top.
                searching = false;
                continue;
            }
            searching = false;
            // The re-check after the fence: every queue once more (task/idle.rs).
            let woke = if self.visible(at) || shared.stop.0.load(Ordering::Acquire) {
                shared.idle.back(&worker.mark)
            } else {
                // Honor any pending cross-thread spare-frame trim request before parking idle
                // (`blaze_mem::frame::ask_spare_trim`): 1 relaxed atomic load on the fast path.
                let _ = blaze_mem::frame::settle_spare();
                let clock = &self.0.clock;
                let parked = worker.parker.park("bstd.task.worker.idle", Some(Deadline::after(clock.now(), PARK_SLICE)), clock);
                let woke = shared.idle.back(&worker.mark);
                if let Err(why) = parked {
                    *worker.broken.lock() = Some(why);
                    if woke == Woke::Searching {
                        shared.idle.leave_search();
                    }
                    break;
                }
                woke
            };
            searching = woke == Woke::Searching;
        }
        worker.exited.store(true, Ordering::Release);
        if self.0.left.fetch_add(1, Ordering::AcqRel) + 1 == self.0.started.load(Ordering::Acquire) {
            self.0.stopper.unpark();
        }
    }

    fn take_mailbox(&self, at: u32) {
        let worker = &self.shared().workers[at as usize].0;
        let job = worker.mailbox.swap(ptr::null_mut(), Ordering::Acquire);
        worker.mark.took_mailbox();
        match NonNull::new(job) {
            Some(job) => self.run_job(at, JobPtr(job)),
            None => unreachable!("bstd::task::threads worker {at}: reserved with an empty mailbox"),
        }
    }

    /// The next task for worker `at`, in the module doc's order. `searching` says whether it
    /// is counted a searcher; it asks to become one before it steals.
    fn find(&self, at: u32, searching: &mut bool) -> Option<JobPtr> {
        let shared = self.shared();
        let worker = &shared.workers[at as usize].0;
        let own = &shared.groups[worker.group as usize];
        if let Some(job) = self.own(at) {
            return Some(job);
        }
        let classes = match (worker.class, self.0.plants.find) {
            (Class::Foreground, FindPlant::BackgroundFirst) => [Class::Background, Class::Foreground],
            (Class::Foreground, FindPlant::None | FindPlant::FarFirst) => [Class::Foreground, Class::Background],
            (Class::Background, _) => [Class::Background, Class::Foreground],
        };
        if let Some(job) = take(own, classes[0]) {
            return Some(job);
        }
        if !*searching {
            if !shared.idle.begin_search(&worker.mark) {
                return None;
            }
            *searching = true;
        }
        let groups = shared.groups.len();
        let me = worker.group as usize;
        // The planted control starts one group on: the thief's own group comes last.
        let first = usize::from(self.0.plants.find == FindPlant::FarFirst);
        for step in 0..groups {
            let at_group = (me + first + step) % groups;
            let group = &shared.groups[at_group];
            // Start past the thief in its own group, so thieves spread over victims.
            for k in 0..group.len {
                let victim = group.first + (at + 1 + k) % group.len;
                if victim == at {
                    continue;
                }
                match shared.workers[victim as usize].0.deque.steal_half_into(&worker.deque) {
                    Steal::Taken(job) => {
                        let counter = if at_group == me { &worker.stole_near } else { &worker.stole_far };
                        counter.store(counter.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
                        return Some(JobPtr(job));
                    }
                    Steal::Empty | Steal::Lost => {}
                }
            }
            if at_group != me
                && let Some(job) = take(group, classes[0])
            {
                return Some(job);
            }
        }
        for step in 0..groups {
            if let Some(job) = take(&shared.groups[(me + step) % groups], classes[1]) {
                return Some(job);
            }
        }
        None
    }

    /// Whether any task is visible to worker `at`: the idle protocol's re-check (100% lock-free).
    fn visible(&self, at: u32) -> bool {
        let shared = self.shared();
        let worker = &shared.workers[at as usize].0;
        worker.pinned_count.load(Ordering::Acquire) != 0
            || shared.workers.iter().any(|it| !it.0.deque.looks_empty())
            || shared.groups.iter().any(|group| {
                group.fg_len.0.load(Ordering::Acquire) != 0 || group.bg_len.0.load(Ordering::Acquire) != 0
            })
    }

    /// Asks every worker to stop once it finds nothing more to run, and wakes them.
    pub(crate) fn stop(&self) {
        let Some(shared) = self.made() else { return };
        shared.stop.0.store(true, Ordering::SeqCst);
        for worker in &shared.workers {
            worker.0.parker.unpark();
        }
    }

    /// Whether worker `at`'s thread left its loop.
    pub(crate) fn exited(&self, at: u32) -> bool {
        self.shared().workers[at as usize].0.exited.load(Ordering::Acquire)
    }

    /// The workers left running after a stop, or broken, by worker: `None` when there are none.
    /// A worker whose thread never started ran nothing and is not one.
    pub(crate) fn hung(&self) -> Option<Hung> {
        let mut stuck = Vec::new_in(Global);
        let Some(shared) = self.made() else { return None };
        for (at, worker) in shared.workers.iter().enumerate() {
            let at = u32::try_from(at).unwrap_or(u32::MAX);
            if !worker.0.thread.load(Ordering::Acquire) {
                continue;
            }
            let broken = worker.0.broken.lock().take();
            if broken.is_some() || !worker.0.exited.load(Ordering::Acquire) {
                let task = match worker.0.current.read() {
                    Named::Task(name) => name,
                    Named::Changing => "(between two tasks: its name was being written)",
                };
                stuck.push(Stuck { worker: at, task, broken });
            }
        }
        if stuck.is_empty() { None } else { Some(Hung { stuck }) }
    }

    /// Steals by worker `at` from its own group and from the others: for a test.
    #[cfg(test)]
    pub(crate) fn steals(&self, at: u32) -> (u64, u64) {
        let worker = &self.shared().workers[at as usize].0;
        (worker.stole_near.load(Ordering::Relaxed), worker.stole_far.load(Ordering::Relaxed))
    }

    /// How many workers are parked now: for a test that needs them idle first.
    #[cfg(test)]
    pub(crate) fn parked(&self) -> u32 {
        self.shared().idle.counts().1
    }

    /// How many workers' threads have reached their loops: for a test that needs them all up.
    #[cfg(test)]
    pub(crate) fn arrived(&self) -> u32 {
        self.0.arrived.load(Ordering::SeqCst)
    }

    /// The victims worker `at` tries, in order: for a test of the order.
    #[cfg(test)]
    pub(crate) fn victims(&self, at: u32) -> Vec<u32, Global> {
        let shared = self.shared();
        let me = shared.workers[at as usize].0.group as usize;
        let groups = shared.groups.len();
        let mut order = Vec::new_in(Global);
        for step in 0..groups {
            let group = &shared.groups[(me + step) % groups];
            for k in 0..group.len {
                let victim = group.first + (at + 1 + k) % group.len;
                if victim != at {
                    order.push(victim);
                }
            }
        }
        order
    }
}

/// `job` onto `queue`, which holds at most `most`: never grown (its room was made at the
/// pool's start), so a full one gives the task back.
fn bounded_push(queue: &mut VecDeque<JobPtr, Global>, most: usize, job: JobPtr) -> Result<(), Full> {
    if queue.len() >= most {
        return Err(Full(job));
    }
    queue.push_back(job);
    Ok(())
}

/// One task off `group`'s queue of `class` (lock-free fast path when empty).
fn take(group: &GroupState, class: Class) -> Option<JobPtr> {
    let (queue, count) = match class {
        Class::Foreground => (&group.foreground, &group.fg_len.0),
        Class::Background => (&group.background, &group.bg_len.0),
    };
    if count.load(Ordering::Acquire) == 0 {
        return None;
    }
    let mut guard = queue.lock();
    let job = guard.pop_front();
    if job.is_some() {
        count.store(guard.len(), Ordering::Release);
    }
    job
}

impl Drop for Shared {
    /// The pool's last handle went: every worker has left its loop (rt joins them first), so
    /// whatever is still queued is a reference to a job some handle already ran or dropped.
    fn drop(&mut self) {
        let mut left = Vec::new_in(Global);
        for worker in &self.workers {
            let worker = &worker.0;
            while let Some(job) = worker.deque.pop() {
                left.push(JobPtr(job));
            }
            for slot in [&worker.lifo, &worker.mailbox] {
                if let Some(job) = NonNull::new(slot.swap(ptr::null_mut(), Ordering::Relaxed)) {
                    left.push(JobPtr(job));
                }
            }
            while let Some(job) = worker.pinned.lock().pop_front() {
                left.push(job);
            }
        }
        for group in &self.groups {
            for queue in [&group.foreground, &group.background] {
                while let Some(job) = queue.lock().pop_front() {
                    left.push(job);
                }
            }
        }
        for job in left {
            // SAFETY: the queue's reference, taken out of its queue here.
            let head = unsafe { job.0.as_ref() };
            assert!(!head.claim.by_worker(), "bstd::task::threads: the pool stopped with task {} never run", head.name);
            // SAFETY: the queue's reference, given up once.
            unsafe { (head.release)(job.0) };
        }
    }
}
