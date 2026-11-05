//! A build as it runs now: the one model of it that its client's live view, `blaze status`
//! and the explorer's daemon source read (docs/v2/plan/observe/build-view.md). [`Now`] is the
//! wire type, said to a build's client about twice a second; [`Fold`] is the sans-IO core
//! that builds it from the plane's graph records ([`Event::Node`], [`Event::Nodes`],
//! [`Event::Moved`]), the clock it is handed ([`Fold::advance_clock`]) and what one look at
//! the daemon found ([`Look`]: each worker's open spans, the downloads, the stage's words).
//!
//! What it answers: the phase and what it really does; how many workers are busy and, for each,
//! its innermost span; what the unfinished tasks wait on (the [`Blocker`]s) and how much each
//! holds back; the downloads; the slowest tasks so far; and how long nothing moved.

#![blaze::sans_io]

use serde::{Deserialize, Serialize};

use crate::events::{Event, NodeKind, NodeState};

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
    pub tally: Tally,
    /// The pool's workers; those not in `busy` are idle.
    pub workers: u32,
    /// Each busy worker, the one inside its span longest first.
    pub busy: Vec<Busy>,
    /// The unfinished tasks whose inputs are all done (running or ready), the one holding the
    /// most back first ([`Blocker`]). At most [`BLOCKERS`].
    pub blockers: Vec<Blocker>,
    /// Every download in flight, oldest first.
    pub downloads: Vec<Download>,
    /// The settled tasks that took longest, longest first. At most [`SLOWEST`].
    pub slowest: Vec<Took>,
    /// How long since a task settled, a download got a byte or a span closed.
    pub quiet_ms: u64,
    /// The daemon's memory against its ceiling, and how pressed it is.
    pub memory: Memory,
}
keys [busy, at_ms, phase, stage, tally, memory, slowest, workers, blockers, quiet_ms, downloads]
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

    /// Whether nothing moves: no worker is busy, no download in flight, and nothing settled
    /// for [`STALL_MS`]. With a worker busy the build is slow, not stalled.
    pub fn stalled(&self) -> bool {
        self.quiet_ms >= STALL_MS && self.busy.is_empty() && self.downloads.is_empty()
    }

    /// Whether nothing settled for [`STALL_MS`] while a worker is busy or a download runs.
    pub fn slow(&self) -> bool {
        self.quiet_ms >= STALL_MS && !self.stalled()
    }
}

crate::in_key_order! {
/// The daemon's memory as admission sees it (`blaze_graph::admission::Pressure`): what the
/// process holds against the ceiling it runs under, the subsystem past its fair share, and what
/// the plane holds back for memory. Andrew's build of 2026-11-05 reached 99 GiB with nothing on
/// screen to say so: a view shows this in its header, always.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
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
    /// Releases (front-end memory let go under pressure) during this build.
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

crate::in_key_order! {
/// How many of the build's tasks are where.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    pub waiting: u32,
    pub ready: u32,
    pub running: u32,
    /// Built, cached or skipped.
    pub done: u32,
    pub failed: u32,
    pub total: u32,
}
keys [done, ready, total, failed, running, waiting]
}

crate::in_key_order! {
/// A task of the build's graph: a plane node.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Task {
    pub id: u32,
    pub kind: NodeKind,
    /// `<package> <target>`, as the plane names it.
    pub name: String,
}
keys [id, kind, name]
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

/// A worker thread as one look found it: its name, the plane node its `task` span runs (when
/// it is inside one), and its open spans, outermost first. A worker with no open span is idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worker {
    pub thread: String,
    pub node: Option<u32>,
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
    /// Spans closed on the build's threads since the last look: progress.
    pub closed: u64,
    /// The daemon's memory now ([`Memory::peak`] and [`Memory::releases`] are the fold's to
    /// keep: the look's are ignored).
    pub memory: &'a Memory,
}

/// The sans-IO core: folds a build's graph records and looks into [`Now`]s.
#[derive(Debug, Default)]
pub struct Fold {
    // Filled in by the fold's implementation.
}

impl Fold {
    pub fn new() -> Fold {
        Fold::default()
    }

    /// The time, as nanoseconds since the build began.
    pub fn advance_clock(&mut self, _at_ns: u64) {
        todo!("the fold")
    }

    /// One of the plane's records of the build. Records of any other kind are ignored.
    pub fn take(&mut self, _event: &Event) {
        todo!("the fold")
    }

    /// A release ran (memory let go under pressure): counted in [`Memory::releases`].
    pub fn released(&mut self) {
        todo!("the fold")
    }

    /// The build as of the clock, with what this look found.
    pub fn look(&mut self, _look: &Look<'_>) -> Now {
        todo!("the fold")
    }
}

#[allow(dead_code, reason = "NodeState is the fold's input")]
fn _uses(_: NodeState) {}

