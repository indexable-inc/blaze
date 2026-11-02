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
//! thread task's time and what it held at most ([`arena::measure`]), a loop task's time from
//! its start to its end.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::{Duration, Instant},
};

use blaze_graph::{
    Budget, Error, Plane as Graph, PlaneConfig, PlaneInput, PlaneOutput, RequestId, RequestStats,
    admission::{Predict, Prediction, TaskKind},
    arena,
    machine::{NodeId, Outcome, Pool},
    plane::{Key, NodeRef, Nodes, Unsettled},
};
use blaze_ids::rng::SplitMix64 as Rng;
use blaze_protocol::events::{Event, NodeState};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;

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
    /// A node settled: done (`why` none) or not (failed, skipped or cancelled, saying why).
    Settled { id: NodeId, ok: bool, why: Option<String> },
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
    GaveWay { group: Option<u32> },
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
}

/// The daemon's: work tasks on the process's worker pool, IO tasks on threads of their own.
pub struct Threads<W: Work> {
    /// Every task's way back, shared by reference: a task posted counts no sender.
    done_tx: &'static Sender<Ran<W::Value>>,
    done: Receiver<Ran<W::Value>>,
    io: Sender<IoJob<W>>,
    running: usize,
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
                            match run_measured(&*job.work, job.task.arg, job.task.build, readers.take().unwrap_or_default()) {
                                (Some(ended), peak, ran_ns, puts) => break (ended, peak, ran_ns, puts),
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
        Threads { done_tx, done, io: io_tx, running: 0 }
    }
}

/// Runs `work` inside a measure: `None` when it gave way (its readers dropped with it), else
/// its value or its panic; how long it ran; and what its expansions put once it ended. A task that gave way
/// leaves its puts in its thread's buffer, handed on by that thread's next take (or as
/// orphans, if the thread ends), so none is lost or kept twice.
fn run_measured<W: Work>(
    work: &W,
    arg: u32,
    build: u32,
    readers: W::Readers,
) -> (Option<Ended<W::Value>>, u64, u64, Vec<proc_macro_api::kept::Put>) {
    let _build = blaze_profile::flight::building(build);
    let start = Instant::now();
    let (result, peak) = arena::measure(|| catch_unwind(AssertUnwindSafe(|| work.run(arg, readers))));
    let ran_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    match result {
        Ok(None) => (None, peak, ran_ns, Vec::new()),
        Ok(Some(did)) => (Some(Ended::Did(did)), peak, ran_ns, proc_macro_api::kept::take_puts()),
        Err(panic) => (Some(Ended::Panicked(panicked(&*panic))), peak, ran_ns, proc_macro_api::kept::take_puts()),
    }
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

/// The panics the hook saw and no task took yet, oldest first: (message, where).
static RAISED: Mutex<VecDeque<(Option<String>, String)>> = Mutex::new(VecDeque::new());

/// How many panics [`RAISED`] holds: a panic no task catches (a thread's own) is dropped
/// once this many are newer.
const RAISED_KEPT: usize = 64;

/// Whether a task of `kind` is a part of a program's build: admitted by that program's own
/// measure, and alone until it has one ([`Plane::push_program`]).
const fn of_program(kind: TaskKind) -> bool {
    matches!(kind, kinds::PROGRAM | kinds::PROGRAM_LINK)
}

/// Has every panic of the process recorded where it was raised, for the task that catches it
/// ([`Panic::at`]), then printed as before. Once, as the daemon starts.
pub fn record_panics() {
    let printed = std::panic::take_hook();
    // alloc: the process's one panic hook
    std::panic::set_hook(Box::new(move |info| {
        if let Some(location) = info.location() {
            let thread = std::thread::current();
            let at = format!("{location} on thread {}", thread.name().unwrap_or("<unnamed>")); // alloc: a panic, once
            let message = info.payload_as_str().map(str::to_owned); // alloc: a panic, once
            let mut raised = RAISED.lock();
            if raised.len() == RAISED_KEPT {
                raised.pop_front();
            }
            raised.push_back((message, at));
        }
        printed(info);
    }));
}

/// The panic `payload` caught, with where the hook saw the newest one of its message raised.
// dyn: a panic payload, read once per panicked task
pub(crate) fn panicked(payload: &(dyn std::any::Any + Send)) -> Panic {
    let message = stdx::panic_text(payload).map(str::to_owned); // alloc: a panic, once
    let mut raised = RAISED.lock();
    let at = raised.iter().rposition(|(it, _)| *it == message).and_then(|place| raised.remove(place)).map(|(_, at)| at);
    Panic { message, at }
}

fn post_work<W: Work>(task: Posting, work: Arc<W>, readers: W::Readers, done: &'static Sender<Ran<W::Value>>) {
    let pool = blaze_graph::exec::shared();
    // The batch's work comes back with the result, for a task that gave way to be posted again
    // without counting another reference.
    let run = move || (run_measured(&*work, task.arg, task.build, readers), work);
    let then = move |result: std::thread::Result<_>| match result {
        Ok(((Some(ended), peak, ran_ns, puts), _)) => _ = done.send(Ran { task, ended, peak, ran_ns, puts }),
        Ok(((None, peak, ran_ns, puts), _)) if task.reads => {
            let ended = Ended::GaveWay { group: blaze_graph::pool::group() };
            _ = done.send(Ran { task, ended, peak, ran_ns, puts });
        }
        // Gave way: posted again behind what is ready on the group it ran on.
        Ok(((None, ..), work)) => {
            post_work(Posting { near: blaze_graph::pool::group(), ..task }, work, W::Readers::default(), done);
        }
        // A panic outside the measure: its puts stay in the thread's buffer.
        Err(panic) => {
            _ = done.send(Ran { task, ended: Ended::Panicked(panicked(&*panic)), peak: 0, ran_ns: 0, puts: Vec::new() });
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
            Pool::Work => post_work(task, work, readers, self.done_tx),
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
}

type Posted<W> = (Posting, Arc<W>, <W as Work>::Readers);

impl<W: Work> Inline<W> {
    pub fn new(seed: u64) -> Inline<W> {
        Inline { rng: Rng::new(seed), queue: VecDeque::new(), posted: Vec::new() }
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
        self.rng.shuffle(&mut posted);
        let now = 1 + self.rng.below(posted.len() as u64) as usize;
        self.queue.extend(posted.drain(now..));
        for (task, work, readers) in posted.drain(..) {
            match run_measured(&*work, task.arg, task.build, readers) {
                (Some(ended), peak, _, puts) => out.push(Ran { task, ended, peak, ran_ns: 0, puts }),
                (None, peak, _, puts) if task.reads => {
                    out.push(Ran { task, ended: Ended::GaveWay { group: None }, peak, ran_ns: 0, puts });
                }
                (None, ..) => self.queue.push_back((task, work, W::Readers::default())),
            }
        }
        self.posted = posted;
    }

    fn ready(&self) -> bool {
        !self.queue.is_empty()
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
    /// Each program's build that runs alone, never measured ([`Plane::push_program`]): what
    /// the process held as it started, for the most it held while it ran.
    alone: BTreeMap<NodeId, u64>,
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
            alone: BTreeMap::new(),
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
        Fresh { nodes, tasks: Vec::with_capacity(count as usize), keys: Vec::with_capacity(count as usize), count }
    }

    /// The next node of `fresh`, its id; its memory raised to what its kind and key are
    /// predicted to hold, its cost the predicted one where there is one.
    ///
    /// # Panics
    ///
    /// When `fresh` has all the nodes it was made for, or `task` is [`Task::Gone`].
    pub fn push(&self, fresh: &mut Fresh, mut node: NodeRef<'_>, task: Task) -> NodeId {
        assert!((fresh.tasks.len() as u32) < fresh.count, "a batch of {} nodes given another", fresh.count);
        let kind = match task {
            Task::Thread { kind, .. } | Task::Loop { kind } => kind,
            Task::Gone => panic!("node {} is pushed with no task", node.name),
        };
        let Prediction { cost, peak } = self.predict.predict(kind, node.key);
        node.memory = node.memory.max(peak);
        if let Some(cost) = cost {
            node.cost = cost;
        }
        fresh.tasks.push(task);
        fresh.keys.push(node.key);
        fresh.nodes.push(node)
    }

    /// The next node of `fresh`, a part of a program's build of `kind` ([`kinds::PROGRAM`], its
    /// codegen, or [`kinds::PROGRAM_LINK`], its link), task `arg` of the batch's work. It is
    /// predicted and taught by `program`, the key of that part of the program, which the plane
    /// never sees: the node has no key, so nothing single-flights or memoizes it by that. With
    /// a budget, a part the predictor has not measured, or one measured to hold all of it, is
    /// predicted the whole budget and runs alone, its peak then what the whole process held
    /// beyond its start (docs/memory-budget.md).
    ///
    /// # Panics
    ///
    /// As [`Plane::push`], when `node` has a key, and when `kind` is no program's.
    pub fn push_program(&self, fresh: &mut Fresh, mut node: NodeRef<'_>, kind: TaskKind, arg: u32, program: Key) -> NodeId {
        assert!((fresh.tasks.len() as u32) < fresh.count, "a batch of {} nodes given another", fresh.count);
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
        fresh.nodes.push(node)
    }

    /// Whether the build of program `key` that starts now runs alone, never measured: its
    /// peak is then the whole process's ([`Plane::push_program`]).
    fn watched(&self, kind: TaskKind, key: Option<Key>) -> bool {
        of_program(kind) && self.budget().is_some() && !self.predict.measured(kind, key)
    }

    /// What a thread task of `kind` that ended teaches its key, from `peak`, what its thread
    /// held at most: a program's build that ran alone, the most the process held beyond its
    /// start; one that ran beside others, never less than it was predicted, since its thread
    /// does not see what the threads it hands its compiles to hold.
    fn learned(&mut self, id: NodeId, kind: TaskKind, key: Option<Key>, peak: u64) -> u64 {
        match self.alone.remove(&id) {
            Some(start) => peak.max(arena::high_water().saturating_sub(start)),
            None if of_program(kind) => peak.max(self.predict.predict(kind, key).peak),
            None => peak,
        }
    }

    fn reserve(&mut self, count: u32) -> NodeId {
        let first = match self.next_node.checked_add(count) {
            Some(_) => self.next_node,
            None => 0,
        };
        let end = first + count;
        let free = self.batches.range(..end).next_back().is_none_or(|(at, batch)| at + batch.tasks.len() as u32 <= first);
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

    fn settled(&mut self, id: NodeId) {
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
        self.step(input).unwrap_or_else(|error| panic!("the plane refused to open {request}: {error}"));
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
        let Fresh { nodes, tasks, keys, count } = fresh;
        let work = work.map(Arc::new); // alloc: one per batch, shared by its tasks
        assert_eq!(tasks.len() as u32, count, "a batch is added with every node it was made for");
        assert!(
            work.is_some() || !tasks.iter().any(|it| matches!(it, Task::Thread { .. })),
            "a batch with thread tasks is given their work"
        );
        let first = nodes.first();
        if count > 0 {
            self.batches.insert(first.0, Batch { tasks, keys, live: count, work, reads, request: request.0 });
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

    /// A loop task is over: the time since it started teaches the predictor.
    pub fn ended(&mut self, at: Duration, id: NodeId, outcome: Outcome) {
        let kind = match self.task(id) {
            Some((_, Task::Loop { kind })) => Some(kind),
            _ => None,
        };
        if let (Some(start), Some(kind)) = (self.loops.remove(&id), kind) {
            let key = self.key(id);
            self.predict.observe(kind, key, ns(at).saturating_sub(start), 0);
        }
        let input = PlaneInput::Done { at_ns: ns(at), id, outcome, added: Vec::new(), group: None, peak: 0 };
        self.step(input).unwrap_or_else(|error| panic!("the plane refused the end of loop task {id}: {error}"));
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

    /// A thread task of [`Happened::Post`] the loop has no readers for: it ends failed.
    pub fn refuse(&mut self, at: Duration, task: Posting, why: String) {
        self.alone.remove(&task.id);
        let input = PlaneInput::Done { at_ns: ns(at), id: task.id, outcome: Outcome::Failed { why }, added: Vec::new(), group: None, peak: 0 };
        self.step(input).unwrap_or_else(|error| panic!("the plane refused the end of task {}: {error}", task.id));
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
                    self.happened.push(Happened::Post { task: Posting { near: group, ..task }, again: true });
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
            let key = self.key(id);
            let peak = self.learned(id, task.kind, key, peak);
            self.predict.observe(task.kind, key, ran_ns, peak);
            let input = PlaneInput::Done { at_ns: ns(at), id, outcome, added: Vec::new(), group: None, peak };
            self.step(input).unwrap_or_else(|error| panic!("the plane refused the end of task {id}: {error}"));
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

    /// How many loop tasks run ([`Happened::Run`], not yet [`Plane::ended`]).
    pub fn loops_running(&self) -> usize {
        self.loops.len()
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
                    if !state.settled() {
                        continue;
                    }
                    let id = NodeId(id);
                    self.settled(id);
                    let (ok, why) = match state {
                        NodeState::Built | NodeState::Cached { .. } => (true, None),
                        NodeState::Failed { why } => (false, Some(why)),
                        NodeState::Skipped { because } => {
                            (false, Some(format!("skipped: node {because} it waited for did not end well")))
                        }
                        _ => (false, Some("cancelled".to_owned())),
                    };
                    self.happened.push(Happened::Settled { id, ok, why });
                }
                PlaneOutput::Run { id, pool, near, critical } => {
                    let watch = match self.task(id) {
                        Some((_, Task::Thread { kind, .. })) => self.watched(kind, self.key(id)),
                        _ => false,
                    };
                    if watch {
                        self.alone.insert(id, arena::high_water_from_now());
                    }
                    self.run(id, pool, near, critical);
                }
                PlaneOutput::Finished { request, ok } => {
                    let stats = self
                        .graph
                        .stats(request)
                        .unwrap_or_else(|| panic!("the plane finished {request}, which it holds no stats of"));
                    self.graph
                        .release(request)
                        .unwrap_or_else(|error| panic!("the plane refused to release {request}: {error}"));
                    self.happened.push(Happened::Finished { request, ok, stats });
                }
                // An idle task asked to give way holds its thread until its `Done`, as the plane
                // allows: none is stopped mid-run.
                PlaneOutput::Yield { .. } => {}
            }
        }
    }

    /// The node the plane runs: its thread task posted, or its loop task handed to the loop.
    fn run(&mut self, id: NodeId, pool: Pool, near: Option<u32>, critical: bool) {
        match self.task(id) {
            Some((batch, Task::Thread { kind, arg })) => {
                let task = Posting { id, pool, near, critical, kind, arg, reads: batch.reads, build: batch.request };
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
            Some((_, Task::Gone)) | None => panic!("the plane runs node {id}, which the daemon gave no task"),
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
            run: RunKey { code: id, given: id },
            traced: Traced::default(),
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
        let task = |pool| Posting { id: NodeId(4), pool, near: None, critical: false, kind: kinds::COMPILE, arg: 20, reads: false, build: 1 };
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
            let [Ran { ended: Ended::Did(Did { value: 21, .. }), .. }] = ran.as_slice() else { panic!("the task ends with its value") };
            assert_eq!(runner.running(), 0);
            assert!(!runner.ready());
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
        let task = Posting { id: NodeId(9), pool: Pool::Work, near: None, critical: false, kind: kinds::COMPILE, arg: 0, reads: false, build: 1 };
        let mut runner = Inline::new(5);
        runner.post(task, work, ());
        let mut ran = Vec::new();
        runner.take(&mut ran);
        let [Ran { ended: Ended::Panicked(panic), .. }] = ran.as_slice() else { panic!("the task ends panicked") };
        assert_eq!(panic.message.as_deref(), Some("plane-test: the task's panic"));
        let at = panic.at.as_deref().expect("the hook saw where it was raised");
        assert!(at.contains("serve/plane.rs:") && at.ends_with(" on thread plane-test-raiser"), "{at}");
        assert_eq!(panic.to_string(), format!("panicked at {at}: plane-test: the task's panic"));
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
        let task = Posting { id: NodeId(7), pool: Pool::Work, near: None, critical: false, kind: kinds::COMPILE, arg: 0, reads: false, build: 1 };
        let mut runner = Inline::new(3);
        runner.post(task, work, ());
        let mut ran = Vec::new();
        while ran.is_empty() {
            runner.take(&mut ran);
        }
        let [Ran { ended: Ended::Did(_), puts, .. }] = ran.as_slice() else { panic!("one task ends") };
        assert_eq!(ours(puts, "plane-a"), ["plane-a before", "plane-a after"]);
    }

    #[test]
    fn a_task_that_reads_and_gave_way_leaves_its_puts_for_the_next_take() {
        let work = Arc::new(Each(|_| {
            proc_macro_api::kept::put(put("plane-b gave way"));
            None
        }));
        let task = Posting { id: NodeId(8), pool: Pool::Work, near: None, critical: false, kind: kinds::COMPILE, arg: 0, reads: true, build: 1 };
        let mut runner = Inline::new(5);
        runner.post(task, work, ());
        let mut ran = Vec::new();
        runner.take(&mut ran);
        let [Ran { ended: Ended::GaveWay { .. }, puts, .. }] = ran.as_slice() else { panic!("the task gives way") };
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
        let node = |kind, name, key| NodeRef { kind, name, deps: &[], pool: Pool::Work, cost: 1, key, memory: 0 };
        let mut fresh = plane.fresh(2, 16, 0);
        plane.push(&mut fresh, node(NodeKind::Std, "thread", Some(key)), Task::Thread { kind: kinds::COMPILE, arg: 0 });
        let looped = plane.push(&mut fresh, node(NodeKind::ScriptRun, "loop", None), Task::Loop { kind: loops::SCRIPT_RUN });
        plane.add(Duration::ZERO, request, fresh, Some(Each(built)), false).unwrap();
        assert_eq!(*asked.lock(), [(kinds::COMPILE, Some(key)), (loops::SCRIPT_RUN, None)]);

        let (mut happened, mut ran_loop) = (Vec::new(), false);
        while !ran_loop || observed.lock().is_empty() {
            plane.poll(Duration::ZERO);
            plane.happened(&mut happened);
            ran_loop |= happened.iter().any(|it| matches!(it, Happened::Run { id } if *id == looped));
            happened.clear();
        }
        let seen: Vec<_> = observed.lock().iter().map(|&(kind, key, ran_ns, _)| (kind, key, ran_ns)).collect();
        assert_eq!(seen, [(kinds::COMPILE, Some(key), 0)], "the simulation's tasks take no time");
        plane.ended(Duration::from_millis(5), looped, Outcome::Built);
        let last = observed.lock().last().copied();
        assert_eq!(last, Some((loops::SCRIPT_RUN, None, 5_000_000, 0)));
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
        let budget = Budget { bytes: 1 << 30, from: "the test's budget".to_owned() };
        let config = PlaneConfig { io: 1, work: 2, budget: Some(budget), starve_after: 8 };
        let history = blaze_history::History::new(blaze_history::Config::default());
        let mut plane = Plane::new(config, history, Runner::Inline(Inline::new(4))).unwrap();
        let (program, other) = (Key([5; 32]), Key([6; 32]));
        let node = |name| NodeRef { kind: NodeKind::Codegen, name, deps: &[], pool: Pool::Work, cost: 1, key: None, memory: 0 };
        assert!(!plane.predict().measured(kinds::PROGRAM, Some(program)));
        assert!(plane.watched(kinds::PROGRAM, Some(program)));
        assert!(!plane.watched(kinds::COMPILE, Some(program)), "only a program's build runs alone");

        let request = plane.open(Duration::ZERO, true, false);
        let mut fresh = plane.fresh(2, 16, 0);
        assert!(plane.watched(kinds::PROGRAM_LINK, Some(program)), "a program's link runs alone as its codegen does");
        let first = plane.push_program(&mut fresh, node("app"), kinds::PROGRAM, 0, program);
        let again = plane.push_program(&mut fresh, node("app again"), kinds::PROGRAM, 1, program);
        plane.add(Duration::ZERO, request, fresh, Some(Each(built)), false).unwrap();
        assert_eq!(settle(&mut plane, &[first, again]), [(first, true), (again, true)]);
        assert!(plane.alone.is_empty(), "every watch ends with its task");
        assert!(plane.predict().measured(kinds::PROGRAM, Some(program)));
        assert!(!plane.watched(kinds::PROGRAM, Some(program)), "measured, it runs beside others");
        assert!(plane.watched(kinds::PROGRAM, Some(other)), "another program is not measured by it");
        let record = plane.predict().record(kinds::PROGRAM, &program).expect("its runs teach its key");
        assert_eq!(record.seen, 2);
    }
}
