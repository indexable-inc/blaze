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
//! nothing any other task held ([`admission::run_as`]), a loop task's time from its start to its
//! end; a run that replays what was kept teaches nothing ([`Plane::replays`]). What a task held
//! at most also teaches [`admission::Peaks`], which forecasts the next of its kind and size
//! class; the process's gate (`blaze_mem::gate`) admits every page as it is committed, so a
//! forecast decides only when a task starts, never whether memory runs out.

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
    admission::{self, Predict, TaskKind},
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
    /// The process's gate wounded it ([`blaze_mem::gate`]): its frame unwound at a safe point
    /// with [`blaze_mem::frame::Cancelled`], everything it held is back, and the plane makes it
    /// ready again at its age ([`PlaneInput::Yielded`]). Never posted again where it ran: a
    /// wounded task that starts again goes through the plane and the gate's start, which holds
    /// it while the elder it gave way to runs.
    Wounded,
    /// The gate held it at its start ([`blaze_mem::gate::HeldBack`]): it did not run and holds
    /// nothing. The plane keeps it ready but unstarted ([`PlaneInput::Yielded`] with `held`)
    /// until the gate names it unheld ([`PlaneInput::Unheld`], from [`Plane::poll`]).
    Held,
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
    /// Its forecast as admission started it ([`blaze_graph::plane::Admitted::forecast`]): whole
    /// quanta, 0 at or under the standing frame. For its `task` span, against its peak.
    pub forecast: u64,
    /// Whether the forecast is its kind and size class's peak record, not one quantum.
    pub recorded: bool,
    /// Its rank at the process's gate ([`blaze_graph::Plane::rank`]): what its frame's task
    /// starts at ([`blaze_mem::frame::task_ranked`]), so a wounded task keeps its age and is not
    /// started again while the elder it gave way to runs. `None` posted outside a plane.
    pub rank: Option<blaze_mem::gate::Ranked>,
}

/// Where a forecast came from, as a `task` and `admission` span names it: the peak record of
/// its kind and size class ([`admission::Peaks`]), or one quantum for a pair never recorded.
fn basis_name(recorded: bool) -> &'static str {
    match recorded {
        true => "peak record",
        false => "one quantum",
    }
}

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
    let Posting { id, arg, build, kind, forecast, recorded, rank, .. } = task;
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
        granted: forecast,
        passed: 0,
        basis: blaze_profile::span::Name::kept(basis_name(recorded)),
        krate,
        unit,
    }
    .open();
    let start = Instant::now();
    // The gate's word unwinds through the frame's task, not inside it: its end tells a wounded
    // task from one that ended only while the unwind passes it (`blaze_mem::gate`'s end), so
    // the frame's own catch lets `Cancelled` and `TooBig` go on, and the outer one takes them.
    let run = || {
        let _job = InJob::enter();
        match catch_unwind(AssertUnwindSafe(|| work.run(arg, readers))) {
            Err(word) if of_the_gate(&*word) => std::panic::resume_unwind(word),
            other => other,
        }
    };
    // Ranked, it never waits at the gate: held back, `run` does not run and the thread is
    // free at once ([`admission::run_ranked`]).
    let gated = catch_unwind(AssertUnwindSafe(|| match rank {
        Some(rank) => admission::run_ranked(kind, rank, run),
        None => Ok(admission::run_as(kind, run)),
    }));
    let ran_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let (result, spent) = match gated {
        Ok(Ok(ran)) => ran,
        Ok(Err(held)) => {
            span.ended(blaze_profile::span::TaskEnded {
                peak: None,
                heap_allocated: None,
                heap_freed: None,
            });
            // Held at its start behind an elder or the room: nothing ran, nothing is held,
            // and the plane holds it until the gate names it unheld (`Plane::poll`).
            debug_assert_eq!(rank.map(|it| it.task), Some(held.task), "{held}");
            return (Some(Ended::Held), 0, ran_ns, Vec::new());
        }
        Err(word) => {
            span.ended(blaze_profile::span::TaskEnded {
                peak: None,
                heap_allocated: None,
                heap_freed: None,
            });
            // Wounded: everything it held is back, and the plane makes it ready again at its
            // age. Too big: its node fails naming the task, its bytes and the ceiling.
            // A panic of the frame's task itself (outside `run`) fails it as any panic does.
            let ended = if word.is::<blaze_mem::frame::Cancelled>() {
                Ended::Wounded
            } else if let Some(too_big) = word.downcast_ref::<blaze_mem::budget::TooBig>() {
                // alloc: a failure, once
                Ended::Panicked(Panic { message: Some(too_big.to_string()), at: None })
            } else {
                Ended::Panicked(panicked(&*word))
            };
            return (Some(ended), 0, ran_ns, Vec::new());
        }
    };
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

/// Whether an unwind is the process gate's word to its task ([`blaze_mem::gate`]): give way
/// ([`blaze_mem::frame::Cancelled`]) or fail as too big ([`blaze_mem::budget::TooBig`]). Those
/// pass the task's own catch, so they reach the frame's task end as unwinds.
// dyn: a panic payload, read once per unwound task
fn of_the_gate(payload: &(dyn std::any::Any + Send)) -> bool {
    payload.is::<blaze_mem::frame::Cancelled>() || payload.is::<blaze_mem::budget::TooBig>()
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
    /// Each node's unit's source bytes, 0 for none ([`Plane::push_sized`]): its size class,
    /// which its peak record is kept under ([`admission::Peaks`]).
    sizes: Vec<u64>,
    live: u32,
    work: Option<Arc<W>>,
    reads: bool,
    /// Its request's number, which its tasks' spans carry.
    request: u32,
}

/// What gives the first request's number when the plane first opens one.
// dyn: once a daemon, the history's open it waits for
pub type Later = Box<dyn FnOnce() -> Result<u32, super::observe::Unopened> + Send + Sync>;

#[cfg(test)]
thread_local! {
    /// The planted bug of the lazy numbering's control: [`Plane::number`] asks for the number
    /// at once, as the start did when it waited for the history.
    static ASKS_AT_NUMBER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// How the plane numbers its requests ([`Plane::number`]).
pub enum Numbered {
    /// From this number on.
    From(u32),
    /// From the number this gives, asked at the first [`Plane::open`].
    Later(Later),
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
    /// The first request's number, asked at the first [`Plane::open`] ([`Numbered::Later`]).
    later: Option<Later>,
    /// Why the requests could not be numbered, for the daemon to stop on
    /// ([`Plane::unnumbered`]).
    unnumbered: Option<super::observe::Unopened>,
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
    /// Each thread task failed while its thread still runs it, stuck where nothing can unwind
    /// it ([`Plane::abandon`]): its late end, if one ever comes, is dropped.
    abandoned: HashSet<NodeId, Global>,
    /// The one predictor of memory (memory.md 4.3): the latest peak a task of each kind and
    /// size class held, read at push as its forecast (one quantum when never recorded), written
    /// as a task ends. Throughput only: a forecast too low costs a wound, never the ceiling.
    peaks: admission::Peaks,
    /// The process's gate ([`blaze_mem::gate::Gate`]) and the ceiling it was installed under,
    /// with what says so: what the plane tells free room from ([`Plane::set_room`]). `None` for
    /// a plane no gate admits (a simulation's): memory then holds no task back.
    gate: Option<(&'static blaze_mem::gate::Gate, Budget)>,
    /// What the machine's ledger last held for this process ([`Plane::set_room`]); `None`
    /// until it first said.
    room: Option<u64>,
    /// The forecast of each running task the gate knows by rank, by its age (its
    /// `blaze_mem::gate::TaskId`): what [`Plane::tell_room`] counts as still to come.
    /// Inserted as it is posted, removed as it is taken back.
    forecasts: HashMap<u64, u64, Global>,
    /// Each task the gate held at its start, by its age, to the node the plane knows it by:
    /// what [`Plane::unheld_now`] tells the plane is unheld.
    held_at_gate: HashMap<u64, NodeId, Global>,
    /// The ages the gate let go before their held start reached the loop ([`Ended::Held`]).
    unheld_early: HashSet<u64, Global>,
    /// [`blaze_mem::gate::Gate::take_unheld`]'s buffer, reused.
    unheld: Vec<blaze_mem::budget::TaskId>,
    /// Each node ready and not started: its `admission` span, open from the step it became
    /// ready, and that step's time ([`Plane::admitting`]).
    admitting: HashMap<NodeId, Admitting, Global>, // alloc: the plane's, an entry a ready node
    /// The thread tasks posted and not yet taken whose kind is codegen's
    /// (`admission::tag_of`: a library's, a unit's or a program's codegen, a reach): each may
    /// walk a program over codegen's kept walks ([`Plane::walks_in_flight`]).
    walking: usize,
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

pub(crate) fn ns(at: Duration) -> u64 {
    u64::try_from(at.as_nanos()).unwrap_or(u64::MAX)
}

/// The peak record's key of a task of `kind` over `source` bytes of its unit's source (0: not
/// sized): its kind's code id and its input's size class (memory.md 4.3, rule 65: per kind and
/// size class, never per crate).
fn peak_key(kind: TaskKind, source: u64) -> (u64, u8) {
    (u64::from(kind.0), admission::class_of(source))
}

impl<W: Work, P: Predict> Plane<W, P> {
    pub fn new(config: PlaneConfig, predict: P, runner: Runner<W>) -> Result<Plane<W, P>, Error> {
        Ok(Plane {
            graph: Graph::new(config)?,
            predict,
            runner,
            batches: BTreeMap::new(),
            next_node: 0,
            next_request: 0,
            later: None,
            unnumbered: None,
            happened: Vec::new(),
            ran: Vec::new(),
            said: Vec::new(),
            now_ns: 0,
            loops: BTreeMap::new(),
            replays: HashSet::new_in(Global), // alloc: the plane's, once
            abandoned: HashSet::new_in(Global),
            peaks: admission::Peaks::new(),
            gate: None,
            room: None,
            forecasts: HashMap::new_in(Global), // alloc: the plane's, once
            held_at_gate: HashMap::new_in(Global), // alloc: the plane's, once
            unheld_early: HashSet::new_in(Global), // alloc: the plane's, once
            unheld: Vec::new(),
            admitting: HashMap::new_in(Global), // alloc: the plane's, once
            walking: 0,
            folds: Folds::new(),
        })
    }

    /// From now on the plane admits under `gate`, the process's, installed under `ceiling`:
    /// the room it tells the plane is the gate's limit less what it has committed
    /// ([`Plane::set_room`]). Once, as the daemon starts, before any request.
    pub fn admit_under(&mut self, gate: &'static blaze_mem::gate::Gate, ceiling: Budget) {
        self.gate = Some((gate, ceiling));
    }

    /// What the gate counts as committed now, and what the head-of-line task memory holds
    /// back wants past it ([`blaze_graph::Plane::wanted`]): what the machine's ledger is asked
    /// to cover (`super::daemon::fit_room`). `(0, 0)` with no gate.
    pub fn gate_needs(&self) -> (u64, u64) {
        match &self.gate {
            Some((gate, _)) => (gate.committed(), self.graph.wanted()),
            None => (0, 0),
        }
    }

    /// The machine's ledger holds `held` bytes for this process at `at`: the gate's room
    /// ([`blaze_mem::gate::Gate::room`], which marks the youngest holders when it falls under
    /// what they hold; their frames unwind at their next safe point), then the plane is told
    /// what the gate leaves free ([`PlaneInput::Room`]): its limit, the ceiling or the room,
    /// less what it has committed. Nothing with no gate.
    pub fn set_room(&mut self, at: Duration, held: u64) {
        let Some((gate, _)) = self.gate else { return };
        gate.room(held);
        self.room = Some(held);
        self.tell_room(at);
        // A room that grew lets held tasks go.
        self.unheld_now(at);
    }

    /// Tells the plane what the gate leaves free for new starts now ([`PlaneInput::Room`],
    /// `blaze_graph::admission::free_for_starts`): its limit (the ceiling, or the machine's
    /// room under it) less what it has committed, the workers' overdrafts and what each
    /// running task is still forecast to take past what it holds. Without the last, a start
    /// would be forecast into room an earlier start has not reached yet. One lock of the gate
    /// and O(running tasks). Nothing with no gate.
    fn tell_room(&mut self, at: Duration) {
        let Some((gate, _)) = self.gate else { return };
        let forecasts = &self.forecasts;
        let free = gate.free_for_starts(|task| forecasts.get(&task.0).copied().unwrap_or(0));
        self.step(PlaneInput::Room { at_ns: ns(at), free })
            .unwrap_or_else(|error| panic!("the plane refused the gate's room: {error}"));
    }

    /// What the gate counts as committed (memory.md 4.2: every commit is counted as it is
    /// reserved, so it is at least the footprint the gate answers for), the ceiling it was
    /// installed under (`None` with no gate), and the machine's room under it while the
    /// machine's ledger holds this process lower.
    pub fn memory(&self) -> (u64, Option<u64>, Option<u64>) {
        let Some((gate, ceiling)) = &self.gate else { return (0, None, None) };
        let room = self.room.filter(|room| *room < ceiling.bytes);
        (gate.committed(), Some(ceiling.bytes), room)
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

    /// Each running task forecast memory, by its node and name, to `each`: what a task held
    /// back for memory waits on. A task several requests share comes once each.
    pub fn granted_running<'a>(&'a self, mut each: impl FnMut(NodeId, &'a str)) {
        self.graph.unsettled(|it| {
            if matches!(it.state, NodeState::Running)
                && self.graph.admitted(it.id).is_some_and(|admitted| admitted.forecast > 0)
            {
                each(it.id, it.name);
            }
        });
    }

    /// The ceiling the process's gate was installed under, and what says so; `None` with no
    /// gate ([`Plane::admit_under`]).
    pub fn budget(&self) -> Option<&Budget> {
        self.gate.as_ref().map(|(_, ceiling)| ceiling)
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
        Fresh { nodes, tasks, keys, sizes, count }
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
        fresh.nodes.push(node)
    }

    /// The next node of `fresh`, as [`Plane::push`], but predicted and taught by `learn`, a
    /// key of the predictor's alone: the node has no key of the plane's, so nothing
    /// single-flights or memoizes it by `learn`. A unit's phase is so given the cost of its own
    /// last runs, not its kind's mean, and the plane starts the longest chain first by what
    /// each node takes (docs/v2/plan/design/scheduler.md §6, step 1).
    ///
    /// Its memory is its kind's peak record ([`Plane::admit`]), never its key's: memory is
    /// recorded per kind and size class (rule 65).
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
        let cost = self.admit(&mut node, kind, learn, 0);
        if let Some(cost) = cost {
            node.cost = cost;
        }
        fresh.tasks.push(task);
        fresh.keys.push(learn);
        fresh.sizes.push(0);
        fresh.nodes.push(node)
    }

    /// The next node of `fresh`, a phase of a unit of `source` bytes of Rust source, predicted
    /// and taught by `learn` as [`Plane::push_learned`] is, its memory the peak record of its
    /// kind and its source's size class ([`Plane::admit`]): one large crate's codegen never
    /// makes a small one's forecast. `source` 0 (not read) predicts as [`Plane::push_learned`].
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
        let cost = self.admit(&mut node, kind, Some(learn), source);
        if let Some(cost) = cost {
            node.cost = cost;
        }
        fresh.tasks.push(task);
        fresh.keys.push(Some(learn));
        fresh.sizes.push(source);
        fresh.nodes.push(node)
    }

    /// Gives `node`, the next of `fresh`, its memory: the one predictor's forecast (memory.md
    /// 4.3), the latest peak a task of its kind and `source`'s size class held, one quantum
    /// when none was recorded, and never under what `node` says itself. The plane rounds it to
    /// quanta and takes 0 at or under a worker's standing frame. Throughput only: a forecast
    /// too low costs a wound at the gate, never the ceiling, and none is refused for its
    /// forecast (only the gate's [`blaze_mem::budget::TooBig`], at run time, fails a task for
    /// memory). Its predicted cost, from `learn`'s record, for the caller.
    ///
    /// History, deleted 2026-11-13: forecasts from a key's own record, bytes per KiB of
    /// source, a kind's windowed peak, a thread's share of the budget and a correction factor;
    /// a run alone past the budget; a refusal at push for a record over it.
    fn admit(
        &self,
        node: &mut NodeRef<'_>,
        kind: TaskKind,
        learn: Option<Key>,
        source: u64,
    ) -> Option<u64> {
        let (code, class) = peak_key(kind, source);
        node.memory = node.memory.max(self.peaks.forecast(code, class));
        self.predict.predict(kind, learn).cost
    }

    /// The kind of node `id`'s task, while the plane holds it and it has not settled.
    pub fn kind_of(&self, id: NodeId) -> Option<TaskKind> {
        match self.task(id)?.1 {
            Task::Thread { kind, .. } | Task::Loop { kind } => Some(kind),
            Task::Gone => None,
        }
    }

    /// The peak record admission forecasts from ([`admission::Peaks`]): the one home of what
    /// a task of a kind and size class held, for the daemon's image to keep
    /// ([`admission::Peaks::encode`]).
    pub fn peaks(&self) -> &admission::Peaks {
        &self.peaks
    }

    /// The peak record a daemon's image kept ([`admission::Peaks::decode`]), taken in place of
    /// the empty one, before any request: a restarted daemon forecasts as the one before it.
    pub fn restore_peaks(&mut self, peaks: admission::Peaks) {
        self.peaks = peaks;
    }

    /// Whether a task with memory was held back for it as the plane last dispatched.
    pub fn held_any_back(&self) -> bool {
        self.graph.held_back()
    }

    /// The next node of `fresh`, a part of a program's build of `kind` ([`kinds::PROGRAM`], its
    /// codegen, or [`kinds::PROGRAM_LINK`], its link), task `arg` of the batch's work. Its cost
    /// is predicted and taught by `program`, the key of that part of the program, which the
    /// plane never sees: the node has no key, so nothing single-flights or memoizes it by that.
    /// Its memory is forecast as any task's is ([`Plane::admit`]), by `kind` and the size class
    /// of `source`, its root unit's bytes of source.
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
        let cost = self.admit(&mut node, kind, Some(program), source);
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

    /// Whether the forecast of node `id`'s task, of `kind`, is a peak record of its kind and
    /// size class, not one quantum ([`Plane::admit`]): for its spans.
    fn recorded(&self, id: NodeId, kind: TaskKind) -> bool {
        let (code, class) = peak_key(kind, self.size(id));
        self.peaks.peak(code, class).is_some()
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

    /// How requests are numbered ([`super::observe::Observe::numbering`]): from the daemon's
    /// first build's number on, past every build its history names, so a request's number is
    /// its build's id in the flight recorder and the history. Never 0. [`Numbered::Later`] is
    /// asked at the first [`Plane::open`], not before: a daemon answering from certificates
    /// opens none, and its start waits for nothing of the history.
    pub fn number(&mut self, by: Numbered) {
        match by {
            Numbered::From(first) => {
                self.next_request = first.max(1);
                self.later = None;
            }
            #[cfg(test)]
            Numbered::Later(later) if ASKS_AT_NUMBER.get() => {
                self.next_request = later().unwrap_or(1).max(1);
            }
            Numbered::Later(later) => self.later = Some(later),
        }
    }

    /// Why the requests could not be numbered, once: the history did not open, so the daemon
    /// stops on it (the request opened meanwhile was numbered 1 and is written to no history).
    pub fn unnumbered(&mut self) -> Option<super::observe::Unopened> {
        self.unnumbered.take()
    }

    /// A request, of weight 1: every job's share of the threads is the same. An `idle` one runs
    /// on what no other can use until it is promoted ([`Plane::promote`]).
    /// The next request number, taken for a build the plane runs nothing of (a solo answer the
    /// daemon took from its kept rows, `Daemon::solo_taken`): the one sequence of build ids. A
    /// [`Numbered::Later`] numbering is asked here as at the first [`Plane::open`].
    pub fn number_unplanned(&mut self) -> u32 {
        if let Some(later) = self.later.take() {
            self.next_request = match later() {
                Ok(first) => first.max(1),
                Err(unopened) => {
                    self.unnumbered = Some(unopened);
                    1
                }
            };
        }
        let number = self.next_request;
        self.next_request = self.next_request.wrapping_add(1).max(1);
        number
    }

    pub fn open(&mut self, at: Duration, keep_going: bool, idle: bool) -> RequestId {
        let request = RequestId(self.number_unplanned());
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
        let Fresh { nodes, tasks, keys, sizes, count } = fresh;
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

    /// The thread task of node `id` is failed with `why` while its thread still runs it: the
    /// task watchdog found that thread stuck where nothing can unwind it (a lock or a signal
    /// that never comes, `serve::stuck`). Its request goes on without it and ends failed with
    /// `why`; its task's end, if the thread ever comes back, is dropped ([`Plane::poll`]).
    /// `false`, doing nothing, when `id` is no thread task that runs now (it ended since the
    /// watchdog looked, or is a loop task, which the loop's own guard bounds).
    pub fn abandon(&mut self, at: Duration, id: NodeId, why: String) -> bool {
        if !matches!(self.task(id), Some((_, Task::Thread { .. }))) || self.abandoned.contains(&id)
        {
            return false;
        }
        let mut running = false;
        self.graph.unsettled(|it| {
            running |= it.id == id && matches!(it.state, NodeState::Running);
        });
        if !running {
            return false;
        }
        let input = PlaneInput::Done {
            at_ns: ns(at),
            id,
            outcome: Outcome::Failed { why },
            added: Vec::new(),
            group: None,
            peak: 0,
        };
        self.step(input)
            .unwrap_or_else(|error| panic!("the plane refused the end of stuck task {id}: {error}"));
        self.abandoned.insert(id);
        true
    }

    /// Takes the thread tasks that ended into the plane: a task done teaches its peak to
    /// [`admission::Peaks`]; one the gate wounded is ready again, keeping its rank. Nothing is
    /// measured on a timer and nothing purged: the gate counts every committed page as it is
    /// committed, so what is free is the gate's to say ([`Plane::set_room`]).
    pub fn poll(&mut self, at: Duration) {
        let mut ran = std::mem::take(&mut self.ran);
        self.runner.take(&mut ran);
        for Ran { task, ended, peak, ran_ns, puts } in ran.drain(..) {
            let id = task.id;
            self.walks(task.kind, false);
            // Failed already, as its thread was found stuck: its late end is of nothing now.
            if self.abandoned.remove(&id) {
                if let Some(rank) = task.rank {
                    self.forecasts.remove(&rank.task.0);
                    self.unheld_early.remove(&rank.task.0);
                }
                self.replays.remove(&id);
                continue;
            }
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
                Ended::Wounded => {
                    // The gate wounded it and it unwound at a safe point, its frames given
                    // back: ready again, its age kept, so its next start waits at the gate
                    // behind the elder it gave way to, never before it (wound-wait).
                    if let Some(rank) = task.rank {
                        self.forecasts.remove(&rank.task.0);
                    }
                    let input = PlaneInput::Yielded { at_ns: ns(at), id, held: false };
                    self.step(input).unwrap_or_else(|error| {
                        panic!("the plane refused that the gate wounded task {id}: {error}")
                    });
                    continue;
                }
                Ended::Held => {
                    let Some(rank) = task.rank else {
                        panic!("task {id} was held at the gate's start with no rank, which only a ranked start can be")
                    };
                    let age = rank.task.0;
                    self.forecasts.remove(&age);
                    // The gate may have let it go already, named by a `take_unheld` before this
                    // end reached the loop: then it is ready at once, never held for a word
                    // that came and went.
                    let held = !self.unheld_early.remove(&age);
                    if held {
                        self.held_at_gate.insert(age, id);
                    }
                    let input = PlaneInput::Yielded { at_ns: ns(at), id, held };
                    self.step(input).unwrap_or_else(|error| {
                        panic!("the plane refused that the gate held task {id} at its start: {error}")
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
            if let Some(rank) = task.rank {
                // Ended for good: a word of the gate that named it while it ran (a wounded
                // task's park let go as it started again) is of nothing now.
                self.forecasts.remove(&rank.task.0);
                self.unheld_early.remove(&rank.task.0);
            }
            // What its work held at most, as its memory account was charged: its own and its
            // fan-out's bytes, never another task's, so it is what its key learns as it is.
            let key = self.key(id);
            if !self.replays.remove(&id) {
                self.predict.observe(task.kind, key, ran_ns, peak);
                // A task that failed or panicked may have stopped short of its peak: only a
                // whole run teaches the forecast (a short peak would under-forecast the next).
                if peak > 0 && matches!(outcome, Outcome::Built | Outcome::Cached { .. }) {
                    let (code, class) = peak_key(task.kind, self.size(id));
                    self.peaks.observe(code, class, peak);
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
        // A task's end and a wound's unwind gave pages back: the gate says what is free.
        self.tell_room(at);
        self.unheld_now(at);
    }

    /// The tasks the gate let go since ([`blaze_mem::gate::Gate::take_unheld`]), each told
    /// the plane as [`PlaneInput::Unheld`]: ready to start again. What lets a task go is an end
    /// or a give-way of another task, or the machine's room growing ([`Plane::set_room`]), and
    /// each of those is a step of this loop that calls this, so no word of the gate waits for
    /// a turn nothing brings. A task named before its own held start reached the loop
    /// (its end is still on its way) is kept, and that end makes it ready at once.
    fn unheld_now(&mut self, at: Duration) {
        let Some(&(gate, _)) = self.gate.as_ref() else { return };
        if !gate.any_unheld() {
            return;
        }
        let mut unheld = std::mem::take(&mut self.unheld);
        gate.take_unheld(&mut unheld);
        for task in unheld.drain(..) {
            match self.held_at_gate.remove(&task.0) {
                Some(id) => {
                    self.step(PlaneInput::Unheld { at_ns: ns(at), id }).unwrap_or_else(|error| {
                        panic!("the plane refused that the gate let task {id} go: {error}")
                    });
                }
                None => {
                    self.unheld_early.insert(task.0);
                }
            }
        }
        self.unheld = unheld;
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
    /// by a rule of admission, and the stall names which); `None` with no gate, where
    /// admission holds nothing back for memory.
    pub fn admission_says(&self) -> Option<String> {
        use std::fmt::Write as _;
        let &(gate, _) = self.gate.as_ref()?;
        let graph = &self.graph;
        let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
        // alloc: a stall's line, once
        let mut said = format!(
            "memory admission: the gate has {:.2} GiB committed of a ceiling of {:.2} GiB; running tasks are forecast {:.2} GiB",
            gib(gate.committed()),
            gib(gate.ceiling()),
            gib(graph.running_forecast()),
        );
        if let Some(room) = self.room.filter(|room| *room < gate.ceiling()) {
            _ = write!(said, "; the machine's ledger holds this process to {:.2} GiB", gib(room));
        }
        match graph.free() {
            Some(free) => _ = write!(said, "; {:.2} GiB free for new starts", gib(free)),
            None => said.push_str("; the gate has not said its room yet, so nothing waits for memory"),
        }
        if graph.held_back() {
            _ = write!(
                said,
                "; the head-of-line task is held back for a forecast of {:.2} GiB, and no task with a forecast starts before it",
                gib(graph.wanted()),
            );
        }
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
                    self.settled(id);
                    let (ok, why, skipped) = match state {
                        NodeState::Built | NodeState::Cached { .. } => (true, None, None),
                        NodeState::Failed { why } => (false, Some(why), None),
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
                // allows: none is stopped mid-run. The gate's wounds reach a task in its own
                // frames (it unwinds at its next safe point, `Ended::Wounded`), so the daemon
                // never tells the plane a `Wound` and no `Yield` here is one.
                PlaneOutput::Yield { .. } => {}
                // A wounded task the gate keeps parked under its age, every node of it skipped
                // before it ran again: the gate forgets it, or the younger tasks it holds
                // behind that age would wait for an elder that never comes back. One that gave
                // way as idle work ended at the gate as it unwound, unparked: nothing to forget.
                PlaneOutput::Dropped { age } => {
                    self.held_at_gate.remove(&age);
                    self.unheld_early.remove(&age);
                    let Some((gate, _)) = self.gate else { continue };
                    let task = blaze_mem::budget::TaskId(age);
                    let _forgotten: bool = gate.end_parked(task).unwrap_or_else(|misuse| {
                        panic!("the gate refused to forget the dropped task of age {age}: {misuse:?}")
                    });
                }
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
    /// ([`blaze_graph::Plane::waits_for_memory`]: its task's forecast does not fit what the
    /// gate leaves free, with a thread of its pool free), and says it once, with the bytes:
    /// what the task is forecast and what is free. Walked only after a step whose dispatch
    /// held a task back for memory.
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
                free = self.graph.free().unwrap_or(u64::MAX),
                running_forecast = self.graph.running_forecast(),
                "admission holds a ready task back for memory"
            );
        }
    }

    /// Node `id` started: its wait for admission ends, saying what held it back. Memory, when
    /// its task did not fit in a step of its wait ([`Plane::held_for_memory`]); else a thread
    /// of its pool; nothing, when it started in the step it became ready. No task passes
    /// another for memory (head of line), so `passed` is always 0.
    fn admitted(&mut self, id: NodeId) {
        let Some(Admitting { span, since_ns, memory }) = self.admitting.remove(&id) else {
            return;
        };
        let Some(admitted) = self.graph.admitted(id) else { return };
        let kind = match self.task(id) {
            Some((_, Task::Thread { kind, .. } | Task::Loop { kind })) => Some(kind),
            Some((_, Task::Gone)) | None => None,
        };
        let waited_for = if since_ns == self.now_ns {
            "nothing"
        } else if memory {
            "memory"
        } else {
            match admitted.pool {
                Pool::Io => "an io thread",
                Pool::Work => "a work thread",
            }
        };
        span.ended(blaze_profile::span::Admitted {
            waited_for: blaze_profile::span::Name::kept(waited_for),
            passed: 0,
            granted: admitted.forecast,
            basis: blaze_profile::span::Name::kept(basis_name(
                kind.is_some_and(|kind| self.recorded(id, kind)),
            )),
        });
    }

    /// The node the plane runs: its thread task posted, or its loop task handed to the loop.
    fn run(&mut self, id: NodeId, pool: Pool, near: Option<u32>, critical: bool) {
        match self.task(id) {
            Some((batch, Task::Thread { kind, arg })) => {
                let forecast = self.graph.admitted(id).map_or(0, |it| it.forecast);
                // Named at the gate by its age, which it keeps as it gives way, so a wounded
                // task started again waits behind its elder, never before it. Ranked only under
                // a gate: a simulation's plane admits under none.
                let rank = self.gate.as_ref().and_then(|_| self.graph.rank(id)).map(|(band, age)| {
                    blaze_mem::gate::Ranked { task: blaze_mem::budget::TaskId(age.0), band, age }
                });
                let task = Posting {
                    id,
                    pool,
                    near,
                    critical,
                    kind,
                    arg,
                    reads: batch.reads,
                    build: batch.request,
                    forecast,
                    recorded: self.recorded(id, kind),
                    rank,
                };
                if batch.reads {
                    self.happened.push(Happened::Post { task, again: false });
                } else {
                    let work = batch.work.clone().expect("checked by add");
                    self.happened.push(Happened::Started { id });
                    self.walks(task.kind, true);
                    self.runner.post(task, work, W::Readers::default());
                }
                if let Some(rank) = rank {
                    self.forecasts.insert(rank.task.0, forecast);
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
    use blaze_graph::admission::Prediction;
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
            forecast: 0,
            recorded: false,
            rank: None,
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
                    forecast: 0,
                    recorded: false,
                    rank: None,
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
            forecast: 0,
            recorded: false,
            rank: None,
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
                forecast: 0,
                recorded: false,
                rank: None,
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
            forecast: 0,
            recorded: false,
            rank: None,
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
            forecast: 0,
            recorded: false,
            rank: None,
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
        let config = PlaneConfig { io: 1, work: 2 };
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
        let config = PlaneConfig { io: 1, work: pool };
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

    /// The invariant: a node is forecast the peak its kind last held at its source's size
    /// class ([`admission::Peaks`]), one quantum (so nothing, at or under the standing frame)
    /// where none was recorded, never under what the node says itself; and no forecast refuses
    /// a node, however far past any ceiling (the gate's `TooBig` is the one refusal, at run
    /// time). A restored record forecasts as the one that was kept, so a restarted daemon
    /// admits as the one before it. Controls: the node of the same kind one size class up and
    /// the node of another kind of the same size are not forecast by that record, which a
    /// lookup by kind alone, or by size alone, would get wrong.
    #[test]
    fn a_node_is_forecast_its_kind_and_size_class_peak_and_none_is_refused_for_it() {
        use blaze_mem::budget::QUANTUM;
        let gib = 1u64 << 30;
        let config = PlaneConfig { io: 1, work: 2 };
        let history = blaze_history::History::new(blaze_history::Config::default());
        let mut plane = Plane::new(config, history, Runner::Inline(Inline::new(4))).unwrap();
        let small = 4u64 << 10;
        let mut peaks = admission::Peaks::new();
        // Codegen of a 4 KiB unit once held 120 GiB: past any machine this test runs on.
        let (code, class) = peak_key(kinds::COMPILE, small);
        peaks.observe(code, class, 120 * gib);
        // Round-tripped as the daemon's image keeps it.
        let kept = admission::Peaks::decode(&peaks.encode()).expect("the record it encoded");
        plane.restore_peaks(kept);

        let request = plane.open(Duration::ZERO, true, false);
        let node = |name, memory| NodeRef {
            kind: NodeKind::Codegen,
            name,
            deps: &[],
            pool: Pool::Work,
            cost: 1,
            key: None,
            memory,
        };
        let task = |kind, arg| Task::Thread { kind, arg };
        let mut fresh = plane.fresh(4, 64, 0);
        let recorded =
            plane.push_sized(&mut fresh, node("recorded", 0), task(kinds::COMPILE, 0), Key([1; 32]), small);
        let class_up = plane.push_sized(
            &mut fresh,
            node("a class up", 0),
            task(kinds::COMPILE, 1),
            Key([2; 32]),
            small * 2,
        );
        let other_kind = plane.push_sized(
            &mut fresh,
            node("another kind", 0),
            task(kinds::PROGRAM, 2),
            Key([3; 32]),
            small,
        );
        let says_more = plane.push_sized(
            &mut fresh,
            node("says more", 3 * gib),
            task(kinds::PROGRAM, 3),
            Key([4; 32]),
            small,
        );
        let ids = [recorded, class_up, other_kind, says_more];
        plane.add(Duration::ZERO, request, fresh, Some(Each(built)), false).unwrap();
        let forecast = |id| plane.graph.memory_of(id);
        assert_eq!(forecast(recorded), Some(admission::forecast_of(120 * gib)));
        assert!(plane.recorded(recorded, kinds::COMPILE));
        assert_eq!(forecast(class_up), Some(admission::forecast_of(QUANTUM)), "control: by class");
        assert!(!plane.recorded(class_up, kinds::COMPILE));
        assert_eq!(forecast(other_kind), Some(admission::forecast_of(QUANTUM)), "control: by kind");
        assert_eq!(forecast(says_more), Some(admission::forecast_of(3 * gib)));
        let settled = settle(&mut plane, &ids);
        assert!(settled.iter().all(|it| it.1), "no node is refused for its forecast: {settled:?}");
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
    /// builds of one program both run, and its runs teach its key's cost, not another
    /// program's. No peak recorded, it is forecast one quantum.
    #[test]
    fn a_program_is_taught_by_its_own_key_and_never_single_flighted_by_it() {
        let config = PlaneConfig { io: 1, work: 2 };
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
        assert!(!plane.recorded(first, kinds::PROGRAM));
        assert_eq!(
            plane.graph.memory_of(first),
            Some(admission::forecast_of(blaze_mem::budget::QUANTUM))
        );
        assert_eq!(settle(&mut plane, &[first, again]), [(first, true), (again, true)]);
        assert!(plane.predict().measured(kinds::PROGRAM, Some(program)));
        assert!(
            !plane.predict().measured(kinds::PROGRAM, Some(other)),
            "another program is not measured by it"
        );
        let record =
            plane.predict().record(kinds::PROGRAM, &program).expect("its runs teach its key");
        assert_eq!(record.seen, 2);
    }

    /// The invariant: a plane numbered later asks for its first number at its first open and
    /// not before, once; its requests are numbered from the answer on; an answer that is the
    /// history's failure numbers the request 1 and is kept, once, for the daemon to stop on,
    /// never a panic. Control: the planted `ASKS_AT_NUMBER` asks before any open, and the
    /// oracle sees it.
    #[test]
    fn a_plane_numbered_later_asks_once_at_its_first_open() {
        type Idle = Each<fn(u32) -> Option<Did<u32>>>;
        let plane = || -> Plane<Idle, _> {
            let config = PlaneConfig { io: 1, work: 2 };
            let history = blaze_history::History::new(blaze_history::Config::default());
            Plane::new(config, history, Runner::Inline(Inline::new(4))).unwrap()
        };
        let asked_before_open = |planted: bool| {
            ASKS_AT_NUMBER.set(planted);
            let mut plane = plane();
            let asked = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
            let counted = std::sync::Arc::clone(&asked);
            plane.number(Numbered::Later(Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(41)
            })));
            ASKS_AT_NUMBER.set(false);
            let before = asked.load(Ordering::SeqCst);
            assert_eq!(plane.open(Duration::ZERO, false, false), RequestId(41));
            assert_eq!(plane.open(Duration::ZERO, false, false), RequestId(42));
            assert_eq!(asked.load(Ordering::SeqCst), 1, "asked once");
            assert!(plane.unnumbered().is_none());
            before
        };
        assert_eq!(asked_before_open(false), 0, "not asked before the first open");
        assert_eq!(asked_before_open(true), 1, "the control: the planted plane asks before any open");

        let mut failed = plane();
        let unopened = super::super::observe::Unopened {
            dir: "/home/u/.cache/blaze/v2/profile".into(),
            why: super::super::observe::Unopen::Gone,
        };
        let said = unopened.clone(); // clone: compared below
        failed.number(Numbered::Later(Box::new(move || Err(said))));
        assert_eq!(failed.open(Duration::ZERO, false, false), RequestId(1));
        assert_eq!(failed.unnumbered(), Some(unopened));
        assert_eq!(failed.unnumbered(), None, "said once");
        assert_eq!(failed.open(Duration::ZERO, false, false), RequestId(2));

        let mut from = plane();
        from.number(Numbered::From(0));
        assert_eq!(from.open(Duration::ZERO, false, false), RequestId(1), "never 0");
    }
}
