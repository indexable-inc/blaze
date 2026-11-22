//! What a run used and where its time went, for every program on bstd by default (D178,
//! docs/v2/plan/design/end-state.md): no crate above bstd adds a line for it.
//!
//! Each worker keeps one [`Cell`]: aggregates, each bounded and allocation-free on the hot path
//! (every buffer is reserved when the cell is made and never grows). At exit, after the pool
//! stopped, one thread merges every worker's cell into one typed [`Report`] with no lock,
//! reconciles it against [`blaze_meter::Process::now`], and the runtime writes it to the
//! [`Sink`] the program was granted, on every exit path.
//!
//! # Where the cell lives
//!
//! In bstd's own fields of a worker (`workers.rs` `Own`, beside its `Tally`), not a D171 slot:
//! the hooks in `Cx::span`, the pool loop and `Io::call` must reach it without a slot handle,
//! and a slot cell is at most `CELL_MOST` (4096) bytes. Its shape is a slot's anyway
//! (`Cell::new(CellOpts, alloc)` is a `make(WorkerId, Alloc)`), so it moves to a slot unchanged
//! if `Own` should shrink. A `Workers` has cells only when registered with
//! `WorkersOpts::telemetry(level, alloc)`; `alloc` is the process heap's shared handle, and each
//! cell is made at its worker's first hold. Without the registration every hook is a no-op.
//!
//! # Wiring (the spec of the hooks)
//!
//! 1. **The level is chosen before the runtime starts.** `entry::run_with_heap_at(os, level,
//!    main)` takes `level: fn(&Args) -> Level`; the runtime reads the command line, calls
//!    `level` once, registers the cells at that level, and only then makes the pool. Nothing is
//!    raised after the start, and the choice sees the arguments only (rule 75). `blaze` passes
//!    `Spans` for `--stats` among its command's flags, else `Totals`.
//! 2. **A span is the life of the value [`Cx::span`](crate::cx::Cx::span) returns.** From `Spans`,
//!    `span` opens a slot of the worker's open table ([`OPENS`] slots: past it a span is counted
//!    in [`Cell::unrecorded`], not kept) and puts its token in the returned `Cx` alone: a child
//!    of it (`until`, `scoped`, `span`, a task's context) copies its path but not its token.
//!    `Cx` is not `Send` and a started task never changes thread, so the token drops on the
//!    worker that opened it, and its drop is the close: there is no close call to forget
//!    (rule 76). The span's wall time is open to close on the opener; its count is one per open.
//! 3. **Spans across tasks.** A task carries its spawner's path (in its `Seed`). At its start
//!    its worker's cell takes a snapshot; at its end it charges what it used, less what tasks
//!    nested in it charged, to its path and each ancestor (rows are inclusive), adding to the
//!    sums and not to the count or the wall. Every span or task still open on that worker
//!    encloses the task (a worker's stretches nest: a task runs inline, or on top of a wait) and
//!    is told the task's use was charged apart, so its own close subtracts it: no use is counted
//!    twice. So a span whose tasks ran on many workers has, in the merged report, the opener's
//!    one close and wall time, and as its use the **sum over every worker** that ran under it.
//!    A task's own time in the chain is its wall less the waits charged while it was current.
//! 4. **Waits name their task from the cell.** A task's start makes it the worker cell's
//!    current task (its id is `worker << 40 | serial`, the one below it saved for its end);
//!    `Cx::charged` reads that id from the cell to build the wait's [`WaitRec`]: no wait site
//!    changes. A wait's target is `Target::Thing` with the site's id: no wait site knows a
//!    joined task's id yet, so the chain does not link through joins (a gap, stated).
//! 5. **Effects.** `Io::call` adds the call's kind and time to its worker's effect row (bytes
//!    are not known there and are 0).
//! 6. **The exit.** After `main` and the drain, on one thread, with no task running, the
//!    runtime holds every made cell, stops its pool time at now, merges it with its tally into
//!    one [`Report`], finishes it with how `main` ended, reconciles it against the process's
//!    totals at the start and now, and writes its text to standard error, from `Spans` only.
//!    A `main` that panics has it written as `Panicked` by a guard's drop on the unwind. A
//!    report that cannot be gathered or written fails the run, naming why.
//! 7. **Not wired yet** (each says so where it shows): the pool loop's park, unpark and idle
//!    marks (the pool's threads reach no `Workers`); the sampler's drivers (the report says
//!    `sampler: not started`, or `not on this target` off Linux and macOS, where there is no
//!    driver: spans and distributions are kept without samples); allocation by site; the grant
//!    of a [`Sink`] (`grant.rs` `Kind::Telemetry`) for a file or a CBOR form.
//!
//! # The aggregates, their bounds, their cost per event
//!
//! | aggregate | bound | cost per event |
//! |---|---|---|
//! | [`Dist`] | 64 power-of-two buckets, 512 B | `record`: one leading-zero count and one add |
//! | [`Spans`] | [`SPAN_ROWS`] paths of at most [`SPAN_DEPTH`] names; past the depth a path is its ancestor's marked `deeper`, past the rows it is the `other` row: never dropped | open: one [`Brief`] snapshot (20 loads) and an FNV step over the name; close: a snapshot, 20 subtractions, a probe, 42 adds or maxes |
//! | [`Waits`] | [`WAIT_DIST_ROWS`] names and an `other` row | a probe over at most 16 names, three adds, a `Dist::record` |
//! | [`Effects`] | one row per [`blaze_meter::Call`] kind (13), syncs a row apart | three adds and a max; a `Dist::record` from `Spans` up |
//! | [`Pool`] | a ring of [`DEPTH_SAMPLES`] ready-queue depths, the longest task | an add per stretch, a compare per task, a store per sample |
//! | [`Chain`] | [`CHAIN_TASKS`] tasks and [`CHAIN_WAITS`] waits a worker; past either, the longest by time are kept (a min-heap) | a push, or past the cap a compare and a sift of at most 10 levels |
//! | [`Sites`] (`Detail`) | [`SITE_ROWS`] task names and an `other` row | a probe and a `Dist::record` of the allocation's size |
//!
//! # Levels
//!
//! [`Level`] is a run-time option, chosen by the program from its arguments before the runtime
//! starts ("Wiring", 1):
//!
//! - [`Level::Totals`], always on: the worker's `Tally` (counts, calls, waits by name), the
//!   effect rows' count, bytes and time, and the pool's facts. Per event a few adds on a cell
//!   the code already holds: under 5 ns; nothing reads the system.
//! - [`Level::Spans`] adds the span table, a [`Dist`] for every wait name and effect kind, and
//!   the chain's records: per span two cell snapshots and the row update, about 60 to 100 ns; per
//!   wait or effect about 5 ns more; no system call.
//! - [`Level::Detail`] adds the system's readings to every span snapshot (instructions,
//!   cycles, user and system time, faults: one `perf` read and one `getrusage` on Linux, one
//!   `thread_info` on macOS, about 0.5 to 2 µs a snapshot) and allocation by site ([`Sites`]:
//!   per allocation a thread-local probe and a bucket add, 5 to 10 ns).
//!
//! From `Spans` the [`sampler`] runs too (on-CPU share by function, by top-level span, and the
//! callers of the hottest; its own module doc gives the drivers and costs). Its hot-path hooks
//! are two stores a park and two a top-level span; the sample itself is the driver's.
//!
//! Nothing of the report reaches a key or a record (rule 25): the sink is the one way out.

use core::fmt;

use blaze_own::planted::Plants;

mod brief;
mod chain;
mod dist;
mod effects;
mod encode;
mod pool;
mod report;
pub mod sampler;
mod sink;
mod sites;
mod spans;
mod waits;

#[cfg(test)]
mod tests;

pub use brief::{Brief, FIELDS, Field, Unit};
pub use chain::{
    CHAIN_SHOWN, CHAIN_TASKS, CHAIN_WAITS, Chain, ChainReport, Link, Partial, Target, TaskRec, Then, WaitRec, longest, longest_chain,
};
pub use dist::{BUCKETS, Bracket, Dist, bucket};
pub use effects::{EffectRow, Effects};
pub use encode::ENCODING;
pub use pool::{DEPTH_SAMPLES, DepthSample, Idle, Longest, Pool};
pub use report::{Exit, REPORT_SITE_ROWS, REPORT_SPAN_ROWS, REPORT_WAIT_ROWS, Reconciled, Report, ReportOpts, WorkerRow};
pub use sampler::{Period, Profile, Roster, SamplerError, SamplerPlant, SamplerRow};
pub use sink::{Form, Sink, SinkError, SinkOpts, SinkTo};
pub(crate) use sink::text_into;
pub use sites::{SITE_ROWS, SiteRow, Sites};
pub use spans::{Open, SPAN_DEPTH, SPAN_ROWS, SpanPath, SpanRow, Spans};
pub use waits::{WAIT_DIST_ROWS, WaitDistRow, Waits};

/// How much a run keeps, chosen at run time (the module doc says the cost of each).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    /// Always on: the tally, effect totals, the pool.
    Totals,
    /// Adds the span table, distributions and the chain.
    Spans,
    /// Adds system readings per span and allocation by site.
    Detail,
}

impl Level {
    /// Every level, in order.
    pub const ALL: [Level; 3] = [Level::Totals, Level::Spans, Level::Detail];

    pub const fn name(self) -> &'static str {
        match self {
            Level::Totals => "totals",
            Level::Spans => "spans",
            Level::Detail => "detail",
        }
    }

    /// Its number in the encoding.
    pub const fn code(self) -> u64 {
        match self {
            Level::Totals => 0,
            Level::Spans => 1,
            Level::Detail => 2,
        }
    }

    /// Whether this level keeps what `at` adds.
    pub const fn keeps(self, at: Level) -> bool {
        self.code() >= at.code()
    }
}

/// The bugs a control plants in the distribution ([`Dist`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DistPlant {
    None,
    /// The bucket is one past its value's (`64 - lzcnt`): 1 lands in [2, 3].
    BucketOffByOne,
    /// A percentile answers the bucket below the one its rank is in.
    PercentileBucketBelow,
}

impl Plants for DistPlant {}

/// The bugs a control plants in the longest chain ([`longest_chain`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChainPlant {
    None,
    /// Only a task's first wait is followed, not its largest.
    FirstWaitOnly,
    /// A task that waited is valued by its waits alone, its own time dropped.
    OwnDropped,
}

impl Plants for ChainPlant {}

/// The bugs a control plants in what the hooks feed a cell and in the merge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TelemetryPlant {
    None,
    /// A span's close is not recorded: it stays open.
    SpanLeftOpen,
    /// A wait's end is not recorded: neither the tally nor the cell sees it.
    WaitUnrecorded,
    /// A worker's cell merged a second time is not refused.
    MergedTwice,
}

impl Plants for TelemetryPlant {}

/// What the telemetry refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TelemetryError {
    /// A cell's or report's buffers could not be reserved: `what`, `bytes`.
    Reserve { what: &'static str, bytes: usize },
    /// Worker `worker`'s cell was merged into the report already.
    MergedTwice { worker: u32 },
    /// Worker `worker` is past the report's `most` workers.
    WorkerPastMost { worker: u32, most: u32 },
    /// A cell of level `cell` merged into a report of level `report`.
    LevelDiffers { cell: Level, report: Level },
    /// A span closed on worker `worker` that its cell did not open (its token is another
    /// worker's).
    ClosedElsewhere { worker: u32, opened: u32 },
    /// The chain's records wait in a circle through task `task`: records that cannot be.
    ChainCycle { task: u64 },
    /// The report was read or encoded before [`Report::finish`].
    Unfinished,
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TelemetryError::Reserve { what, bytes } => write!(f, "bstd::telemetry: could not reserve {bytes} bytes for {what}"),
            TelemetryError::MergedTwice { worker } => write!(f, "bstd::telemetry: worker {worker}'s cell was merged twice"),
            TelemetryError::WorkerPastMost { worker, most } => {
                write!(f, "bstd::telemetry: worker {worker} is past the report's {most} workers")
            }
            TelemetryError::LevelDiffers { cell, report } => {
                write!(f, "bstd::telemetry: a cell kept at level {} merged into a report of level {}", cell.name(), report.name())
            }
            TelemetryError::ClosedElsewhere { worker, opened } => {
                write!(f, "bstd::telemetry: a span opened on worker {opened} closed on worker {worker}")
            }
            TelemetryError::ChainCycle { task } => {
                write!(f, "bstd::telemetry: the chain's waits go in a circle through task {task}")
            }
            TelemetryError::Unfinished => f.write_str("bstd::telemetry: the report was read before it was finished"),
        }
    }
}

/// How a worker's cell is made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellOpts {
    /// The worker's index (its `WorkerId`).
    pub worker: u32,
    pub level: Level,
}

/// The spans and tasks one worker holds open at once, from `Spans` (the module doc,
/// "Spans across tasks"): past it a span or task is not recorded and is counted in
/// [`Cell::unrecorded`].
pub const OPENS: usize = 64;

/// A slot of a worker's open table: what [`Cell::span_open`] or [`Cell::task_open`] hands
/// back, for the close.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slot(u32);

/// What a slot of the open table holds.
#[derive(Clone, Copy, Debug)]
enum Opened {
    /// A span: its open, and what tasks nested in its stretch charged to its path apart.
    Span { open: Open, nested: Brief },
    /// A task: its id and name, its start, its snapshot, the path it charges, what tasks
    /// nested in it charged to that path apart, and the task it ran on top of.
    Task { task: u64, name: &'static str, start: u64, at: Brief, path: SpanPath, nested: Brief, below: Option<Current> },
}

/// The task running on a worker now, for its waits' records.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Current {
    task: u64,
    /// The waits charged to it so far: its own time is its wall less these.
    waited: u64,
}

/// A task begun at `Totals` (no open table): what its close takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Begun {
    pub name: &'static str,
    pub start: u64,
    slot: Option<Slot>,
}

/// One worker's telemetry: every aggregate of the module doc, at the cell's level. Its buffers
/// are reserved here, in `alloc`, and never grow: no event allocates.
pub struct Cell<A: core::alloc::Allocator + Copy> {
    worker: u32,
    level: Level,
    /// Spans opened and closed on this worker (every level counts them).
    opened: u64,
    closed: u64,
    spans: Spans<A>,
    waits: Waits<A>,
    effects: Effects<A>,
    pool: Pool,
    chain: Chain<A>,
    sites: Sites<A>,
    /// The open table, [`OPENS`] slots from `Spans` (none below).
    opens: blaze_own::Vec<Option<Opened>, A>,
    /// The task running on this worker now (from `Spans`).
    current: Option<Current>,
    /// Tasks begun on this worker: the next task's id is `worker << 40 | serial`.
    serial: u64,
    /// Tasks running on this worker now, nested (a task run inline, or on top of a wait).
    depth: u32,
    /// Whether its first event was seen (the pool's start).
    started: bool,
    /// Spans and tasks past the open table: not recorded.
    unrecorded: u64,
}

impl<A: core::alloc::Allocator + Copy> Cell<A> {
    /// A worker's cell, its buffers reserved for `opts.level` (nothing for what the level does
    /// not keep).
    pub fn new(opts: CellOpts, alloc: A) -> Result<Cell<A>, TelemetryError> {
        let level = opts.level;
        let slots = if level.keeps(Level::Spans) { OPENS } else { 0 };
        let mut opens = blaze_own::Vec::try_with_capacity_in(slots, alloc)
            .map_err(|_| TelemetryError::Reserve { what: "the open table", bytes: slots * size_of::<Option<Opened>>() })?;
        for _ in 0..slots {
            opens.push(None);
        }
        Ok(Cell {
            worker: opts.worker,
            level,
            opened: 0,
            closed: 0,
            spans: Spans::new(level.keeps(Level::Spans), alloc)?,
            waits: Waits::new(level.keeps(Level::Spans), alloc)?,
            effects: Effects::new(level.keeps(Level::Spans), alloc)?,
            pool: Pool::ZERO,
            chain: Chain::new(level.keeps(Level::Spans), alloc)?,
            sites: Sites::new(level.keeps(Level::Detail), alloc)?,
            opens,
            current: None,
            serial: 0,
            depth: 0,
            started: false,
            unrecorded: 0,
        })
    }

    /// The pool's start at this worker's first event.
    #[inline]
    fn seen(&mut self, wall_ns: u64) {
        if !self.started {
            self.started = true;
            self.pool.started(wall_ns);
        }
    }

    /// The open in `slot`, taken out of the table when it is of the kind asked (a span, or a
    /// task); otherwise left as it is.
    fn take_open(&mut self, slot: Slot, span: bool) -> Option<Opened> {
        let held = self.opens.get_mut(slot.0 as usize)?;
        let kind_asked = match held {
            Some(Opened::Span { .. }) => span,
            Some(Opened::Task { .. }) => !span,
            None => false,
        };
        if kind_asked { held.take() } else { None }
    }

    /// A free slot of the open table, or `None` (counted) when it is full or not kept.
    fn free_slot(&mut self) -> Option<usize> {
        let free = self.opens.iter().position(Option::is_none);
        if free.is_none() {
            self.unrecorded += 1;
        }
        free
    }

    /// The hook of `Cx::span`, from `Spans`: a span named `name` opened under `parent` at
    /// `wall_ns`, `at` its snapshot. Its slot (for [`Cell::span_close`]) and its path (for the
    /// child context), or `None` with the table full (counted in [`Cell::unrecorded`]).
    pub fn span_open(&mut self, parent: &SpanPath, name: &'static str, wall_ns: u64, at: Brief) -> Option<(Slot, SpanPath)> {
        if !self.level.keeps(Level::Spans) {
            return None;
        }
        self.seen(wall_ns);
        let free = self.free_slot()?;
        let open = self.open(parent, name, wall_ns, at);
        let path = open.path;
        self.opens[free] = Some(Opened::Span { open, nested: Brief::ZERO });
        Some((Slot(free as u32), path))
    }

    /// The span of `slot` closed at `wall_ns`, `now` its snapshot: its row takes one close,
    /// its wall time on this worker, and what it used less what tasks nested in its stretch
    /// charged to its path themselves.
    ///
    /// A slot that holds no span (the token's world had its workers replaced while it was open)
    /// is counted in [`Cell::unrecorded`], which the report prints: the drop that calls this
    /// has no way to return an error.
    pub fn span_close(&mut self, slot: Slot, wall_ns: u64, now: Brief) {
        let Some(Opened::Span { open, nested }) = self.take_open(slot, true) else {
            self.unrecorded += 1;
            return;
        };
        let mut at = open.at;
        at.plus(&nested);
        // The open is this cell's (it came from its own table): `close` cannot refuse it.
        if self.close(Open { at, ..open }, wall_ns, now, blaze_own::planted::NoPlant).is_err() {
            self.unrecorded += 1;
        }
    }

    /// The hook of a task's start: task `name` began on this worker at `wall_ns` under the span
    /// `path` (its spawner's), `at` its snapshot. From `Spans` it takes a slot of the open
    /// table and becomes the worker's current task (its waits' records name it).
    pub fn task_open(&mut self, name: &'static str, path: &SpanPath, wall_ns: u64, at: Brief) -> Begun {
        self.seen(wall_ns);
        self.serial += 1;
        self.depth += 1;
        let task = (u64::from(self.worker) << 40) | self.serial;
        let slot = if self.level.keeps(Level::Spans) {
            self.free_slot().map(|free| {
                let below = self.current.replace(Current { task, waited: 0 });
                self.opens[free] = Some(Opened::Task { task, name, start: wall_ns, at, path: *path, nested: Brief::ZERO, below });
                Slot(free as u32)
            })
        } else {
            None
        };
        Begun { name, start: wall_ns, slot }
    }

    /// The task `begun` ended at `wall_ns` (`failed`: its panic or error failed it), `now` its
    /// snapshot. The pool counts it (a task not nested in another on this worker adds its
    /// wall time to the worker's working time); from `Spans` the chain takes its record, its
    /// use less what nested tasks charged is charged to its path, and every span or task open
    /// on this worker with that same path (each encloses it: a worker's stretches nest) is told
    /// it was charged apart, so no use is counted twice.
    ///
    /// A slot that holds no task (the world's workers were replaced while it ran) is counted
    /// in [`Cell::unrecorded`] and the task in the pool's counts only.
    pub fn task_close(&mut self, begun: Begun, wall_ns: u64, now: Brief, failed: bool) {
        self.depth = self.depth.saturating_sub(1);
        let wall = wall_ns.saturating_sub(begun.start);
        if self.depth == 0 {
            self.pool.worked(wall);
        }
        let Some(slot) = begun.slot else {
            self.pool.task_ended(begun.name, wall, failed);
            return;
        };
        let Some(Opened::Task { task, name, start, at, path, nested, below }) = self.take_open(slot, false) else {
            self.unrecorded += 1;
            self.pool.task_ended(begun.name, wall, failed);
            return;
        };
        let waited = self.current.map_or(0, |current| current.waited);
        self.current = below;
        let whole = now.since(&at);
        let own = whole.since(&nested);
        let spans = &mut self.spans;
        path.each_up(|up| spans.charge(up, own));
        // Every stretch still open on this worker encloses this task's (a worker's stretches
        // nest): the task's use is its path's, charged above, never theirs.
        for opened in self.opens.iter_mut().flatten() {
            match opened {
                Opened::Span { nested, .. } | Opened::Task { nested, .. } => nested.plus(&whole),
            }
        }
        self.task_ended(TaskRec { task, name, start, end: wall_ns, own_ns: wall.saturating_sub(waited) }, failed);
    }

    /// The task running on this worker now (from `Spans`): what a wait's record names. 0 outside
    /// every task (`main`'s own code).
    pub fn current_task(&self) -> u64 {
        self.current.map_or(0, |current| current.task)
    }

    /// The worker's loop ended (or the run did, at exit) at `wall_ns`: the pool's stop.
    pub fn stop(&mut self, wall_ns: u64) {
        if self.pool.stopped_ns.is_none() {
            self.seen(wall_ns);
            self.pool.stopped(wall_ns);
        }
    }

    /// Spans and tasks past the open table, not recorded.
    pub fn unrecorded(&self) -> u64 {
        self.unrecorded
    }

    pub fn worker(&self) -> u32 {
        self.worker
    }

    pub fn level(&self) -> Level {
        self.level
    }

    /// The snapshot a span's open and close take at this level: the tally and the thread's
    /// cells, and from `Detail` the system's readings too. The hook's one call for both.
    #[inline]
    pub fn snapshot(&self, tally: &blaze_meter::Tally) -> Brief {
        match self.level {
            Level::Detail => Brief::now_detailed(tally),
            Level::Spans => Brief::now(tally),
            Level::Totals => Brief::ZERO,
        }
    }

    /// A span named `name` opened under `parent`, at `wall_ns` on the world's clock, `at` its
    /// snapshot ([`Cell::snapshot`]): the token its close hands back.
    #[inline]
    pub fn open(&mut self, parent: &SpanPath, name: &'static str, wall_ns: u64, at: Brief) -> Open {
        self.opened += 1;
        let path = match self.level {
            Level::Totals => SpanPath::ROOT,
            Level::Spans | Level::Detail => parent.child(name),
        };
        Open { worker: self.worker, path, wall_ns, at }
    }

    /// The span `open` closed at `wall_ns`, `now` its snapshot.
    #[inline]
    pub fn close(
        &mut self,
        open: Open,
        wall_ns: u64,
        now: Brief,
        plant: impl blaze_own::planted::Planted<TelemetryPlant>,
    ) -> Result<(), TelemetryError> {
        if open.worker != self.worker {
            return Err(TelemetryError::ClosedElsewhere { worker: self.worker, opened: open.worker });
        }
        if plant.has(TelemetryPlant::SpanLeftOpen) {
            return Ok(());
        }
        self.closed += 1;
        if self.level.keeps(Level::Spans) {
            self.spans.close(&open.path, wall_ns.saturating_sub(open.wall_ns), now.since(&open.at));
        }
        Ok(())
    }

    /// A wait `rec` ended on this worker: charged to the worker's `tally` (its `Waited` row, by
    /// what it waited on) and, from `Spans`, to its distribution and the chain's records. The
    /// one seam a wait's way out calls (`workers::charge_wait`).
    #[inline]
    pub fn wait_ended(&mut self, tally: &mut blaze_meter::Tally, rec: WaitRec, plant: impl blaze_own::planted::Planted<TelemetryPlant>) {
        if plant.has(TelemetryPlant::WaitUnrecorded) {
            return;
        }
        let ns = rec.end.saturating_sub(rec.start);
        let id = match rec.target {
            Target::Task(id) => Some(id),
            Target::Thing { id } => id,
        };
        tally.waited(rec.on, id, ns);
        if let Some(current) = self.current.as_mut() {
            current.waited = current.waited.wrapping_add(ns);
        }
        if self.level.keeps(Level::Spans) {
            self.waits.record(rec.on, ns);
            self.chain.wait(rec);
        }
    }

    /// An effect of kind `kind` completed after `ns`, moving `bytes`.
    #[inline]
    pub fn effect(&mut self, kind: blaze_meter::Call, bytes: u64, ns: u64) {
        self.effects.record(kind, bytes, ns);
    }

    /// A task `rec` ended (`failed`: its panic or error failed it); `own_ns` in `rec` is its
    /// time less the waits it was charged.
    #[inline]
    pub fn task_ended(&mut self, rec: TaskRec, failed: bool) {
        self.pool.task_ended(rec.name, rec.end.saturating_sub(rec.start), failed);
        if self.level.keeps(Level::Spans) {
            self.chain.task(rec);
        }
    }

    /// The pool loop's facts: its time working, parked and idle, and its samples.
    pub fn pool(&mut self) -> &mut Pool {
        &mut self.pool
    }

    /// Allocation by site (`Detail`): the thread's site rows, moved here at a task's end.
    pub fn sites(&mut self) -> &mut Sites<A> {
        &mut self.sites
    }

    /// Spans opened and not closed.
    pub fn open_spans(&self) -> u64 {
        self.opened.wrapping_sub(self.closed)
    }

    pub(crate) fn parts(&self) -> (&Spans<A>, &Waits<A>, &Effects<A>, &Pool, &Chain<A>, &Sites<A>) {
        (&self.spans, &self.waits, &self.effects, &self.pool, &self.chain, &self.sites)
    }
}

/// The FNV-1a step over `bytes` from `hash`: the span path's hash, and nothing else.
pub(crate) const fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut at = 0;
    while at < bytes.len() {
        hash ^= bytes[at] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        at += 1;
    }
    hash
}

/// FNV-1a's offset basis.
pub(crate) const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;
