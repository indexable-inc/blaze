//! A build as it runs now: the one model of it that its client's live view, `blaze status`
//! and the explorer's daemon source read (docs/v2/plan/design/build-view.md). [`Now`] is the
//! wire type, said to a build's client about twice a second; [`Fold`] is the sans-IO core
//! that builds it from tasks and their moves ([`TaskRecord`]s through [`Fold::take`], [`State`]s
//! through [`Fold::moved`]), the clock it is handed ([`Fold::advance_clock`]) and what one look
//! at the daemon found ([`Look`]: each worker's open spans, the downloads, the stage's words).
//!
//! Its input is generic, so the explorer's page (compiled to `wasm32-unknown-unknown`) folds any
//! program's live feed with the same core: a task is a `u64` id, a kind that is its layer's label
//! and a name, with the ids it waits on. The plane's graph records ([`Event::Node`],
//! [`Event::Nodes`], [`Event::Moved`]) become tasks in one place, [`Fold::take_event`], their
//! kind the [`NodeKind`]'s name (`codegen`, `script-run`). A program that is no build (no task of
//! a build's kind) is in [`Phase::Running`].
//!
//! What it answers: the phase and what it really does; how many workers are busy and, for each,
//! its innermost span; what the unfinished tasks wait on (the [`Blocker`]s) and how much each
//! holds back; the downloads; the slowest tasks so far; and how long nothing moved.

#![blaze::sans_io]

use std::{alloc::Global, cmp::Reverse};

use blaze_hash::HashMap;

use serde::{Deserialize, Serialize};

use crate::events::{Event, NodeKind, NodeState, Order};

/// Where a build is, as a person names it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// Before its graph has a node.
    Starting,
    /// A download is in flight and no task runs.
    Fetching,
    /// Reading manifests and the lock file, choosing versions.
    Resolving,
    /// Waiting for its plan or its lowering: nothing of its graph is ready to run yet.
    Planning,
    Compiling,
    Linking,
    Testing,
    /// Every task settled; the build is writing its answer.
    Finishing,
    /// A program that is not a build: tasks run, no phase. No task it knows has a kind of
    /// [`NodeKind::ALL`]'s names.
    Running,
}

impl Phase {
    /// The word the view names it by.
    pub const fn word(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Fetching => "fetching",
            Phase::Resolving => "resolving",
            Phase::Planning => "planning",
            Phase::Compiling => "compiling",
            Phase::Linking => "linking",
            Phase::Testing => "testing",
            Phase::Finishing => "finishing",
            Phase::Running => "running",
        }
    }
}

crate::in_key_order! {
/// One build as of `at_ms` after it began.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Now {
    pub at_ms: u64,
    pub phase: Phase,
    /// What the daemon says its stage waits on (`waits for its pinned native inputs, 1 of 1
    /// layouts left`), or empty while its units build.
    pub stage: String,
    /// The daemon's top stall in its wait-for graph, in one line, while one lasts: a deadlock
    /// (`deadlock: thread 3 waits lock store.packs (2400 ms); ...`) or a convoy behind one head
    /// (`convoy: 61 behind thread 7 on lock flight.intern (oldest 2400 ms); the head is
    /// running`), as `blaze_core::lock::stall::Stall::line` says it. Its loop watchdog
    /// (`serve::stop`) checks each look once a wait passes its threshold; `None` for none.
    pub stall: Option<String>,
    pub tally: Tally,
    /// The pool's workers; those not in `busy` are idle.
    pub workers: u32,
    /// Each busy worker, the one inside its span longest first.
    pub busy: Vec<Busy>,
    /// Every running task, the one running longest first, then by id: at most [`RUNNING`].
    /// What a view lists a row each, so everything under way is seen at once.
    pub running: Vec<Running>,
    /// How many tasks settled in the last [`RECENT_MS`] (or since the clock began, when that
    /// is shorter): the build's rate, counted, never extrapolated.
    pub recent: u32,
    /// The tasks that failed (not those cancelled), in the order they failed: at most
    /// [`FAILURES`].
    pub failures: Vec<Task>,
    /// The unfinished tasks whose inputs are all done (running or ready), the one holding the
    /// most back first ([`Blocker`]). At most [`BLOCKERS`].
    pub blockers: Vec<Blocker>,
    /// Every download in flight, oldest first.
    pub downloads: Vec<Download>,
    /// The settled tasks that took longest, longest first. At most [`SLOWEST`].
    pub slowest: Vec<Took>,
    /// How long since a task settled or a download got a byte.
    pub quiet_ms: u64,
    /// The daemon's memory against its ceiling, and how pressed it is.
    pub memory: Memory,
    /// The proc-macro expansion threads: how many there are and what the busy ones expand.
    pub expanding: Expansion,
}
keys [busy, at_ms, phase, stage, stall, tally, memory, recent, running, slowest, workers, blockers, failures, quiet_ms, downloads, expanding]
}

impl Default for Phase {
    fn default() -> Phase {
        Phase::Starting
    }
}

impl Now {
    /// Workers not running anything.
    pub fn idle(&self) -> u32 {
        self.workers.saturating_sub(self.busy.len() as u32)
    }

    /// Whether nothing moves: no worker is busy, no download in flight, no process runs (a
    /// build script's or a test's), and nothing settled for [`STALL_MS`]. With a worker busy
    /// or a process running the build is slow, not stalled.
    pub fn stalled(&self) -> bool {
        let process = |it: &Running| {
            matches!(it.task.kind.as_str(), "script-run" | "test") && it.thread.is_none()
        };
        self.quiet_ms >= STALL_MS
            && self.busy.is_empty()
            && self.downloads.is_empty()
            && !self.running.iter().any(process)
    }

    /// Whether nothing settled for [`STALL_MS`] while a worker is busy, a process runs or a
    /// download moves.
    pub fn slow(&self) -> bool {
        self.quiet_ms >= STALL_MS && !self.stalled()
    }
}

crate::in_key_order! {
/// The daemon's memory as admission sees it (`blaze_graph::admission::Pressure`): what the
/// process holds against the ceiling it runs under, the subsystem past its fair share, and what
/// the plane holds back for memory. Andrew's build of 2026-11-05 reached 99 GiB with nothing on
/// screen to say so: a view shows this in its header, always.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Memory {
    /// The process's footprint as the system counts it.
    pub held: u64,
    /// The most it held during this build.
    pub peak: u64,
    /// The ceiling (`blaze_graph::admission::limit`: the least of the machine's, its cgroup's
    /// and `BLAZE_DAEMON_MEMORY_GB`); 0 for none.
    pub limit: u64,
    /// The first subsystem the release lowers that holds more than its fair share of the
    /// limit, by its tag's name (`mir`, `infer`).
    pub over: Option<String>,
    /// Tasks ready to run that admission keeps waiting until memory frees.
    pub held_back: u32,
    /// Releases (front-end memory let go under pressure) during this build. A [`Look`] carries
    /// the daemon's count since it started; the fold says how many came since the build's
    /// first look.
    pub releases: u32,
}
keys [held, over, peak, limit, releases, held_back]
}

/// How pressed the daemon's memory is, as a view colors it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Pressure {
    /// Under three quarters of the limit, or no limit.
    Calm,
    /// Past three quarters.
    Rising,
    /// Past the release line (the limit less its eighth, `admission::LINE`), a subsystem
    /// over its share, or tasks held back for memory: the build slows to stay under.
    Squeezed,
    /// At or past the limit.
    Over,
}

impl Memory {
    pub fn pressure(&self) -> Pressure {
        if self.limit == 0 {
            return if self.held_back > 0 { Pressure::Squeezed } else { Pressure::Calm };
        }
        if self.held >= self.limit {
            Pressure::Over
        } else if self.held >= self.limit - self.limit / 8
            || self.over.is_some()
            || self.held_back > 0
        {
            Pressure::Squeezed
        } else if self.held >= self.limit / 4 * 3 {
            Pressure::Rising
        } else {
            Pressure::Calm
        }
    }
}

/// How long nothing may move before a view says so.
pub const STALL_MS: u64 = 10_000;

/// The most blockers a [`Now`] names.
pub const BLOCKERS: usize = 16;

/// The most slowest tasks a [`Now`] names.
pub const SLOWEST: usize = 10;

/// The most tasks a [`Blocker`] names as held behind it.
pub const BEHIND: usize = 4;

/// The most running tasks a [`Now`] lists; the rest are in [`Tally::running`].
pub const RUNNING: usize = 256;

/// The most failed tasks a [`Now`] names.
pub const FAILURES: usize = 16;

/// The window [`Now::recent`] counts settles over.
pub const RECENT_MS: u64 = 30_000;

crate::in_key_order! {
/// How many of the build's tasks are where.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    pub waiting: u32,
    pub ready: u32,
    pub running: u32,
    /// Settled well ([`State::Done`]: for a build, built, cached or skipped).
    pub done: u32,
    /// Failed or cancelled.
    pub failed: u32,
    pub total: u32,
}
keys [done, ready, total, failed, running, waiting]
}

crate::in_key_order! {
/// A task of the program's graph: for a build, a plane node.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Task {
    pub id: u64,
    /// Its layer's label: for a build, the [`NodeKind`]'s name (`codegen`, `script-run`).
    pub kind: String,
    /// For a build, the unit as the plane names it: `object 0.36.7 build-script (host)`, its
    /// package, version, target and what it is built for.
    pub name: String,
}
keys [id, kind, name]
}

/// Where a task is.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// A task it waits on is not settled.
    Waiting,
    /// Everything it waits on settled; it waits for a worker.
    Ready,
    Running,
    /// Settled by doing its work, or by not needing to (for a build: built, cached or skipped).
    Done,
    Failed,
    /// Never run: what it was part of was cancelled.
    Cancelled,
}

impl State {
    /// Every state, in the order a task goes through them.
    pub const ALL: [State; 6] =
        [State::Waiting, State::Ready, State::Running, State::Done, State::Failed, State::Cancelled];

    /// Whether it may still move: waiting, ready or running.
    pub const fn unfinished(self) -> bool {
        matches!(self, State::Waiting | State::Ready | State::Running)
    }

    /// Its word on a wire (the explorer's `tasks.state`): `waiting`, `ready`, `running`,
    /// `done`, `failed`, `cancelled`.
    pub const fn word(self) -> &'static str {
        match self {
            State::Waiting => "waiting",
            State::Ready => "ready",
            State::Running => "running",
            State::Done => "done",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
        }
    }

    /// The state `word` names ([`State::word`]); `None` for any other word.
    pub fn from_word(word: &str) -> Option<State> {
        State::ALL.into_iter().find(|it| it.word() == word)
    }
}

impl From<&NodeState> for State {
    fn from(state: &NodeState) -> State {
        match state {
            NodeState::Waiting => State::Waiting,
            NodeState::Ready => State::Ready,
            NodeState::Running => State::Running,
            NodeState::Built | NodeState::Cached { .. } | NodeState::Skipped { .. } => State::Done,
            NodeState::Failed { .. } => State::Failed,
            NodeState::Cancelled => State::Cancelled,
        }
    }
}

crate::in_key_order! {
/// A task as a feed says it: its id, its layer's label, its name and the ids it waits on (a
/// dep not yet said is unsettled until it is). What [`Fold::take`] adds or updates.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub id: u64,
    pub kind: String,
    pub name: String,
    pub deps: Vec<u64>,
}
keys [id, deps, kind, name]
}

crate::in_key_order! {
/// An open span as a view names it.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// The span's name: what it does (`codegen`, `copy_prop`, `infer_query`).
    pub name: String,
    /// The item it is of, by path (`core::ptr::is_freeze`).
    pub item: Option<String>,
    pub krate: Option<String>,
    /// What it waits on, when it is a wait.
    pub awaits: Option<String>,
    pub for_ms: u64,
}
keys [item, name, krate, awaits, for_ms]
}

crate::in_key_order! {
/// A busy worker: its thread, the task its outermost `task` span runs, and its innermost
/// span.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Busy {
    pub thread: String,
    pub task: Option<Task>,
    pub span: Span,
    /// The spans strictly between its outermost and its innermost, by name, outermost first:
    /// `codegen`, `copy_prop` of `task` > `codegen` > `copy_prop` > `is_freeze`. The outermost
    /// is the `task` span that `task` names and the innermost is `span`; a view shows the path
    /// expanded, ending in `span`'s name.
    pub path: Vec<String>,
}
keys [path, span, task, thread]
}

crate::in_key_order! {
/// The proc-macro expansion lane: its threads (`blaze wasm expansion`), which are no pool
/// workers and serve every build at once, and the busy ones, each with what it expands (the
/// macro by path, the crate it expands in).
#[derive(Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Expansion {
    pub threads: u32,
    /// The ones inside a span that is no wait, the one in it longest first.
    pub busy: Vec<Busy>,
}
keys [busy, threads]
}

crate::in_key_order! {
/// A running task: since when, and, when a worker is inside it, that worker's innermost span.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub task: Task,
    /// Since it began running.
    pub for_ms: u64,
    /// The worker inside it, by thread name; `None` for a task no worker thread is inside (a
    /// process it waits on, a task that runs on the plane's loop, a job parked on its fiber).
    pub thread: Option<String>,
    /// That worker's innermost span: a wait ([`Span::awaits`]) when the task is under way but
    /// its thread waits, which a view says as waiting, not as work.
    pub span: Option<Span>,
    /// That worker's spans strictly between its outermost and its innermost ([`Busy::path`]).
    pub path: Vec<String>,
}
keys [path, span, task, for_ms, thread]
}

crate::in_key_order! {
/// An unfinished task whose inputs are all done, and what waits on it.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    pub task: Task,
    /// Running on a worker; otherwise ready and waiting for one.
    pub running: bool,
    /// Since it began running, or since it became ready.
    pub for_ms: u64,
    /// The innermost span of the worker running it.
    pub span: Option<Span>,
    /// How many unfinished tasks wait on it, directly or not.
    pub holds: u32,
    /// The longest path of unfinished tasks behind it, itself not counted.
    pub chain: u32,
    /// The first few tasks waiting on it directly, by name ([`BEHIND`]).
    pub behind: Vec<String>,
}
keys [span, task, chain, holds, behind, for_ms, running]
}

crate::in_key_order! {
/// A download in flight.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Download {
    /// What it is: the URL's file name, or what the downloader called it.
    pub what: String,
    pub done: u64,
    pub total: Option<u64>,
    /// Bytes a second over the last few seconds; `None` before there are two samples.
    pub rate: Option<u64>,
    /// Since its last byte.
    pub idle_ms: u64,
    pub elapsed_ms: u64,
}
keys [done, rate, what, total, idle_ms, elapsed_ms]
}

impl Download {
    /// How long until it is done at its rate, when both are known.
    pub fn eta_ms(&self) -> Option<u64> {
        let (total, rate) = (self.total?, self.rate?);
        if rate == 0 {
            return None;
        }
        Some(total.saturating_sub(self.done).saturating_mul(1000) / rate)
    }
}

crate::in_key_order! {
/// A settled task and how long it ran.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Took {
    pub task: Task,
    pub ms: u64,
}
keys [ms, task]
}

/// A worker thread as one look found it: its name, the task its `task` span runs (when it is
/// inside one), by id, and its open spans, outermost first. A worker with no open span is idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worker {
    pub thread: String,
    pub task: Option<u64>,
    pub open: Vec<Span>,
}

/// What one look at the daemon found besides the graph.
#[derive(Debug, Clone, Copy)]
pub struct Look<'a> {
    /// The pool's workers, busy or not.
    pub workers: u32,
    /// Each worker thread of the pool with its open spans, any order.
    pub threads: &'a [Worker],
    pub downloads: &'a [Download],
    /// The stage's words, empty while the units build ([`Now::stage`]).
    pub stage: &'a str,
    /// The daemon's top stall's line, while one lasts ([`Now::stall`]).
    pub stall: Option<&'a str>,
    /// The daemon's memory now: [`Memory::peak`] is the fold's to keep (the look's is ignored),
    /// and [`Memory::releases`] is the daemon's count since it started.
    pub memory: &'a Memory,
    /// The proc-macro expansion threads, each with its open spans (none for an idle one).
    pub expanders: &'a [Worker],
    /// How many expansion threads there are, idle ones included.
    pub expansion_threads: u32,
}

/// The sans-IO core: folds a program's tasks, their moves and looks into [`Now`]s.
///
/// It keeps each task in a slot, the slots in the order the tasks were first said, with a map
/// from id to slot (only ever asked, never walked, so its order reaches no output): its kind,
/// name, deps, where it is and when it became ready, began running and was first seen; and a
/// short list of the slowest settled tasks, kept as they settle. At most [`MOST_TASKS`] tasks are
/// kept: a new one past them is ignored. A [`Fold::look`] reads them with what the look found;
/// it reuses its buffers from one look to the next.
///
/// **Phase.** The first of these that holds:
///
/// 1. [`Phase::Running`]: it knows a task and none has a build's kind (one of
///    [`NodeKind::ALL`]'s names): the program is no build.
/// 2. [`Phase::Finishing`]: it knows a task and none is unfinished (waiting, ready or running).
/// 3. [`Phase::Fetching`]: a download is in flight and no task runs.
/// 4. [`Phase::Starting`]: it knows no task (and no download is in flight).
/// 5. A task runs: [`Phase::Testing`] when one of them is a [`NodeKind::Test`], else
///    [`Phase::Linking`] when one is a [`NodeKind::Link`], else [`Phase::Compiling`].
/// 6. Nothing runs and nothing is ready: [`Phase::Planning`] when the stage's words mention
///    `plan` or `lowering`, else [`Phase::Resolving`] when they mention `fetch` or `resolve`
///    or a [`NodeKind::Manifest`] or [`NodeKind::Fetch`] task is unfinished.
/// 7. Otherwise [`Phase::Compiling`].
///
/// A kind is told by its name ([`NodeKind::name`]), since a task's kind is its label.
///
/// **Blockers.** The frontier is every unfinished task whose deps have all settled (a failed
/// dep counts as settled; a dep never said does not). The unfinished tasks are walked in
/// dependency order (Kahn's, one step per edge, so a cycle, which the plane never sends, is left
/// out rather than walked), each carrying the set of frontier tasks it waits on, directly or not,
/// as a bitset over the first 256 of the frontier (the running ones first, then the longest
/// ready). A blocker's `holds` is how many unfinished tasks off the frontier carry it, `chain`
/// the longest path of unfinished tasks behind it, `behind` the first few waiting on it directly,
/// in the order they were said. A frontier task past the cap is still a blocker, holding 0: it is
/// carried by no one.
#[derive(Debug, Default)]
pub struct Fold {
    /// The clock, as nanoseconds since the build began; it never goes back.
    now_ns: u64,
    /// Whether the clock was set: the quiet time counts from its first setting.
    clocked: bool,
    /// When a task last settled or a download last got a byte.
    moved_ns: u64,
    /// The tasks, in the order first said.
    tasks: Vec<Slot>,
    /// Each task's slot in `tasks`, by id. Asked, never iterated.
    slots: HashMap<u64, u32, Global>,
    /// The daemon's release count at the build's first look ([`Memory::releases`]).
    releases_base: Option<u32>,
    /// When each task settled in the last [`RECENT_MS`], oldest first ([`Now::recent`]).
    settles: std::collections::VecDeque<u64>,
    /// The failed tasks' ids, in the order they failed: at most [`FAILURES`].
    failures: Vec<u64>,
    /// The most memory held at any look ([`Memory::peak`]).
    peak: u64,
    /// The slowest settled tasks that ran, as how long each ran and its id, longest first and
    /// then by id: at most [`SLOWEST`].
    slowest: Vec<(u64, u64)>,
    /// What reads the plane's records keeps from one to the next.
    records: Records,
    /// The blocker walk's buffers.
    walk: Walk,
}

/// One of the plane's graph records as a task: what [`Records::read`] makes of an
/// [`Event::Node`], each node of an [`Event::Nodes`] and an [`Event::Moved`]. Ids are the plane's
/// node ids, widened.
#[derive(Debug, Clone, Copy)]
pub enum Record<'a> {
    /// A node said: its kind is the [`NodeKind`]'s name.
    Task { id: u64, kind: &'static str, name: &'a str, deps: &'a [u64] },
    /// A node moved: `state` as a task's, `node` as the plane said it (the outcome a view
    /// names: cached, cached remotely, skipped, failed and why).
    Moved { id: u64, state: State, node: &'a NodeState },
}

/// What reads the plane's records keeps from one to the next: the order of an [`Event::Nodes`]
/// and the buffer a record's deps are widened into. The one place a plane record becomes a task:
/// the [`Fold`] reads through it, and so does the explorer's daemon source.
#[derive(Debug, Default)]
pub struct Records {
    order: Order,
    widened: Vec<u64>,
}

impl Records {
    pub fn new() -> Records {
        Records::default()
    }

    /// Each task `event` says, to `each`, in the order said. Records of any other kind say
    /// none, and so does a malformed [`Event::Nodes`].
    pub fn read(&mut self, event: &Event, mut each: impl FnMut(Record<'_>)) {
        match event {
            Event::Node { id, kind, name, deps } => {
                self.widened.clear();
                // alloc: the widening buffer, kept from record to record.
                self.widened.extend(deps.iter().map(|&dep| u64::from(dep)));
                each(Record::Task { id: u64::from(*id), kind: kind.name(), name, deps: &self.widened });
            }
            Event::Nodes { .. } => {
                let Some(Ok(batch)) = event.nodes(&mut self.order) else { return };
                for node in batch.iter() {
                    self.widened.clear();
                    // alloc: the widening buffer, kept from record to record.
                    self.widened.extend(node.deps.iter().map(|&dep| u64::from(dep)));
                    each(Record::Task {
                        id: u64::from(node.id),
                        kind: node.kind.name(),
                        name: node.name,
                        deps: &self.widened,
                    });
                }
            }
            Event::Moved { id, state } => {
                each(Record::Moved { id: u64::from(*id), state: State::from(state), node: state })
            }
            _ => {}
        }
    }
}

/// The most tasks a fold keeps; a new task past them is ignored.
pub const MOST_TASKS: usize = 1 << 22;

/// How many frontier tasks the blocker walk tells apart; the rest hold 0.
const FRONTIER: usize = 256;

/// Nanoseconds in a millisecond.
const MS: u64 = 1_000_000;

/// No place: a task off the walk, or off the frontier.
const NONE: u32 = u32::MAX;

/// Whether `kind` is a build's: one of [`NodeKind::ALL`]'s names.
fn build_kind(kind: &str) -> bool {
    NodeKind::ALL.iter().any(|it| it.name() == kind)
}

/// A task as the fold keeps it.
#[derive(Debug)]
struct Slot {
    id: u64,
    kind: String,
    /// Whether `kind` is a build's ([`build_kind`]).
    build: bool,
    name: String,
    deps: Vec<u64>,
    state: State,
    /// When it was first said.
    seen_ns: u64,
    /// When it last became ready.
    ready_ns: Option<u64>,
    /// When it last began running.
    running_ns: Option<u64>,
}

impl Slot {
    /// Since when it is where it is, for a blocker's `for_ms`: since it began running, else
    /// since it became ready, else since it was first seen.
    fn since_ns(&self) -> u64 {
        match (self.state, self.running_ns, self.ready_ns) {
            (State::Running, Some(running), _) => running,
            (_, _, Some(ready)) => ready,
            _ => self.seen_ns,
        }
    }

    fn task(&self) -> Task {
        // alloc: a task named in a look's answer, which owns its strings.
        Task { id: self.id, kind: self.kind.clone(), name: self.name.clone() }
    }
}

/// The blocker walk's buffers, kept from one look to the next so a warm look reuses them.
/// "Place" is a task's index among the unfinished ones, in slot order.
#[derive(Debug, Default)]
struct Walk {
    /// Each slot's place, or [`NONE`] when it is not unfinished.
    place: Vec<u32>,
    /// The unfinished tasks' slots, by place.
    unfinished: Vec<u32>,
    /// Each place's deps, by place ([`NONE`] for a dep that is not unfinished), place after
    /// place.
    deps: Vec<u32>,
    /// Where each place's deps begin in `deps` (`len + 1` of them).
    deps_at: Vec<u32>,
    /// The frontier's places: the tracked ones first, in the order their bits go.
    frontier: Vec<u32>,
    /// Each place's frontier bit, or [`NONE`].
    bit: Vec<u32>,
    /// Where each place's dependents begin in `dependents` (`len + 1` of them).
    start: Vec<u32>,
    /// Where each place's dependents end in `dependents`: a dependent that lists it twice is
    /// kept once.
    end: Vec<u32>,
    /// The unfinished tasks that list each place as a dep, by place, in place order.
    dependents: Vec<u32>,
    /// How many unfinished deps each place has left to walk.
    waiting: Vec<u32>,
    /// The places in dependency order: each after those it lists.
    order: Vec<u32>,
    /// Each place's set of tracked frontier bits, `words` to a place.
    sets: Vec<u64>,
    /// The longest path of unfinished tasks behind each place.
    down: Vec<u32>,
    /// How many unfinished tasks off the frontier carry each tracked bit.
    holds: Vec<u32>,
    /// The frontier as ranked: holds, chain, for how long in ms, id, place.
    ranked: Vec<(u32, u32, u64, u64, u32)>,
}

impl Fold {
    pub fn new() -> Fold {
        Fold::default()
    }

    /// The time, as nanoseconds since the build began.
    pub fn advance_clock(&mut self, at_ns: u64) {
        if !self.clocked {
            self.clocked = true;
            self.moved_ns = self.moved_ns.max(at_ns);
        }
        self.now_ns = self.now_ns.max(at_ns);
    }

    /// A task said: added, waiting, or, said again, given the new description while it keeps
    /// where it is. A new task past [`MOST_TASKS`] is ignored.
    pub fn take(&mut self, task: &TaskRecord) {
        self.add(task.id, &task.kind, &task.name, &task.deps);
    }

    /// A task moved: when it became ready or began running is kept, and a settle restarts the
    /// quiet time and, for a task that ran, enters the slowest list. A move of a task never
    /// said is ignored.
    pub fn moved(&mut self, id: u64, state: State) {
        let now = self.now_ns;
        let Some(&at) = self.slots.get(&id) else { return };
        let slot = &mut self.tasks[at as usize];
        let was = slot.state;
        slot.state = state;
        let mut ran = None;
        match state {
            State::Ready if was != State::Ready => slot.ready_ns = Some(now),
            State::Running if was != State::Running => slot.running_ns = Some(now),
            State::Done | State::Failed | State::Cancelled if was.unfinished() => {
                self.moved_ns = self.moved_ns.max(now);
                ran = slot.running_ns.map(|began| now.saturating_sub(began));
                // alloc: the settles of the last RECENT_MS, pruned at each look.
                self.settles.push_back(now);
                if state == State::Failed && self.failures.len() < FAILURES {
                    // alloc: at most FAILURES ids.
                    self.failures.push(id);
                }
            }
            _ => {}
        }
        if state.unfinished() && !was.unfinished() {
            self.slowest.retain(|&(_, it)| it != id);
        }
        if let Some(ran) = ran {
            self.keep_slowest(id, ran);
        }
    }

    /// One of the plane's records of the build, read through [`Records`] (the one place they
    /// become tasks) and taken with [`Fold::take_record`].
    pub fn take_event(&mut self, event: &Event) {
        let mut records = std::mem::take(&mut self.records);
        records.read(event, |record| self.take_record(record));
        self.records = records;
    }

    /// A plane record as a task: a [`Record::Task`] is [`Fold::take`], a [`Record::Moved`] is
    /// [`Fold::moved`].
    pub fn take_record(&mut self, record: Record<'_>) {
        match record {
            Record::Task { id, kind, name, deps } => self.add(id, kind, name, deps),
            Record::Moved { id, state, .. } => self.moved(id, state),
        }
    }

    /// The build as of the clock, with what this look found.
    pub fn look(&mut self, look: &Look<'_>) -> Now {
        let tally = self.tally();
        self.peak = self.peak.max(look.memory.held);
        let base = *self.releases_base.get_or_insert(look.memory.releases);
        let memory = Memory {
            held: look.memory.held,
            peak: self.peak,
            limit: look.memory.limit,
            // alloc: the look's answer owns its strings.
            over: look.memory.over.clone(),
            held_back: look.memory.held_back,
            releases: look.memory.releases.saturating_sub(base),
        };
        let busy = self.busy(look.threads);
        let running = self.running(look.threads);
        let expanding = Expansion { threads: look.expansion_threads, busy: self.busy(look.expanders) };
        let since = self.now_ns.saturating_sub(RECENT_MS * MS);
        while self.settles.front().is_some_and(|&at| at < since) {
            self.settles.pop_front();
        }
        let failures = self.failures.iter().filter_map(|&id| self.task(id)).collect(); // alloc: at most FAILURES
        let blockers = blockers(&self.tasks, &self.slots, self.now_ns, &mut self.walk, &busy);
        let phase = self.phase(&tally, look);
        let quiet_ms = self.quiet_ms(look);
        Now {
            at_ms: self.now_ns / MS,
            phase,
            // alloc: the look's answer owns its strings.
            stage: look.stage.to_owned(),
            // alloc: the look's answer owns its strings; a stall's line, rare.
            stall: look.stall.map(str::to_owned),
            tally,
            workers: look.workers,
            busy,
            running,
            recent: self.settles.len() as u32,
            failures,
            blockers,
            // alloc: the look's answer owns its downloads.
            downloads: look.downloads.to_vec(),
            slowest: self.slowest_tasks(),
            quiet_ms,
            memory,
            expanding,
        }
    }

    /// A task with its deps: [`Fold::take`]'s, by parts.
    fn add(&mut self, id: u64, kind: &str, name: &str, deps: &[u64]) {
        let build = build_kind(kind);
        if let Some(&at) = self.slots.get(&id) {
            let slot = &mut self.tasks[at as usize];
            slot.kind.clear();
            slot.kind.push_str(kind);
            slot.build = build;
            slot.name.clear();
            slot.name.push_str(name);
            slot.deps.clear();
            slot.deps.extend_from_slice(deps);
            return;
        }
        if self.tasks.len() >= MOST_TASKS {
            return;
        }
        // alloc: the id's slot, kept for the build.
        self.slots.insert(id, self.tasks.len() as u32);
        // alloc: the tasks, in the order said, grown as they come.
        self.tasks.push(Slot {
            id,
            // alloc: a task's kind, kept for the build.
            kind: kind.to_owned(),
            build,
            // alloc: a task's name, kept for the build.
            name: name.to_owned(),
            // alloc: a task's deps, kept for the build.
            deps: deps.to_vec(),
            state: State::Waiting,
            seen_ns: self.now_ns,
            ready_ns: None,
            running_ns: None,
        });
    }

    /// Puts a settled task that ran `ran_ns` in its place in the slowest list, if it is among
    /// the [`SLOWEST`].
    fn keep_slowest(&mut self, id: u64, ran_ns: u64) {
        self.slowest.retain(|&(_, it)| it != id);
        let key = (Reverse(ran_ns), id);
        let place = self.slowest.partition_point(|&(ns, it)| (Reverse(ns), it) < key);
        if place < SLOWEST {
            // alloc: the slowest tasks, at most SLOWEST + 1 for a moment.
            self.slowest.insert(place, (ran_ns, id));
            self.slowest.truncate(SLOWEST);
        }
    }

    fn tally(&self) -> Tally {
        let mut tally = Tally::default();
        for slot in &self.tasks {
            let count = match slot.state {
                State::Waiting => &mut tally.waiting,
                State::Ready => &mut tally.ready,
                State::Running => &mut tally.running,
                State::Done => &mut tally.done,
                State::Failed | State::Cancelled => &mut tally.failed,
            };
            *count += 1;
            tally.total += 1;
        }
        tally
    }

    /// The busy workers: each inside a span whose innermost is no wait, with the span it is
    /// shown by ([`shown`]) and its task, the one in its outermost span longest first, then by
    /// thread.
    fn busy(&self, threads: &[Worker]) -> Vec<Busy> {
        // alloc: the busy workers of one look, a few dozen at most.
        let mut on: Vec<&Worker> = threads
            .iter()
            .filter(|it| it.open.last().is_some_and(|span| span.awaits.is_none()))
            .collect();
        on.sort_by(|a, b| {
            let outer = |it: &Worker| it.open.first().map_or(0, |span| span.for_ms);
            outer(b)
                .cmp(&outer(a))
                .then_with(|| a.thread.cmp(&b.thread))
                .then_with(|| a.task.cmp(&b.task))
        });
        // alloc: the look's answer, one entry a busy worker.
        let mut busy = Vec::with_capacity(on.len());
        for worker in on {
            let (span, path) = shown(&worker.open);
            busy.push(Busy {
                // alloc: the look's answer owns its strings.
                thread: worker.thread.clone(),
                task: worker.task.and_then(|id| self.task(id)),
                span,
                path,
            });
        }
        busy
    }

    /// Every running task, longest first, then by id, at most [`RUNNING`]; each with the busy
    /// worker inside it, when one is.
    fn running(&self, threads: &[Worker]) -> Vec<Running> {
        // alloc: one look's running tasks, a few dozen to a few hundred.
        let mut on: Vec<(u64, u64, &Slot)> = self
            .tasks
            .iter()
            .filter(|slot| slot.state == State::Running)
            .map(|slot| (slot.running_ns.unwrap_or(slot.seen_ns), slot.id, slot))
            .collect();
        on.sort_unstable_by_key(|&(began, id, _)| (began, id));
        on.truncate(RUNNING);
        on.into_iter()
            .map(|(began, id, slot)| {
                let worker = threads
                    .iter()
                    .filter(|it| it.task == Some(id) && !it.open.is_empty())
                    // A thread doing its work before one waiting, then by name.
                    .min_by_key(|it| {
                        (it.open.last().is_some_and(|span| span.awaits.is_some()), &it.thread)
                    });
                let (span, path) = match worker {
                    Some(it) => {
                        let (span, path) = shown(&it.open);
                        (Some(span), path)
                    }
                    None => (None, Vec::new()), // alloc: none, empty
                };
                Running {
                    task: slot.task(),
                    for_ms: self.now_ns.saturating_sub(began) / MS,
                    // alloc: the look's answer owns its strings.
                    thread: worker.map(|it| it.thread.clone()),
                    span,
                    path,
                }
            })
            // alloc: the look's answer, at most RUNNING.
            .collect()
    }

    fn task(&self, id: u64) -> Option<Task> {
        self.slots.get(&id).map(|&at| self.tasks[at as usize].task())
    }

    /// The phase, by the rule in [`Fold`]'s documentation.
    fn phase(&self, tally: &Tally, look: &Look<'_>) -> Phase {
        if tally.total > 0 && !self.tasks.iter().any(|slot| slot.build) {
            return Phase::Running;
        }
        let unfinished = tally.waiting + tally.ready + tally.running;
        if tally.total > 0 && unfinished == 0 {
            return Phase::Finishing;
        }
        if !look.downloads.is_empty() && tally.running == 0 {
            return Phase::Fetching;
        }
        if tally.total == 0 {
            return Phase::Starting;
        }
        let is = |slot: &Slot, kind: NodeKind| slot.kind == kind.name();
        if tally.running > 0 {
            let running = || self.tasks.iter().filter(|slot| slot.state == State::Running);
            if running().any(|slot| is(slot, NodeKind::Test)) {
                return Phase::Testing;
            }
            if running().any(|slot| is(slot, NodeKind::Link)) {
                return Phase::Linking;
            }
            return Phase::Compiling;
        }
        if tally.ready == 0 {
            let stage = look.stage;
            if mentions(stage, "plan") || mentions(stage, "lowering") {
                return Phase::Planning;
            }
            let resolving = self.tasks.iter().any(|slot| {
                slot.state.unfinished()
                    && (is(slot, NodeKind::Manifest) || is(slot, NodeKind::Fetch))
            });
            if mentions(stage, "fetch") || mentions(stage, "resolve") || resolving {
                return Phase::Resolving;
            }
        }
        Phase::Compiling
    }

    /// How long nothing moved: since a task settled or a download got a byte. 0 before the
    /// clock is set.
    fn quiet_ms(&mut self, look: &Look<'_>) -> u64 {
        for download in look.downloads {
            let byte = self.now_ns.saturating_sub(download.idle_ms.saturating_mul(MS));
            self.moved_ns = self.moved_ns.max(byte);
        }
        if !self.clocked {
            return 0;
        }
        self.now_ns.saturating_sub(self.moved_ns) / MS
    }

    fn slowest_tasks(&self) -> Vec<Took> {
        self.slowest
            .iter()
            .filter_map(|&(ns, id)| Some(Took { task: self.task(id)?, ms: ns / MS }))
            // alloc: the look's answer, at most SLOWEST.
            .collect()
    }
}

/// Whether a span only wraps the work under it: a pool job or scope span, named by its closure's
/// type (`Pool::submit<...>::{{closure}}`), with no item or crate of its own. A view names the
/// work by the innermost span that is not one.
fn wrapper(span: &Span) -> bool {
    let named = |it: &str| {
        it == "job" || it.contains("submit") || it.contains("{{closure}}") || it.contains("::spawn")
    };
    span.krate.is_none()
        && (named(&span.name) || span.item.as_deref().is_some_and(named))
}

/// The span a worker is shown by, of its open spans (outermost first, at least one), and the
/// names of those strictly between its outermost and it: its innermost span, or, when that only
/// wraps a pool job ([`wrapper`]), the innermost one that does not, its wait kept (an
/// `awaits` on the innermost stays on the one shown). With every span a wrapper, the innermost
/// is named `pool job`, never by its closure's type.
fn shown(open: &[Span]) -> (Span, Vec<String>) {
    let last = open.len().saturating_sub(1);
    let at = open.iter().rposition(|it| !wrapper(it));
    // alloc: the look's answer owns its spans.
    let mut span = open[at.unwrap_or(last)].clone();
    if at.is_none() {
        // alloc: as above.
        span.name = "pool job".to_owned();
        span.item = None;
    }
    if span.awaits.is_none() {
        // alloc: as above.
        span.awaits = open[last].awaits.clone();
    }
    let end = at.unwrap_or(last);
    let path = open
        .get(1..end)
        .unwrap_or(&[])
        .iter()
        .filter(|it| !wrapper(it))
        // alloc: the look's answer owns its span names.
        .map(|it| it.name.clone())
        .collect();
    (span, path)
}

/// Whether `words` has `word` in it, ignoring ASCII case.
fn mentions(words: &str, word: &str) -> bool {
    let word = word.as_bytes();
    words.as_bytes().windows(word.len()).any(|it| it.eq_ignore_ascii_case(word))
}

/// The ranked blockers: the walk in [`Fold`]'s documentation. `busy` gives each running
/// blocker its worker's innermost span.
fn blockers(
    tasks: &[Slot],
    slots: &HashMap<u64, u32, Global>,
    now_ns: u64,
    walk: &mut Walk,
    busy: &[Busy],
) -> Vec<Blocker> {
    let Walk {
        place,
        unfinished,
        deps,
        deps_at,
        frontier,
        bit,
        start,
        end,
        dependents,
        waiting,
        order,
        sets,
        down,
        holds,
        ranked,
    } = walk;

    // The unfinished tasks, by place in slot order.
    place.clear();
    // alloc: the walk's buffer, kept from look to look.
    place.resize(tasks.len(), NONE);
    unfinished.clear();
    for (at, it) in tasks.iter().enumerate() {
        if it.state.unfinished() {
            place[at] = unfinished.len() as u32;
            // alloc: the walk's buffer, kept from look to look.
            unfinished.push(at as u32);
        }
    }
    let len = unfinished.len();
    let slot = |at: u32| &tasks[unfinished[at as usize] as usize];

    // Each place's deps by place, each id asked of the map once a look; the frontier: the
    // places whose every dep is said and settled.
    deps.clear();
    deps_at.clear();
    // alloc: the walk's buffer, kept from look to look.
    deps_at.push(0);
    frontier.clear();
    for at in 0..len as u32 {
        let mut open = false;
        for dep in &slot(at).deps {
            let of = slots.get(dep).map(|&it| it as usize);
            open |= of.is_none_or(|it| tasks[it].state.unfinished());
            // alloc: the walk's buffer, kept from look to look.
            deps.push(of.map_or(NONE, |it| place[it]));
        }
        // alloc: the walk's buffer, kept from look to look.
        deps_at.push(deps.len() as u32);
        if !open {
            // alloc: the walk's buffer, kept from look to look.
            frontier.push(at);
        }
    }
    let deps_of = |at: usize| &deps[deps_at[at] as usize..deps_at[at + 1] as usize];

    // Each place's dependents, by counting and then filling, a duplicate dep kept once; how
    // many unfinished deps each waits on.
    start.clear();
    // alloc: the walk's buffer, kept from look to look.
    start.resize(len + 1, 0);
    for at in 0..len {
        for &dep in deps_of(at) {
            if dep != NONE {
                start[dep as usize + 1] += 1;
            }
        }
    }
    let mut sum = 0;
    for it in start.iter_mut() {
        sum += *it;
        *it = sum;
    }
    end.clear();
    // alloc: the walk's buffer, kept from look to look.
    end.extend_from_slice(&start[..len]);
    dependents.clear();
    // alloc: the walk's buffer, kept from look to look.
    dependents.resize(start[len] as usize, NONE);
    waiting.clear();
    // alloc: the walk's buffer, kept from look to look.
    waiting.resize(len, 0);
    for at in 0..len {
        for &dep in deps_of(at) {
            if dep == NONE {
                continue;
            }
            let (from, to) = (start[dep as usize] as usize, end[dep as usize] as usize);
            if to > from && dependents[to - 1] == at as u32 {
                continue;
            }
            dependents[to] = at as u32;
            end[dep as usize] += 1;
            waiting[at] += 1;
        }
    }
    let after = |at: u32| &dependents[start[at as usize] as usize..end[at as usize] as usize];

    // The frontier's bits: the running ones first, then the longest ready, then by id.
    frontier.sort_by_key(|&at| {
        let it = slot(at);
        (it.state != State::Running, it.since_ns(), it.id)
    });
    let tracked = frontier.len().min(FRONTIER);
    let words = tracked.div_ceil(64);
    bit.clear();
    // alloc: the walk's buffer, kept from look to look.
    bit.resize(len, NONE);
    sets.clear();
    // alloc: the walk's buffer, kept from look to look.
    sets.resize(len * words, 0);
    for (b, &at) in frontier[..tracked].iter().enumerate() {
        bit[at as usize] = b as u32;
        sets[at as usize * words + b / 64] |= 1u64 << (b % 64);
    }

    // Dependency order (Kahn's): each place goes once its unfinished deps have gone, so the
    // walk takes one step per edge and leaves out what waits in a cycle.
    order.clear();
    // alloc: the walk's buffer, kept from look to look.
    order.extend((0..len as u32).filter(|&at| waiting[at as usize] == 0));
    let mut head = 0;
    while head < order.len() {
        let at = order[head];
        head += 1;
        for &next in after(at) {
            waiting[next as usize] -= 1;
            if waiting[next as usize] == 0 {
                // alloc: the walk's buffer, kept from look to look.
                order.push(next);
            }
        }
    }

    // Each place's set, carried to its dependents in dependency order.
    if words > 0 {
        for &at in order.iter() {
            for &next in after(at) {
                for word in 0..words {
                    let carried = sets[at as usize * words + word];
                    sets[next as usize * words + word] |= carried;
                }
            }
        }
    }

    // The longest path behind each place, in the reverse order.
    down.clear();
    // alloc: the walk's buffer, kept from look to look.
    down.resize(len, 0);
    for &at in order.iter().rev() {
        let longest = after(at).iter().map(|&next| down[next as usize] + 1).max();
        down[at as usize] = longest.unwrap_or(0);
    }

    // What each tracked bit holds: the places off the frontier that carry it.
    holds.clear();
    // alloc: the walk's buffer, kept from look to look.
    holds.resize(tracked, 0);
    for (at, &b) in bit.iter().enumerate() {
        if b != NONE {
            continue;
        }
        for word in 0..words {
            let mut set = sets[at * words + word];
            while set != 0 {
                holds[word * 64 + set.trailing_zeros() as usize] += 1;
                set &= set - 1;
            }
        }
    }

    // Ranked by holds, chain, for how long, then id; the first BLOCKERS named.
    ranked.clear();
    for &at in frontier.iter() {
        let it = slot(at);
        let b = bit[at as usize];
        let held = if b == NONE { 0 } else { holds[b as usize] };
        let for_ms = now_ns.saturating_sub(it.since_ns()) / MS;
        // alloc: the walk's buffer, kept from look to look.
        ranked.push((held, down[at as usize], for_ms, it.id, at));
    }
    ranked.sort_unstable_by_key(|&(held, chain, for_ms, id, _)| {
        (Reverse(held), Reverse(chain), Reverse(for_ms), id)
    });
    // alloc: the look's answer, at most BLOCKERS.
    let mut named = Vec::with_capacity(ranked.len().min(BLOCKERS));
    for &(held, chain, for_ms, id, at) in ranked.iter().take(BLOCKERS) {
        let it = slot(at);
        let span = busy
            .iter()
            .find(|worker| worker.task.as_ref().is_some_and(|task| task.id == id))
            // alloc: the look's answer owns its spans.
            .map(|worker| worker.span.clone());
        let behind = after(at)
            .iter()
            .take(BEHIND)
            // alloc: the look's answer owns its names, at most BEHIND.
            .map(|&next| slot(next).name.clone())
            // alloc: the look's answer, at most BEHIND.
            .collect();
        named.push(Blocker {
            task: it.task(),
            running: it.state == State::Running,
            for_ms,
            span,
            holds: held,
            chain,
            behind,
        });
    }
    named
}

#[cfg(test)]
mod tests;
