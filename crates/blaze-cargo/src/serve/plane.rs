//! The plane (docs/v2/plan.md) as the daemon's loop steps it: every job's
//! tasks are nodes of one [`blaze_graph::Plane`], with single flight, fair share, critical path
//! and memory admission. The loop owns the plane and steps it with the driver's clock, so a run
//! under the simulation driver is a function of its seed.
//!
//! A node's task runs where its [`Task`] says: on a thread ([`Task::Thread`]: the process's
//! worker pool for [`Pool::Work`], an IO thread for [`Pool::Io`]) through a [`Runner`] (the
//! daemon's [`Threads`], or the simulation's [`Inline`]), its batch's [`Work`] called, or on
//! the loop itself ([`Task::Loop`]: user code, which runs as the driver's spawns). Every node is
//! given its cost and memory by the plane's [`Predict`] (the daemon's is a
//! `blaze_history::History`) from its kind and key, and each one's own run teaches it: a
//! thread task's time and what its work held at most, its fan-out to the pool included and
//! nothing any other task held ([`arena::account`]), a loop task's time from its start to its
//! end; a run that replays what was kept teaches nothing ([`Plane::replays`]).

#![forbid(unsafe_code)]

use std::{
    alloc::Global,
    cell::Cell,
    collections::{BTreeMap, VecDeque},
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::{Duration, Instant},
};

use blaze_graph::{
    Budget, Error, Plane as Graph, PlaneConfig, PlaneInput, PlaneOutput, RequestId, RequestStats,
    admission::{self, Predict, Prediction, TaskKind},
    arena,
    machine::{NodeId, Outcome, Pool},
    plane::{Key, NodeRef, Nodes, Unsettled},
};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::rng::SplitMix64 as Rng;
use blaze_protocol::{
    events::{Event, NodeState},
    now::{self, Fold, Now},
};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

/// The kinds of the daemon's own tasks, for their estimates and their subsystems' budgets.
pub use blaze_graph::admission::kinds;

/// The kinds of the loop's tasks, past every one of [`kinds`]: they hold none of the daemon's
/// memory, so only their times are learned.
pub mod loops {
    use blaze_graph::admission::TaskKind;

    /// A build script run.
    pub const SCRIPT_RUN: TaskKind = TaskKind(0x2_0001);
}

/// What a thread task came to: how the plane settles its node, and its value for the loop.
pub struct Did<T> {
    pub outcome: Outcome,
    pub value: T,
}

/// The thread tasks of one batch of nodes, one value per batch shared by its tasks, never one
/// per node. Every batch of a plane has the same type: the daemon's is a `match` over the
/// kinds of its jobs' steps.
pub trait Work: Send + Sync + 'static {
    /// What a task hands the loop.
    type Value: Send + 'static;
    /// What a task of a batch that reads is posted with; the default for one that does not.
    type Readers: Default + Send + 'static;

    /// Task `arg` run with the readers the loop posted it with: what it came to, or `None`
    /// when it gave way (a cycle of waits, a cancel, memory), to be posted again whole.
    fn run(&self, arg: u32, readers: Self::Readers) -> Option<Did<Self::Value>>;

    /// Task `arg`'s crate and unit, for its `task` span: a unit phase's, `None` for a task of
    /// no unit.
    fn subject(&self, arg: u32) -> Option<(String, String)> {
        let _ = arg;
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Task {
    /// The batch's [`Work::run`] called with `arg`.
    Thread { kind: TaskKind, arg: u32 },
    /// Run by the loop once the plane says so ([`Happened::Run`]); it ends with
    /// [`Plane::ended`].
    Loop { kind: TaskKind },
    /// Settled, or never added.
    Gone,
}

const _: () = assert!(std::mem::size_of::<Task>() == 12);

/// A batch of nodes being made ([`Plane::fresh`]): the ids reserved for it, the plane's
/// columns of it, and each node's task, filled by [`Plane::push`] and added whole by
/// [`Plane::add`].
pub struct Fresh {
    nodes: Nodes,
    tasks: Vec<Task>,
    keys: Vec<Option<Key>>,
    /// Each node's unit's source bytes ([`Plane::push_sized`]); 0 for a node not sized.
    sizes: Vec<u64>,
    /// Where each node's forecast came from ([`Plane::admit`]).
    bases: Vec<Basis>,
    /// Each node whose own measured runs predict more than the whole budget, and that peak:
    /// what the plane's refusal of it says it came from ([`Plane::admit`]). Few, usually none.
    over: Vec<(NodeId, u64)>,
    count: u32,
}

impl Fresh {
    pub fn first(&self) -> NodeId {
        self.nodes.first()
    }

    pub fn count(&self) -> u32 {
        self.count
    }
}

/// What the plane did, for the loop to act on, in order.
pub enum Happened<T> {
    /// What a thread task's expansions put, for the store's one owner to keep: before the
    /// task's [`Happened::Did`] and its node's settling.
    Kept { id: NodeId, puts: Vec<proc_macro_api::kept::Put> },
    /// A thread task's value, before its node settles.
    Did { id: NodeId, value: T },
    /// A thread task was posted: it starts.
    Started { id: NodeId },
    /// A thread task of a batch that reads to post now, `again` when it gave way before:
    /// the loop makes its readers and gives them to [`Plane::post`].
    Post { task: Posting, again: bool },
    /// A loop task to start now.
    Run { id: NodeId },
    /// A thread task panicked, before its node settles failed with the panic as its `why`.
    Panicked { id: NodeId, panic: Panic },
    /// A node settled: done (`why` none) or not (failed, skipped or cancelled, saying why);
    /// `skipped` names the failed node it was skipped behind.
    Settled { id: NodeId, ok: bool, why: Option<String>, skipped: Option<NodeId> },
    /// Every node of the request settled and it was closed; what it did, taken before the
    /// plane let it go.
    Finished { request: RequestId, ok: bool, stats: RequestStats },
}

/// A thread task that ended, or gave way.
pub struct Ran<T> {
    pub task: Posting,
    pub ended: Ended<T>,
    pub peak: u64,
    /// How long its last run took, the one that ended or gave way; none under the simulation,
    /// whose tasks take no time.
    pub ran_ns: u64,
    /// What the task's expansions left for the store's owner to keep.
    pub puts: Vec<proc_macro_api::kept::Put>,
}

pub enum Ended<T> {
    Did(Did<T>),
    Panicked(Panic),
    /// A task of a batch that reads gave way, on the thread group `group`: the loop posts it
    /// again with new readers. Others are posted again where they ran.
    GaveWay {
        group: Option<u32>,
    },
}

/// Runs thread tasks and hands back how each ended: the daemon's threads, or the simulation's
/// seeded inline runs.
pub enum Runner<W: Work> {
    Threads(Threads<W>),
    Inline(Inline<W>),
}

impl<W: Work> Runner<W> {
    /// `work` is the batch's, shared: posting costs one reference count, not a closure. A
    /// `critical` task is on its request's critical path: it goes to the fastest cores.
    pub fn post(&mut self, task: Posting, work: Arc<W>, readers: W::Readers) {
        match self {
            Runner::Threads(it) => it.post(task, work, readers),
            Runner::Inline(it) => it.post(task, work, readers),
        }
    }

    /// Moves the tasks that ended into `out`.
    pub fn take(&mut self, out: &mut Vec<Ran<W::Value>>) {
        match self {
            Runner::Threads(it) => it.take(out),
            Runner::Inline(it) => it.take(out),
        }
    }

    /// Whether a task ended and was not taken yet: the loop then polls the driver without
    /// waiting.
    pub fn ready(&self) -> bool {
        match self {
            Runner::Threads(it) => it.ready(),
            Runner::Inline(it) => it.ready(),
        }
    }

    /// How many tasks were posted and not taken.
    pub fn running(&self) -> usize {
        match self {
            Runner::Threads(it) => it.running(),
            Runner::Inline(it) => it.running(),
        }
    }

    /// Whether a task's end ends the wait of the loop's driver ([`Back`]): the loop then sets
    /// no deadline of its own to read the ends by. The simulation's tasks run on the loop.
    pub fn wakes(&self) -> bool {
        match self {
            Runner::Threads(it) => it.wakes(),
            Runner::Inline(_) => false,
        }
    }

    /// How many of [`Runner::running`] were posted to the process's worker pool ([`Pool::Work`]
    /// on [`Threads`]): each ends only as its pool job's `done` runs, so with the pool idle and
    /// none of its jobs parked, such a task can never end. The simulation's run on the loop.
    pub fn running_on_pool(&self) -> usize {
        match self {
            Runner::Threads(it) => it.on_pool,
            Runner::Inline(_) => 0,
        }
    }
}

/// A thread task as the plane posts it.
#[derive(Clone, Copy, Debug)]
pub struct Posting {
    pub id: NodeId,
    pub pool: Pool,
    pub near: Option<u32>,
    pub critical: bool,
    pub kind: TaskKind,
    pub arg: u32,
    /// Its batch reads the front end: it is posted with readers the loop made.
    pub reads: bool,
    /// The number of its request, the build its spans are of ([`blaze_profile::flight::building`]).
    pub build: u32,
    /// What memory admission did with it as it started ([`blaze_graph::plane::Admitted`]),
    /// for its `task` span: the bytes granted, and the times passed over for memory.
    pub granted: u64,
    pub passed: u32,
    /// Where its forecast came from ([`blaze_graph::forecast::Basis`]), for its `task` span.
    pub basis: Basis,
    /// Where it says the memory account it runs in as it starts, so the loop reads what it
    /// has realized while it runs ([`PlaneInput::Realized`]); `None` posted outside a plane.
    pub began: Option<&'static Began>,
}

/// A running task's memory account, as [`run_measured`] says it to its plane: read with
/// [`arena::peak_now`] while the task runs.
pub type Began = Sender<(NodeId, arena::Account)>;

/// A running task's account as the loop reads it, and the realized bytes it last told the plane.
#[derive(Clone, Copy, Debug)]
struct Realizing {
    account: arena::Account,
    told: u64,
}

/// A purge admission asked of the purger's thread ([`crate::raw::memory::Purger`]), not yet
/// heard done.
#[derive(Clone, Copy, Debug)]
struct Purging {
    /// Its ticket: done once [`crate::raw::memory::Purger::done`] reaches it.
    ticket: u64,
    /// A task ended or gave way since it was asked, so freed bytes its reading would hold.
    freed: bool,
}

impl Purging {
    fn asked() -> Purging {
        Purging { ticket: crate::raw::memory::purger().ask(), freed: false }
    }
}

/// The last footprint reading the plane was given ([`Plane::reading`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// The process's footprint, as the system counts it where it says.
    pub footprint: u64,
    /// What of it the allocators keep freed (`blaze_graph::arena::kept_of`): an estimate.
    pub kept: u64,
    /// The readings so far, and those of them taken right after a purge.
    pub readings: u64,
    pub purged: u64,
}

/// How much a running task's realized bytes grow before the plane is told again: 1/16 of its
/// grant, and at least 64 MiB. Each telling is a step of the plane; past its grant it is told
/// at once ([`Plane::realized`]).
const REALIZED_STEP: u64 = 64 << 20;

/// The daemon's: work tasks on the process's worker pool, IO tasks on threads of their own.
pub struct Threads<W: Work> {
    /// Every task's way back, shared by reference: a task posted counts no sender.
    done_tx: &'static Back<W::Value>,
    done: Receiver<Ran<W::Value>>,
    io: Sender<IoJob<W>>,
    running: usize,
    /// Of `running`, those posted to the worker pool.
    on_pool: usize,
}

/// A task's way back to the loop: its end queued, then the loop's wait ended where its driver
/// has a waker ([`blaze_core::Driver::waker`]), in that order, so the turn the wake starts
/// reads the end. With none (`wake` is `None`), the loop reads the queue at its own deadline
/// (`daemon::Woken::PlaneTasks`).
struct Back<V> {
    tx: Sender<Ran<V>>,
    wake: Option<blaze_core::Waker>,
}

impl<V> Back<V> {
    fn send(&self, ran: Ran<V>) {
        // The loop holds the receiver for as long as the process lives.
        _ = self.tx.send(ran);
        if let Some(wake) = &self.wake {
            wake.wake();
        }
    }
}

struct IoJob<W: Work> {
    task: Posting,
    work: Arc<W>,
    readers: W::Readers,
}

impl<W: Work> Threads<W> {
    /// `io` threads for IO tasks, for a plane that runs at most `io` IO tasks and `work` work
    /// tasks at once (its `PlaneConfig`): what `done` holds, so a task's send never waits and
    /// the channel allocates nothing after it is made.
    ///
    /// # Panics
    ///
    /// When a thread cannot be started, as [`std::thread::spawn`].
    ///
    /// `wake` is the waker of the driver the plane's loop waits in, which each task's end
    /// calls ([`Back`]); `None` for a loop that reads the ends at a deadline of its own.
    pub fn new(io: usize, work: usize, wake: Option<blaze_core::Waker>) -> Threads<W> {
        let (done_tx, done) = bounded(io.max(1) + work.max(1) + 1);
        // alloc: one sender per runner, kept for the process's life so no task clones it.
        let done_tx: &'static Back<W::Value> = Box::leak(Box::new(Back { tx: done_tx, wake }));
        let (io_tx, io_rx) = unbounded::<IoJob<W>>();
        for at in 0..io.max(1) {
            let io_rx = io_rx.clone();
            std::thread::Builder::new()
                .name(format!("blaze-serve-io-{at}"))
                .spawn(move || {
                    for job in io_rx {
                        // A task that gave way runs again on this thread: an IO task waits on
                        // the world, not on another task. One that reads goes back for new
                        // readers.
                        let mut readers = Some(job.readers);
                        let (ended, peak, ran_ns, puts) = loop {
                            match run_measured(
                                &*job.work,
                                job.task,
                                readers.take().unwrap_or_default(),
                            ) {
                                (Some(ended), peak, ran_ns, puts) => {
                                    break (ended, peak, ran_ns, puts);
                                }
                                (None, peak, ran_ns, puts) if job.task.reads => {
                                    break (Ended::GaveWay { group: None }, peak, ran_ns, puts);
                                }
                                (None, ..) => {}
                            }
                        };
                        done_tx.send(Ran { task: job.task, ended, peak, ran_ns, puts });
                    }
                })
                .unwrap_or_else(|why| panic!("an IO thread of the daemon's plane: {why}"));
        }
        Threads { done_tx, done, io: io_tx, running: 0, on_pool: 0 }
    }
}

/// Runs `task`'s `work` as a task of its kind ([`admission::run_as`]): in a frame tagged with
/// its subsystem, so what it allocates is charged to that subsystem and not to the heap outside
/// any task, and inside a measure. Says `None` when it gave way (its readers dropped with it),
/// else its value or its panic; the most it held; how long it ran; and what its expansions put
/// once it ended. A task that gave way
/// leaves its puts in its thread's buffer, handed on by that thread's next take (or as
/// orphans, if the thread ends), so none is lost or kept twice.
fn run_measured<W: Work>(
    work: &W,
    task: Posting,
    readers: W::Readers,
) -> (Option<Ended<W::Value>>, u64, u64, Vec<proc_macro_api::kept::Put>) {
    let Posting { id, arg, build, kind, granted, passed, basis, began, .. } = task;
    let _build = blaze_profile::flight::building(build);
    // Named for the profile: what the task is, what admission granted and how often it held it
    // back, and as it ends the most it held, so a forecast that serialized the plane is a
    // query (`task_memory` in crates/blaze-profile/sql/examples.sql), not a guess.
    // Interned here, on the thread the span opens on: a name is a token of its thread's.
    let (krate, unit) = match work.subject(arg) {
        Some((krate, unit)) => {
            (Some(blaze_profile::span::Name::of(&krate)), Some(blaze_profile::span::Name::of(&unit)))
        }
        None => (None, None),
    };
    let span = blaze_profile::span::Task {
        kind: task_name(kind),
        node: u64::from(id.0),
        granted,
        passed: u64::from(passed),
        basis: blaze_profile::span::Name::kept(basis.name()),
        krate,
        unit,
    }
    .open();
    let start = Instant::now();
    let (result, spent) = admission::run_as(kind, || {
        if let Some(began) = began {
            // The account `run_as` opened: what this task's work is charged to, read by the
            // loop as it runs. The loop holds the receiver for its life, so a send fails only
            // as the daemon ends.
            // alloc: a block of the channel per 31 tasks, reused
            _ = began.send((id, arena::current()));
        }
        let _job = InJob::enter();
        catch_unwind(AssertUnwindSafe(|| work.run(arg, readers)))
    });
    let ran_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let ended = matches!(result, Ok(Some(_)));
    span.ended(blaze_profile::span::TaskEnded {
        peak: ended.then_some(spent.peak),
        heap_allocated: ended.then_some(spent.heap.allocated),
        heap_freed: ended.then_some(spent.heap.freed),
    });
    let peak = spent.peak;
    match result {
        Ok(None) => (None, peak, ran_ns, Vec::new()),
        Ok(Some(did)) => (Some(Ended::Did(did)), peak, ran_ns, proc_macro_api::kept::take_puts()),
        Err(panic) => (
            Some(Ended::Panicked(panicked(&*panic))),
            peak,
            ran_ns,
            proc_macro_api::kept::take_puts(),
        ),
    }
}

/// A task kind as its `task` span names it: the daemon's own by name, a query kind by number,
/// each with the subsystem whose budget admits it (`unit bodies (Infer)`). Interned once a
/// kind: the kinds are few.
fn task_name(kind: TaskKind) -> blaze_profile::span::Name {
    let tag = admission::tag_of(kind);
    // alloc: a span name's text, interned (a kind's second run finds it)
    let text = match admission::name_of(kind) {
        Some(name) => format!("{name} ({tag:?})"),
        None => format!("query kind {:#x} ({tag:?})", kind.0),
    };
    blaze_profile::span::Name::of(&text)
}

/// A thread task's panic: what it was raised with, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Panic {
    /// `None` for a payload that is no text.
    pub message: Option<String>,
    /// The thread and `file:line:column` it was raised at, as [`record_panics`]' hook saw it.
    /// `None` where no hook records them, as under the simulation; a task's panic re-raised
    /// from a pool's thread (rayon's) is where it was first raised.
    pub at: Option<String>,
}

impl fmt::Display for Panic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("panicked")?;
        if let Some(at) = &self.at {
            write!(f, " at {at}")?;
        }
        match &self.message {
            Some(message) => write!(f, ": {message}"),
            None => f.write_str(" with no message"),
        }
    }
}

/// Whether a task of `kind` is a part of a program's build: admitted by that program's own
/// measure, and alone until it has one ([`Plane::push_program`]).
const fn of_program(kind: TaskKind) -> bool {
    matches!(kind, kinds::PROGRAM | kinds::PROGRAM_LINK)
}

/// Has every panic of the process recorded as it is raised, then printed as before: the one
/// record of panics, `blaze_profile::panics`, whose ring the task that catches one reads where
/// it was raised from ([`Panic::at`]) and the history writes as `panics` rows, each with its
/// stack and whether a job caught it ([`in_a_job`]). Once, as the daemon starts; a later call
/// does nothing, so the hook never wraps itself and records each panic once (tests call it
/// each).
pub fn record_panics() {
    blaze_profile::panics::record(in_a_job);
}

/// Has an allocation the allocator could not make panic, naming its size, instead of aborting
/// the process: the job that asked fails with that panic, as any job's panic does, and the
/// daemon goes on serving every other (`panics-fail-one-job`). In coord-98 one block of
/// 137,740 bytes for a parsed tree, which blaze-mem's used-up 160 KiB class could not give,
/// aborted the daemon, and the 1,627 tests of 21 crates were lost with it. Once, as the daemon
/// starts; a failure while a panic unwinds, or where unwinding is not allowed, still aborts.
pub fn record_alloc_failures() {
    std::alloc::set_alloc_error_hook(alloc_failed);
}

thread_local! {
    /// How many of [`run_measured`]'s tasks this thread is inside, off a fiber (an IO thread's,
    /// the simulation's): a panic there is caught and fails that task alone. A task on a fiber
    /// is not counted here: it can wait (its own parallel work does) and resume on another
    /// worker, so a count raised on one thread would be lowered on another. On a fiber,
    /// [`blaze_graph::sched::fiber::on_fiber`] answers instead.
    static IN_JOB: Cell<u32> = const { Cell::new(0) };
}

/// [`IN_JOB`] raised while a task off a fiber runs, lowered as it ends or unwinds, on the same
/// thread: off a fiber nothing moves the task to another.
struct InJob {
    counted: bool,
}

impl InJob {
    fn enter() -> Self {
        let counted = !blaze_graph::sched::fiber::on_fiber();
        if counted {
            IN_JOB.with(|depth| depth.set(depth.get() + 1));
        }
        Self { counted }
    }
}

impl Drop for InJob {
    fn drop(&mut self) {
        if self.counted {
            IN_JOB.with(|depth| depth.set(depth.get() - 1));
        }
    }
}

/// Whether a panic raised here is caught by a job's runner: a plane task's, or a pool job's on
/// its fiber (blaze-graph's hosts catch each job's panic). Elsewhere (the plane's loop, an IO
/// thread) nothing catches it, and the thread would end without a word.
pub(super) fn in_a_job() -> bool {
    IN_JOB.try_with(|depth| depth.get() > 0).unwrap_or(false)
        || blaze_graph::sched::fiber::on_fiber()
}

/// What [`record_alloc_failures`] has an allocation failure do: in a job, panic, failing that
/// job alone; elsewhere return, so std aborts as it would without the hook, rather than end a
/// thread the daemon needs without a word.
fn alloc_failed(layout: std::alloc::Layout) {
    if !in_a_job() {
        return;
    }
    panic!(
        "an allocation of {} bytes (aligned to {}) failed: the allocator had no memory to give",
        layout.size(),
        layout.align()
    );
}

/// The panic `payload` caught, with where the hook saw the newest one of its message raised.
/// An unwind the query engine raised with a typed payload (a query past its bound, out of
/// fuel) is named by its own text: it reaches no hook, and has no `panic!` text.
// dyn: a panic payload, read once per panicked task
pub(crate) fn panicked(payload: &(dyn std::any::Any + Send)) -> Panic {
    let message = stdx::panic_text(payload)
        .map(str::to_owned) // alloc: a panic, once
        .or_else(|| blaze_query::db::unwind_text(payload));
    let at = blaze_profile::panics::claim(message.as_deref());
    Panic { message, at }
}

fn post_work<W: Work>(
    task: Posting,
    work: Arc<W>,
    readers: W::Readers,
    done: &'static Back<W::Value>,
) {
    let pool = blaze_graph::exec::shared();
    // The batch's work comes back with the result, for a task that gave way to be posted again
    // without counting another reference.
    let run = move || (run_measured(&*work, task, readers), work);
    let then = move |result: std::thread::Result<_>| match result {
        Ok(((Some(ended), peak, ran_ns, puts), _)) => {
            done.send(Ran { task, ended, peak, ran_ns, puts })
        }
        Ok(((None, peak, ran_ns, puts), _)) if task.reads => {
            let ended = Ended::GaveWay { group: blaze_graph::pool::group() };
            done.send(Ran { task, ended, peak, ran_ns, puts });
        }
        // Gave way: posted again behind what is ready on the group it ran on.
        Ok(((None, ..), work)) => {
            post_work(
                Posting { near: blaze_graph::pool::group(), ..task },
                work,
                W::Readers::default(),
                done,
            );
        }
        // A panic outside the measure: its puts stay in the thread's buffer.
        Err(panic) => {
            done.send(Ran {
                task,
                ended: Ended::Panicked(panicked(&*panic)),
                peak: 0,
                ran_ns: 0,
                puts: Vec::new(),
            });
        }
    };
    match task.critical {
        true => pool.submit_critical(task.near, run, then),
        false => pool.submit(task.near, run, then),
    }
}

impl<W: Work> Threads<W> {
    fn post(&mut self, task: Posting, work: Arc<W>, readers: W::Readers) {
        self.running += 1;
        match task.pool {
            Pool::Work => {
                self.on_pool += 1;
                post_work(task, work, readers, self.done_tx);
            }
            Pool::Io => {
                let job = IoJob { task, work, readers };
                // The IO threads hold the receiver for as long as the process lives.
                _ = self.io.send(job);
            }
        }
    }

    fn take(&mut self, out: &mut Vec<Ran<W::Value>>) {
        while let Ok(ran) = self.done.try_recv() {
            self.running -= 1;
            if matches!(ran.task.pool, Pool::Work) {
                self.on_pool -= 1;
            }
            out.push(ran);
        }
    }

    fn ready(&self) -> bool {
        !self.done.is_empty()
    }

    fn running(&self) -> usize {
        self.running
    }

    /// Whether a task's end ends the loop's wait ([`Back`]).
    fn wakes(&self) -> bool {
        self.done_tx.wake.is_some()
    }
}

/// The simulation's: tasks run on the loop's thread when it takes them, a seeded part of what
/// is posted each time in a seeded order, so a run is a function of its seed. The readers of
/// the tasks it holds queued stay alive until they run, so a write the loop asks for meanwhile
/// waits for them as it would for a worker's.
pub struct Inline<W: Work> {
    rng: Rng,
    queue: VecDeque<Posted<W>>,
    /// Reused by every take.
    posted: Vec<Posted<W>>,
    /// Kinds whose tasks stay queued, still running, until released ([`Inline::hold`]).
    held: Vec<TaskKind>,
}

type Posted<W> = (Posting, Arc<W>, <W as Work>::Readers);

impl<W: Work> Inline<W> {
    pub fn new(seed: u64) -> Inline<W> {
        // alloc: the simulation's runner, once per daemon; `held` stays empty unless a test holds a kind
        Inline { rng: Rng::new(seed), queue: VecDeque::new(), posted: Vec::new(), held: Vec::new() }
    }

    /// Every task of `kind`, posted or to be, stays queued and counted running until
    /// [`Inline::release`]: a task that runs long on a worker (a large front end's store), so a
    /// test can ask something while it runs, which an inline run, over as it is taken, never
    /// lets happen. Nothing else changes: with no kind held, a take is as it was, seed for seed.
    pub fn hold(&mut self, kind: TaskKind) {
        self.held.push(kind);
    }

    /// The tasks of `kind` run again from the next take ([`Inline::hold`]).
    pub fn release(&mut self, kind: TaskKind) {
        self.held.retain(|it| *it != kind);
    }

    fn post(&mut self, task: Posting, work: Arc<W>, readers: W::Readers) {
        self.queue.push_back((task, work, readers));
    }

    fn take(&mut self, out: &mut Vec<Ran<W::Value>>) {
        if self.queue.is_empty() {
            return;
        }
        let mut posted = std::mem::take(&mut self.posted);
        posted.extend(self.queue.drain(..));
        if !self.held.is_empty() {
            let mut at = 0;
            while at < posted.len() {
                if self.held.contains(&posted[at].0.kind) {
                    self.queue.push_back(posted.remove(at));
                } else {
                    at += 1;
                }
            }
            if posted.is_empty() {
                self.posted = posted;
                return;
            }
        }
        self.rng.shuffle(&mut posted);
        let now = 1 + self.rng.below(posted.len() as u64) as usize;
        self.queue.extend(posted.drain(now..));
        for (task, work, readers) in posted.drain(..) {
            match run_measured(&*work, task, readers) {
                (Some(ended), peak, _, puts) => {
                    out.push(Ran { task, ended, peak, ran_ns: 0, puts })
                }
                (None, peak, _, puts) if task.reads => {
                    out.push(Ran {
                        task,
                        ended: Ended::GaveWay { group: None },
                        peak,
                        ran_ns: 0,
                        puts,
                    });
                }
                (None, ..) => self.queue.push_back((task, work, W::Readers::default())),
            }
        }
        self.posted = posted;
    }

    fn ready(&self) -> bool {
        self.queue.iter().any(|(task, ..)| !self.held.contains(&task.kind))
    }

    fn running(&self) -> usize {
        self.queue.len()
    }
}

/// Nodes added together, ids `first..first + tasks.len()` (reserved at once, [`Plane::fresh`]):
/// each one's task by offset, 12 bytes a node, and the one work their thread tasks share. Let
/// go once every node of it settled.
struct Batch<W> {
    tasks: Vec<Task>,
    /// Each node's key, read once when it ends, for what its run teaches.
    keys: Vec<Option<Key>>,
    /// Each node's unit's source bytes, 0 for none ([`Plane::push_sized`]): what its peak
    /// teaches its kind's bytes held per KiB of source.
    sizes: Vec<u64>,
    /// [`Fresh::bases`]: where each node's forecast came from, for its spans.
    bases: Vec<Basis>,
    /// [`Fresh::over`]'s nodes that have not run.
    over: Vec<(NodeId, u64)>,
    live: u32,
    work: Option<Arc<W>>,
    reads: bool,
    /// Its request's number, which its tasks' spans carry.
    request: u32,
}

/// The plane of thread tasks whose batches are each a `W`, given their costs and memory by `P`.
pub struct Plane<W: Work, P: Predict> {
    graph: Graph,
    predict: P,
    runner: Runner<W>,
    /// By first id: a node's batch is the last one starting at or below it, O(log batches).
    // BTreeMap: range queries by node id.
    batches: BTreeMap<u32, Batch<W>>,
    next_node: u32,
    next_request: u32,
    happened: Vec<Happened<W::Value>>,
    ran: Vec<Ran<W::Value>>,
    said: Vec<PlaneOutput>,
    /// The time of the input being stepped.
    now_ns: u64,
    /// When each loop task that runs started.
    loops: BTreeMap<NodeId, u64>,
    /// Each node whose task replays what was kept, so its run teaches nothing
    /// ([`Plane::replays`]), until it ends or settles.
    replays: HashSet<NodeId, Global>,
    /// By kind, the bytes a unit's phase held per KiB of its source, learned as sized nodes
    /// end ([`Plane::push_sized`]) by [`admission::Estimate`]'s rule: a high quantile of the
    /// kind's recent ratios. Not kept in the image: a daemon starts from
    /// [`blaze_graph::forecast::UNSEEN_PER_KIB`] and learns again in its first build.
    per_kib: HashMap<TaskKind, admission::Estimate, Global>, // alloc: the plane's, an entry a kind
    /// What each codegen task that ended held at most, by node, until the loop says how many
    /// instances its walk reached ([`Plane::walked`]); and the bytes a walk held per instance
    /// reached lately, by [`admission::Estimate`]'s rule: what a unit's codegen is forecast
    /// by once its dependencies' walks ended ([`Plane::forecast_instances`]).
    walk_peaks: HashMap<NodeId, u64, Global>, // alloc: the plane's, an entry a codegen task ended and not yet told
    per_instance: admission::Estimate,
    /// Each basis's forecasts against the peaks their tasks measured ([`Plane::forecast_check`]).
    check: blaze_graph::forecast::Check,
    /// Each node ready and not started: its `admission` span, open from the step it became
    /// ready, and that step's time ([`Plane::admitting`]).
    admitting: HashMap<NodeId, Admitting, Global>, // alloc: the plane's, an entry a ready node
    /// What the system last said of memory ([`Plane::pressed`]), and the cap on the budget it
    /// makes as last given to the plane ([`PlaneInput::Cap`]).
    pressure: blaze_graph::pressure::Pressured,
    capped: Option<u64>,
    /// Where each thread task says its memory account as it starts, and the accounts of those
    /// that run, by node ([`Plane::measure`]).
    began_tx: &'static Began,
    began: Receiver<(NodeId, arena::Account)>,
    realizing: HashMap<NodeId, Realizing, Global>, // alloc: the plane's, an entry a running task
    /// The plane asked a purge since the loop last did one ([`Graph::purge_due`]), or the
    /// give-back rule did ([`Plane::ask_purge`]).
    purge_due: bool,
    /// How many footprint readings the plane was given ([`Plane::measure`]), and how many of
    /// them right after a purge: what a give-back is measured by ([`Plane::reading`]).
    readings: u64,
    purged_readings: u64,
    /// The give-back rule asked a purge ([`Plane::ask_purge`]) and no reading marked purged
    /// came since: a purge under way that a task's end made stale is asked again for it.
    purge_wanted: bool,
    /// What this daemon could give back now and keep its floor, as its give-back last said
    /// ([`Plane::set_gives`]): what the machine's ledger records for the other daemons
    /// (`blaze_graph::ledger::Record::gives`). 0 while it has work.
    gives: u64,
    /// The thread tasks posted and not yet taken whose kind is codegen's
    /// (`admission::tag_of`: a library's, a unit's or a program's codegen, a reach): each may
    /// walk a program over codegen's kept walks ([`Plane::walks_in_flight`]).
    walking: usize,
    /// The purge asked of the purger's thread and not yet done ([`Purging`]).
    purging: Option<Purging>,
    /// Each request's live model, fed every record the plane says of it ([`Folds`]).
    folds: Folds,
}

/// Each plane request's live model (`blaze_protocol::now`, docs/v2/plan/design/build-view.md):
/// a [`Fold`] made as the request opens ([`Plane::open`]), fed every record the plane says of
/// it ([`PlaneOutput::Said`]), looked at once a second while its job runs
/// (`super::job::doing`) and dropped as its job ends ([`Plane::forget`]). The same records,
/// read once through [`now::Records`], also go to the explorer's task table when the daemon
/// publishes one ([`Plane::publish_tasks`]): every request's, until each task settles.
pub struct Folds {
    // alloc: the plane's, an entry a request whose job has not ended
    by: HashMap<RequestId, Live, Global>,
    /// What reads the plane's records keeps from one to the next.
    records: now::Records,
    /// The daemon's tasks for the explorer (`blaze_explore::Tasks`); a daemon with no explorer
    /// (a simulation's) has none.
    #[cfg(feature = "native")]
    tasks: Option<std::sync::Arc<blaze_explore::Tasks>>,
}

/// One request's fold and when it opened on the plane's clock: a record's `at_ns` is the
/// plane's clock, the fold's is since the request began.
struct Live {
    fold: Fold,
    began_ns: u64,
}

impl Folds {
    fn new() -> Folds {
        Folds {
            by: HashMap::new_in(Global), // alloc: the plane's, once
            records: now::Records::new(),
            #[cfg(feature = "native")]
            tasks: None,
        }
    }

    /// One record of `request`, at `at_ns` of the plane's clock, to its fold and the explorer's
    /// task table. A request with no fold is one whose job ended ([`Plane::forget`]): what the
    /// plane still says of it (its cancel's moves) is no live view's, and its fold has none of
    /// it; the task table still takes it, so its tasks settle there.
    fn said(&mut self, request: RequestId, at_ns: u64, event: &Event) {
        if !matches!(event, Event::Node { .. } | Event::Nodes { .. } | Event::Moved { .. }) {
            return;
        }
        let mut live = self.by.get_mut(&request);
        if let Some(live) = live.as_deref_mut() {
            live.fold.advance_clock(at_ns.saturating_sub(live.began_ns));
        }
        #[cfg(feature = "native")]
        let mut table = self.tasks.as_deref().map(blaze_explore::Tasks::lock);
        self.records.read(event, |record| {
            if let Some(live) = live.as_deref_mut() {
                live.fold.take_record(record);
            }
            #[cfg(feature = "native")]
            if let Some(table) = table.as_mut() {
                table.said(request.0, at_ns, record);
            }
        });
    }

    /// `request`'s build as of `at` on the plane's clock, with what one look found; `None` when
    /// the plane holds no fold of it (no such request was opened, or its job ended).
    pub fn look(&mut self, request: RequestId, at: Duration, look: &now::Look<'_>) -> Option<Now> {
        let live = self.by.get_mut(&request)?;
        live.fold.advance_clock(ns(at).saturating_sub(live.began_ns));
        Some(live.fold.look(look))
    }
}

/// A ready node's wait for admission, as its profile span says it ([`Plane::admitting`]).
struct Admitting {
    span: blaze_profile::span::Detached<blaze_profile::span::Admitted>,
    since_ns: u64,
    /// Memory held it back in a step of its wait ([`Plane::held_for_memory`]): what its span
    /// says it waited for.
    memory: bool,
}

/// Where a node's forecast came from: the one rule's ([`blaze_graph::forecast`]).
pub use blaze_graph::forecast::Basis;

/// `why` a node failed, saying where its predicted peak came from when the plane refused it for
/// memory by `over`, its own measured runs' peak ([`Plane::take_over`]).
fn refused_by_own_runs(why: String, over: Option<u64>) -> String {
    match over {
        // alloc: the message of a node refused for memory, once
        Some(peak) => format!(
            "{why}; {:.2} GB is what this unit's own last runs held at most (its key's record in the \
             daemon's history), not a guess from its kind or size",
            peak as f64 / f64::from(1 << 30)
        ),
        None => why,
    }
}

pub(crate) fn ns(at: Duration) -> u64 {
    u64::try_from(at.as_nanos()).unwrap_or(u64::MAX)
}

impl<W: Work, P: Predict> Plane<W, P> {
    pub fn new(config: PlaneConfig, predict: P, runner: Runner<W>) -> Result<Plane<W, P>, Error> {
        let (began_tx, began) = unbounded();
        // alloc: one sender per plane, kept for the process's life so no task clones it
        let began_tx: &'static Began = Box::leak(Box::new(began_tx));
        Ok(Plane {
            graph: Graph::new(config)?,
            predict,
            runner,
            batches: BTreeMap::new(),
            next_node: 0,
            next_request: 0,
            happened: Vec::new(),
            ran: Vec::new(),
            said: Vec::new(),
            now_ns: 0,
            loops: BTreeMap::new(),
            replays: HashSet::new_in(Global), // alloc: the plane's, once
            per_kib: HashMap::new_in(Global), // alloc: the plane's, once
            walk_peaks: HashMap::new_in(Global), // alloc: the plane's, once
            per_instance: admission::Estimate::default(),
            check: blaze_graph::forecast::Check::default(),
            admitting: HashMap::new_in(Global), // alloc: the plane's, once
            pressure: blaze_graph::pressure::Pressured::default(),
            capped: None,
            began_tx,
            began,
            realizing: HashMap::new_in(Global), // alloc: the plane's, once
            purge_due: false,
            readings: 0,
            purged_readings: 0,
            purge_wanted: false,
            gives: 0,
            walking: 0,
            purging: None,
            folds: Folds::new(),
        })
    }

    /// The system says memory is short at `at` (docs/memory-budget.md section 7, "Pressure"):
    /// from the next [`Plane::poll`] until [`blaze_graph::pressure::HOLD_NS`] after the last
    /// such event, the budget the plane admits against is held at what the process holds now.
    /// Whether the front ends are to be asked for a release for it
    /// ([`blaze_graph::pressure::Pressured::release_due`]).
    pub fn pressed(&mut self, at: Duration) -> bool {
        self.pressure = self.pressure.pressed(ns(at), arena::own());
        self.pressure.release_due(ns(at))
    }

    /// What admission counts the daemon at (its footprint as the plane last heard it and its
    /// running tasks' grants not yet in it, at least what is granted), the budget it admits
    /// against (its allowance; `None` with no budget), and the pressure cap on it while there is
    /// one.
    pub fn memory(&self) -> (u64, Option<u64>, Option<u64>) {
        let budget = self.graph.config().budget.as_ref().map(|it| it.bytes);
        (self.graph.charged(), budget, self.graph.cap())
    }

    /// How the measured peaks of the tasks that ended compared with their grants, and the
    /// purges admission asked ([`blaze_graph::footprint::Grades`]).
    pub fn grades(&self) -> blaze_graph::footprint::Grades {
        self.graph.grades()
    }

    /// The requests (by number) with a ready task held back for memory alone now
    /// ([`blaze_graph::Plane::waits_for_memory`]), into `into`, ascending, each once.
    pub fn held_back(&self, into: &mut Vec<u32>) {
        for &id in self.admitting.keys() {
            if self.graph.waits_for_memory(id)
                && let Some((batch, _)) = self.task(id)
            {
                into.push(batch.request); // alloc: none once it has held this many.
            }
        }
        into.sort_unstable();
        into.dedup();
    }

    /// Each running task granted memory, by its node and name, to `each`: what a task held back
    /// for memory waits on. A task several requests share comes once each.
    pub fn granted_running<'a>(&'a self, mut each: impl FnMut(NodeId, &'a str)) {
        self.graph.unsettled(|it| {
            if matches!(it.state, NodeState::Running)
                && self.graph.admitted(it.id).is_some_and(|admitted| admitted.granted > 0)
            {
                each(it.id, it.name);
            }
        });
    }

    pub fn budget(&self) -> Option<&Budget> {
        self.graph.config().budget.as_ref()
    }

    /// What the plane predicts by, for the daemon's image.
    pub fn predict(&self) -> &P {
        &self.predict
    }

    pub fn stats(&self, request: RequestId) -> Option<RequestStats> {
        self.graph.stats(request)
    }

    /// Whether `request` is known and not finished: one [`Plane::cancel`] takes.
    pub fn is_open(&self, request: RequestId) -> bool {
        self.graph.is_open(request)
    }

    /// A batch of `count` nodes with consecutive ids no node of this plane has, its columns
    /// sized for `name_bytes` of names and `deps` deps in all.
    ///
    /// # Panics
    ///
    /// When the ids wrapped around onto a batch that has not settled, after 2^32 nodes.
    pub fn fresh(&mut self, count: u32, name_bytes: usize, deps: usize) -> Fresh {
        let first = self.reserve(count);
        let nodes = Nodes::with_capacity(first, count as usize, name_bytes, deps);
        let (tasks, keys, sizes) = (
            Vec::with_capacity(count as usize),
            Vec::with_capacity(count as usize),
            Vec::with_capacity(count as usize),
        );
        let bases = Vec::with_capacity(count as usize); // alloc: a batch's column, once
        Fresh { nodes, tasks, keys, sizes, bases, over: Vec::new(), count }
    }

    /// The next node of `fresh`, its id; its memory raised to what its kind and key are
    /// predicted to hold, its cost the predicted one where there is one.
    ///
    /// # Panics
    ///
    /// When `fresh` has all the nodes it was made for, or `task` is [`Task::Gone`].
    pub fn push(&self, fresh: &mut Fresh, node: NodeRef<'_>, task: Task) -> NodeId {
        let learn = node.key;
        self.push_predicted(fresh, node, task, learn)
    }

    /// The next node of `fresh`, as [`Plane::push`], but with the memory and cost `node` says
    /// and no prediction: for a task that runs because memory is short (a give-back,
    /// `kinds::GIVE_BACK`), which a predicted peak would hold back behind the very tasks it
    /// makes room for.
    ///
    /// # Panics
    ///
    /// As [`Plane::push`].
    pub fn push_free(&self, fresh: &mut Fresh, node: NodeRef<'_>, task: Task) -> NodeId {
        assert!(
            (fresh.tasks.len() as u32) < fresh.count,
            "a batch of {} nodes given another",
            fresh.count
        );
        assert!(task != Task::Gone, "node {} is pushed with no task", node.name);
        fresh.tasks.push(task);
        fresh.keys.push(node.key);
        fresh.sizes.push(0);
        fresh.bases.push(Basis::Given);
        fresh.nodes.push(node)
    }

    /// The next node of `fresh`, as [`Plane::push`], but predicted and taught by `learn`, a
    /// key of the predictor's alone: the node has no key of the plane's, so nothing
    /// single-flights or memoizes it by `learn`. A unit's phase is so given the cost of its own
    /// last runs, not its kind's mean, and the plane starts the longest chain first by what
    /// each node takes (docs/v2/plan/design/scheduler.md §6, step 1).
    ///
    /// Its memory is its key's own record once a run of it was measured: what that run's work
    /// held at most, the jobs it handed to the pool (a library's codegen, the front end's
    /// fan-out) included ([`arena::account`]). Before accounts a task learned only what its own
    /// thread held, so its memory was floored at its kind's peak, the largest of any task of
    /// the kind lately, and every unit predicted the biggest one's. Under a budget, a guess (its
    /// kind's peak, its key never measured) is never more than the whole budget: such a node
    /// runs alone, never refused for what other tasks held ([`admitted`]).
    ///
    /// # Panics
    ///
    /// As [`Plane::push`], and when `node` has a key.
    pub fn push_learned(
        &self,
        fresh: &mut Fresh,
        node: NodeRef<'_>,
        task: Task,
        learn: Key,
    ) -> NodeId {
        assert!(
            node.key.is_none(),
            "node {} is given a key of the plane's beside the one it learns by",
            node.name
        );
        self.push_predicted(fresh, node, task, Some(learn))
    }

    /// The next node of `fresh`, its memory and cost predicted by `learn` (or its kind alone),
    /// which its run then teaches ([`Plane::key`]).
    fn push_predicted(
        &self,
        fresh: &mut Fresh,
        mut node: NodeRef<'_>,
        task: Task,
        learn: Option<Key>,
    ) -> NodeId {
        assert!(
            (fresh.tasks.len() as u32) < fresh.count,
            "a batch of {} nodes given another",
            fresh.count
        );
        let kind = match task {
            Task::Thread { kind, .. } | Task::Loop { kind } => kind,
            Task::Gone => panic!("node {} is pushed with no task", node.name),
        };
        let cost = self.admit(fresh, &mut node, kind, learn, 0);
        if let Some(cost) = cost {
            node.cost = cost;
        }
        fresh.tasks.push(task);
        fresh.keys.push(learn);
        fresh.sizes.push(0);
        fresh.nodes.push(node)
    }

    /// The next node of `fresh`, a phase of a unit of `source` bytes of Rust source, predicted
    /// and taught by `learn` as [`Plane::push_learned`] is, but sized by the unit itself, not
    /// floored at its kind's peak.
    ///
    /// A kind's peak is the most any task of it held lately (it rises at once and comes down
    /// by halves only after [`admission::WINDOW`] smaller ones in a row): one large crate's
    /// codegen (15.78 GB of blaze's own) made every unit's codegen predict that much,
    /// `ahash`'s and a build script's alike, and a test build with 31 GB held refused ten
    /// targets for it (memadmit). A unit is instead predicted its source's KiB times what its
    /// kind held per KiB lately ([`Plane::per_kib`]; [`blaze_graph::forecast::UNSEEN_PER_KIB`]
    /// until measured), at least [`blaze_graph::forecast::GUESS_FLOOR`], at most the kind's
    /// peak and one work thread's share ([`Plane::admit`]). One whose key has a record is
    /// forecast that record, as it is: a record under its size's guess is the better measure.
    /// `source` 0 (not read) predicts as [`Plane::push_learned`].
    ///
    /// # Panics
    ///
    /// As [`Plane::push_learned`].
    pub fn push_sized(
        &self,
        fresh: &mut Fresh,
        mut node: NodeRef<'_>,
        task: Task,
        learn: Key,
        source: u64,
    ) -> NodeId {
        if source == 0 {
            return self.push_learned(fresh, node, task, learn);
        }
        assert!(
            node.key.is_none(),
            "node {} is given a key of the plane's beside the one it learns by",
            node.name
        );
        assert!(
            (fresh.tasks.len() as u32) < fresh.count,
            "a batch of {} nodes given another",
            fresh.count
        );
        let kind = match task {
            Task::Thread { kind, .. } | Task::Loop { kind } => kind,
            Task::Gone => panic!("node {} is pushed with no task", node.name),
        };
        let cost = self.admit(fresh, &mut node, kind, Some(learn), source);
        if let Some(cost) = cost {
            node.cost = cost;
        }
        fresh.tasks.push(task);
        fresh.keys.push(Some(learn));
        fresh.sizes.push(source);
        fresh.nodes.push(node)
    }

    /// Gives `node`, the next of `fresh`, the memory it asks of the plane: its forecast by the
    /// one rule ([`blaze_graph::forecast::forecast`]) from what is known of it: its learn key's
    /// own record, else a model of its inputs (`source` bytes of its unit's source times its
    /// kind's measured bytes per KiB, or its kind's measured peak), a guess held to one work
    /// thread's share of the budget, so it never takes the whole budget and runs alone. Its
    /// basis goes in `fresh` for its spans. One whose own record says more than the whole
    /// budget keeps that, for the plane to refuse it as it becomes ready, and is noted so the
    /// refusal says where the number came from. Its predicted cost, for the caller.
    ///
    /// History: a guess over the budget was the whole budget and ran alone (the smoke's cold
    /// build, 2026-11-12, budget 205,049,909,248 bytes: `program (Codegen)` was granted all of
    /// it for a peak of 579,740,555, its link 1,519 times its peak, and each ran alone); a
    /// unit's own record under what its size guessed gave way to the guess; a first guess was
    /// floored at a share of half the budget; and a correction factor (deleted 2026-11-12)
    /// multiplied every forecast to 66 GB and stalled the build. A forecast decides how many
    /// tasks run at once; what keeps the process under its limit is the ceiling on the
    /// footprint (docs/memory-budget.md), and a running task's grant rises as it realizes more
    /// ([`PlaneInput::Realized`]), so a forecast too low costs a wait at the ceiling's gate,
    /// never the machine.
    fn admit(
        &self,
        fresh: &mut Fresh,
        node: &mut NodeRef<'_>,
        kind: TaskKind,
        learn: Option<Key>,
        source: u64,
    ) -> Option<u64> {
        let (forecast, cost) = self.forecast(kind, learn, source, node.memory);
        let budget = self.budget().map(|it| it.bytes);
        node.memory = forecast.bytes;
        fresh.bases.push(forecast.basis);
        if budget.is_some_and(|budget| node.memory > budget) {
            let id = NodeId(fresh.nodes.first().0 + fresh.nodes.len() as u32);
            fresh.over.push((id, node.memory));
        }
        cost
    }

    /// The forecast of a task of `kind` taught by `learn`, of a unit of `source` bytes of
    /// source (0: not sized), whose node asks `least` at least ([`Plane::admit`]), and its
    /// predicted cost.
    fn forecast(
        &self,
        kind: TaskKind,
        learn: Option<Key>,
        source: u64,
        least: u64,
    ) -> (blaze_graph::forecast::Forecast, Option<u64>) {
        // One ask of the predictor a node: with no record of its key, what it predicts is its
        // kind's peak ([`Predict::predict`]), so the kind's is not asked again.
        let Prediction { cost, peak } = self.predict.predict(kind, learn);
        let own = learn.is_some_and(|key| self.predict.measured(kind, Some(key)));
        let kind_peak = (!own && self.predict.measured(kind, None)).then_some(peak);
        let known = blaze_graph::forecast::Known {
            own: own.then_some(peak),
            source,
            per_kib: self.per_kib(kind),
            kind: kind_peak,
            least,
        };
        let budget = self.budget().map(|it| it.bytes);
        let share = blaze_graph::forecast::share(budget, self.graph.config().work);
        (blaze_graph::forecast::forecast(&known, share), cost)
    }

    /// How each basis's forecasts compared with the peaks their tasks then measured
    /// ([`blaze_graph::forecast::Check`]): the forecast's model, checked.
    pub fn forecast_check(&self) -> blaze_graph::forecast::Check {
        self.check
    }

    /// The running thread task whose memory account reached the most, its bytes, and how
    /// many thread tasks with an account run: what the ceiling's last step decides on
    /// (`blaze_graph::footprint::past_ceiling`). Ties go to the older node, so the answer
    /// hangs on no map's order.
    pub fn biggest_running(&self) -> Option<(NodeId, u64, usize)> {
        let mut most: Option<(NodeId, u64)> = None;
        let mut count = 0;
        for (&id, seen) in self.realizing.iter() {
            let Some(bytes) = arena::peak_now(seen.account) else { continue };
            count += 1;
            if most.is_none_or(|(at, held)| bytes > held || (bytes == held && id < at)) {
                most = Some((id, bytes));
            }
        }
        most.map(|(id, bytes)| (id, bytes, count))
    }

    /// Node `id` settled, and its unit's codegen walk reached `reached` instances (`None`
    /// for a node that is no codegen's, or whose walk did not end): what its task held per
    /// instance teaches the forecast of the walks to come. Called for every unit node that
    /// settles, so the peak kept for it is let go either way.
    pub fn walked(&mut self, id: NodeId, reached: Option<u64>) {
        let Some(peak) = self.walk_peaks.remove(&id) else { return };
        if let Some(reached) = reached.filter(|it| *it > 0) {
            self.per_instance.observe(peak / reached);
        }
    }

    /// Node `id`, a unit's codegen not started yet, is forecast by `instances`, the most a
    /// walk of a unit it depends on reached: that many times what a walk held per instance
    /// lately ([`PlaneInput::Forecast`], which only raises). Nothing until a walk of this
    /// daemon was measured.
    pub fn forecast_instances(&mut self, at: Duration, id: NodeId, instances: u64) {
        if self.per_instance.seen == 0 {
            return;
        }
        let bytes = instances.saturating_mul(self.per_instance.predicted);
        self.step(PlaneInput::Forecast { at_ns: ns(at), id, bytes })
            .unwrap_or_else(|error| panic!("the plane refused the forecast of node {id}: {error}"));
    }

    /// The kind of node `id`'s task, while the plane holds it and it has not settled.
    pub fn kind_of(&self, id: NodeId) -> Option<TaskKind> {
        match self.task(id)?.1 {
            Task::Thread { kind, .. } | Task::Loop { kind } => Some(kind),
            Task::Gone => None,
        }
    }

    /// The peak node `id` was refused by, its own measured runs' over the whole budget
    /// ([`Plane::admit`]), if it was noted and has not run; taken.
    fn take_over(&mut self, id: NodeId) -> Option<u64> {
        let (_, batch) = self.batches.range_mut(..=id.0).next_back()?;
        let at = batch.over.iter().position(|(it, _)| *it == id)?;
        Some(batch.over.swap_remove(at).1)
    }

    /// The bytes a sized node of `kind` held per KiB of its source lately, once one ended:
    /// what [`Plane::push_sized`] predicts by.
    pub fn per_kib(&self, kind: TaskKind) -> Option<u64> {
        self.per_kib.get(&kind).filter(|it| it.seen > 0).map(|it| it.predicted)
    }

    /// The bytes of what the process holds besides the tasks granted memory that held back a
    /// task for memory as the plane last dispatched ([`blaze_graph::Plane::short`]): 0 when
    /// none was, or only granted tasks did. The loop gives back for it, cheapest first
    /// (`blaze_graph::giveback`).
    pub fn short(&self) -> u64 {
        self.graph.short()
    }

    /// The machine ledger's word on what this daemon may grant now, taken at `at` while the
    /// daemon holds the ledger's lock ([`PlaneInput::Allowance`], `super::machine`): what now
    /// fits starts, and until the next one the plane grants no more than it then holds and
    /// its lease ([`Plane::lease`]).
    pub fn allow(&mut self, at: Duration, bytes: u64) {
        self.step(PlaneInput::Allowance { at_ns: ns(at), bytes })
            .unwrap_or_else(|error| panic!("the plane refused the machine's allowance: {error}"));
    }

    /// What the process held at the plane's last measure ([`Plane::poll`]).
    pub fn held(&self) -> u64 {
        self.graph.held()
    }

    /// What the tasks running now were granted.
    pub fn granted(&self) -> u64 {
        self.graph.granted()
    }

    /// The most the plane grants until the next [`Plane::allow`]
    /// ([`blaze_graph::Plane::room`]).
    pub fn room(&self) -> u64 {
        self.graph.room()
    }

    /// What the plane may grant past what it holds or granted until the next [`Plane::allow`],
    /// which the ledger records in this daemon's charge ([`blaze_graph::Plane::lease`]).
    pub fn lease(&self) -> u64 {
        self.graph.lease()
    }

    /// Whether a task with memory was held back for it as the plane last dispatched.
    pub fn held_any_back(&self) -> bool {
        self.graph.held_back()
    }

    /// Whether a turn of the ledger now would change what the plane may grant
    /// ([`blaze_graph::Plane::turn_wanted`]): the room holds a task back, the lease runs low,
    /// or a lease is held with no work.
    pub fn turn_wanted(&self) -> bool {
        self.graph.turn_wanted()
    }

    /// The next node of `fresh`, a part of a program's build of `kind` ([`kinds::PROGRAM`], its
    /// codegen, or [`kinds::PROGRAM_LINK`], its link), task `arg` of the batch's work. It is
    /// predicted and taught by `program`, the key of that part of the program, which the plane
    /// never sees: the node has no key, so nothing single-flights or memoizes it by that. It is
    /// forecast as any task is ([`Plane::admit`]): its own record once measured, else
    /// `source`, its root unit's bytes of source, times what parts of programs of `kind` held
    /// per KiB of it lately, a guess held to one work thread's share. Its peak is what its own
    /// work held, its account's (`arena::account`), never what the whole process held beyond
    /// its start (docs/memory-budget.md). Until 2026-11-12 a part never measured was forecast
    /// the whole budget and ran alone.
    ///
    /// # Panics
    ///
    /// As [`Plane::push`], when `node` has a key, and when `kind` is no program's.
    pub fn push_program(
        &self,
        fresh: &mut Fresh,
        mut node: NodeRef<'_>,
        kind: TaskKind,
        arg: u32,
        program: Key,
        source: u64,
    ) -> NodeId {
        assert!(
            (fresh.tasks.len() as u32) < fresh.count,
            "a batch of {} nodes given another",
            fresh.count
        );
        assert!(node.key.is_none(), "program build {} is given a key of the plane's", node.name);
        assert!(of_program(kind), "{kind:?} is no part of a program's build");
        let cost = self.admit(fresh, &mut node, kind, Some(program), source);
        if let Some(cost) = cost {
            node.cost = cost;
        }
        fresh.tasks.push(Task::Thread { kind, arg });
        fresh.keys.push(Some(program));
        fresh.sizes.push(source);
        fresh.nodes.push(node)
    }

    fn reserve(&mut self, count: u32) -> NodeId {
        let first = match self.next_node.checked_add(count) {
            Some(_) => self.next_node,
            None => 0,
        };
        let end = first + count;
        let free = self
            .batches
            .range(..end)
            .next_back()
            .is_none_or(|(at, batch)| at + batch.tasks.len() as u32 <= first);
        assert!(free, "the plane's node ids wrapped onto nodes that have not settled");
        self.next_node = end;
        NodeId(first)
    }

    fn task(&self, id: NodeId) -> Option<(&Batch<W>, Task)> {
        let (first, batch) = self.batches.range(..=id.0).next_back()?;
        batch.tasks.get((id.0 - first) as usize).map(|task| (batch, *task))
    }

    fn key(&self, id: NodeId) -> Option<Key> {
        let (first, batch) = self.batches.range(..=id.0).next_back()?;
        batch.keys.get((id.0 - first) as usize).copied().flatten()
    }

    /// Node `id`'s unit's source bytes, 0 for a node not sized.
    fn size(&self, id: NodeId) -> u64 {
        let Some((first, batch)) = self.batches.range(..=id.0).next_back() else { return 0 };
        batch.sizes.get((id.0 - first) as usize).copied().unwrap_or(0)
    }

    /// Where node `id`'s forecast came from ([`Plane::admit`]).
    fn basis(&self, id: NodeId) -> Basis {
        let Some((first, batch)) = self.batches.range(..=id.0).next_back() else {
            return Basis::Given;
        };
        batch.bases.get((id.0 - first) as usize).copied().unwrap_or(Basis::Given)
    }

    fn settled(&mut self, id: NodeId) {
        self.replays.remove(&id);
        let Some((&first, batch)) = self.batches.range_mut(..=id.0).next_back() else { return };
        let Some(task) = batch.tasks.get_mut((id.0 - first) as usize) else { return };
        if *task != Task::Gone {
            *task = Task::Gone;
            batch.live -= 1;
            if batch.live == 0 {
                self.batches.remove(&first);
            }
        }
    }

    /// Requests are numbered from `first` on: the daemon's first build's number
    /// ([`super::observe::Observe::first_build`]), past every build its history names, so a
    /// request's number is its build's id in the flight recorder and the history. Never 0.
    pub fn number_from(&mut self, first: u32) {
        self.next_request = first.max(1);
    }

    /// A request, of weight 1: every job's share of the threads is the same. An `idle` one runs
    /// on what no other can use until it is promoted ([`Plane::promote`]).
    pub fn open(&mut self, at: Duration, keep_going: bool, idle: bool) -> RequestId {
        let request = RequestId(self.next_request);
        self.next_request = self.next_request.wrapping_add(1).max(1);
        let input = PlaneInput::Open { at_ns: ns(at), request, weight: 1, keep_going, idle };
        // alloc: a request's live model, once per request
        self.folds.by.insert(request, Live { fold: Fold::new(), began_ns: ns(at) });
        self.step(input)
            .unwrap_or_else(|error| panic!("the plane refused to open {request}: {error}"));
        request
    }

    /// Every request's live model, for a look ([`Folds::look`]).
    pub fn folds(&mut self) -> &mut Folds {
        &mut self.folds
    }

    /// From now on, every node and move the plane says also goes to `tasks`, the explorer's
    /// task table (`blaze_explore::Tasks`), read by its sessions' live feeds.
    #[cfg(feature = "native")]
    pub fn publish_tasks(&mut self, tasks: std::sync::Arc<blaze_explore::Tasks>) {
        self.folds.tasks = Some(tasks);
    }

    /// The job of `request` ended, or the request was its job's and the job opened another:
    /// its live model goes. A request no job looks at (a kept front end's store) goes as it
    /// finishes.
    pub fn forget(&mut self, request: RequestId) {
        self.folds.by.remove(&request);
    }

    /// The nodes of `fresh` added to `request`, in one input to the plane, their thread tasks
    /// run by `work`; with `reads`, each posted with readers the loop makes ([`Happened::Post`]).
    /// An error (a cycle, a dep of another request) adds none of them.
    ///
    /// # Panics
    ///
    /// When `fresh` has fewer nodes than it was made for, or a thread task has no `work`.
    pub fn add(
        &mut self,
        at: Duration,
        request: RequestId,
        fresh: Fresh,
        work: Option<W>,
        reads: bool,
    ) -> Result<(), Error> {
        let Fresh { nodes, tasks, keys, sizes, bases, over, count } = fresh;
        let work = work.map(Arc::new); // alloc: one per batch, shared by its tasks
        assert_eq!(tasks.len() as u32, count, "a batch is added with every node it was made for");
        assert!(
            work.is_some() || !tasks.iter().any(|it| matches!(it, Task::Thread { .. })),
            "a batch with thread tasks is given their work"
        );
        let first = nodes.first();
        if count > 0 {
            self.batches.insert(
                first.0,
                Batch {
                    tasks,
                    keys,
                    sizes,
                    bases,
                    over,
                    live: count,
                    work,
                    reads,
                    request: request.0,
                },
            );
        }
        match self.step(PlaneInput::AddNodes { at_ns: ns(at), request, nodes }) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.batches.remove(&first.0);
                Err(error)
            }
        }
    }

    /// An idle request runs from now on as one opened not idle.
    pub fn promote(&mut self, at: Duration, request: RequestId) {
        self.step(PlaneInput::Promote { at_ns: ns(at), request })
            .unwrap_or_else(|error| panic!("the plane refused to promote {request}: {error}"));
    }

    /// No more nodes are added to `request`.
    pub fn close(&mut self, at: Duration, request: RequestId) {
        self.step(PlaneInput::Close { at_ns: ns(at), request })
            .unwrap_or_else(|error| panic!("the plane refused to close {request}: {error}"));
    }

    /// Skips what of `request` has not started; it finishes not ok once what runs is done.
    pub fn cancel(&mut self, at: Duration, request: RequestId) {
        self.now_ns = ns(at);
        let mut said = std::mem::take(&mut self.said);
        self.graph
            .cancel_into(ns(at), request, &mut said)
            .unwrap_or_else(|error| panic!("the plane refused to cancel {request}: {error}"));
        self.outputs(&mut said);
        self.said = said;
    }

    /// A loop task is over: the time since it started teaches the predictor, unless it replayed
    /// what was kept ([`Plane::replays`]).
    pub fn ended(&mut self, at: Duration, id: NodeId, outcome: Outcome) {
        let kind = match self.task(id) {
            Some((_, Task::Loop { kind })) => Some(kind),
            _ => None,
        };
        let replayed = self.replays.remove(&id);
        if let (Some(start), Some(kind)) = (self.loops.remove(&id), kind)
            && !replayed
        {
            let key = self.key(id);
            self.predict.observe(kind, key, ns(at).saturating_sub(start), 0);
        }
        let input = PlaneInput::Done {
            at_ns: ns(at),
            id,
            outcome,
            added: Vec::new(),
            group: None,
            peak: 0,
        };
        self.step(input)
            .unwrap_or_else(|error| panic!("the plane refused the end of loop task {id}: {error}"));
    }

    /// A thread task of [`Happened::Post`] posted with the readers the loop made for it.
    pub fn post(&mut self, task: Posting, again: bool, readers: W::Readers) {
        let Some((batch, Task::Thread { .. })) = self.task(task.id) else {
            panic!("the daemon posts node {}, which is no thread task waiting", task.id)
        };
        let work = batch.work.clone().expect("checked by add");
        if !again {
            self.happened.push(Happened::Started { id: task.id });
        }
        self.walks(task.kind, true);
        self.runner.post(task, work, readers);
    }

    /// Node `id`'s task replays what was kept (its unit is fresh: its record is in the store),
    /// so its run teaches the predictor nothing, neither its key nor its kind: a replay's
    /// near-zero time and peak are not what the work they predict takes, and a few warm
    /// builds would otherwise bring a unit's prediction to nothing for the build that really
    /// compiles it. Said before its task can end: before a thread task is posted
    /// ([`Plane::post`]), before a loop task is [`Plane::ended`].
    pub fn replays(&mut self, id: NodeId) {
        self.replays.insert(id);
    }

    /// A thread task of [`Happened::Post`] the loop has no readers for: it ends failed.
    pub fn refuse(&mut self, at: Duration, task: Posting, why: String) {
        let input = PlaneInput::Done {
            at_ns: ns(at),
            id: task.id,
            outcome: Outcome::Failed { why },
            added: Vec::new(),
            group: None,
            peak: 0,
        };
        self.step(input).unwrap_or_else(|error| {
            panic!("the plane refused the end of task {}: {error}", task.id)
        });
    }

    /// Takes the thread tasks that ended into the plane, then measures: what each running task
    /// has realized and the footprint ([`Plane::measure`]), purging first when admission asked.
    pub fn poll(&mut self, at: Duration) {
        self.began_now();
        let mut ran = std::mem::take(&mut self.ran);
        self.runner.take(&mut ran);
        for Ran { task, ended, peak, ran_ns, puts } in ran.drain(..) {
            let id = task.id;
            self.walks(task.kind, false);
            if let Some(purging) = &mut self.purging {
                purging.freed = true;
            }
            self.realizing.remove(&id);
            if !puts.is_empty() {
                self.happened.push(Happened::Kept { id, puts });
            }
            let outcome = match ended {
                Ended::GaveWay { group } => {
                    self.happened.push(Happened::Post {
                        task: Posting { near: group, ..task },
                        again: true,
                    });
                    continue;
                }
                Ended::Did(Did { outcome, value }) => {
                    self.happened.push(Happened::Did { id, value });
                    outcome
                }
                Ended::Panicked(panic) => {
                    let why = panic.to_string(); // alloc: a panic, once
                    self.happened.push(Happened::Panicked { id, panic });
                    Outcome::Failed { why }
                }
            };
            // What its work held at most, as its memory account was charged: its own and its
            // fan-out's bytes, never another task's, so it is what its key learns as it is.
            let key = self.key(id);
            if !self.replays.remove(&id) {
                if task.basis != Basis::Given {
                    let forecast =
                        blaze_graph::forecast::Forecast { bytes: task.granted, basis: task.basis };
                    self.check.observe(forecast, peak);
                }
                self.predict.observe(task.kind, key, ran_ns, peak);
                if task.kind == kinds::COMPILE && peak > 0 {
                    self.walk_peaks.insert(id, peak);
                }
                let source = self.size(id);
                if source > 0 {
                    self.per_kib
                        .entry(task.kind)
                        .or_default()
                        .observe(peak / source.div_ceil(1024));
                }
            }
            let input = PlaneInput::Done {
                at_ns: ns(at),
                id,
                outcome,
                added: Vec::new(),
                group: None,
                peak,
            };
            self.step(input)
                .unwrap_or_else(|error| panic!("the plane refused the end of task {id}: {error}"));
        }
        self.ran = ran;
        self.measure(at, false);
        if std::mem::take(&mut self.purge_due) && self.purging.is_none() {
            // Admission held work back while the allocators may keep freed memory: the purger's
            // thread gives it back (it waits for every worker's heaps, which the loop must not),
            // and the plane hears a reading marked purged only once that purge ended. No start
            // waits for it (`blaze_graph::Plane::pick`'s invariant): an ask that comes while
            // this one is on its way is taken as this one.
            self.purging = Some(Purging::asked());
        }
        if let Some(purging) = self.purging {
            // A purger that stopped serves no purge again, and admission waits on one: the
            // daemon stops naming why, as it does when the plane refuses what it is told.
            let done = crate::raw::memory::purger().done().unwrap_or_else(|why| {
                panic!("admission waits on a memory purge that cannot finish: {why}")
            });
            if done >= purging.ticket {
                let needed = self.graph.held_back() || self.purge_wanted;
                if purging.freed && needed {
                    // A task ended or gave way after the ask: what it freed, a worker that
                    // already gave back keeps, so a reading now would call kept bytes gone.
                    // Asked again; bounded, since each round needs another task to end.
                    self.purging = Some(Purging::asked());
                } else if purging.freed {
                    // Nothing waits for the purge any more (no work is held back for memory,
                    // the give-back rule asked none): it is not chased. A build that made
                    // nothing purged 45 times, once a batch of its tasks that ended, for a
                    // wait that was over (Mac, hello-world, 2026-11-12). The reading is
                    // taken as any other, not marked purged; the plane waits for none
                    // (letting one go while a task that fits only alone still waited for a
                    // purged reading stalled the smoke's second build, the same day).
                    self.purging = None;
                } else {
                    self.purging = None;
                    self.measure(at, true);
                }
            }
        }
        // The pressure cap, given as it changes: on an event, and once it passes.
        let cap = self.pressure.cap(ns(at));
        if cap != self.capped {
            self.capped = cap;
            let input = PlaneInput::Cap { at_ns: ns(at), bytes: cap };
            self.step(input)
                .unwrap_or_else(|error| panic!("the plane refused the pressure cap: {error}"));
        }
    }

    /// The accounts the thread tasks that started since said ([`Posting::began`]).
    fn began_now(&mut self) {
        while let Ok((id, account)) = self.began.try_recv() {
            self.realizing.insert(id, Realizing { account, told: 0 });
        }
    }

    /// Tells the plane what each running task has realized, where it grew past its grant or
    /// by a [`REALIZED_STEP`] since it was last told, then the process's footprint and what of
    /// it the allocators keep freed ([`PlaneInput::Footprint`]), `purged` when a purge came
    /// just before. A task whose account closed ended: its bytes are the footprint's to say.
    fn measure(&mut self, at: Duration, purged: bool) {
        self.began_now();
        let at_ns = ns(at);
        let mut realizing = std::mem::take(&mut self.realizing);
        realizing.retain(|&id, seen| {
            let Some(bytes) = arena::peak_now(seen.account) else { return false };
            let granted = self.graph.admitted(id).map_or(0, |it| it.granted);
            let step = (granted / 16).max(REALIZED_STEP);
            if bytes > granted || bytes >= seen.told.saturating_add(step) {
                seen.told = bytes;
                let input = PlaneInput::Realized { at_ns, id, bytes };
                self.step(input).unwrap_or_else(|error| {
                    panic!("the plane refused what task {id} realized: {error}")
                });
            }
            true
        });
        self.realizing = realizing;
        let snapshot = blaze_mem::tag::snapshot();
        let footprint = arena::own_of(&snapshot);
        // The free pages the allocators keep ([`arena::kept_of`]). An estimate (it holds the
        // code and the stacks too, never purged), so a purge is asked at most every
        // `blaze_graph::footprint::PURGE_EVERY_NS` while work is held back.
        let kept = arena::kept_of(&snapshot, footprint);
        let input = PlaneInput::Footprint { at_ns, bytes: footprint, kept, purged };
        self.step(input).unwrap_or_else(|error| panic!("the plane refused the footprint: {error}"));
        self.readings += 1;
        self.purged_readings += u64::from(purged);
        if purged {
            self.purge_wanted = false;
        }
    }

    /// The last footprint reading, as the give-back rule reads it
    /// (`blaze_graph::giveback::Now`): the footprint, what of it the allocators keep freed,
    /// and how many readings and purged readings came so far.
    pub fn reading(&self) -> Reading {
        Reading {
            footprint: self.graph.held(),
            kept: self.graph.allocator_kept(),
            readings: self.readings,
            purged: self.purged_readings,
        }
    }

    /// How many thread tasks that may walk a program run now: those of codegen's kinds
    /// posted to the runner and not yet taken back. While it is 0 no walk holds codegen's
    /// kept walks, so letting go of them frees them at once and loses no walk's result
    /// (`blaze_graph::giveback::Rung::Caches`); while one runs, it keeps its own table until
    /// it ends and what it then keeps is dropped with that table.
    pub fn walks_in_flight(&self) -> usize {
        self.walking
    }

    /// A thread task of `kind` is posted to the runner (`posted`) or taken back from it:
    /// counted where it may walk a program ([`Plane::walks_in_flight`]).
    fn walks(&mut self, kind: TaskKind, posted: bool) {
        if admission::tag_of(kind) != blaze_mem::Tag::Codegen {
            return;
        }
        match posted {
            true => self.walking += 1,
            false => {
                self.walking = self.walking.checked_sub(1).unwrap_or_else(|| {
                    panic!("a codegen task of kind {:#x} was taken back that was never posted", kind.0)
                });
            }
        }
    }

    /// What this daemon wants of the machine: what its best task held back for memory is
    /// short of (`blaze_graph::Plane::wants`), for the machine's ledger.
    pub fn wants(&self) -> u64 {
        self.graph.wants()
    }

    /// What this daemon could give back and keep its floor, as its give-back last said.
    pub fn gives(&self) -> u64 {
        self.gives
    }

    /// The give-back says what this daemon could give back now and keep its floor; 0 while
    /// it has work.
    pub fn set_gives(&mut self, bytes: u64) {
        self.gives = bytes;
    }

    /// The give-back rule asks a purge (`blaze_graph::giveback::Rung::Allocator`): done by
    /// the purger's thread from the next [`Plane::poll`], as one the plane asks is, and
    /// followed by a reading marked purged.
    pub fn ask_purge(&mut self) {
        self.purge_due = true;
        self.purge_wanted = true;
    }

    /// What happened since the last call, in order, swapped into `into`, which is empty: the
    /// two buffers trade places each call and neither is allocated again.
    pub fn happened(&mut self, into: &mut Vec<Happened<W::Value>>) {
        debug_assert!(into.is_empty(), "what happened before was taken");
        std::mem::swap(&mut self.happened, into);
    }

    /// Whether a thread task ended and waits to be taken.
    pub fn ready(&self) -> bool {
        self.runner.ready()
    }

    /// Whether the ready node `id` is held back for memory alone now
    /// ([`blaze_graph::Plane::waits_for_memory`]).
    pub fn waits_for_memory(&self, id: NodeId) -> bool {
        self.graph.waits_for_memory(id)
    }

    /// What memory admission holds and allows now, as a stall says it beside the ready nodes it
    /// did not start (rule errors-name-the-thing: a ready node that never starts is held back
    /// by a rule of admission, and the stall names which); `None` with no budget, where
    /// admission holds nothing back for memory.
    pub fn admission_says(&self) -> Option<String> {
        use std::fmt::Write as _;
        let graph = &self.graph;
        let budget = graph.config().budget.as_ref()?.bytes;
        let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
        // alloc: a stall's line, once
        let mut said = format!(
            "memory admission: charged {:.2} GiB of a budget of {:.2} GiB, {:.2} GiB of it granted to running tasks",
            gib(graph.charged()),
            gib(budget),
            gib(graph.granted()),
        );
        match graph.allowance() {
            u64::MAX => said.push_str("; the machine's ledger said no allowance"),
            allowance => _ = write!(said, "; allowance {:.2} GiB", gib(allowance)),
        }
        match graph.room() {
            u64::MAX => {}
            room => _ = write!(said, ", room until the ledger's next turn {:.2} GiB", gib(room)),
        }
        if let Some(cap) = graph.cap() {
            _ = write!(said, "; held at {:.2} GiB by the system's memory pressure", gib(cap));
        }
        _ = write!(
            said,
            "; a task that does not fit beside that starts alone: {}",
            match graph.may_start_alone() {
                true => "yes, at a turn of the ledger if it is over the room",
                false => "no, a running task holds a grant",
            }
        );
        if graph.holds_others_back() {
            said.push_str("; a task passed over for memory holds back every other task that asks any until it starts");
        }
        _ = write!(
            said,
            "; a turn of the ledger is wanted: {}; a purge is on its way: {}",
            graph.turn_wanted(),
            self.purging.is_some() || self.purge_due
        );
        Some(said)
    }

    /// How many thread tasks run.
    pub fn running(&self) -> usize {
        self.runner.running()
    }

    /// Whether a thread task's end ends the loop's wait ([`Runner::wakes`]).
    pub fn wakes(&self) -> bool {
        self.runner.wakes()
    }

    /// How many thread tasks run on the worker pool ([`Runner::running_on_pool`]).
    pub fn running_on_pool(&self) -> usize {
        self.runner.running_on_pool()
    }

    /// Its runner, for a test to hold a kind of task on the simulation's ([`Inline::hold`]).
    #[cfg(test)]
    pub(super) fn runner_mut(&mut self) -> &mut Runner<W> {
        &mut self.runner
    }

    /// How many loop tasks run ([`Happened::Run`], not yet [`Plane::ended`]).
    pub fn loops_running(&self) -> usize {
        self.loops.len()
    }

    /// Whether `id` is a loop task that runs ([`Happened::Run`], not yet [`Plane::ended`]).
    pub fn loop_runs(&self, id: NodeId) -> bool {
        self.loops.contains_key(&id)
    }

    /// Each node of an open request that has not settled, to `each`, in no order.
    pub fn unsettled<'a>(&'a self, each: impl FnMut(Unsettled<'a>)) {
        self.graph.unsettled(each);
    }

    /// One input, its outputs taken through `said`, which is reused by every step.
    fn step(&mut self, input: PlaneInput) -> Result<(), Error> {
        self.now_ns = input.at_ns();
        let mut said = std::mem::take(&mut self.said);
        let taken = self.graph.step_into(input, &mut said);
        // Asked once a reading: kept until the loop purges, whatever steps follow.
        self.purge_due |= self.graph.purge_due();
        if taken.is_ok() {
            self.outputs(&mut said);
            self.held_for_memory();
        }
        said.clear();
        self.said = said;
        taken
    }

    fn outputs(&mut self, outputs: &mut Vec<PlaneOutput>) {
        for output in outputs.drain(..) {
            match output {
                PlaneOutput::Said { request, record } => {
                    self.folds.said(request, record.at_ns, &record.event);
                    let Event::Moved { id, state } = record.event else { continue };
                    match state {
                        NodeState::Ready => self.admitting(NodeId(id)),
                        NodeState::Running => self.admitted(NodeId(id)),
                        _ => {}
                    }
                    if !state.settled() {
                        continue;
                    }
                    let id = NodeId(id);
                    // One that settles unstarted ends its wait unsaid.
                    self.admitting.remove(&id);
                    let over = self.take_over(id);
                    self.settled(id);
                    let (ok, why, skipped) = match state {
                        NodeState::Built | NodeState::Cached { .. } => (true, None, None),
                        NodeState::Failed { why } => {
                            (false, Some(refused_by_own_runs(why, over)), None)
                        }
                        NodeState::Skipped { because } => {
                            let why =
                                format!("skipped: node {because} it waited for did not end well"); // alloc: a node skipped, once
                            (false, Some(why), Some(NodeId(because)))
                        }
                        _ => (false, Some("cancelled".to_owned()), None), // alloc: a node cancelled, once
                    };
                    self.happened.push(Happened::Settled { id, ok, why, skipped });
                }
                PlaneOutput::Run { id, pool, near, critical } => {
                    self.run(id, pool, near, critical);
                }
                PlaneOutput::Finished { request, ok } => {
                    let stats = self.graph.stats(request).unwrap_or_else(|| {
                        panic!("the plane finished {request}, which it holds no stats of")
                    });
                    self.graph.release(request).unwrap_or_else(|error| {
                        panic!("the plane refused to release {request}: {error}")
                    });
                    self.happened.push(Happened::Finished { request, ok, stats });
                }
                // An idle task asked to give way holds its thread until its `Done`, as the plane
                // allows: none is stopped mid-run.
                PlaneOutput::Yield { .. } => {}
            }
        }
    }

    /// Node `id` became ready: its wait for admission begins, as a detached `admission` span
    /// of its build (rule every-wait-says-what: no thread waits, the plane queues it).
    fn admitting(&mut self, id: NodeId) {
        let Some((batch, task)) = self.task(id) else { return };
        let kind = match task {
            Task::Thread { kind, .. } | Task::Loop { kind } => kind,
            Task::Gone => return,
        };
        let _build = blaze_profile::flight::building(batch.request);
        let span = blaze_profile::span::detached(blaze_profile::span::Admission {
            kind: task_name(kind),
            node: u64::from(id.0),
        });
        // A node made ready again (after it gave way) begins a new wait; the old one ends unsaid.
        self.admitting.insert(id, Admitting { span, since_ns: self.now_ns, memory: false });
    }

    /// Marks each ready node that memory holds back as this step leaves the plane
    /// ([`blaze_graph::Plane::waits_for_memory`]: its task does not fit beside what is charged,
    /// with a thread of its pool free), and says it once, with the bytes: what the task is
    /// forecast, what admission charges, and the budget it counts against now. Walked only
    /// after a step whose dispatch held a task back for memory.
    ///
    /// Until 2026-11-12 a wait was said to be for memory only when another task with memory
    /// had started over it ([`blaze_graph::plane::Admitted::passed`]), so a wait in which
    /// nothing fit was said to be for `a work thread`: 22 tasks of a stalled build that
    /// admission never granted read as waiting for threads on an idle pool.
    fn held_for_memory(&mut self) {
        if !self.graph.held_back() {
            return;
        }
        for (&id, admitting) in self.admitting.iter_mut() {
            if admitting.memory || !self.graph.waits_for_memory(id) {
                continue;
            }
            admitting.memory = true;
            tracing::debug!(
                target: "blaze",
                node = id.0,
                forecast = self.graph.memory_of(id).unwrap_or(0),
                charged = self.graph.charged(),
                granted = self.graph.granted(),
                budget = self.graph.budget_now().unwrap_or(0),
                "admission holds a ready task back for memory"
            );
        }
    }

    /// Node `id` started: its wait for admission ends, saying what held it back. Memory, when
    /// its task did not fit in a step of its wait ([`Plane::held_for_memory`]) or another task
    /// with memory started over it ([`blaze_graph::plane::Admitted::passed`]); else a thread of
    /// its pool; nothing, when it started in the step it became ready.
    fn admitted(&mut self, id: NodeId) {
        let Some(Admitting { span, since_ns, memory }) = self.admitting.remove(&id) else {
            return;
        };
        let Some(admitted) = self.graph.admitted(id) else { return };
        let waited_for = if since_ns == self.now_ns {
            "nothing"
        } else if memory || admitted.passed > 0 {
            "memory"
        } else {
            match admitted.pool {
                Pool::Io => "an io thread",
                Pool::Work => "a work thread",
            }
        };
        span.ended(blaze_profile::span::Admitted {
            waited_for: blaze_profile::span::Name::kept(waited_for),
            passed: u64::from(admitted.passed),
            granted: admitted.granted,
            basis: blaze_profile::span::Name::kept(self.basis(id).name()),
        });
    }

    /// The node the plane runs: its thread task posted, or its loop task handed to the loop.
    fn run(&mut self, id: NodeId, pool: Pool, near: Option<u32>, critical: bool) {
        // It runs, so no refusal for memory is its to say ([`Plane::take_over`]).
        _ = self.take_over(id);
        match self.task(id) {
            Some((batch, Task::Thread { kind, arg })) => {
                let admitted = self.graph.admitted(id);
                let task = Posting {
                    id,
                    pool,
                    near,
                    critical,
                    kind,
                    arg,
                    reads: batch.reads,
                    build: batch.request,
                    granted: admitted.map_or(0, |it| it.granted),
                    passed: admitted.map_or(0, |it| it.passed),
                    basis: self.basis(id),
                    began: Some(self.began_tx),
                };
                if batch.reads {
                    self.happened.push(Happened::Post { task, again: false });
                } else {
                    let work = batch.work.clone().expect("checked by add");
                    self.happened.push(Happened::Started { id });
                    self.walks(task.kind, true);
                    self.runner.post(task, work, W::Readers::default());
                }
            }
            Some((_, Task::Loop { .. })) => {
                self.loops.insert(id, self.now_ns);
                self.happened.push(Happened::Run { id });
            }
            Some((_, Task::Gone)) | None => {
                panic!("the plane runs node {id}, which the daemon gave no task")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use blaze_core::Traced;
    use blaze_ids::ContentId;
    use blaze_protocol::events::NodeKind;
    use blaze_usercode::RunKey;

    use super::*;

    fn put(what: &str) -> proc_macro_api::kept::Put {
        let id = ContentId::of("blaze-cargo plane test put", what.as_bytes());
        proc_macro_api::kept::Put {
            keep: proc_macro_api::kept::Keep::Run {
                run: RunKey { code: id, given: id },
                traced: Traced::default(),
            },
            value: what.as_bytes().to_vec(),
            what: what.to_owned(),
        }
    }

    /// Puts of other tests may come with a take as orphans: only this test's are compared.
    fn ours<'a>(puts: &'a [proc_macro_api::kept::Put], prefix: &str) -> Vec<&'a str> {
        puts.iter().map(|it| it.what.as_str()).filter(|it| it.starts_with(prefix)).collect()
    }

    /// A test's batch: its tasks are one closure of their `arg`, posted with no readers.
    struct Each<F>(F);

    impl<F: Fn(u32) -> Option<Did<u32>> + Send + Sync + 'static> Work for Each<F> {
        type Value = u32;
        type Readers = ();

        fn run(&self, arg: u32, (): ()) -> Option<Did<u32>> {
            (self.0)(arg)
        }
    }

    fn built(value: u32) -> Option<Did<u32>> {
        Some(Did { outcome: Outcome::Built, value })
    }

    type Next = Each<fn(u32) -> Option<Did<u32>>>;

    fn next(arg: u32) -> Option<Did<u32>> {
        built(arg + 1)
    }

    #[test]
    fn each_runner_runs_a_task_and_hands_back_its_value() {
        let task = |pool| Posting {
            id: NodeId(4),
            pool,
            near: None,
            critical: false,
            kind: kinds::COMPILE,
            arg: 20,
            reads: false,
            build: 1,
            granted: 0,
            passed: 0,
            basis: Basis::Given,
            began: None,
        };
        let runners: [(Runner<Next>, Pool); 3] = [
            (Runner::Threads(Threads::new(1, 1, None)), Pool::Io),
            (Runner::Threads(Threads::new(1, 1, None)), Pool::Work),
            (Runner::Inline(Inline::new(1)), Pool::Work),
        ];
        for (mut runner, pool) in runners {
            runner.post(task(pool), Arc::new(Each(next as fn(u32) -> Option<Did<u32>>)), ());
            assert_eq!(runner.running(), 1);
            let mut ran = Vec::new();
            while ran.is_empty() {
                runner.take(&mut ran);
            }
            let [Ran { ended: Ended::Did(Did { value: 21, .. }), .. }] = ran.as_slice() else {
                panic!("the task ends with its value")
            };
            assert_eq!(runner.running(), 0);
            assert!(!runner.ready());
        }
    }

    /// coord-102 (B5): the plane ran its tasks in no `blaze_mem::frame::task`, so everything a
    /// unit's job allocated outside its queries counted to the heap outside any task (8.18 GiB
    /// of 28.27 at the last sample), named by no subsystem. A task runs in a frame of its own
    /// kind's subsystem ([`admission::tag_of`]), on every runner and pool, and so does what it
    /// allocates.
    #[test]
    fn a_task_runs_as_its_kinds_subsystem_on_every_runner() {
        fn running(_: u32) -> Option<Did<u32>> {
            built(blaze_mem::frame::running_tag().map_or(u32::MAX, |tag| tag as u32))
        }
        for kind in [
            kinds::COMPILE,
            kinds::UNIT_NAMES,
            kinds::UNIT_BODIES,
            kinds::UNIT_LINK,
            kinds::UNIT_STORE,
        ] {
            let runners: [(Runner<Next>, Pool); 3] = [
                (Runner::Threads(Threads::new(1, 1, None)), Pool::Io),
                (Runner::Threads(Threads::new(1, 1, None)), Pool::Work),
                (Runner::Inline(Inline::new(1)), Pool::Work),
            ];
            for (mut runner, pool) in runners {
                let task = Posting {
                    id: NodeId(5),
                    pool,
                    near: None,
                    critical: false,
                    kind,
                    arg: 0,
                    reads: false,
                    build: 1,
                    granted: 0,
                    passed: 0,
                    basis: Basis::Given,
                    began: None,
                };
                runner.post(task, Arc::new(Each(running as fn(u32) -> Option<Did<u32>>)), ());
                let mut ran = Vec::new();
                while ran.is_empty() {
                    runner.take(&mut ran);
                }
                let [Ran { ended: Ended::Did(Did { value, .. }), .. }] = ran.as_slice() else {
                    panic!("the task ends with its value")
                };
                let want = blaze_graph::admission::tag_of(kind);
                assert_eq!(*value, want as u32, "a {kind:?} task on {pool:?} runs as {want:?}");
            }
        }
    }

    #[test]
    fn a_task_that_panics_ends_with_its_message_and_where_it_was_raised_on_another_thread() {
        record_panics();
        let work = Arc::new(Each(|_| -> Option<Did<u32>> {
            let raised = std::thread::Builder::new()
                .name("plane-test-raiser".to_owned())
                .spawn(|| -> u32 { panic!("plane-test: the task's panic") })
                .expect("a thread")
                .join()
                .expect_err("the thread panics");
            std::panic::resume_unwind(raised)
        }));
        let task = Posting {
            id: NodeId(9),
            pool: Pool::Work,
            near: None,
            critical: false,
            kind: kinds::COMPILE,
            arg: 0,
            reads: false,
            build: 1,
            granted: 0,
            passed: 0,
            basis: Basis::Given,
            began: None,
        };
        let mut runner = Inline::new(5);
        runner.post(task, work, ());
        let mut ran = Vec::new();
        runner.take(&mut ran);
        let [Ran { ended: Ended::Panicked(panic), .. }] = ran.as_slice() else {
            panic!("the task ends panicked")
        };
        assert_eq!(panic.message.as_deref(), Some("plane-test: the task's panic"));
        let at = panic.at.as_deref().expect("the hook saw where it was raised");
        assert!(
            at.contains("serve/plane.rs:") && at.ends_with(" on thread plane-test-raiser"),
            "{at}"
        );
        assert_eq!(panic.to_string(), format!("panicked at {at}: plane-test: the task's panic"));
    }

    /// coord-98: a block blaze-mem's used-up size class could not give aborted the daemon, and
    /// every crate's tests went with it. An allocation that fails in one task fails that task,
    /// naming the size, and the tasks beside it end with their values: this test's process
    /// living to check them is the proof that nothing aborted.
    #[test]
    fn an_allocation_that_fails_fails_its_task_alone_and_the_rest_finish() {
        record_panics();
        record_alloc_failures();
        let work = Arc::new(Each(|arg| -> Option<Did<u32>> {
            if arg == 2 {
                // What a `Vec`, a `Box` or a parsed tree's block does when its allocator has no
                // block to give, as blaze-mem's full class region had none.
                let layout = std::alloc::Layout::from_size_align(137_740, 64).expect("a layout");
                std::alloc::handle_alloc_error(layout);
            }
            built(arg + 100)
        }));
        let mut runner = Inline::new(4);
        for arg in 0..4u32 {
            let task = Posting {
                id: NodeId(20 + arg),
                pool: Pool::Work,
                near: None,
                critical: false,
                kind: kinds::COMPILE,
                arg,
                reads: false,
                build: 1,
                granted: 0,
                passed: 0,
                basis: Basis::Given,
                began: None,
            };
            runner.post(task, work.clone(), ());
        }
        let mut ran = Vec::new();
        while ran.len() < 4 {
            runner.take(&mut ran);
        }
        ran.sort_by_key(|it| it.task.arg);
        for (arg, it) in ran.iter().enumerate() {
            match (&it.ended, arg) {
                (Ended::Panicked(panic), 2) => {
                    let message = panic.message.as_deref().expect("the panic's message");
                    assert!(
                        message.starts_with("an allocation of 137740 bytes (aligned to 64) failed"),
                        "{message}"
                    );
                }
                (Ended::Did(Did { value, .. }), _) => assert_eq!(*value, arg as u32 + 100),
                _ => {
                    panic!("task {arg} should have {}", if arg == 2 { "panicked" } else { "built" })
                }
            }
        }
    }

    #[test]
    fn a_task_hands_on_what_its_expansions_put_once_it_ends_and_not_as_it_gives_way() {
        let gave_way = Arc::new(AtomicBool::new(false));
        let work = Arc::new(Each({
            let gave_way = gave_way.clone();
            move |_| {
                if !gave_way.swap(true, Ordering::AcqRel) {
                    proc_macro_api::kept::put(put("plane-a before"));
                    return None;
                }
                proc_macro_api::kept::put(put("plane-a after"));
                built(0)
            }
        }));
        let task = Posting {
            id: NodeId(7),
            pool: Pool::Work,
            near: None,
            critical: false,
            kind: kinds::COMPILE,
            arg: 0,
            reads: false,
            build: 1,
            granted: 0,
            passed: 0,
            basis: Basis::Given,
            began: None,
        };
        let mut runner = Inline::new(3);
        runner.post(task, work, ());
        let mut ran = Vec::new();
        while ran.is_empty() {
            runner.take(&mut ran);
        }
        let [Ran { ended: Ended::Did(_), puts, .. }] = ran.as_slice() else {
            panic!("one task ends")
        };
        assert_eq!(ours(puts, "plane-a"), ["plane-a before", "plane-a after"]);
    }

    #[test]
    fn a_task_that_reads_and_gave_way_leaves_its_puts_for_the_next_take() {
        let work = Arc::new(Each(|_| {
            proc_macro_api::kept::put(put("plane-b gave way"));
            None
        }));
        let task = Posting {
            id: NodeId(8),
            pool: Pool::Work,
            near: None,
            critical: false,
            kind: kinds::COMPILE,
            arg: 0,
            reads: true,
            build: 1,
            granted: 0,
            passed: 0,
            basis: Basis::Given,
            began: None,
        };
        let mut runner = Inline::new(5);
        runner.post(task, work, ());
        let mut ran = Vec::new();
        runner.take(&mut ran);
        let [Ran { ended: Ended::GaveWay { .. }, puts, .. }] = ran.as_slice() else {
            panic!("the task gives way")
        };
        assert!(puts.is_empty());
        assert_eq!(ours(&proc_macro_api::kept::take_puts(), "plane-b"), ["plane-b gave way"]);
    }

    type Asked = Vec<(TaskKind, Option<Key>)>;
    type Observed = Vec<(TaskKind, Option<Key>, u64, u64)>;

    struct Recording {
        asked: Arc<blaze_core::lock::Lock<Asked>>,
        observed: Arc<blaze_core::lock::Lock<Observed>>,
    }

    impl Predict for Recording {
        fn predict(&self, kind: TaskKind, key: Option<Key>) -> Prediction {
            self.asked.lock().push((kind, key));
            Prediction { cost: Some(1000), peak: 0 }
        }

        fn observe(&mut self, kind: TaskKind, key: Option<Key>, ran_ns: u64, peak: u64) {
            self.observed.lock().push((kind, key, ran_ns, peak));
        }

        fn encode(&self) -> Vec<u8> {
            Vec::new()
        }
    }

    #[test]
    fn every_node_is_predicted_and_its_own_run_teaches_the_predictor() {
        let asked = Arc::new(blaze_core::lock::Lock::new("serve.plane.tests.asked", Vec::new()));
        let observed =
            Arc::new(blaze_core::lock::Lock::new("serve.plane.tests.observed", Vec::new()));
        let predict = Recording { asked: Arc::clone(&asked), observed: Arc::clone(&observed) };
        let config = PlaneConfig { io: 1, work: 2, budget: None, starve_after: 8 };
        let mut plane = Plane::new(config, predict, Runner::Inline(Inline::new(9))).unwrap();
        let request = plane.open(Duration::ZERO, false, false);
        let key = Key([3; 32]);
        let node = |kind, name, key| NodeRef {
            kind,
            name,
            deps: &[],
            pool: Pool::Work,
            cost: 1,
            key,
            memory: 0,
        };
        let mut fresh = plane.fresh(2, 16, 0);
        plane.push(
            &mut fresh,
            node(NodeKind::Std, "thread", Some(key)),
            Task::Thread { kind: kinds::COMPILE, arg: 0 },
        );
        let looped = plane.push(
            &mut fresh,
            node(NodeKind::ScriptRun, "loop", None),
            Task::Loop { kind: loops::SCRIPT_RUN },
        );
        plane.add(Duration::ZERO, request, fresh, Some(Each(built)), false).unwrap();
        assert_eq!(*asked.lock(), [(kinds::COMPILE, Some(key)), (loops::SCRIPT_RUN, None)]);

        let (mut happened, mut ran_loop) = (Vec::new(), false);
        while !ran_loop || observed.lock().is_empty() {
            plane.poll(Duration::ZERO);
            plane.happened(&mut happened);
            ran_loop |=
                happened.iter().any(|it| matches!(it, Happened::Run { id } if *id == looped));
            happened.clear();
        }
        let seen: Vec<_> =
            observed.lock().iter().map(|&(kind, key, ran_ns, _)| (kind, key, ran_ns)).collect();
        assert_eq!(seen, [(kinds::COMPILE, Some(key), 0)], "the simulation's tasks take no time");
        plane.ended(Duration::from_millis(5), looped, Outcome::Built);
        let last = observed.lock().last().copied();
        assert_eq!(last, Some((loops::SCRIPT_RUN, None, 5_000_000, 0)));
    }

    /// Many programs built at once (the 211 test binaries of a `blaze test --workspace`) run
    /// on the process's one worker pool, their own parallel work too: the threads that ran
    /// them are that pool's workers and no more, however many programs there are. In the
    /// released engine each program's build made a pool of its own, and 211 of them at once
    /// ran past macOS's per-process thread limit (1,150 "failed to spawn thread" panics).
    #[test]
    fn programs_built_at_once_share_the_one_pool_whatever_their_count() {
        const PROGRAMS: u32 = 256;
        let pool = blaze_graph::exec::shared().workers();
        let config = PlaneConfig { io: 1, work: pool, budget: None, starve_after: 8 };
        let history = blaze_history::History::new(blaze_history::Config::default());
        let mut plane =
            Plane::new(config, history, Runner::Threads(Threads::new(1, pool, None))).unwrap();
        let ran_on = Arc::new(blaze_core::lock::Lock::new("serve.plane.tests.ran_on", Vec::<String>::new()));
        let work = Each({
            let ran_on = Arc::clone(&ran_on);
            move |arg| {
                // A program's build is parallel inside, as codegen and the linker are.
                let names = blaze_graph::par::map(0..64usize, |_| {
                    std::thread::current().name().unwrap_or("a thread with no name").to_owned()
                });
                let mut ran_on = ran_on.lock();
                ran_on.extend(names);
                ran_on.push(
                    std::thread::current().name().unwrap_or("a thread with no name").to_owned(),
                );
                built(arg)
            }
        });
        let request = plane.open(Duration::ZERO, true, false);
        let names: Vec<String> = (0..PROGRAMS).map(|it| format!("program {it}")).collect();
        let mut fresh = plane.fresh(PROGRAMS, names.iter().map(String::len).sum(), 0);
        let ids: Vec<NodeId> = (0..PROGRAMS)
            .map(|at| {
                let node = NodeRef {
                    kind: NodeKind::Codegen,
                    name: &names[at as usize],
                    deps: &[],
                    pool: Pool::Work,
                    cost: 1,
                    key: None,
                    memory: 0,
                };
                plane.push(&mut fresh, node, Task::Thread { kind: kinds::COMPILE, arg: at })
            })
            .collect();
        plane.add(Duration::ZERO, request, fresh, Some(work), false).unwrap();
        let settled = settle(&mut plane, &ids);
        assert!(settled.iter().all(|it| it.1), "every program builds: {settled:?}");

        let mut threads = std::mem::take(&mut *ran_on.lock());
        assert_eq!(
            threads.len(),
            PROGRAMS as usize * 65,
            "every program and its parallel work ran"
        );
        threads.sort_unstable();
        threads.dedup();
        assert!(
            threads.iter().all(|it| it.starts_with("blaze-work-")),
            "only the pool's workers ran the programs: {threads:?}"
        );
        assert!(
            threads.len() <= pool,
            "{} threads ran {PROGRAMS} programs, past the pool's {pool}",
            threads.len()
        );
    }

    /// A guess is held to one work thread's share of the budget, never the whole budget, so it
    /// neither runs alone nor is refused; only a unit's own measured runs over the budget
    /// refuse it, and the refusal says the number is its own. nftest2: a kind's peak of
    /// 121.59 GB refused every build script of a test build under a 59 GB budget; the smoke's
    /// cold build, 2026-11-12: a program never measured was granted the whole budget.
    #[test]
    fn a_guess_is_held_to_a_threads_share_and_only_a_units_own_runs_refuse_it() {
        let gib = 1u64 << 30;

        let budget = Budget { bytes: gib, from: "the test's budget".to_owned(), kept_back: 0 };
        let config = PlaneConfig { io: 1, work: 2, budget: Some(budget), starve_after: 8 };
        let mut history = blaze_history::History::new(blaze_history::Config::default());
        let (other, script, unseen, big) =
            (Key([7; 32]), Key([8; 32]), Key([9; 32]), Key([10; 32]));
        // Another unit's codegen held 120 GB: its kind's peak from now on.
        history.observe(kinds::COMPILE, Some(&other), 1_000, 120 * gib);
        history.observe(kinds::COMPILE, Some(&big), 1_000, 2 * gib);
        let mut plane = Plane::new(config, history, Runner::Inline(Inline::new(4))).unwrap();
        let request = plane.open(Duration::ZERO, true, false);
        let node = |name| NodeRef {
            kind: NodeKind::Codegen,
            name,
            deps: &[],
            pool: Pool::Work,
            cost: 1,
            key: None,
            memory: 0,
        };
        let task = |arg| Task::Thread { kind: kinds::COMPILE, arg };
        let mut fresh = plane.fresh(3, 64, 0);
        // A build script of 4 KiB never measured: sized, at the floor, nowhere near the kind's.
        let sized = plane.push_sized(&mut fresh, node("build script"), task(0), script, 4 << 10);
        // Never measured and not sized: its kind's peak, a guess, so the whole budget, alone.
        let guessed = plane.push_learned(&mut fresh, node("never measured"), task(1), unseen);
        // Its own last run held 2 GiB, more than the whole budget: that alone refuses it.
        let own = plane.push_sized(&mut fresh, node("measured big"), task(2), big, 4 << 10);
        // 4 KiB at the unseen rate is under the floor; the kind's 120 GB peak is held to the
        // share of 1 GiB over 2 work threads; the own record stands as it is.
        plane.add(Duration::ZERO, request, fresh, Some(Each(built)), false).unwrap();
        assert_eq!(plane.basis(sized), Basis::Unseen);
        assert_eq!(plane.basis(guessed), Basis::Kind);
        assert_eq!(plane.basis(own), Basis::Own);
        assert_eq!(plane.graph.memory_of(sized), Some(blaze_graph::forecast::GUESS_FLOOR));
        assert_eq!(plane.graph.memory_of(guessed), Some(gib / 2));
        let (mut happened, mut settled) = (Vec::new(), Vec::new());
        while settled.len() < 3 {
            plane.poll(Duration::ZERO);
            plane.happened(&mut happened);
            for it in happened.drain(..) {
                if let Happened::Settled { id, ok, why, .. } = it {
                    settled.push((id, ok, why));
                }
            }
        }
        settled.sort_by_key(|it| it.0);
        assert_eq!(settled[0].0, sized);
        assert!(
            settled[0].1,
            "a small unit is not refused for its kind's peak: {:?}",
            settled[0].2
        );
        assert_eq!(settled[1].0, guessed);
        assert!(settled[1].1, "a guess runs on a thread's share: {:?}", settled[1].2);
        assert_eq!(settled[2].0, own);
        assert!(!settled[2].1, "a unit whose own runs held more than the budget is refused");
        let why = settled[2].2.as_deref().unwrap_or_default();
        assert!(why.contains("measured big needs 2.00 GB of memory"), "{why}");
        assert!(why.contains("2.00 GB is what this unit's own last runs held at most"), "{why}");
    }

    /// A cold daemon has measured no task of any kind: a unit is predicted by its own size, not
    /// capped at the default for a kind never seen. #1726: every unit of a cold workspace build
    /// was granted 64 MiB, hir-def's front end held 2.39 GiB on it, and the daemon went past its
    /// limit. With a budget, a thread's share holds the guess.
    #[test]
    fn a_cold_daemon_predicts_a_unit_by_its_size_not_the_unseen_kinds_default() {
        use blaze_graph::forecast::{Basis, Forecast, GUESS_FLOOR};
        let mib = 1u64 << 20;
        let config = PlaneConfig { io: 1, work: 2, budget: None, starve_after: 8 };
        let history = blaze_history::History::new(blaze_history::Config::default());
        // No task runs: the batch's type only names one.
        type Idle = Each<fn(u32) -> Option<Did<u32>>>;
        let plane: Plane<Idle, _> =
            Plane::new(config, history, Runner::Inline(Inline::new(4))).unwrap();
        let sized = |plane: &Plane<Idle, _>, source| plane.forecast(kinds::COMPILE, None, source, 0).0;
        // 4 MiB of source at the unseen rate: 4 GiB, though the kind was never seen.
        assert_eq!(sized(&plane, 4 * mib), Forecast { bytes: 4096 * mib, basis: Basis::Unseen });
        // A small unit stays at the floor.
        assert_eq!(sized(&plane, 4 << 10).bytes, GUESS_FLOOR);

        // Once the kind is measured, its peak caps the guess again.
        let mut history = blaze_history::History::new(blaze_history::Config::default());
        history.observe(kinds::COMPILE, None, 1_000, 512 * mib);
        let config = PlaneConfig { io: 1, work: 2, budget: None, starve_after: 8 };
        let plane: Plane<Idle, _> =
            Plane::new(config, history, Runner::Inline(Inline::new(4))).unwrap();
        assert_eq!(sized(&plane, 4 * mib).bytes, 512 * mib);

        // Under a budget of 2 GiB over 2 work threads, 4 GiB is held to the share of 1 GiB.
        let budget = Budget { bytes: 2 << 30, from: "the test's budget".to_owned(), kept_back: 0 };
        let config = PlaneConfig { io: 1, work: 2, budget: Some(budget), starve_after: 8 };
        let history = blaze_history::History::new(blaze_history::Config::default());
        let plane: Plane<Idle, _> =
            Plane::new(config, history, Runner::Inline(Inline::new(4))).unwrap();
        assert_eq!(sized(&plane, 4 * mib).bytes, 1 << 30);
    }

    /// Runs `plane` until each of `ids` settled, saying which did well.
    fn settle<W: Work, P: Predict>(plane: &mut Plane<W, P>, ids: &[NodeId]) -> Vec<(NodeId, bool)> {
        let (mut happened, mut settled) = (Vec::new(), Vec::new());
        while settled.len() < ids.len() {
            plane.poll(Duration::ZERO);
            plane.happened(&mut happened);
            for it in happened.drain(..) {
                if let Happened::Settled { id, ok, .. } = it
                    && ids.contains(&id)
                {
                    settled.push((id, ok));
                }
            }
        }
        settled.sort_by_key(|it| it.0);
        settled
    }

    /// A program's build is taught by its program's key and never single-flighted by it: two
    /// builds of one program both run. One the history has not measured is forecast by its
    /// size, a guess held to a thread's share (never the whole budget, alone: the smoke's cold
    /// build, 2026-11-12), and is measured, after which the program is forecast by its own
    /// record.
    #[test]
    fn a_program_is_admitted_by_its_own_measure_and_one_never_measured_by_its_size() {
        let budget = Budget { bytes: 1 << 30, from: "the test's budget".to_owned(), kept_back: 0 };
        let config = PlaneConfig { io: 1, work: 2, budget: Some(budget), starve_after: 8 };
        let history = blaze_history::History::new(blaze_history::Config::default());
        let mut plane = Plane::new(config, history, Runner::Inline(Inline::new(4))).unwrap();
        let (program, other) = (Key([5; 32]), Key([6; 32]));
        let node = |name| NodeRef {
            kind: NodeKind::Codegen,
            name,
            deps: &[],
            pool: Pool::Work,
            cost: 1,
            key: None,
            memory: 0,
        };
        assert!(!plane.predict().measured(kinds::PROGRAM, Some(program)));

        let request = plane.open(Duration::ZERO, true, false);
        let mut fresh = plane.fresh(2, 16, 0);
        let first =
            plane.push_program(&mut fresh, node("app"), kinds::PROGRAM, 0, program, 4 << 10);
        let again =
            plane.push_program(&mut fresh, node("app again"), kinds::PROGRAM, 1, program, 4 << 10);
        plane.add(Duration::ZERO, request, fresh, Some(Each(built)), false).unwrap();
        assert_eq!(plane.basis(first), Basis::Unseen);
        assert_eq!(plane.graph.memory_of(first), Some(blaze_graph::forecast::GUESS_FLOOR));
        assert_eq!(settle(&mut plane, &[first, again]), [(first, true), (again, true)]);
        assert!(plane.predict().measured(kinds::PROGRAM, Some(program)));
        assert!(
            !plane.predict().measured(kinds::PROGRAM, Some(other)),
            "another program is not measured by it"
        );
        let record =
            plane.predict().record(kinds::PROGRAM, &program).expect("its runs teach its key");
        assert_eq!(record.seen, 2);
        assert_eq!(plane.forecast(kinds::PROGRAM, Some(program), 4 << 10, 0).0.basis, Basis::Own);
    }
}
