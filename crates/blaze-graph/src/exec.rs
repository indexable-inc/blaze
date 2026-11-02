//! Does what the plane asks (docs/driver.md, "The executor"; docs/v2/plan/lanes/core/plane.md, "The executor").
//!
//! [`drive`] runs one build of a [`Machine`] on the thread that calls it, which is the only one
//! that steps the machine: its work nodes on a worker pool lent to the build alone, its IO nodes
//! on a second one (they wait, they don't compute). [`Scheduler`] is the daemon's one
//! scheduler: it owns the [`Plane`] and steps it on a thread of its own, the owner thread, whose
//! one wait is for the next input; it never waits for a worker. Its tasks run on the process's
//! [`shared`] pool, near the group the plane names, each in a frame of its own
//! ([`blaze_mem::frame::task`]) inside [`arena::measure`] and [`blaze_mem::frame::measure`],
//! and what each held and how long it ran teach the [`Predict`] the next task's cost and
//! memory come from.

use std::{
    any::Any,
    cell::UnsafeCell,
    collections::VecDeque,
    mem::ManuallyDrop,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    ptr::NonNull,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use blaze_hash::HashMap;
use blaze_mem::{Cache, MemError, Pooled, Slab, Tag as MemTag, frame};
use blaze_protocol::events::{Event, Record};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::{
    Cancelled, Key,
    admission::{self, Predict, TaskKind},
    arena,
    machine::{Error, Input, Machine, NodeId, Outcome, Output, Pool, Spec},
    plane::{Memo, Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId},
    pool::{self, Header, Pool as Workers},
    pump::Lock,
    topology::Topology,
};

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

/// `count` node ids no other graph of this process has, the first of them: what one process
/// says of several graphs is one stream.
pub fn ids(count: usize) -> u32 {
    NEXT_ID.fetch_add(u32::try_from(count).unwrap_or(u32::MAX), Ordering::Relaxed)
}

/// What a node's work came to, and the nodes it found.
#[derive(Debug, PartialEq, Eq)]
pub struct Did {
    pub outcome: Outcome,
    pub added: Vec<Spec>,
}

impl Did {
    pub fn built() -> Did {
        // alloc: empty, allocates nothing
        Did { outcome: Outcome::Built, added: Vec::new() }
    }

    pub fn failed(why: impl Into<String>) -> Did {
        // alloc: empty, allocates nothing
        Did { outcome: Outcome::Failed { why: why.into() }, added: Vec::new() }
    }
}

/// The process's worker pool: a worker per CPU of [`Topology::detect`], bound to it.
///
/// # Panics
///
/// Where the machine's topology cannot be read, or a worker cannot be started or bound,
/// naming why: the pool is never made smaller or shaped by a guess.
pub fn shared() -> &'static Workers {
    static SHARED: OnceLock<Workers> = OnceLock::new();
    SHARED.get_or_init(|| {
        let bind = cfg!(any(target_os = "linux", target_os = "macos"));
        Topology::detect()
            .and_then(|topology| Workers::new(&topology, bind))
            .unwrap_or_else(|why| panic!("the process's worker pool: {why}"))
    })
}

/// Runs the build of `nodes` on `machine` to its end: `work` does a node's work on a thread of
/// its pool, `said` hears every record as the machine says it, and `trace`, when given, gets
/// every input the machine took, in order ([`crate::trace`]). Whether every node was done.
///
/// The work nodes run on a pool of the machine's `work` workers and the IO nodes on one of its
/// `io`, both lent to this build alone and kept for the next build of their size once it's
/// over (not bound to CPUs: several builds may run at once). `work` may borrow what the caller
/// has, and runs in a frame of its own ([`blaze_mem::frame::task`]), freed when the node is
/// done.
///
/// A node whose work panics fails, and once the build is over the panic goes on on the calling
/// thread, as from a thread of [`std::thread::scope`]: a panic that unwinds on purpose, as
/// the query engine's cancellation does, reaches whoever catches it. Work the frames' cap cancels
/// ([`blaze_mem::frame::Cancelled`]) runs again on the same thread.
pub fn drive<W, S>(
    machine: &mut Machine,
    nodes: Vec<Spec>,
    work: &W,
    said: &mut S,
    trace: Option<&mut Vec<Input>>,
) -> Result<bool, Error>
where
    W: Fn(NodeId) -> Did + Sync,
    S: FnMut(&Record),
{
    let start = Instant::now();
    let now = || ns(start);
    let config = machine.config();
    let workers = Lent::take(config.work, "worker");
    let io = Lent::take(config.io, "IO");
    // The machine runs at most `work + io` nodes at once and starts another only once it has
    // taken a `Done`, so this many never fill it: a worker's send never waits, and no send
    // allocates (an unbounded channel makes a block every 31).
    let (done_tx, done_rx) = bounded::<(NodeId, Did, u64)>(config.work + config.io);
    // Taken only to keep a node's panic, once it has unwound.
    // dyn: a panic's payload, as `catch_unwind` gives it
    let first_panic: Lock<Option<Box<dyn Any + Send>>> = Lock::new(None);
    // dyn: as above, once per node that panicked
    let failed = |panic: Box<dyn Any + Send>| {
        let did = Did::failed(panicked(&*panic));
        first_panic.lock_said("a build's first panic").get_or_insert(panic);
        did
    };
    let over = workers.pool().scope(|jobs| {
        io.pool().scope(|waits| {
            let post = |id: NodeId, at: Pool| {
                let (done_tx, failed, now) = (&done_tx, &failed, &now);
                let done = move |did: thread::Result<Did>| {
                    _ = done_tx.send((id, did.unwrap_or_else(failed), now()));
                };
                let run = move || loop {
                    match catch_unwind(AssertUnwindSafe(|| frame::task(MemTag::Other, |_| work(id)))) {
                        Ok(did) => break did,
                        Err(panic) if panic.is::<frame::Cancelled>() => {}
                        Err(panic) => resume_unwind(panic),
                    }
                };
                match at {
                    Pool::Io => waits.submit(None, run, done),
                    Pool::Work => jobs.submit(None, run, done),
                }
            };
            run(machine, nodes, &post, &done_rx, &now, said, trace)
        })
    });
    drop((workers, io));
    if let Some(panic) = first_panic.into_inner()
    {
        resume_unwind(panic);
    }
    over
}

fn run<P, N, S>(
    machine: &mut Machine,
    nodes: Vec<Spec>,
    post: &P,
    done_rx: &Receiver<(NodeId, Did, u64)>,
    now: &N,
    said: &mut S,
    mut trace: Option<&mut Vec<Input>>,
) -> Result<bool, Error>
where
    P: Fn(NodeId, Pool),
    N: Fn() -> u64,
    S: FnMut(&Record),
{
    // alloc: once per build; each step's outputs are drained from it
    let mut outputs = Vec::new();
    let mut feed = |input: Input| -> Result<Option<bool>, Error> {
        if let Some(trace) = trace.as_deref_mut() {
            // clone: only when the caller asked for a trace, to replay the build
            trace.push(input.clone());
        }
        let mut over = None;
        machine.step_into(input, &mut outputs)?;
        for output in outputs.drain(..) {
            match output {
                Output::Said(record) => said(&record),
                Output::Run { id, pool } => post(id, pool),
                Output::Finished { ok } => over = Some(ok),
            }
        }
        Ok(over)
    };
    let mut over = feed(Input::Add { at_ns: now(), nodes })?;
    if over.is_none() {
        over = feed(Input::Close { at_ns: now() })?;
    }
    while over.is_none() {
        // Every node the machine runs is on a thread that sends what came of it.
        let Ok((id, did, at_ns)) = recv_said(&done_rx, "the build's nodes done") else {
            unreachable!("`drive` keeps a sender until the build is over");
        };
        over = feed(Input::Done { at_ns, id, outcome: did.outcome, added: did.added })?;
    }
    Ok(over.unwrap_or(false))
}

/// The pools no [`drive`] is using, each kept for the next build of its size, so a build
/// starts no thread once one of its size has run. Taken only to take or give back a pool.
// alloc: empty, allocates nothing; a pool's entry is pushed once per pool size
static SPARE: Lock<Vec<Workers>> = Lock::new(Vec::new());

/// A pool of `workers` threads that only one [`drive`] uses, handed back to [`SPARE`] when
/// it's over: a build runs as many nodes at once as its machine says, whatever else runs.
struct Lent(Option<Workers>);

impl Lent {
    fn take(workers: usize, what: &str) -> Lent {
        let spare = {
            let mut spare = SPARE.lock_said("the spare pools");
            spare.iter().position(|it| it.workers() == workers).map(|at| spare.swap_remove(at))
        };
        Lent(Some(spare.unwrap_or_else(|| {
            Workers::new(&Topology::flat(workers), false)
                .unwrap_or_else(|why| panic!("the build's {workers} {what} threads: {why}"))
        })))
    }

    fn pool(&self) -> &Workers {
        self.0.as_ref().expect("a lent pool is held until it's dropped")
    }
}

impl Drop for Lent {
    fn drop(&mut self) {
        if let Some(pool) = self.0.take() {
            SPARE.lock_said("the spare pools").push(pool);
        }
    }
}

fn ns(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

// dyn: a panic's payload, read once per task that panicked
fn panicked(panic: &(dyn Any + Send)) -> String {
    let what = stdx::panic_text(panic).unwrap_or("a panic with no message");
    // alloc: the failure's message
    format!("panicked: {what}")
}

/// What runs a [`Scheduler`]'s tasks: the one database behind it. A task carries its
/// [`Runner::Work`], data the runner reads (a query and its key), not code of its own, so a
/// run is a static call and posting a task boxes nothing.
pub trait Runner: Send + Sync + 'static {
    /// A task's work, as the runner reads it.
    type Work: Send + 'static;
    /// Runs `work` on this thread: what it came to, or [`Cancelled`] to be posted again whole.
    fn run(&self, work: &Self::Work) -> Result<Did, Cancelled>;
}

/// A node of a [`Scheduler`]'s request, with its work.
///
/// The work of a node with a key puts its value in the database before it returns: the plane
/// keeps no memo, and asks the scheduler's [`Memo`] whether a key is settled before it runs a
/// node of it. A value that reached the database only after the `Done` would leave a node of
/// its key that became ready in between to run again.
///
/// The nodes a task finds go in its [`Did::added`] as [`Handle::found`] gives them, which
/// gives the scheduler their work first; a found node with no work fails the task that found
/// it, saying so.
#[derive(Clone)]
pub struct Task<W> {
    pub spec: Spec,
    pub kind: TaskKind,
    pub work: W,
}

/// What a request's opener hears of it, in order.
#[derive(Debug, PartialEq, Eq)]
pub enum Heard {
    Said(Record),
    /// The last of a request: nothing more comes.
    Finished { ok: bool },
    /// What the opener asked that the plane could not take, which changed nothing.
    Refused(Error),
}

pub struct Opened {
    pub request: RequestId,
    pub heard: Receiver<Heard>,
}

enum Msg<W> {
    Open { request: RequestId, weight: u32, keep_going: bool, heard: Sender<Heard> },
    Add { request: RequestId, tasks: Vec<Task<W>> },
    Found { request: RequestId, works: Vec<FoundWork<W>> },
    Close { request: RequestId },
    Cancel { request: RequestId },
    Done {
        at_ns: u64,
        /// The run's handle in the owner's [`Table`], as bits, free again once this is taken;
        /// `None` for a node run with no work, or with no room for its run, which had none.
        slot: Option<u64>,
        id: NodeId,
        request: RequestId,
        kind: TaskKind,
        key: Option<Key>,
        did: Did,
        /// What its last run held and took; `None` when it panicked.
        measure: Option<Measure>,
        group: Option<u32>,
    },
    /// What the predictor learned, as [`Predict::encode`] gives it.
    Estimates { reply: Sender<Vec<u8>> },
    /// How many slots the owner's table has made.
    #[cfg(test)]
    Slots { reply: Sender<usize> },
    Stop,
}

/// What the owner keeps of a found [`Task`]: its spec goes back to the task that found it.
struct FoundWork<W> {
    id: NodeId,
    key: Option<Key>,
    kind: TaskKind,
    work: W,
}

/// What every run of a scheduler reaches besides its slot. Leaked as the scheduler is made: a
/// run's last act is its `Done`'s send through `tx`, which nothing can wait for the end of, so
/// the sender (and the channel it keeps) must outlive every run.
struct Ctx<R: Runner> {
    tx: Sender<Msg<R::Work>>,
    /// The IO threads' queue; `None` stops one of them.
    io: Sender<Option<Ran<R>>>,
    pool: &'static Workers,
    start: Instant,
    /// Every run's slot, which is its pool job: at an address that stays put while the run is
    /// under way, reused once its `Done` is in.
    slots: blaze_mem::Pool<Run<R>>,
    runner: R,
}

/// What a task's run that ended held at most, and the time it ran (its own, not the tries
/// that gave way before it).
#[derive(Debug, Clone, Copy)]
struct Measure {
    peak: u64,
    ran_ns: u64,
}

/// A run under way: the node, and the work it runs, moved out of the owner's entry for it.
struct Live<W> {
    id: NodeId,
    request: RequestId,
    kind: TaskKind,
    key: Option<Key>,
    /// On its request's critical path, as the plane said when it ran it.
    critical: bool,
    work: W,
}

/// A run's slot in the scheduler's [`Ctx::slots`]: the pool's job itself (its header first),
/// so posting a run allocates nothing and clones no `Arc` or sender. Filled by the owner, then
/// read only by the thread that runs it until it sends its `Done`; then the owner's again.
#[repr(C)]
struct Run<R: Runner> {
    header: Header,
    ctx: &'static Ctx<R>,
    /// Its handle in the owner's [`Table`], as bits: what its `Done` names.
    at: u64,
    live: UnsafeCell<Option<Live<R::Work>>>,
}

// SAFETY: `live` has one reader or writer at a time: the owner while it fills the slot, then
// the thread holding its `Ran`, handed over by the pool's queue or the IO queue and back by
// the `Done` message, each of which orders the writes before the reads; so its work moves
// between threads and is never shared, which `Runner::Work: Send` allows. The other fields are
// written only before the slot is first posted, but for the header's `next`, which the pool's
// inject queue writes while the run is queued: an atomic, so shared writes are no race.
unsafe impl<R: Runner> Sync for Run<R> {}

/// The most runs under way at once. The plane runs no more tasks at once than its pools have
/// threads, so this is far past what any machine reaches; past it a node fails, naming the
/// table's [`MemError`].
const MAX_RUNS: u32 = 1 << 16;

/// How many slots the run table makes at once: one batch of its pool.
const RUNS: usize = blaze_mem::pool::BATCH;

/// The owner's mailbox. Only a run's `Done` comes from a run, and they are at most the runs
/// under way; every other message is a handle's, whose sender waits while it is full. The
/// owner itself never sends to it ([`Owner::later`]), so it always drains.
const MAILBOX: usize = 1 << 12;

/// The most messages the owner takes, without waiting, between two readings of what blaze
/// holds ([`arena::own`] reads every subsystem's counters, lines the workers keep writing): so
/// a reading costs a run 1/64 of it, and a stream of messages never holds it back for long.
const HELD_EVERY: usize = 64;

/// A run under way in the owner's [`Table`]: its slot, and what holding it back needs.
struct Slot<R: Runner> {
    run: Pooled<'static, Run<R>>,
    tag: MemTag,
    pool: Pool,
    near: Option<u32>,
}

/// The runs under way, named by generational handles (docs/v2/plan/design/memory.md,
/// "Generational handles"): a `Done` naming a run whose slot was freed is detected as a stale
/// handle, never read. Each run's own slot is in the scheduler's pool ([`Ctx::slots`]), whose
/// slots never move, so a slot's address is its job's for as long as it runs; only the owner
/// thread takes and frees them, through its own cache, and a run that ends sends its handle
/// back, nothing else. A warm take and free make no call to the global allocator.
struct Table<R: Runner> {
    cache: Cache<'static, Run<R>>,
    /// Dropped only with no run under way: a run under way still reads its slot.
    live: ManuallyDrop<Slab<Slot<R>>>,
}

impl<R: Runner> Drop for Table<R> {
    fn drop(&mut self) {
        // An owner unwinding with runs under way leaves their slots to them: the slots are the
        // scheduler's pool, which is never freed, and the handles naming them go with them.
        if self.live.is_empty() {
            // SAFETY: dropped once, here, and `live` is not touched again.
            unsafe { ManuallyDrop::drop(&mut self.live) };
        }
    }
}

impl<R: Runner> Table<R> {
    fn new(ctx: &'static Ctx<R>) -> Result<Table<R>, MemError> {
        Ok(Table {
            cache: ctx.slots.cache(),
            live: ManuallyDrop::new(Slab::new(MAX_RUNS, MemTag::Plane)?),
        })
    }

    fn running(&self) -> usize {
        self.live.len()
    }

    /// A slot filled with `live`, to be run on `pool` near `near` once `tag` lets it: its
    /// handle's bits.
    fn take(
        &mut self,
        ctx: &'static Ctx<R>,
        live: Live<R::Work>,
        tag: MemTag,
        pool: Pool,
        near: Option<u32>,
    ) -> Result<u64, MemError> {
        let header = Header::new(run_pooled::<R>);
        let live = UnsafeCell::new(Some(live));
        let run = self.cache.try_alloc(Run { header, ctx, at: u64::MAX, live })?;
        let handle = self.live.try_insert(Slot { run, tag, pool, near })?;
        let slot = self.live.get_mut(handle).expect("a run just placed is in the table");
        slot.run.at = handle.to_bits();
        Ok(handle.to_bits())
    }

    /// The one name of run `at`'s slot, to post it, and where it goes.
    /// Called once per run: two `Ran`s of one slot would both run it.
    fn ran(&self, at: u64) -> (Ran<R>, Pool, Option<u32>) {
        let slot = self.slot(at);
        (Ran(NonNull::from(&*slot.run)), slot.pool, slot.near)
    }

    fn slot(&self, at: u64) -> &Slot<R> {
        self.live.get(blaze_mem::Handle::from_bits(at)).unwrap_or_else(|| {
            panic!("run {at:#x} is not under way: a stale handle to the scheduler's run table")
        })
    }

    /// Frees run `at`'s slot, its `Done` being in (the `Ran` that named it was consumed
    /// sending it, which orders the run's last touch before this); the run's subsystem.
    fn free(&mut self, at: u64) -> MemTag {
        let Some(Slot { run, tag, .. }) = self.live.remove(blaze_mem::Handle::from_bits(at)) else {
            panic!("a `Done` names run {at:#x}, which is not under way: a stale handle")
        };
        self.cache.recycle(run);
        tag
    }
}

/// No run: the end of a [`Throttle`]'s queue.
const NO_RUN: u64 = u64::MAX;

#[derive(Clone, Copy)]
struct Queue {
    head: u64,
    tail: u64,
}

/// A run held back, and the one behind it.
struct Waiting {
    run: u64,
    next: u64,
}

/// Holds back the runs of a subsystem over its budget (docs/v2/plan/design/memory.md, "Each
/// subsystem has a budget"; `blaze_mem::tag::over_budget`), before the machine's budget is
/// reached: such a run waits while another of its subsystem runs, and starts once one ends with
/// the subsystem under its budget again, or once none of it runs. So a subsystem whose memory
/// sits in tables its tasks do not free still has its tasks run, one at a time, and nothing
/// waits for an end that never comes. Runs of one subsystem start in the order the plane
/// started them. The plane counts a held run as running, with its memory granted: holding it
/// back never lets the plane grant more.
struct Throttle {
    running: [u32; MemTag::COUNT],
    queues: [Queue; MemTag::COUNT],
    waiting: Slab<Waiting>,
}

impl Throttle {
    fn new() -> Result<Throttle, MemError> {
        Ok(Throttle {
            running: [0; MemTag::COUNT],
            queues: [Queue { head: NO_RUN, tail: NO_RUN }; MemTag::COUNT],
            waiting: Slab::new(MAX_RUNS, MemTag::Plane)?,
        })
    }

    /// Whether `run`, of subsystem `tag`, starts now, `over` saying which subsystems are over
    /// their budgets; if not, it waits in its subsystem's queue.
    fn admit(&mut self, tag: MemTag, run: u64, over: impl Fn(MemTag) -> bool) -> bool {
        let at = tag.index();
        if self.running[at] > 0 && (self.queues[at].head != NO_RUN || over(tag)) {
            let waiting = self.waiting.insert(Waiting { run, next: NO_RUN }).to_bits();
            let queue = &mut self.queues[at];
            match queue.tail {
                NO_RUN => queue.head = waiting,
                tail => {
                    let tail = blaze_mem::Handle::from_bits(tail);
                    self.waiting.get_mut(tail).expect("a queued run is waiting").next = waiting;
                }
            }
            self.queues[at].tail = waiting;
            return false;
        }
        self.running[at] += 1;
        true
    }

    /// A run of `tag` that started has ended.
    fn ended(&mut self, tag: MemTag) {
        self.running[tag.index()] -= 1;
    }

    /// The next held run that may start now, and its subsystem, `over` saying which are over
    /// their budgets: asked until `None` after each end.
    fn next(&mut self, over: impl Fn(MemTag) -> bool) -> Option<u64> {
        if self.waiting.is_empty() {
            return None;
        }
        let tag = MemTag::ALL.into_iter().find(|&tag| {
            let at = tag.index();
            self.queues[at].head != NO_RUN && (self.running[at] == 0 || !over(tag))
        })?;
        Some(self.start(tag))
    }

    /// Any held run, started whatever the budgets: the scheduler is stopping.
    fn any(&mut self) -> Option<u64> {
        let tag = MemTag::ALL.into_iter().find(|&tag| self.queues[tag.index()].head != NO_RUN)?;
        Some(self.start(tag))
    }

    fn start(&mut self, tag: MemTag) -> u64 {
        let at = tag.index();
        let head = blaze_mem::Handle::from_bits(self.queues[at].head);
        let Waiting { run, next } = self.waiting.remove(head).expect("a queue's head is waiting");
        self.queues[at].head = next;
        if next == NO_RUN {
            self.queues[at].tail = NO_RUN;
        }
        self.running[at] += 1;
        run
    }

    #[cfg(test)]
    fn held(&self) -> usize {
        self.waiting.len()
    }
}

/// The one name of a filled slot, held by the thread that runs it: made by [`Table::ran`] once
/// per run, as the owner starts it, consumed by [`Ran::done`] or by posting it again.
struct Ran<R: Runner>(NonNull<Run<R>>);

// SAFETY: a `Ran` moves to the thread that runs it with the pool's queue or the IO queue (both
// order the owner's writes to the slot before the reads); what the slot holds is `Send`
// (`Runner::Work: Send`) and its context `Sync` (`Runner: Sync`).
unsafe impl<R: Runner> Send for Ran<R> {}

impl<R: Runner> Ran<R> {
    fn run(&self) -> &Run<R> {
        // SAFETY: a slot is in the scheduler's pool, whose slots never move and which is never
        // freed (its context is leaked), and the owner recycles it only once this `Ran` is
        // consumed sending its `Done`.
        unsafe { self.0.as_ref() }
    }

    fn live(&self) -> &Live<R::Work> {
        // SAFETY: while a `Ran` lives its slot is filled and nobody writes it (see `Run`).
        unsafe { &*self.run().live.get() }.as_ref().expect("a slot given to a run is filled")
    }

    /// Says what came of the run. Its last touch of the slot is reading it for the message.
    fn done(self, did: Did, measure: Option<Measure>, group: Option<u32>) {
        let (run, live) = (self.run(), self.live());
        let ctx = run.ctx;
        let msg = Msg::Done {
            at_ns: ns(ctx.start),
            slot: Some(run.at),
            id: live.id,
            request: live.request,
            kind: live.kind,
            key: live.key,
            did,
            measure,
            group,
        };
        _ = ctx.tx.send(msg);
    }
}

/// Asks a [`Scheduler`] for requests from any thread, the work of its tasks included.
pub struct Handle<W> {
    tx: Sender<Msg<W>>,
    next: Arc<AtomicU32>,
}

impl<W> Clone for Handle<W> {
    fn clone(&self) -> Handle<W> {
        // clone: a sender and a counter, once per holder of a handle, never per task
        Handle { tx: self.tx.clone(), next: Arc::clone(&self.next) }
    }
}

impl<W> Handle<W> {
    /// A request of `weight` (its share of the threads against the others'); with
    /// `keep_going` false its first failure skips what of it has not started. Its records and
    /// its end come to [`Opened::heard`]; it is gone when the scheduler is.
    pub fn open(&self, weight: u32, keep_going: bool) -> Opened {
        let request = RequestId(self.next.fetch_add(1, Ordering::Relaxed));
        let (heard_tx, heard) = unbounded();
        _ = self.tx.send(Msg::Open { request, weight, keep_going, heard: heard_tx });
        Opened { request, heard }
    }

    /// Tasks of `request`, besides those it had; each one's spec takes the scheduler's
    /// [`Prediction`](crate::admission::Prediction) for its kind and key (its memory never
    /// under the predicted peak, its cost the predicted one when there is one).
    pub fn add(&self, request: RequestId, tasks: Vec<Task<W>>) {
        _ = self.tx.send(Msg::Add { request, tasks });
    }

    /// Gives the scheduler the work of the nodes a running task of `request` found, called by
    /// that task's work on its own thread before it returns; what it gives back goes in the
    /// task's [`Did::added`]. A request closed takes new nodes only so.
    pub fn found(&self, request: RequestId, tasks: Vec<Task<W>>) -> Vec<Spec> {
        let mut specs = Vec::with_capacity(tasks.len());
        let mut works = Vec::with_capacity(tasks.len());
        for Task { spec, kind, work } in tasks {
            works.push(FoundWork { id: spec.id, key: spec.key, kind, work });
            specs.push(spec);
        }
        _ = self.tx.send(Msg::Found { request, works });
        specs
    }

    /// No more tasks are added to `request` but by its tasks.
    pub fn close(&self, request: RequestId) {
        _ = self.tx.send(Msg::Close { request });
    }

    /// Skips what of `request` has not started; what runs is let finish, and the request
    /// finishes not ok.
    pub fn cancel(&self, request: RequestId) {
        _ = self.tx.send(Msg::Cancel { request });
    }

    /// What the scheduler's predictor has learned, as its [`Predict::encode`] gives it, to be
    /// kept in the daemon's image.
    ///
    /// # Panics
    ///
    /// When the scheduler is gone.
    pub fn estimates(&self) -> Vec<u8> {
        let (reply, estimates) = crossbeam_channel::bounded(1);
        _ = self.tx.send(Msg::Estimates { reply });
        recv_said(&estimates, "the scheduler's estimates").unwrap_or_else(|_| panic!("the scheduler is gone"))
    }

    #[cfg(test)]
    fn slots(&self) -> usize {
        let (reply, slots) = crossbeam_channel::bounded(1);
        _ = self.tx.send(Msg::Slots { reply });
        slots.recv().unwrap_or_else(|_| panic!("the scheduler is gone"))
    }
}

/// The daemon's one scheduler (docs/v2/plan/lanes/core/plane.md): requests from any thread, through
/// [`Handle`]s; the plane stepped on its owner thread; tasks on a worker pool, IO tasks on
/// threads of their own. Dropping it waits for the tasks under way (pool and IO, each posted
/// again until it ends; what came of them is not taken), then stops the owner thread and the
/// IO threads: so it is never dropped by one of its own tasks.
///
/// A run is a slot of the scheduler's blaze-mem pool, named by a generational handle in the
/// owner's table and reused once its `Done` is in, and the slot itself is the pool's job:
/// posting a task allocates nothing and clones no `Arc` or sender, and its end is one message
/// to the owner. A run whose subsystem ([`admission::tag_of`]) is over its budget waits while
/// another run of that subsystem is under way ([`Throttle`]).
///
/// A task's work runs in a frame of its own ([`blaze_mem::frame::task`], tagged with its
/// subsystem: a pool worker's chunks are on its NUMA node, and the frame is freed when the task
/// ends) inside [`arena::measure`] and [`blaze_mem::frame::measure`], and its `Done` gives the
/// plane the peak it held in both and the group it ran on. With a budget, the frames grow
/// under its [`admission::share`] for [`MemTag::Frames`] ([`blaze_mem::frame::Cap`]); a task
/// the cap cancels is posted again, as one that gives way. With a budget, the plane hears
/// what blaze holds ([`arena::own`]) after the `Done`s the owner took without waiting, at
/// most [`HELD_EVERY`] messages apart, and before it waits again. The scheduler's
/// [`Predict`] gives each task added or found its cost and memory before the plane sees it,
/// and learns the peak and own run time of each run that ends with a result (a run that gave
/// way and one that panicked teach it nothing), under the task's kind and its spec's key. A
/// task the plane says is on its request's
/// critical path runs on a fast group of the pool ([`Workers::submit_critical`]). Work that
/// returns [`Cancelled`] is posted again (the plane still sees it running), behind what is
/// ready on its group. The clock is read here, never by the plane.
///
/// Every request is released as it opens: once it finishes, the plane gives its room (nodes,
/// names, deps, its id) to the next, so a daemon's plane holds only the requests under way.
///
/// It is of one database ([`Runner`] and [`Memo`]) and one [`Predict`], each a type parameter
/// of its constructor: the owner thread's loop and the runs are made once for the three, and
/// every call to them is static. Holders name only the work, `Scheduler<W>`.
pub struct Scheduler<W> {
    handle: Handle<W>,
    owner: Option<JoinHandle<()>>,
    io: Vec<JoinHandle<()>>,
}

impl<W: Send + 'static> Scheduler<W> {
    /// A scheduler of the plane `config` says, on the process's [`shared`] pool. `runner`, the
    /// database, runs each task's work; the plane asks `memo`, the database's, whether a key is
    /// settled before it runs a node of it; `predict` gives each task's cost and memory and
    /// learns from each run that ends.
    pub fn new<R, P, M>(
        config: PlaneConfig,
        runner: R,
        predict: P,
        memo: M,
    ) -> Result<Scheduler<W>, Error>
    where
        R: Runner<Work = W>,
        P: Predict + 'static,
        M: Memo + Send + 'static,
    {
        Scheduler::with_pool(config, runner, predict, memo, shared())
    }

    /// A scheduler whose tasks run on `pool`.
    ///
    /// # Panics
    ///
    /// When a thread cannot be started, as [`std::thread::spawn`].
    pub fn with_pool<R, P, M>(
        config: PlaneConfig,
        runner: R,
        predict: P,
        memo: M,
        pool: &'static Workers,
    ) -> Result<Scheduler<W>, Error>
    where
        R: Runner<Work = W>,
        P: Predict + 'static,
        M: Memo + Send + 'static,
    {
        let io_threads = config.io;
        if let Some(budget) = &config.budget {
            admission::set_budgets(budget.bytes);
            // The frames' share of the budget is their hard backstop. Nothing spills frame
            // memory (a frame is freed when its task ends), so the spills go unheard.
            let limit = budget.bytes / 64 * admission::share(MemTag::Frames);
            let (cap, _spills) = frame::Cap::new(limit, limit);
            frame::set_cap(Some(cap));
        }
        let plane = Plane::with_memo(config, memo)?;
        let (tx, rx) = bounded::<Msg<W>>(MAILBOX);
        // Each message is a run under way or the stop of one thread, so a send never waits.
        let (io_tx, io_rx) = bounded::<Option<Ran<R>>>(MAX_RUNS as usize + io_threads);
        let io = (0..io_threads)
            .map(|at| {
                // clone: once per IO thread, setup
                let io_rx = io_rx.clone();
                thread::Builder::new()
                    // alloc: a thread's name, once per IO thread
                    .name(format!("blaze-io-{at}"))
                    .spawn(move || {
                        // Ends when the owner, its runs over, says so.
                        while let Ok(Some(run)) = recv_said(&io_rx, "the scheduler's IO runs") {
                            run_io(run);
                        }
                    })
                    .unwrap_or_else(|why| panic!("an IO thread of the scheduler: {why}"))
            })
            // alloc: the IO threads, once per scheduler
            .collect();
        let slots = blaze_mem::Pool::new(MAX_RUNS, MemTag::Plane)
            .unwrap_or_else(|why| panic!("the scheduler's run slots: {why}"));
        // alloc: the runs' context, once per scheduler and leaked (see `Ctx`)
        let ctx: &'static Ctx<R> = Box::leak(Box::new(Ctx {
            // clone: once per scheduler, setup
            tx: tx.clone(),
            io: io_tx,
            pool,
            start: Instant::now(),
            slots,
            runner,
        }));
        let table =
            Table::new(ctx).unwrap_or_else(|why| panic!("the scheduler's run table: {why}"));
        let throttle =
            Throttle::new().unwrap_or_else(|why| panic!("the scheduler's throttle: {why}"));
        let owner = Owner {
            plane,
            predict,
            heard: HashMap::default(),
            works: HashMap::default(),
            ctx,
            table,
            throttle,
            later: VecDeque::new(),
            io_threads,
            said: Vec::new(), // alloc: none, empty; the owner's buffer, kept across steps
            new: Vec::new(), // alloc: none, empty; the owner's buffer, kept across adds
            held_due: false,
        };
        let owner = thread::Builder::new()
            // alloc: a thread's name, once per scheduler
            .name("blaze-plane".to_owned())
            .spawn(move || owner.serve(&rx))
            .unwrap_or_else(|why| panic!("the scheduler's owner thread: {why}"));
        Ok(Scheduler {
            // alloc: the request counter, once per scheduler
            handle: Handle { tx, next: Arc::new(AtomicU32::new(0)) },
            owner: Some(owner),
            io,
        })
    }

    pub fn handle(&self) -> Handle<W> {
        // clone: once per holder of a handle, never per task
        self.handle.clone()
    }

    pub fn open(&self, weight: u32, keep_going: bool) -> Opened {
        self.handle.open(weight, keep_going)
    }

    pub fn add(&self, request: RequestId, tasks: Vec<Task<W>>) {
        self.handle.add(request, tasks);
    }

    pub fn close(&self, request: RequestId) {
        self.handle.close(request);
    }

    pub fn cancel(&self, request: RequestId) {
        self.handle.cancel(request);
    }

    pub fn estimates(&self) -> Vec<u8> {
        self.handle.estimates()
    }
}

impl<W> Drop for Scheduler<W> {
    fn drop(&mut self) {
        _ = self.handle.tx.send(Msg::Stop);
        if let Some(owner) = self.owner.take() {
            _ = owner.join();
        }
        for io in self.io.drain(..) {
            _ = io.join();
        }
    }
}

struct Entry<W> {
    request: RequestId,
    kind: TaskKind,
    key: Option<Key>,
    /// Moved to its run when the plane runs the node, which it does once.
    work: Option<W>,
}

/// The owner thread's: the plane and everything only it changes.
struct Owner<R: Runner, P, M> {
    plane: Plane<M>,
    predict: P,
    heard: HashMap<RequestId, Sender<Heard>>,
    /// The work of each node added and not settled.
    works: HashMap<NodeId, Entry<R::Work>>,
    ctx: &'static Ctx<R>,
    table: Table<R>,
    throttle: Throttle,
    /// What the owner tells itself (a node it cannot run fails), taken after the message at
    /// hand: never through its own mailbox, which could be full.
    later: VecDeque<Msg<R::Work>>,
    io_threads: usize,
    /// The plane's outputs of a step, kept empty between steps so a step allocates nothing. A
    /// step its outputs start (a run that fails) takes it while the outer step holds it out,
    /// and works from a fresh one.
    said: Vec<PlaneOutput>,
    /// The nodes of an add whose work is new, kept empty between adds: an add the plane
    /// refuses takes their work back out.
    new: Vec<NodeId>,
    /// A run ended since the plane last heard what blaze holds: with a budget, it hears before
    /// the owner next waits. Without one the plane reads no holding, so it is never told.
    held_due: bool,
}

impl<R: Runner, P: Predict, M: Memo> Owner<R, P, M> {
    fn serve(mut self, rx: &Receiver<Msg<R::Work>>) {
        /// Stops the IO threads as the owner ends, unwinding too.
        struct StopIo<R: Runner>(&'static Ctx<R>, usize);
        impl<R: Runner> Drop for StopIo<R> {
            fn drop(&mut self) {
                for _ in 0..self.1 {
                    _ = self.0.io.send(None);
                }
            }
        }
        let _stop_io = StopIo(self.ctx, self.io_threads);
        // The owner's one wait. What is already in the mailbox is taken before the plane hears
        // what blaze holds, up to `HELD_EVERY` messages, and it hears before the next wait.
        'serve: while let Ok(msg) = recv_said(&rx, "the scheduler's mailbox") {
            let mut next = Some(msg);
            let mut taken = 0;
            while let Some(msg) = next {
                if !self.take(msg) {
                    break 'serve;
                }
                while let Some(msg) = self.later.pop_front() {
                    if !self.take(msg) {
                        break 'serve;
                    }
                }
                taken += 1;
                next = if taken < HELD_EVERY { rx.try_recv().ok() } else { None };
            }
            self.hear_held();
        }
        // A run held back is under way to the plane: it runs, its budget aside, so that the
        // table empties.
        while let Some(at) = self.throttle.any() {
            self.start(at);
        }
        // Each run under way reads a slot of the table: it ends before the table goes, and
        // nothing more comes of it.
        while self.table.running() > 0 {
            match recv_said(&rx, "the scheduler's runs ending") {
                Ok(Msg::Done { slot: Some(slot), .. }) => {
                    let tag = self.table.free(slot);
                    self.throttle.ended(tag);
                }
                Ok(_) => {}
                Err(_) => unreachable!("the scheduler's context keeps a sender"),
            }
        }
    }

    /// Takes one input; false once told to stop.
    fn take(&mut self, msg: Msg<R::Work>) -> bool {
        let at_ns = ns(self.ctx.start);
        match msg {
            Msg::Open { request, weight, keep_going, heard } => {
                self.heard.insert(request, heard);
                let input = PlaneInput::Open { at_ns, request, weight, keep_going, idle: false };
                if self.step(request, input) {
                    // Its room in the plane is given back once it finishes; after that a step
                    // naming it is refused as `UnknownRequest`.
                    let released = self.plane.release(request);
                    let said = std::mem::take(&mut self.said);
                    self.outputs(request, released, said);
                } else {
                    self.heard.remove(&request);
                }
            }
            Msg::Add { request, tasks } => {
                // alloc: the plane's input, owned by it; `PlaneInput::AddNodes` is the columnar way
                let mut nodes = Vec::with_capacity(tasks.len());
                let mut new = std::mem::take(&mut self.new);
                new.clear();
                for Task { mut spec, kind, work } in tasks {
                    self.predict.predict(kind, spec.key).apply(&mut spec);
                    // A node added twice is refused by the plane: the first keeps its work.
                    if let std::collections::hash_map::Entry::Vacant(it) = self.works.entry(spec.id) {
                        it.insert(Entry { request, kind, key: spec.key, work: Some(work) });
                        new.push(spec.id);
                    }
                    nodes.push(spec);
                }
                if !self.step(request, PlaneInput::Add { at_ns, request, nodes }) {
                    for id in new.drain(..) {
                        self.works.remove(&id);
                    }
                }
                self.new = new;
            }
            Msg::Found { request, works } => {
                for FoundWork { id, key, kind, work } in works {
                    // Found again by a task posted again: the first keeps its work.
                    let entry = Entry { request, kind, key, work: Some(work) };
                    self.works.entry(id).or_insert(entry);
                }
            }
            Msg::Close { request } => _ = self.step(request, PlaneInput::Close { at_ns, request }),
            Msg::Cancel { request } => {
                let mut said = std::mem::take(&mut self.said);
                let taken = self.plane.cancel_into(at_ns, request, &mut said);
                self.outputs(request, taken, said);
            }
            Msg::Done { at_ns, slot, id, request, kind, key, did, measure, group } => {
                if let Some(slot) = slot {
                    let tag = self.table.free(slot);
                    self.throttle.ended(tag);
                }
                if let Some(Measure { peak, ran_ns }) = measure {
                    self.predict.observe(kind, key, ran_ns, peak);
                }
                let (mut outcome, mut added) = (did.outcome, did.added);
                let workless = added.iter().map(|spec| spec.id).find(|found| {
                    !self.works.get(found).is_some_and(|it| it.request == request)
                });
                if let Some(found) = workless {
                    outcome = Outcome::Failed {
                        // alloc: the failure's message
                        why: format!(
                            "node {found} found by node {id} has no work: a scheduler's task \
                             gives the work of what it finds with `Handle::found`"
                        ),
                    };
                    added.clear();
                }
                for spec in &mut added {
                    let kind = self.works[&spec.id].kind;
                    self.predict.predict(kind, spec.key).apply(spec);
                }
                let peak = measure.map_or(0, |it| it.peak);
                let input = PlaneInput::Done { at_ns, id, outcome, added, group, peak };
                if !self.step(request, input) {
                    // What it found could not be added: the node fails rather than run forever.
                    // alloc: the failure's message
                    let why = format!("what node {id} found could not be added to {request}");
                    let outcome = Outcome::Failed { why };
                    // alloc: empty, allocates nothing
                    let input = PlaneInput::Done { at_ns, id, outcome, added: Vec::new(), group, peak };
                    self.step(request, input);
                }
                self.held_due = self.plane.config().budget.is_some();
                while let Some(at) = self.throttle.next(blaze_mem::tag::over_budget) {
                    self.start(at);
                }
            }
            Msg::Estimates { reply } => _ = reply.send(self.predict.encode()),
            #[cfg(test)]
            Msg::Slots { reply } => _ = reply.send(self.ctx.slots.carved() as usize),
            Msg::Stop => return false,
        }
        true
    }

    /// Steps the plane; what it refuses goes to `request`'s opener. Whether it took it.
    fn step(&mut self, request: RequestId, input: PlaneInput) -> bool {
        let mut said = std::mem::take(&mut self.said);
        let taken = self.plane.step_into(input, &mut said);
        self.outputs(request, taken, said)
    }

    /// Tells the plane what blaze holds now, if a run ended since it was last told. The plane
    /// takes every `Held`, so no opener hears of it.
    fn hear_held(&mut self) {
        if !self.held_due {
            return;
        }
        self.held_due = false;
        let held = PlaneInput::Held { at_ns: ns(self.ctx.start), bytes: arena::own() };
        self.step(RequestId(u32::MAX), held);
    }

    /// Hands on what the plane appended to `said` (nothing when it refused), then keeps `said`,
    /// emptied, for the next step.
    fn outputs(
        &mut self,
        request: RequestId,
        taken: Result<(), Error>,
        mut said: Vec<PlaneOutput>,
    ) -> bool {
        let took = match taken {
            Ok(()) => {
                for output in said.drain(..) {
                    self.output(output);
                }
                true
            }
            Err(error) => {
                if let Some(heard) = self.heard.get(&request) {
                    _ = heard.send(Heard::Refused(error));
                }
                false
            }
        };
        // A step taken while this one drained left a buffer of its own; the larger is kept.
        if said.capacity() >= self.said.capacity() {
            self.said = said;
        }
        took
    }

    fn output(&mut self, output: PlaneOutput) {
        match output {
            PlaneOutput::Said { request, record } => {
                if let Event::Moved { id, state } = &record.event
                    && state.settled()
                {
                    self.works.remove(&NodeId(*id));
                }
                if let Some(heard) = self.heard.get(&request) {
                    _ = heard.send(Heard::Said(record));
                }
            }
            PlaneOutput::Run { id, pool, near, critical } => self.run(id, pool, near, critical),
            PlaneOutput::Yield { id } => {
                unreachable!("node {id} asked to give way: the scheduler opens no idle request")
            }
            PlaneOutput::Finished { request, ok } => {
                // Work found by a task that then gave way, and never added.
                self.works.retain(|_, it| it.request != request);
                if let Some(heard) = self.heard.remove(&request) {
                    _ = heard.send(Heard::Finished { ok });
                }
            }
        }
    }

    fn run(&mut self, id: NodeId, pool: Pool, near: Option<u32>, critical: bool) {
        let taken = self.works.get_mut(&id).and_then(|entry| {
            let work = entry.work.take()?;
            Some((entry.request, entry.kind, entry.key, work))
        });
        let Some((request, kind, key, work)) = taken else {
            // The plane runs only nodes added here, each with its work, and each once: a node
            // it runs with none (or with its work gone to its first run) fails, saying so.
            // alloc: a failure's message, once per failed node.
            let why = format!("node {id} was run with no work");
            self.fail(id, RequestId(u32::MAX), TaskKind(u32::MAX), why);
            return;
        };
        let live = Live { id, request, kind, key, critical, work };
        let tag = admission::tag_of(kind);
        let at = match self.table.take(self.ctx, live, tag, pool, near) {
            Ok(at) => at,
            Err(why) => {
                // alloc: a failure's message, once per failed node.
                self.fail(id, request, kind, format!("node {id} could not be run: {why}"));
                return;
            }
        };
        if self.throttle.admit(tag, at, blaze_mem::tag::over_budget) {
            self.start(at);
        }
    }

    /// Starts run `at`, of the table.
    fn start(&mut self, at: u64) {
        let (run, pool, near) = self.table.ran(at);
        let critical = run.live().critical;
        match pool {
            Pool::Work => post(self.ctx, near, critical, run),
            // The IO threads outlive this send: they end once the owner, its runs over, says so.
            Pool::Io => _ = self.ctx.io.send(Some(run)),
        }
    }

    /// Node `id` fails with `why` without running, its `Done` taken after the message at hand.
    fn fail(&mut self, id: NodeId, request: RequestId, kind: TaskKind, why: String) {
        self.later.push_back(Msg::Done {
            at_ns: ns(self.ctx.start),
            slot: None,
            id,
            request,
            kind,
            key: None,
            did: Did::failed(why),
            measure: None,
            group: None,
        });
    }
}

/// Runs `run` on the scheduler's pool near `near` (on a fast group when it is critical).
#[allow(clippy::needless_pass_by_value, reason = "posting transfers the run's sole ownership to the pool job")]
fn post<R: Runner>(ctx: &Ctx<R>, near: Option<u32>, critical: bool, run: Ran<R>) {
    // SAFETY: the slot's header is its first field (`Run` is `repr(C)`) and runs `run_pooled`;
    // the slot stays in the scheduler's pool, unmoved, until its `Done` is taken, and `run`,
    // moved here, was its one name.
    let job = unsafe { pool::Job::from_header(run.0.cast::<Header>()) };
    ctx.pool.push_placed(near, critical, job);
}

/// A run's pool job: its work, then its `Done`; posted again, behind what is ready on the group
/// it ran on, while its work gives way.
///
/// # Safety
///
/// `header` is the header of a filled slot of a live [`Table`], given to the pool by [`post`].
unsafe fn run_pooled<R: Runner>(header: NonNull<Header>) {
    let run = Ran(header.cast::<Run<R>>());
    let (ctx, critical) = (run.run().ctx, run.live().critical);
    let span = pool::JobSpan::begin(std::any::type_name::<R>());
    let result = catch_unwind(AssertUnwindSafe(|| measured(&ctx.runner, run.live())));
    drop(span);
    let group = pool::group();
    match result {
        Ok((Ok(did), measure)) => run.done(did, Some(measure), group),
        Ok((Err(Cancelled), _)) => post(ctx, group, critical, run),
        Err(panic) if panic.is::<frame::Cancelled>() => post(ctx, group, critical, run),
        Err(panic) => run.done(Did::failed(panicked(&*panic)), None, group),
    }
}

/// `live`'s work run by `runner` in a frame of its own, freed when it ends, the peak it held
/// (through the global allocator and in its frames) and the time it took. The frames' cap
/// cancelling it unwinds from here with [`frame::Cancelled`].
fn measured<R: Runner>(runner: &R, live: &Live<R::Work>) -> (Result<Did, Cancelled>, Measure) {
    let start = Instant::now();
    let tag = admission::tag_of(live.kind);
    let ((did, bytes), frames) =
        frame::measure(|| arena::measure(|| frame::task(tag, |_| runner.run(&live.work))));
    (did, Measure { peak: bytes + frames as u64, ran_ns: ns(start) })
}

/// Runs an IO task on this thread; posted again, behind what is queued for the IO threads,
/// while its work gives way.
fn run_io<R: Runner>(run: Ran<R>) {
    let ctx = run.run().ctx;
    match catch_unwind(AssertUnwindSafe(|| measured(&ctx.runner, run.live()))) {
        Ok((Ok(did), measure)) => run.done(did, Some(measure), None),
        Ok((Err(Cancelled), _)) => _ = ctx.io.send(Some(run)),
        Err(panic) if panic.is::<frame::Cancelled>() => _ = ctx.io.send(Some(run)),
        Err(panic) => run.done(Did::failed(panicked(&*panic)), None, None),
    }
}

/// `rx.recv()`, said as a wait on the channel `name` ([`crate::wait`]) only when nothing is
/// there yet: a receive that finds a message costs a `try_recv`.
fn recv_said<T>(rx: &Receiver<T>, name: &'static str) -> Result<T, crossbeam_channel::RecvError> {
    match rx.try_recv() {
        Ok(it) => Ok(it),
        Err(crossbeam_channel::TryRecvError::Disconnected) => Err(crossbeam_channel::RecvError),
        Err(crossbeam_channel::TryRecvError::Empty) => {
            crate::wait::wait(crate::wait::WaitOn::Channel(name), None, || rx.recv())
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU32, Ordering},
        },
        thread::ThreadId,
        time::Duration,
    };

    use blaze_hash::{HashMap, HashSet};
    use blaze_ids::rng::SplitMix64;
    use blaze_protocol::events::{Event, NodeKind, NodeState, Record};
    use crossbeam_channel::bounded;
    use parking_lot::Mutex;

    use blaze_mem::{Tag as MemTag, frame};

    use super::{
        Ctx, Did, Heard, Live, Opened, RUNS, RequestId, Runner, SPARE, Scheduler, Table, Task,
        Throttle, Workers, drive,
    };
    use crate::{
        Cancelled, Key,
        admission::{Estimates, Predict, Prediction, TaskKind},
        machine::{Config, Machine, NodeId, Outcome, Pool, Spec},
        plane::PlaneConfig,
        pool::group,
        topology::Topology,
    };

    const LONG: Duration = Duration::from_secs(60);

    fn spec(id: u32, deps: &[u32], pool: Pool, key: Option<u8>) -> Spec {
        Spec {
            id: NodeId(id),
            kind: NodeKind::Names,
            name: format!("n{id}"),
            deps: deps.iter().map(|&it| NodeId(it)).collect(),
            pool,
            cost: 1,
            key: key.map(|it| Key([it; 32])),
            memory: 0,
        }
    }

    /// A test task's work: a closure of its own, which [`Closures`] calls.
    // dyn: the tests' work, a different closure for each task they make
    type Work = Arc<dyn Fn() -> Result<Did, Cancelled> + Send + Sync>;

    /// The tests' runner: each task's work is the closure to call.
    struct Closures;

    impl Runner for Closures {
        type Work = Work;

        fn run(&self, work: &Work) -> Result<Did, Cancelled> {
            work()
        }
    }

    fn task(id: u32, deps: &[u32], key: Option<u8>, work: Work) -> Task<Work> {
        Task { spec: spec(id, deps, Pool::Work, key), kind: TaskKind(1), work }
    }

    /// The database a test's work puts each key's value in before its `Done`, as real work
    /// does; the scheduler's memo reads it.
    type Db = Arc<Mutex<HashMap<Key, Outcome>>>;

    fn scheduler(workers: usize) -> (Scheduler<Work>, Db) {
        scheduler_on(&Topology::flat(workers)).0
    }

    /// A scheduler on a pool of `topology`'s shape, and the pool.
    fn scheduler_on(topology: &Topology) -> ((Scheduler<Work>, Db), &'static Workers) {
        scheduler_with(topology, Estimates::default())
    }

    fn scheduler_with(
        topology: &Topology,
        predict: impl Predict + 'static,
    ) -> ((Scheduler<Work>, Db), &'static Workers) {
        let pool: &'static Workers = Box::leak(Box::new(Workers::new(topology, false).unwrap()));
        let workers = pool.workers();
        let config = PlaneConfig { io: 1, work: workers, budget: None, starve_after: 4 };
        let db = Db::default();
        let memo = {
            let db = Arc::clone(&db);
            move |key: Key| db.lock().get(&key).cloned()
        };
        ((Scheduler::with_pool(config, Closures, predict, memo, pool).unwrap(), db), pool)
    }

    /// What a [`Recorder`] was asked and told, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Asked {
        Predict(TaskKind, Option<Key>),
        Observe { kind: TaskKind, key: Option<Key>, ran_ns: u64 },
    }

    /// A predictor that keeps what it is asked and told, and predicts `prediction` for all.
    struct Recorder {
        asked: Arc<Mutex<Vec<Asked>>>,
        prediction: Prediction,
    }

    impl Predict for Recorder {
        fn predict(&self, kind: TaskKind, key: Option<Key>) -> Prediction {
            self.asked.lock().push(Asked::Predict(kind, key));
            self.prediction
        }

        fn observe(&mut self, kind: TaskKind, key: Option<Key>, ran_ns: u64, _peak: u64) {
            self.asked.lock().push(Asked::Observe { kind, key, ran_ns });
        }

        fn encode(&self) -> Vec<u8> {
            self.asked.lock().len().to_le_bytes().to_vec()
        }
    }

    /// The scheduler asks its predictor of each task added and found, with the task's kind and
    /// key, and tells it the own run time of each run that ended: a task that gave way twice
    /// is observed once, for the run that finished, not the tries before it. Its estimates are
    /// the predictor's bytes.
    #[test]
    fn the_scheduler_asks_its_predictor_and_teaches_it_each_run() {
        let asked: Arc<Mutex<Vec<Asked>>> = Arc::default();
        let recorder = Recorder {
            asked: Arc::clone(&asked),
            prediction: Prediction { cost: Some(5), peak: 1 << 10 },
        };
        let ((scheduler, _), _) = scheduler_with(&Topology::flat(2), recorder);
        let opened = scheduler.open(1, false);
        let (request, handle) = (opened.request, scheduler.handle());
        let tries = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&tries);
        let slow: Work = Arc::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) < 2 {
                crate::spin::busy(Duration::from_millis(200));
                return Err(Cancelled);
            }
            crate::spin::busy(Duration::from_millis(5));
            Ok(Did::built())
        });
        let finder: Work = Arc::new(move || {
            let found: Task<Work> = Task {
                spec: spec(52, &[51], Pool::Work, Some(9)),
                kind: TaskKind(3),
                work: Arc::new(|| Ok(Did::built())),
            };
            let added = handle.found(request, vec![found]);
            Ok(Did { outcome: Outcome::Built, added })
        });
        let mut keyed = task(51, &[50], Some(8), finder);
        keyed.kind = TaskKind(2);
        scheduler.add(request, vec![task(50, &[], None, slow), keyed]);
        scheduler.close(request);
        assert!(finish(&opened).0);
        assert_eq!(tries.load(Ordering::SeqCst), 3);

        let asked = asked.lock().clone();
        let key = |it: u8| Some(Key([it; 32]));
        let predicted: Vec<_> =
            asked.iter().filter(|it| matches!(it, Asked::Predict(..))).cloned().collect();
        assert_eq!(predicted, [
            Asked::Predict(TaskKind(1), None),
            Asked::Predict(TaskKind(2), key(8)),
            Asked::Predict(TaskKind(3), key(9)),
        ]);
        let observed: Vec<(TaskKind, Option<Key>, u64)> = asked
            .iter()
            .filter_map(|it| match *it {
                Asked::Observe { kind, key, ran_ns } => Some((kind, key, ran_ns)),
                Asked::Predict(..) => None,
            })
            .collect();
        let kinds: Vec<_> = observed.iter().map(|&(kind, key, _)| (kind, key)).collect();
        assert_eq!(kinds, [(TaskKind(1), None), (TaskKind(2), key(8)), (TaskKind(3), key(9))]);
        let slow_ns = observed[0].2;
        assert!(
            (5_000_000..200_000_000).contains(&slow_ns),
            "the run that finished took {slow_ns} ns, not the tries that gave way"
        );
        assert_eq!(scheduler.estimates(), 6usize.to_le_bytes());
    }

    /// A chain of five tasks on a machine of P and E cores: each, the only work of its request
    /// when it runs, is on its critical path, and the scheduler submits it as critical.
    #[test]
    fn a_task_on_the_critical_path_is_submitted_as_critical() {
        let ((scheduler, _), pool) = scheduler_on(&Topology::from_perflevels(&[(1, 1), (2, 2)]));
        let built: Work = Arc::new(|| Ok(Did::built()));
        let opened = scheduler.open(1, false);
        let chain = (0..5u32)
            .map(|id| {
                let deps: Vec<u32> = (id > 0).then_some(59 + id).into_iter().collect();
                task(60 + id, &deps, None, Arc::clone(&built))
            })
            .collect();
        scheduler.add(opened.request, chain);
        scheduler.close(opened.request);
        assert!(finish(&opened).0);
        assert_eq!(pool.critical_submitted(), 5);
    }

    /// Every task's work runs in a frame of its own, under `drive` and the scheduler both.
    #[test]
    fn work_runs_in_a_task_frame() {
        let in_frame = || frame::in_task() && frame::current(|frame| *frame.alloc(7u64)) == 7;

        let mut machine = Machine::new(Config { io: 1, work: 2, keep_going: false }).unwrap();
        let nodes = vec![spec(0, &[], Pool::Io, None), spec(1, &[0], Pool::Work, None)];
        let work = |_: NodeId| if in_frame() { Did::built() } else { Did::failed("no frame") };
        assert!(drive(&mut machine, nodes, &work, &mut |_| {}, None).unwrap());

        let (scheduler, _) = scheduler(2);
        let work: Work =
            Arc::new(move || Ok(if in_frame() { Did::built() } else { Did::failed("no frame") }));
        let opened = scheduler.open(1, false);
        let io = Task { spec: spec(71, &[], Pool::Io, None), kind: TaskKind(1), work: Arc::clone(&work) };
        scheduler.add(opened.request, vec![io, task(72, &[71], None, work)]);
        scheduler.close(opened.request);
        assert!(finish(&opened).0);
    }

    /// A runner of plain data: each task's work is a number, moved to its run with no closure
    /// or box, and the runner reads each once, on the pool and the IO threads both.
    #[test]
    fn a_runner_reads_each_tasks_work_as_data() {
        struct Squares(Arc<Mutex<Vec<u32>>>);
        impl Runner for Squares {
            type Work = u32;

            fn run(&self, work: &u32) -> Result<Did, Cancelled> {
                self.0.lock().push(*work);
                Ok(Did::built())
            }
        }
        let seen: Arc<Mutex<Vec<u32>>> = Arc::default();
        let pool = Workers::new(&Topology::flat(2), false).unwrap();
        let pool: &'static Workers = Box::leak(Box::new(pool));
        let config = PlaneConfig { io: 1, work: 2, budget: None, starve_after: 4 };
        let runner = Squares(Arc::clone(&seen));
        let memo = crate::plane::NoMemo;
        let scheduler =
            Scheduler::with_pool(config, runner, Estimates::default(), memo, pool).unwrap();
        let opened = scheduler.open(1, false);
        let tasks = (0..20u32)
            .map(|id| {
                let at = if id % 4 == 0 { Pool::Io } else { Pool::Work };
                Task { spec: spec(200 + id, &[], at, None), kind: TaskKind(1), work: id * id }
            })
            .collect();
        scheduler.add(opened.request, tasks);
        scheduler.close(opened.request);
        assert!(finish(&opened).0);
        let mut seen = seen.lock().clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..20u32).map(|id| id * id).collect::<Vec<_>>());
    }

    /// Everything the request said, up to its end, and whether it was ok.
    fn finish(opened: &Opened) -> (bool, Vec<Record>) {
        let mut records = Vec::new();
        loop {
            match opened.heard.recv_timeout(LONG).expect("the request ends") {
                Heard::Said(record) => records.push(record),
                Heard::Finished { ok } => return (ok, records),
                Heard::Refused(error) => panic!("refused: {error}"),
            }
        }
    }

    /// Work nodes run on the build's pool, IO nodes on threads of no pool, and the work borrows
    /// what the caller has.
    #[test]
    fn drive_runs_work_and_io_on_pools_apart_and_keeps_them_for_the_next_build() {
        // Sizes no other test drives, so no other build can borrow these pools in between.
        let build = || {
            let ran: Mutex<Vec<(u32, ThreadId)>> = Mutex::new(Vec::new());
            let mut machine = Machine::new(Config { io: 3, work: 7, keep_going: false }).unwrap();
            let nodes = vec![
                spec(0, &[], Pool::Io, None),
                spec(1, &[0], Pool::Work, None),
                spec(2, &[0], Pool::Work, None),
                spec(3, &[1, 2], Pool::Io, None),
            ];
            let work = |id: NodeId| {
                assert_eq!(group(), Some(0), "node {id} runs on a pool");
                ran.lock().push((id.0, std::thread::current().id()));
                Did::built()
            };
            assert!(drive(&mut machine, nodes, &work, &mut |_| {}, None).unwrap());
            let ran = ran.into_inner();
            let on = |nodes: &[u32]| -> HashSet<ThreadId> {
                ran.iter().filter(|(id, _)| nodes.contains(id)).map(|&(_, at)| at).collect()
            };
            assert_eq!(ran.len(), 4);
            assert!(on(&[0, 3]).is_disjoint(&on(&[1, 2])), "IO and work share a thread");
        };
        let spare = || SPARE.lock().iter().filter(|it| [3, 7].contains(&it.workers())).count();
        // A build that made its pools rather than borrowing the last one's would leave more.
        for _ in 0..4 {
            build();
            assert_eq!(spare(), 2, "the build's two pools are kept for the next");
        }
    }

    /// Two requests of weights 1 and 3 ask for the same ten keys: each key runs once, joined
    /// while it runs or read from the database once done, and both finish. Finished requests
    /// are released: a third takes their node ids again, and the database settles all of it.
    #[test]
    fn shared_keys_run_once_for_two_requests() {
        let (scheduler, db) = scheduler(4);
        let runs: Arc<Mutex<HashMap<u8, u32>>> = Arc::default();
        let keyed = |id: u32, key: u8| {
            let (runs, db) = (Arc::clone(&runs), Arc::clone(&db));
            let work: Work = Arc::new(move || {
                *runs.lock().entry(key).or_default() += 1;
                crate::spin::busy(Duration::from_millis(1));
                db.lock().insert(Key([key; 32]), Outcome::Built);
                Ok(Did::built())
            });
            task(id, &[], Some(key), work)
        };
        let light = scheduler.open(1, false);
        let heavy = scheduler.open(3, false);
        scheduler.add(light.request, (0..10).map(|key| keyed(u32::from(key), key)).collect());
        scheduler.add(heavy.request, (0..10).map(|key| keyed(100 + u32::from(key), key)).collect());
        scheduler.close(light.request);
        scheduler.close(heavy.request);
        assert!(finish(&light).0);
        assert!(finish(&heavy).0);
        {
            let runs = runs.lock();
            assert_eq!(runs.len(), 10);
            assert!(runs.values().all(|&it| it == 1), "{runs:?}");
        }

        let again = scheduler.open(1, false);
        scheduler.add(again.request, (0..10).map(|key| keyed(u32::from(key), key)).collect());
        scheduler.close(again.request);
        let (ok, records) = finish(&again);
        assert!(ok);
        let cached = records
            .iter()
            .filter(|it| matches!(it.event, Event::Moved { state: NodeState::Cached { .. }, .. }))
            .count();
        assert_eq!(cached, 10);
        assert!(runs.lock().values().all(|&it| it == 1), "the database settled it");
    }

    /// A request cancelled while its first task runs: what waits for it never runs, the task
    /// runs to its end, and the request finishes not ok.
    #[test]
    fn a_cancelled_request_finishes_not_ok_and_its_running_work_completes() {
        let (scheduler, _) = scheduler(2);
        let (started_tx, started_rx) = bounded::<()>(1);
        let (gate_tx, gate_rx) = bounded::<()>(0);
        let (ended_tx, ended_rx) = bounded::<()>(1);
        let second_ran = Arc::new(AtomicBool::new(false));
        let first: Work = Arc::new(move || {
            _ = started_tx.send(());
            gate_rx.recv_timeout(LONG).expect("the gate opens");
            _ = ended_tx.send(());
            Ok(Did::built())
        });
        let ran = Arc::clone(&second_ran);
        let second: Work = Arc::new(move || {
            ran.store(true, Ordering::SeqCst);
            Ok(Did::built())
        });
        let opened = scheduler.open(1, false);
        scheduler.add(opened.request, vec![task(0, &[], None, first), task(1, &[0], None, second)]);
        scheduler.close(opened.request);
        started_rx.recv_timeout(LONG).unwrap();
        scheduler.cancel(opened.request);
        gate_tx.send_timeout((), LONG).unwrap();
        let (ok, _) = finish(&opened);
        assert!(!ok);
        ended_rx.recv_timeout(LONG).expect("the running work completes");
        assert!(!second_ran.load(Ordering::SeqCst));
    }

    /// A thousand runs, no more than the pool's two workers under way at once and an IO task
    /// beside each round, take one batch of the run slots' pool: each slot is reused once its
    /// `Done` is in.
    #[test]
    fn runs_reuse_their_slots() {
        let (scheduler, _) = scheduler(2);
        let built: Work = Arc::new(|| Ok(Did::built()));
        for round in 0..10u32 {
            let opened = scheduler.open(1, false);
            let first = 2000 + round * 101;
            let mut tasks: Vec<Task<Work>> =
                (0..100).map(|at| task(first + at, &[], None, Arc::clone(&built))).collect();
            let io = spec(first + 100, &[], Pool::Io, None);
            tasks.push(Task { spec: io, kind: TaskKind(1), work: Arc::clone(&built) });
            scheduler.add(opened.request, tasks);
            scheduler.close(opened.request);
            assert!(finish(&opened).0);
        }
        assert_eq!(scheduler.handle().slots(), RUNS);
    }

    /// Dropping a scheduler waits for the task under way, which reads its slot, and the task
    /// ends.
    #[test]
    fn dropping_the_scheduler_waits_for_its_runs() {
        let (scheduler, _) = scheduler(2);
        let (started_tx, started_rx) = bounded::<()>(1);
        let (gate_tx, gate_rx) = bounded::<()>(0);
        let ended = Arc::new(AtomicBool::new(false));
        let work: Work = {
            let ended = Arc::clone(&ended);
            Arc::new(move || {
                _ = started_tx.send(());
                gate_rx.recv_timeout(LONG).expect("the gate opens");
                ended.store(true, Ordering::SeqCst);
                Ok(Did::built())
            })
        };
        let opened = scheduler.open(1, false);
        scheduler.add(opened.request, vec![task(90, &[], None, work)]);
        scheduler.close(opened.request);
        started_rx.recv_timeout(LONG).unwrap();
        let dropped = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let dropper = scope.spawn(|| {
                drop(scheduler);
                dropped.store(true, Ordering::SeqCst);
            });
            crate::spin::busy(Duration::from_millis(50));
            assert!(!dropped.load(Ordering::SeqCst), "the drop waits for the run under way");
            gate_tx.send_timeout((), LONG).unwrap();
            dropper.join().unwrap();
        });
        assert!(ended.load(Ordering::SeqCst) && dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn work_that_gives_way_twice_is_posted_again_and_done() {
        let (scheduler, _) = scheduler(2);
        let tries = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&tries);
        let work: Work = Arc::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) < 2 { Err(Cancelled) } else { Ok(Did::built()) }
        });
        let opened = scheduler.open(1, false);
        scheduler.add(opened.request, vec![task(7, &[], None, work)]);
        scheduler.close(opened.request);
        let (ok, records) = finish(&opened);
        assert!(ok);
        assert_eq!(tries.load(Ordering::SeqCst), 3);
        assert!(records.iter().any(|it| matches!(
            it.event,
            Event::Moved { id: 7, state: NodeState::Built }
        )));
        // Measured once: the tries that gave way taught nothing.
        let estimates = Estimates::decode(&scheduler.estimates()).unwrap();
        assert_eq!(estimates.by_kind.get(&TaskKind(1)).map(|it| it.seen), Some(1));
    }

    /// Nodes a task finds, their work given through a handle, run after it once the request is
    /// closed; a found node with no work fails the task that found it.
    #[test]
    fn a_task_gives_the_work_of_what_it_finds() {
        let (scheduler, _) = scheduler(2);
        let opened = scheduler.open(2, false);
        let (request, handle) = (opened.request, scheduler.handle());
        let found_ran = Arc::new(AtomicBool::new(false));
        let ran = Arc::clone(&found_ran);
        let finder: Work = Arc::new(move || {
            let ran = Arc::clone(&ran);
            let found: Work = Arc::new(move || {
                ran.store(true, Ordering::SeqCst);
                Ok(Did::built())
            });
            let added = handle.found(request, vec![task(21, &[20], None, found)]);
            Ok(Did { outcome: Outcome::Built, added })
        });
        scheduler.add(request, vec![task(20, &[], None, finder)]);
        scheduler.close(request);
        assert!(finish(&opened).0);
        assert!(found_ran.load(Ordering::SeqCst));

        let opened = scheduler.open(1, true);
        let workless: Work = Arc::new(|| {
            Ok(Did { outcome: Outcome::Built, added: vec![spec(41, &[40], Pool::Work, None)] })
        });
        scheduler.add(opened.request, vec![task(40, &[], None, workless)]);
        scheduler.close(opened.request);
        let (ok, records) = finish(&opened);
        assert!(!ok);
        assert!(records.iter().any(|it| matches!(
            &it.event,
            Event::Moved { id: 40, state: NodeState::Failed { why } } if why.contains("no work")
        )));
    }

    #[test]
    fn a_refused_input_is_heard_by_its_opener() {
        let (scheduler, _) = scheduler(1);
        let opened = scheduler.open(0, false);
        assert!(matches!(opened.heard.recv_timeout(LONG), Ok(Heard::Refused(_))));
        let opened = scheduler.open(1, false);
        let built: Work = Arc::new(|| Ok(Did::built()));
        scheduler.add(opened.request, vec![task(30, &[31], None, built)]);
        assert!(matches!(opened.heard.recv_timeout(LONG), Ok(Heard::Refused(_))));
    }

    /// A scheduler's context on a one-worker pool, for the run table alone.
    fn ctx() -> &'static Ctx<Closures> {
        let pool = Workers::new(&Topology::flat(1), false).unwrap();
        let pool: &'static Workers = Box::leak(Box::new(pool));
        let (tx, _) = bounded(1);
        let (io, _) = bounded(1);
        let slots = blaze_mem::Pool::new(super::MAX_RUNS, MemTag::Plane).unwrap();
        let start = std::time::Instant::now();
        Box::leak(Box::new(Ctx { tx, io, pool, start, slots, runner: Closures }))
    }

    fn live(id: u32, work: &Work) -> Live<Work> {
        Live {
            id: NodeId(id),
            request: RequestId(0),
            kind: TaskKind(1),
            key: None,
            critical: false,
            work: Arc::clone(work),
        }
    }

    /// What the throttle replaces when every subsystem is under its budget (a run starts as
    /// the plane runs it), and what it must be: per subsystem a count and a FIFO queue.
    #[derive(Default)]
    struct Model {
        running: [u32; MemTag::COUNT],
        queues: [std::collections::VecDeque<u64>; MemTag::COUNT],
    }

    impl Model {
        fn admit(&mut self, tag: MemTag, run: u64, over: bool) -> bool {
            let at = tag.index();
            if self.running[at] > 0 && (!self.queues[at].is_empty() || over) {
                self.queues[at].push_back(run);
                return false;
            }
            self.running[at] += 1;
            true
        }

        fn next(&mut self, over: impl Fn(MemTag) -> bool) -> Option<u64> {
            let tag = MemTag::ALL.into_iter().find(|&tag| {
                let at = tag.index();
                !self.queues[at].is_empty() && (self.running[at] == 0 || !over(tag))
            })?;
            self.running[tag.index()] += 1;
            self.queues[tag.index()].pop_front()
        }

        fn held(&self) -> usize {
            self.queues.iter().map(|it| it.len()).sum()
        }
    }

    /// Two hundred seeds of admits, ends and restarts across four subsystems, each over its
    /// budget or not at random: the throttle starts and holds exactly what the model does, in
    /// the same order; with every subsystem under budget it holds nothing.
    #[test]
    fn the_throttle_matches_a_queue_per_subsystem() {
        let tags = [MemTag::Infer, MemTag::Codegen, MemTag::Vfs, MemTag::Mir];
        for seed in 0..200u64 {
            let mut rng = SplitMix64::new(seed);
            let mut throttle = Throttle::new().unwrap();
            let mut model = Model::default();
            let mut over = [false; MemTag::COUNT];
            let mut started: Vec<MemTag> = Vec::new();
            // Each run's subsystem, by run.
            let mut tag_of = [MemTag::Other; 400];
            let quiet = seed % 4 == 0;
            for run in 0..400u64 {
                if !quiet && rng.one_in(8) {
                    let tag = tags[rng.index(4)];
                    over[tag.index()] = !over[tag.index()];
                }
                let is_over = |tag: MemTag| over[tag.index()];
                if started.is_empty() || !rng.one_in(3) {
                    let tag = tags[rng.index(4)];
                    tag_of[run as usize] = tag;
                    let now = throttle.admit(tag, run, is_over);
                    assert_eq!(now, model.admit(tag, run, is_over(tag)), "seed {seed} run {run}");
                    if now {
                        started.push(tag);
                    }
                } else {
                    let tag = started.swap_remove(rng.index(started.len()));
                    throttle.ended(tag);
                    model.running[tag.index()] -= 1;
                    loop {
                        let (got, want) = (throttle.next(is_over), model.next(is_over));
                        assert_eq!(got, want, "seed {seed} run {run}");
                        let Some(run) = got else { break };
                        started.push(tag_of[run as usize]);
                    }
                }
                assert_eq!(throttle.held(), model.held(), "seed {seed} run {run}");
                assert_eq!(throttle.running, model.running, "seed {seed} run {run}");
                if quiet {
                    assert_eq!(throttle.held(), 0, "under budget nothing waits");
                }
            }
        }
    }

    /// Every subsystem over its budget for good: each run held back still starts once those
    /// before it end, so nothing waits forever.
    #[test]
    fn a_subsystem_over_its_budget_still_runs_one_at_a_time() {
        let mut throttle = Throttle::new().unwrap();
        let over = |_: MemTag| true;
        assert!(throttle.admit(MemTag::Infer, 0, over));
        for run in 1..100 {
            assert!(!throttle.admit(MemTag::Infer, run, over));
        }
        assert!(throttle.admit(MemTag::Codegen, 100, over), "another subsystem is not held");
        for want in 1..100 {
            throttle.ended(MemTag::Infer);
            assert_eq!(throttle.next(over), Some(want));
            assert_eq!(throttle.next(over), None, "one at a time");
        }
        throttle.ended(MemTag::Infer);
        assert_eq!(throttle.held(), 0);
        assert_eq!(throttle.any(), None);
    }

    /// A `Done` naming a run whose slot was freed is a stale handle, caught before its slot is
    /// read: the slot it named may hold another run by now.
    #[test]
    #[should_panic(expected = "stale handle")]
    fn a_done_for_a_freed_run_is_a_stale_handle() {
        let ctx = ctx();
        let mut table = Table::new(ctx).unwrap();
        let built: Work = Arc::new(|| Ok(Did::built()));
        let first = table.take(ctx, live(1, &built), MemTag::Infer, Pool::Work, None).unwrap();
        assert_eq!(table.free(first), MemTag::Infer);
        let second = table.take(ctx, live(2, &built), MemTag::Infer, Pool::Work, None).unwrap();
        assert_ne!(first, second, "a reused slot is named anew");
        assert_eq!(table.ran(second).0.live().id, NodeId(2));
        table.free(first);
    }

    /// Once warm, a run's table slot taken, named and freed, with its admission check against
    /// its subsystem's budget and its end, calls the global allocator zero times.
    #[test]
    fn a_warm_run_and_its_admission_allocate_nothing() {
        let ctx = ctx();
        let mut table = Table::new(ctx).unwrap();
        let mut throttle = Throttle::new().unwrap();
        let built: Work = Arc::new(|| Ok(Did::built()));
        let mut cycle = |id: u32| {
            let at = table.take(ctx, live(id, &built), MemTag::Infer, Pool::Work, None).unwrap();
            assert!(throttle.admit(MemTag::Infer, at, blaze_mem::tag::over_budget));
            let (ran, pool, near) = table.ran(at);
            assert_eq!((ran.live().id, pool, near), (NodeId(id), Pool::Work, None));
            let tag = table.free(at);
            throttle.ended(tag);
            assert_eq!(throttle.next(blaze_mem::tag::over_budget), None);
        };
        for id in 0..2 * RUNS as u32 {
            cycle(id);
        }
        blaze_alloc_count::assert_at_most(0, "a warm run and its admission", || {
            for id in 0..10_000 {
                cycle(id);
            }
        });
    }
}
