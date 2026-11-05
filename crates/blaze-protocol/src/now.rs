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

use std::cmp::Reverse;

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
///
/// It keeps the plane's tasks by id (the plane's ids are dense), each with its kind, name, deps,
/// where it is and when it became ready, began running and was first seen, and a short list of
/// the slowest settled tasks, kept as they settle. A [`Fold::look`] reads them with what the
/// look found; it reuses its buffers from one look to the next.
///
/// **Phase.** The first of these that holds:
///
/// 1. [`Phase::Finishing`]: it knows a task and none is unfinished (waiting, ready or running).
/// 2. [`Phase::Fetching`]: a download is in flight and no task runs.
/// 3. [`Phase::Starting`]: it knows no task (and no download is in flight).
/// 4. A task runs: [`Phase::Testing`] when one of them is a [`NodeKind::Test`], else
///    [`Phase::Linking`] when one is a [`NodeKind::Link`], else [`Phase::Compiling`].
/// 5. Nothing runs and nothing is ready: [`Phase::Planning`] when the stage's words mention
///    `plan` or `lowering`, else [`Phase::Resolving`] when they mention `fetch` or `resolve`
///    or a [`NodeKind::Manifest`] or [`NodeKind::Fetch`] task is unfinished.
/// 6. Otherwise [`Phase::Compiling`].
///
/// **Blockers.** The frontier is every unfinished task whose deps have all settled (a failed
/// dep counts as settled). The unfinished tasks are walked in dependency order (Kahn's, one
/// step per edge, so a cycle, which the plane never sends, is left out rather than walked),
/// each carrying the set of frontier tasks it waits on, directly or not, as a bitset over the
/// first 256 of the frontier (the running ones first, then the longest ready). A
/// blocker's `holds` is how many unfinished tasks off the frontier carry it, `chain` the
/// longest path of unfinished tasks behind it. A frontier task past the cap is still a
/// blocker, holding 0: it is carried by no one.
#[derive(Debug, Default)]
pub struct Fold {
    /// The clock, as nanoseconds since the build began; it never goes back.
    now_ns: u64,
    /// Whether the clock was set: the quiet time counts from its first setting.
    clocked: bool,
    /// When a task last settled, a download last got a byte or a look said a span closed.
    moved_ns: u64,
    /// The graph's tasks, by plane id.
    tasks: Vec<Option<Slot>>,
    /// Releases counted ([`Memory::releases`]).
    releases: u32,
    /// The most memory held at any look ([`Memory::peak`]).
    peak: u64,
    /// The slowest settled tasks that ran, as how long each ran and its id, longest first and
    /// then by id: at most [`SLOWEST`].
    slowest: Vec<(u64, u32)>,
    /// What reads an [`Event::Nodes`] keeps from one to the next.
    order: Order,
    /// The blocker walk's buffers.
    walk: Walk,
}

/// The highest plane id the fold keeps a task for. The plane's ids are dense, so a record of a
/// higher one is no build's that a fold could hold, and it is ignored.
const MOST_ID: u32 = 1 << 24;

/// How many frontier tasks the blocker walk tells apart; the rest hold 0.
const FRONTIER: usize = 256;

/// Nanoseconds in a millisecond.
const MS: u64 = 1_000_000;

/// No place: a task off the walk, or off the frontier.
const NONE: u32 = u32::MAX;

/// Where a task is, as the fold counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    Waiting,
    Ready,
    Running,
    /// Built, cached or skipped.
    Done,
    /// Failed or cancelled.
    Failed,
}

impl At {
    fn of(state: &NodeState) -> At {
        match state {
            NodeState::Waiting => At::Waiting,
            NodeState::Ready => At::Ready,
            NodeState::Running => At::Running,
            NodeState::Built | NodeState::Cached { .. } | NodeState::Skipped { .. } => At::Done,
            NodeState::Failed { .. } | NodeState::Cancelled => At::Failed,
        }
    }

    fn unfinished(self) -> bool {
        matches!(self, At::Waiting | At::Ready | At::Running)
    }
}

/// A task of the graph as the fold keeps it.
#[derive(Debug)]
struct Slot {
    kind: NodeKind,
    name: String,
    deps: Vec<u32>,
    at: At,
    /// When its node record came.
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
        match (self.at, self.running_ns, self.ready_ns) {
            (At::Running, Some(running), _) => running,
            (_, _, Some(ready)) => ready,
            _ => self.seen_ns,
        }
    }

    fn task(&self, id: u32) -> Task {
        // alloc: a task named in a look's answer, which owns its strings.
        Task { id, kind: self.kind, name: self.name.clone() }
    }
}

/// The blocker walk's buffers, kept from one look to the next so a warm look reuses them.
/// "Place" is a task's index among the unfinished ones, in id order.
#[derive(Debug, Default)]
struct Walk {
    /// Each id's place, or [`NONE`] when it is not unfinished.
    place: Vec<u32>,
    /// The unfinished tasks' ids, by place.
    unfinished: Vec<u32>,
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
    ranked: Vec<(u32, u32, u64, u32, u32)>,
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

    /// One of the plane's records of the build. Records of any other kind are ignored, and so
    /// are a malformed [`Event::Nodes`], a move of a node never said and an id past any graph
    /// a fold holds.
    pub fn take(&mut self, event: &Event) {
        match event {
            Event::Node { id, kind, name, deps } => self.node(*id, *kind, name, deps),
            Event::Nodes { .. } => {
                if let Some(Ok(batch)) = event.nodes(&mut self.order) {
                    for node in batch.iter() {
                        self.node(node.id, node.kind, node.name, node.deps);
                    }
                }
            }
            Event::Moved { id, state } => self.moved(*id, state),
            _ => {}
        }
    }

    /// A release ran (memory let go under pressure): counted in [`Memory::releases`].
    pub fn released(&mut self) {
        self.releases = self.releases.saturating_add(1);
    }

    /// The build as of the clock, with what this look found.
    pub fn look(&mut self, look: &Look<'_>) -> Now {
        let tally = self.tally();
        self.peak = self.peak.max(look.memory.held);
        let memory = Memory {
            held: look.memory.held,
            peak: self.peak,
            limit: look.memory.limit,
            // alloc: the look's answer owns its strings.
            over: look.memory.over.clone(),
            held_back: look.memory.held_back,
            releases: self.releases,
        };
        let busy = self.busy(look.threads);
        let blockers = blockers(&self.tasks, self.now_ns, &mut self.walk, &busy);
        let phase = self.phase(&tally, look);
        let quiet_ms = self.quiet_ms(look);
        Now {
            at_ms: self.now_ns / MS,
            phase,
            // alloc: the look's answer owns its strings.
            stage: look.stage.to_owned(),
            tally,
            workers: look.workers,
            busy,
            blockers,
            // alloc: the look's answer owns its downloads.
            downloads: look.downloads.to_vec(),
            slowest: self.slowest_tasks(),
            quiet_ms,
            memory,
        }
    }

    /// A node record: a task with its deps. Said again, it takes the new description and keeps
    /// where it is.
    fn node(&mut self, id: u32, kind: NodeKind, name: &str, deps: &[u32]) {
        if id > MOST_ID {
            return;
        }
        let at = id as usize;
        if self.tasks.len() <= at {
            // alloc: the graph's tasks, by id, grown as the plane says them.
            self.tasks.resize_with(at + 1, || None);
        }
        match &mut self.tasks[at] {
            Some(slot) => {
                slot.kind = kind;
                slot.name.clear();
                slot.name.push_str(name);
                slot.deps.clear();
                slot.deps.extend_from_slice(deps);
            }
            empty @ None => {
                *empty = Some(Slot {
                    kind,
                    // alloc: a task's name, kept for the build.
                    name: name.to_owned(),
                    // alloc: a task's deps, kept for the build.
                    deps: deps.to_vec(),
                    at: At::Waiting,
                    seen_ns: self.now_ns,
                    ready_ns: None,
                    running_ns: None,
                });
            }
        }
    }

    /// A task moved: when it became ready or began running is kept, and a settle restarts the
    /// quiet time and, for a task that ran, enters the slowest list.
    fn moved(&mut self, id: u32, state: &NodeState) {
        let now = self.now_ns;
        let Some(Some(slot)) = self.tasks.get_mut(id as usize) else { return };
        let (was, at) = (slot.at, At::of(state));
        slot.at = at;
        let mut ran = None;
        match at {
            At::Ready if was != At::Ready => slot.ready_ns = Some(now),
            At::Running if was != At::Running => slot.running_ns = Some(now),
            At::Done | At::Failed if was.unfinished() => {
                self.moved_ns = self.moved_ns.max(now);
                ran = slot.running_ns.map(|began| now.saturating_sub(began));
            }
            _ => {}
        }
        if at.unfinished() && !was.unfinished() {
            self.slowest.retain(|&(_, it)| it != id);
        }
        if let Some(ran) = ran {
            self.keep_slowest(id, ran);
        }
    }

    /// Puts a settled task that ran `ran_ns` in its place in the slowest list, if it is among
    /// the [`SLOWEST`].
    fn keep_slowest(&mut self, id: u32, ran_ns: u64) {
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
        for slot in self.tasks.iter().flatten() {
            let count = match slot.at {
                At::Waiting => &mut tally.waiting,
                At::Ready => &mut tally.ready,
                At::Running => &mut tally.running,
                At::Done => &mut tally.done,
                At::Failed => &mut tally.failed,
            };
            *count += 1;
            tally.total += 1;
        }
        tally
    }

    /// The busy workers: each with an open span, its innermost span and its task, the one in
    /// its outermost span longest first, then by thread.
    fn busy(&self, threads: &[Worker]) -> Vec<Busy> {
        // alloc: the busy workers of one look, a few dozen at most.
        let mut on: Vec<&Worker> = threads.iter().filter(|it| !it.open.is_empty()).collect();
        on.sort_by(|a, b| {
            let outer = |it: &Worker| it.open.first().map_or(0, |span| span.for_ms);
            outer(b)
                .cmp(&outer(a))
                .then_with(|| a.thread.cmp(&b.thread))
                .then_with(|| a.node.cmp(&b.node))
        });
        // alloc: the look's answer, one entry a busy worker.
        let mut busy = Vec::with_capacity(on.len());
        for worker in on {
            let Some((span, outer)) = worker.open.split_last() else { continue };
            // The spans strictly between the outermost (the `task` span) and the innermost.
            let between = outer.get(1..).unwrap_or(&[]);
            busy.push(Busy {
                // alloc: the look's answer owns its strings.
                thread: worker.thread.clone(),
                task: worker.node.and_then(|id| self.task(id)),
                // alloc: the look's answer owns its spans.
                span: span.clone(),
                // alloc: the look's answer owns its span names.
                path: between.iter().map(|it| it.name.clone()).collect(),
            });
        }
        busy
    }

    fn task(&self, id: u32) -> Option<Task> {
        self.slot(id).map(|slot| slot.task(id))
    }

    fn slot(&self, id: u32) -> Option<&Slot> {
        self.tasks.get(id as usize).and_then(Option::as_ref)
    }

    /// The phase, by the rule in [`Fold`]'s documentation.
    fn phase(&self, tally: &Tally, look: &Look<'_>) -> Phase {
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
        let tasks = || self.tasks.iter().flatten();
        if tally.running > 0 {
            let running = || tasks().filter(|slot| slot.at == At::Running);
            if running().any(|slot| slot.kind == NodeKind::Test) {
                return Phase::Testing;
            }
            if running().any(|slot| slot.kind == NodeKind::Link) {
                return Phase::Linking;
            }
            return Phase::Compiling;
        }
        if tally.ready == 0 {
            let stage = look.stage;
            if mentions(stage, "plan") || mentions(stage, "lowering") {
                return Phase::Planning;
            }
            let resolving = tasks().any(|slot| {
                slot.at.unfinished() && matches!(slot.kind, NodeKind::Manifest | NodeKind::Fetch)
            });
            if mentions(stage, "fetch") || mentions(stage, "resolve") || resolving {
                return Phase::Resolving;
            }
        }
        Phase::Compiling
    }

    /// How long nothing moved: since a task settled, this look said a span closed, or a
    /// download got a byte. 0 before the clock is set.
    fn quiet_ms(&mut self, look: &Look<'_>) -> u64 {
        if look.closed > 0 {
            self.moved_ns = self.now_ns;
        }
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

/// Whether `words` has `word` in it, ignoring ASCII case.
fn mentions(words: &str, word: &str) -> bool {
    let word = word.as_bytes();
    words.as_bytes().windows(word.len()).any(|it| it.eq_ignore_ascii_case(word))
}

/// The ranked blockers: the walk in [`Fold`]'s documentation. `busy` gives each running
/// blocker its worker's innermost span.
fn blockers(tasks: &[Option<Slot>], now_ns: u64, walk: &mut Walk, busy: &[Busy]) -> Vec<Blocker> {
    let Walk {
        place,
        unfinished,
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
    let slot = |id: u32| tasks.get(id as usize).and_then(Option::as_ref);
    let settled = |id: u32| slot(id).is_some_and(|it| !it.at.unfinished());

    // The unfinished tasks, by place in id order.
    place.clear();
    // alloc: the walk's buffer, kept from look to look.
    place.resize(tasks.len(), NONE);
    unfinished.clear();
    for (id, it) in tasks.iter().enumerate() {
        if it.as_ref().is_some_and(|it| it.at.unfinished()) {
            place[id] = unfinished.len() as u32;
            // alloc: the walk's buffer, kept from look to look.
            unfinished.push(id as u32);
        }
    }
    let len = unfinished.len();
    let place_of = |id: u32| place.get(id as usize).copied().filter(|&it| it != NONE);

    // Each place's dependents, by counting and then filling, a duplicate dep kept once; the
    // frontier; how many unfinished deps each waits on.
    start.clear();
    // alloc: the walk's buffer, kept from look to look.
    start.resize(len + 1, 0);
    frontier.clear();
    for (at, &id) in unfinished.iter().enumerate() {
        let Some(it) = slot(id) else { continue };
        let mut open = false;
        for &dep in &it.deps {
            if let Some(dep) = place_of(dep) {
                start[dep as usize + 1] += 1;
            }
            open |= !settled(dep);
        }
        if !open {
            // alloc: the walk's buffer, kept from look to look.
            frontier.push(at as u32);
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
    for (at, &id) in unfinished.iter().enumerate() {
        let Some(it) = slot(id) else { continue };
        for &dep in &it.deps {
            let Some(dep) = place_of(dep) else { continue };
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
        let id = unfinished[at as usize];
        slot(id).map_or((true, 0, id), |it| (it.at != At::Running, it.since_ns(), id))
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
        let id = unfinished[at as usize];
        let Some(it) = slot(id) else { continue };
        let b = bit[at as usize];
        let held = if b == NONE { 0 } else { holds[b as usize] };
        let for_ms = now_ns.saturating_sub(it.since_ns()) / MS;
        // alloc: the walk's buffer, kept from look to look.
        ranked.push((held, down[at as usize], for_ms, id, at));
    }
    ranked.sort_unstable_by_key(|&(held, chain, for_ms, id, _)| {
        (Reverse(held), Reverse(chain), Reverse(for_ms), id)
    });
    // alloc: the look's answer, at most BLOCKERS.
    let mut named = Vec::with_capacity(ranked.len().min(BLOCKERS));
    for &(held, chain, for_ms, id, at) in ranked.iter().take(BLOCKERS) {
        let Some(it) = slot(id) else { continue };
        let span = busy
            .iter()
            .find(|worker| worker.task.as_ref().is_some_and(|task| task.id == id))
            // alloc: the look's answer owns its spans.
            .map(|worker| worker.span.clone());
        let behind = after(at)
            .iter()
            .take(BEHIND)
            .filter_map(|&next| slot(unfinished[next as usize]))
            // alloc: the look's answer owns its names, at most BEHIND.
            .map(|next| next.name.clone())
            // alloc: the look's answer, at most BEHIND.
            .collect();
        named.push(Blocker {
            task: it.task(id),
            running: it.at == At::Running,
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
