//! The plane (docs/v2/plan/lanes/core/plane.md) as the daemon's loop steps it: every job's
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
use blaze_protocol::events::{Event, NodeState};
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
}

/// The daemon's: work tasks on the process's worker pool, IO tasks on threads of their own.
pub struct Threads<W: Work> {
    /// Every task's way back, shared by reference: a task posted counts no sender.
    done_tx: &'static Sender<Ran<W::Value>>,
    done: Receiver<Ran<W::Value>>,
    io: Sender<IoJob<W>>,
    running: usize,
    /// Of `running`, those posted to the worker pool.
    on_pool: usize,
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
    pub fn new(io: usize, work: usize) -> Threads<W> {
        let (done_tx, done) = bounded(io.max(1) + work.max(1) + 1);
        // alloc: one sender per runner, kept for the process's life so no task clones it.
        let done_tx: &'static Sender<Ran<W::Value>> = Box::leak(Box::new(done_tx));
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
                        _ = done_tx.send(Ran { task: job.task, ended, peak, ran_ns, puts });
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
    let Posting { id, arg, build, kind, granted, passed, .. } = task;
    let _build = blaze_profile::flight::building(build);
    // Named for the profile: what the task is, what admission granted and how often it held it
    // back, and as it ends the most it held, so a forecast that serialized the plane is a
    // query (`task_memory` in crates/blaze-profile/sql/examples.sql), not a guess.
    let span = blaze_profile::span::Task {
        kind: task_name(kind),
        node: u64::from(id.0),
        granted,
        passed: u64::from(passed),
    }
    .open();
    let start = Instant::now();
    let (result, peak) = admission::run_as(kind, || {
        let _job = InJob::enter();
        catch_unwind(AssertUnwindSafe(|| work.run(arg, readers)))
    });
    let ran_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    span.ended(blaze_profile::span::TaskEnded {
        peak: matches!(result, Ok(Some(_))).then_some(peak),
    });
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
fn in_a_job() -> bool {
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
    done: &'static Sender<Ran<W::Value>>,
) {
    let pool = blaze_graph::exec::shared();
    // The batch's work comes back with the result, for a task that gave way to be posted again
    // without counting another reference.
    let run = move || (run_measured(&*work, task, readers), work);
    let then = move |result: std::thread::Result<_>| match result {
        Ok(((Some(ended), peak, ran_ns, puts), _)) => {
            _ = done.send(Ran { task, ended, peak, ran_ns, puts })
        }
        Ok(((None, peak, ran_ns, puts), _)) if task.reads => {
            let ended = Ended::GaveWay { group: blaze_graph::pool::group() };
            _ = done.send(Ran { task, ended, peak, ran_ns, puts });
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
            _ = done.send(Ran {
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
    /// kind's recent ratios. Not kept in the image: a daemon starts from [`PER_SOURCE_KIB`]
    /// and learns again in its first build.
    per_kib: HashMap<TaskKind, admission::Estimate, Global>, // alloc: the plane's, an entry a kind
    /// Each node ready and not started: its `admission` span, open from the step it became
    /// ready, and that step's time ([`Plane::admitting`]).
    admitting: HashMap<NodeId, Admitting, Global>, // alloc: the plane's, an entry a ready node
}

/// A ready node's wait for admission, as its profile span says it ([`Plane::admitting`]).
struct Admitting {
    span: blaze_profile::span::Detached<blaze_profile::span::Admitted>,
    since_ns: u64,
}

/// What a unit's phase of a kind with no ratio measured yet is predicted to hold per KiB of
/// its source: 1 MiB, a thousand bytes for each byte of source. Never more than the kind's own
/// peak ([`Plane::push_sized`]), so a guess too high costs no more than the kind floor did.
pub const PER_SOURCE_KIB: u64 = 1 << 20;

/// The least a sized unit's phase is predicted to hold, however small its source: the
/// history's prediction for a kind never seen (`blaze_history::Config::unseen_bytes`).
pub const SIZED_FLOOR: u64 = 64 << 20;

/// Where a node's predicted peak came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Its own key's record: what its own last runs held, as charged to each run's memory
    /// account (`blaze_graph::arena::account`), never what the daemon held besides.
    Own,
    /// Its unit's source size times what its kind held per KiB lately ([`Plane::push_sized`]):
    /// a guess from other units.
    Sized,
    /// Its kind's peak, or a kind never seen's: a guess from other tasks.
    Kind,
}

/// The memory a node predicted to hold `peak` from `basis` asks of the plane under a budget of
/// `budget` bytes: its prediction, but a guess (from its kind or its size) over the whole budget
/// is the whole budget. Such a node then runs alone, once nothing granted memory runs, and its
/// run measures it; refusing it would fail a unit for what other tasks held (a kind's peak is
/// the largest of any task of it lately: 121.59 GB refused every build script of a test build,
/// nftest2). Only its own measured runs over the budget stay over it, which the plane refuses
/// ([`blaze_graph::Plane`]'s `too_big`), since running it could only exhaust the machine.
pub fn admitted(peak: u64, basis: Basis, budget: Option<u64>) -> u64 {
    match (basis, budget) {
        (Basis::Sized | Basis::Kind, Some(budget)) => peak.min(budget),
        _ => peak,
    }
}

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
            admitting: HashMap::new_in(Global), // alloc: the plane's, once
        })
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
        Fresh { nodes, tasks, keys, sizes, over: Vec::new(), count }
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
        let Prediction { cost, peak } = self.predict.predict(kind, learn);
        let basis = match learn.is_some_and(|key| self.predict.measured(kind, Some(key))) {
            true => Basis::Own,
            false => Basis::Kind,
        };
        self.admit(fresh, &mut node, peak, basis);
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
    /// kind held per KiB lately ([`Plane::per_kib`]; [`PER_SOURCE_KIB`] until measured), at
    /// least [`SIZED_FLOOR`] and at most the kind's peak. One whose key has a record is
    /// predicted the more of that record and its sized prediction. `source` 0 (not read)
    /// predicts as [`Plane::push_learned`].
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
        let Prediction { cost, peak } = self.predict.predict(kind, Some(learn));
        let sized = self.sized(kind, source);
        let (predicted, basis) = match self.predict.measured(kind, Some(learn)) {
            true if peak >= sized => (peak, Basis::Own),
            _ => (sized, Basis::Sized),
        };
        self.admit(fresh, &mut node, predicted, basis);
        if let Some(cost) = cost {
            node.cost = cost;
        }
        fresh.tasks.push(task);
        fresh.keys.push(Some(learn));
        fresh.sizes.push(source);
        fresh.nodes.push(node)
    }

    /// Gives `node`, the next of `fresh`, the memory it asks of the plane: `peak`, predicted
    /// from `basis`, as [`admitted`] bounds it by the budget. One whose own measured runs say
    /// more than the whole budget keeps that, for the plane to refuse it as it becomes ready,
    /// and is noted so the refusal says where the number came from.
    fn admit(&self, fresh: &mut Fresh, node: &mut NodeRef<'_>, peak: u64, basis: Basis) {
        let budget = self.budget().map(|it| it.bytes);
        node.memory = admitted(node.memory.max(peak), basis, budget);
        if budget.is_some_and(|budget| node.memory > budget) {
            let id = NodeId(fresh.nodes.first().0 + fresh.nodes.len() as u32);
            fresh.over.push((id, node.memory));
        }
    }

    /// The peak node `id` was refused by, its own measured runs' over the whole budget
    /// ([`Plane::admit`]), if it was noted and has not run; taken.
    fn take_over(&mut self, id: NodeId) -> Option<u64> {
        let (_, batch) = self.batches.range_mut(..=id.0).next_back()?;
        let at = batch.over.iter().position(|(it, _)| *it == id)?;
        Some(batch.over.swap_remove(at).1)
    }

    /// What a unit of `source` bytes of source is predicted to hold in a task of `kind`: its
    /// KiB times the kind's bytes per KiB, at least [`SIZED_FLOOR`], at most the kind's peak
    /// (or the floor, where that is less).
    fn sized(&self, kind: TaskKind, source: u64) -> u64 {
        let per_kib = self.per_kib(kind).unwrap_or(PER_SOURCE_KIB);
        let kind_peak = self.predict.predict(kind, None).peak;
        source
            .div_ceil(1024)
            .saturating_mul(per_kib)
            .max(SIZED_FLOOR)
            .min(kind_peak.max(SIZED_FLOOR))
    }

    /// The bytes a sized node of `kind` held per KiB of its source lately, once one ended:
    /// what [`Plane::push_sized`] predicts by.
    pub fn per_kib(&self, kind: TaskKind) -> Option<u64> {
        self.per_kib.get(&kind).filter(|it| it.seen > 0).map(|it| it.predicted)
    }

    /// The bytes of what the process holds besides the tasks granted memory that held back a
    /// task for memory as the plane last dispatched ([`blaze_graph::Plane::short`]): 0 when
    /// none was, or only granted tasks did. The loop lets go of what it can for it.
    pub fn short(&self) -> u64 {
        self.graph.short()
    }

    /// The next node of `fresh`, a part of a program's build of `kind` ([`kinds::PROGRAM`], its
    /// codegen, or [`kinds::PROGRAM_LINK`], its link), task `arg` of the batch's work. It is
    /// predicted and taught by `program`, the key of that part of the program, which the plane
    /// never sees: the node has no key, so nothing single-flights or memoizes it by that. With
    /// a budget, a part the predictor has not measured, or one measured to hold all of it, is
    /// predicted the whole budget and runs alone; its peak is then what its own work held, its
    /// account's (`arena::account`), as any task's is, never what the whole process held
    /// beyond its start (docs/memory-budget.md).
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
    ) -> NodeId {
        assert!(
            (fresh.tasks.len() as u32) < fresh.count,
            "a batch of {} nodes given another",
            fresh.count
        );
        assert!(node.key.is_none(), "program build {} is given a key of the plane's", node.name);
        assert!(of_program(kind), "{kind:?} is no part of a program's build");
        let Prediction { cost, peak } = self.predict.predict(kind, Some(program));
        node.memory = node.memory.max(peak);
        if let Some(cost) = cost {
            node.cost = cost;
        }
        if let Some(budget) = self.budget()
            && (!self.predict.measured(kind, Some(program)) || node.memory >= budget.bytes)
        {
            node.memory = budget.bytes;
        }
        fresh.tasks.push(Task::Thread { kind, arg });
        fresh.keys.push(Some(program));
        fresh.sizes.push(0);
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
        self.step(input)
            .unwrap_or_else(|error| panic!("the plane refused to open {request}: {error}"));
        request
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
        let Fresh { nodes, tasks, keys, sizes, over, count } = fresh;
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
                Batch { tasks, keys, sizes, over, live: count, work, reads, request: request.0 },
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

    /// Takes the thread tasks that ended into the plane.
    pub fn poll(&mut self, at: Duration) {
        let mut ran = std::mem::take(&mut self.ran);
        self.runner.take(&mut ran);
        for Ran { task, ended, peak, ran_ns, puts } in ran.drain(..) {
            let id = task.id;
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
                self.predict.observe(task.kind, key, ran_ns, peak);
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
        let held = PlaneInput::Held { at_ns: ns(at), bytes: arena::held() };
        self.step(held).unwrap_or_else(|error| panic!("the plane refused what is held: {error}"));
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

    /// How many thread tasks run.
    pub fn running(&self) -> usize {
        self.runner.running()
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
        }
        said.clear();
        self.said = said;
        taken
    }

    fn outputs(&mut self, outputs: &mut Vec<PlaneOutput>) {
        for output in outputs.drain(..) {
            match output {
                PlaneOutput::Said { record, .. } => {
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
        self.admitting.insert(id, Admitting { span, since_ns: self.now_ns });
    }

    /// Node `id` started: its wait for admission ends, saying what held it back. Memory, when
    /// another task with memory started while it did not fit ([`blaze_graph::plane::Admitted::passed`]);
    /// else a thread of its pool; nothing, when it started in the step it became ready.
    fn admitted(&mut self, id: NodeId) {
        let Some(Admitting { span, since_ns }) = self.admitting.remove(&id) else { return };
        let Some(admitted) = self.graph.admitted(id) else { return };
        let waited_for = if since_ns == self.now_ns {
            "nothing"
        } else if admitted.passed > 0 {
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
                };
                if batch.reads {
                    self.happened.push(Happened::Post { task, again: false });
                } else {
                    let work = batch.work.clone().expect("checked by add");
                    self.happened.push(Happened::Started { id });
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
        };
        let runners: [(Runner<Next>, Pool); 3] = [
            (Runner::Threads(Threads::new(1, 1)), Pool::Io),
            (Runner::Threads(Threads::new(1, 1)), Pool::Work),
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
                (Runner::Threads(Threads::new(1, 1)), Pool::Io),
                (Runner::Threads(Threads::new(1, 1)), Pool::Work),
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
        asked: Arc<parking_lot::Mutex<Asked>>,
        observed: Arc<parking_lot::Mutex<Observed>>,
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
        let (asked, observed) = (Arc::default(), Arc::default());
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
            Plane::new(config, history, Runner::Threads(Threads::new(1, pool))).unwrap();
        let ran_on: Arc<parking_lot::Mutex<Vec<String>>> = Arc::default();
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

    /// A guess over the whole budget asks for the budget and runs alone; only a unit's own
    /// measured runs over it refuse it, and the refusal says the number is its own. nftest2: a
    /// kind's peak of 121.59 GB (a daemon's heap, as the uncarried measure read it) refused
    /// every build script of a test build under a 59 GB budget.
    #[test]
    fn a_guess_over_the_budget_runs_alone_and_only_a_units_own_runs_refuse_it() {
        let gib = 1u64 << 30;
        assert_eq!(admitted(5 * gib, Basis::Kind, Some(gib)), gib);
        assert_eq!(admitted(5 * gib, Basis::Sized, Some(gib)), gib);
        assert_eq!(admitted(5 * gib, Basis::Own, Some(gib)), 5 * gib);
        assert_eq!(admitted(5 * gib, Basis::Kind, None), 5 * gib);
        assert_eq!(admitted(gib / 4, Basis::Kind, Some(gib)), gib / 4);

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
        plane.add(Duration::ZERO, request, fresh, Some(Each(built)), false).unwrap();
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
        assert!(settled[1].1, "a guess over the budget runs alone: {:?}", settled[1].2);
        assert_eq!(settled[2].0, own);
        assert!(!settled[2].1, "a unit whose own runs held more than the budget is refused");
        let why = settled[2].2.as_deref().unwrap_or_default();
        assert!(why.contains("measured big needs 2.00 GB of memory"), "{why}");
        assert!(why.contains("2.00 GB is what this unit's own last runs held at most"), "{why}");
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
    /// builds of one program both run. One the history has not measured runs alone and is
    /// measured, after which the program is predicted by what it held.
    #[test]
    fn a_program_is_admitted_by_its_own_measure_and_one_never_measured_runs_alone() {
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
        let first = plane.push_program(&mut fresh, node("app"), kinds::PROGRAM, 0, program);
        let again = plane.push_program(&mut fresh, node("app again"), kinds::PROGRAM, 1, program);
        plane.add(Duration::ZERO, request, fresh, Some(Each(built)), false).unwrap();
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
}
