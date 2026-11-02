//! Where a build's time went, job by job: the fiber switches turned into a timeline per job
//! (running on a core, parked on a named wait, queued for a core), the spans laid on the job
//! they ran in, and each span's time split by those states, so a unit's wall time is never read
//! as its work. Sans-IO: [`Model::build`] takes rows and gives typed tables; [`load`] is the
//! driver that reads them from the history, and [`report`], [`path`], [`trace`] and [`html`]
//! read the model.
//!
//! The model (/tmp/blaze-lanes/prob/9-data-layer.md, docs/profiling.md "Where the time went"):
//!
//! - A job's lifetime is cut into [`Interval`]s, one [`State`] each, end to end: for every job,
//!   running + parked + queued + parked_or_queued + unrecorded = its lifetime.
//! - A run ends at the job's next switch, and never after the next switch of its thread: a
//!   thread runs one fiber at a time, so no two runs on a core overlap and the build's running
//!   time is at most cores x wall.
//! - A span belongs to the job running on its thread when it began (its parent's, when it has
//!   one), and its time is the overlap of its open stretch with its job's intervals. A span's
//!   uncovered time is its running time under no child span; it is a column, never folded into
//!   another.

pub mod html;
pub mod load;
pub mod path;
pub mod report;
pub mod trace;

#[cfg(test)]
mod tests;

use std::fmt;

use blaze_hash::HashMap;

use crate::flight::{Switched, WaitedOn};

/// A switch as the model takes it: a row of `switches`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwitchIn {
    pub process: u32,
    pub thread: u32,
    pub t_ns: i64,
    pub event: Event,
    /// The job (generation << 32 | slot); 0 for a worker's idle and woke rows.
    pub job: u64,
    pub fiber: u32,
    pub core: u32,
}

/// What a switch says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Started,
    /// Waiting for `on` and its value; [`WaitedOn::Moved`] is no wait, only a move to another
    /// core, so what follows it is queued time.
    Parked { on: WaitedOn, value: u64 },
    /// The parked job became runnable, woken by the job `by` when the record says. The
    /// scheduler writes no such row yet (9-data-layer.md, "Record-side gaps"); without it a
    /// wait is [`State::ParkedOrQueued`] to its resume.
    Ready { by: Option<u64> },
    Resumed,
    Ended,
    /// A worker went idle or woke: a row of no job, which ends the run before it.
    Idle,
    Woke,
}

/// The `ready` row's event name: not one of [`Switched`]'s until the scheduler writes it.
pub const READY: &str = "ready";

impl Event {
    /// The event a `switches` row names, with its wait and waker columns.
    pub fn of(event: &str, wait: Option<(&str, Option<u64>)>, by: Option<u64>) -> Result<Event, TimeError> {
        const STARTED: &str = Switched::Started.name();
        const PARKED: &str = Switched::Parked.name();
        const RESUMED: &str = Switched::Resumed.name();
        const ENDED: &str = Switched::Ended.name();
        const IDLE: &str = Switched::Idle.name();
        const WOKE: &str = Switched::Woke.name();
        // alloc: an error
        let bad = |why: &'static str| TimeError::BadSwitch { event: event.to_owned(), why };
        Ok(match event {
            STARTED => Event::Started,
            RESUMED => Event::Resumed,
            ENDED => Event::Ended,
            IDLE => Event::Idle,
            WOKE => Event::Woke,
            READY => Event::Ready { by },
            PARKED => {
                let (kind, value) = wait.ok_or_else(|| bad("a park with no wait_kind"))?;
                let on = waited_on(kind).ok_or_else(|| bad("a park whose wait_kind is none the recorder writes"))?;
                let value = match (on, value) {
                    (WaitedOn::Moved | WaitedOn::Nothing, _) => 0,
                    (_, Some(value)) => value,
                    (_, None) => return Err(bad("a park on a job, key, counter or deadline with no wait_value")),
                };
                Event::Parked { on, value }
            }
            _ => return Err(bad("no event the recorder writes")),
        })
    }

    /// Where it sorts among one job's switches at one instant: a park before the resume that
    /// ends it, a start first and an end last.
    fn rank(self) -> u8 {
        match self {
            Event::Started => 0,
            Event::Ended | Event::Idle => 4,
            Event::Parked { .. } => 1,
            Event::Ready { .. } => 2,
            Event::Resumed | Event::Woke => 3,
        }
    }
}

fn waited_on(name: &str) -> Option<WaitedOn> {
    [WaitedOn::Job, WaitedOn::Key, WaitedOn::Counter, WaitedOn::Until, WaitedOn::Moved, WaitedOn::Nothing]
        .into_iter()
        .find(|it| it.name() == name)
}

/// A span as the model takes it: the columns of `spans` it reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanIn {
    pub process: u32,
    pub thread: u32,
    pub span_id: u64,
    pub parent: Option<u64>,
    pub depth: u16,
    pub name: String,
    pub krate: Option<String>,
    pub function: Option<String>,
    pub unit: Option<String>,
    pub awaits: Option<String>,
    /// For a wait: the task running what it waits for, its job's slot + 1
    /// (`blaze_graph::pump::task`).
    pub owner: Option<u64>,
    pub start_ns: i64,
    pub dur_ns: i64,
}

/// The name of a span that is a wait (`blaze_graph::wait`'s spans).
pub const WAIT_SPAN: &str = "wait";

impl SpanIn {
    pub fn end_ns(&self) -> i64 {
        self.start_ns + self.dur_ns.max(0)
    }

    /// What it is about: its function, else its unit, else its crate, as
    /// `flight::write_working_on` names a span.
    pub fn subject(&self) -> Option<&str> {
        self.function.as_deref().or(self.unit.as_deref()).or(self.krate.as_deref())
    }

    /// Its name and what it is about: `unit codegen serde v1.0.219 (build script)`.
    pub fn item(&self) -> String {
        match self.subject() {
            // alloc: a label, once a row a report
            Some(of) => format!("{} {of}", self.name),
            // alloc: a label, once a row a report
            None => self.name.clone(),
        }
    }

    fn is_wait(&self) -> bool {
        self.name == WAIT_SPAN
    }
}

/// A build's rows.
#[derive(Clone, Debug, Default)]
pub struct Input {
    pub build: u64,
    /// The build's `builds` row: when it started and how long it ran; `None` while it runs.
    pub build_row: Option<(i64, i64)>,
    pub switches: Vec<SwitchIn>,
    pub spans: Vec<SpanIn>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TimeError {
    /// The build has no switch and no span.
    Empty { build: u64 },
    /// A `switches` row the model can't read.
    BadSwitch { event: String, why: &'static str },
    /// A column of the rows read is missing or of a type the reader does not take.
    BadColumn { table: &'static str, column: &'static str, why: String },
    /// Reading the history failed.
    Read { doing: &'static str, why: String },
}

impl fmt::Display for TimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimeError::Empty { build } => write!(f, "build {build} has no switch and no span in the history"),
            TimeError::BadSwitch { event, why } => write!(f, "a switches row of event `{event}` can't be read: {why}"),
            TimeError::BadColumn { table, column, why } => write!(f, "the column {table}.{column} {why}"),
            TimeError::Read { doing, why } => write!(f, "{doing}: {why}"),
        }
    }
}

impl std::error::Error for TimeError {}

/// What a lane is: a job on a fiber, or a thread no switch names whose spans ran on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LaneId {
    Job { process: u32, job: u64 },
    Thread { process: u32, thread: u32 },
}

impl LaneId {
    pub fn process(self) -> u32 {
        match self {
            LaneId::Job { process, .. } | LaneId::Thread { process, .. } => process,
        }
    }

    pub fn job(self) -> Option<u64> {
        match self {
            LaneId::Job { job, .. } => Some(job),
            LaneId::Thread { .. } => None,
        }
    }
}

impl fmt::Display for LaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaneId::Job { job, .. } => write!(f, "job {}.{}", job >> 32, job & 0xffff_ffff),
            LaneId::Thread { thread, .. } => write!(f, "thread {thread}"),
        }
    }
}

/// How a lane's lifetime began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Begin {
    Started,
    /// Its first row is not `started`: it began before the record.
    BeforeRecord,
    /// A thread lane: its first span.
    FirstSpan,
}

/// How a lane's lifetime ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Ended,
    /// No `ended` row: its last run ended at the next switch of its thread, a bound.
    ThreadNext,
    /// Still running, or waiting, at the end of the record.
    RecordEnd,
    /// A thread lane: its last span's end.
    LastSpan,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEnd {
    /// At the job's own next switch.
    Switch,
    /// At the next switch of its thread, of another job or an idle worker.
    ThreadNext,
    RecordEnd,
    /// A thread lane's run, between its spans' waits.
    Spans,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// On a core: `core` is `None` for a thread lane.
    Running { core: Option<u32>, thread: u32, end: RunEnd },
    /// Waiting for [`Model::waits`]`[wait]`, until the `ready` row.
    Parked { wait: usize },
    /// Runnable, waiting for a core: after a `ready` row, or a move to another core.
    Queued { wait: usize },
    /// Parked then queued, the record not saying where one became the other.
    ParkedOrQueued { wait: usize },
    /// The record lost what the job did: its rows were written over, or another job ran on its
    /// thread before its next row.
    Unrecorded,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Running { .. } => "running",
            State::Parked { .. } => "parked",
            State::Queued { .. } => "queued",
            State::ParkedOrQueued { .. } => "parked_or_queued",
            State::Unrecorded => "unrecorded",
        }
    }

    pub fn wait(self) -> Option<usize> {
        match self {
            State::Parked { wait } | State::Queued { wait } | State::ParkedOrQueued { wait } => Some(wait),
            State::Running { .. } | State::Unrecorded => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub from_ns: i64,
    pub to_ns: i64,
    pub state: State,
    /// The innermost span of its lane open just before it ended ([`Model::spans`]).
    pub inside: Option<usize>,
}

impl Interval {
    pub fn dur_ns(&self) -> i64 {
        self.to_ns - self.from_ns
    }
}

/// Time by state: what a lane, a span or a row of a report spent in each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub running_ns: i64,
    pub parked_ns: i64,
    pub queued_ns: i64,
    pub parked_or_queued_ns: i64,
    pub unrecorded_ns: i64,
}

impl Totals {
    pub fn total(&self) -> i64 {
        self.running_ns + self.waiting_ns() + self.unrecorded_ns
    }

    /// Parked, queued, or the record not saying which.
    pub fn waiting_ns(&self) -> i64 {
        self.parked_ns + self.queued_ns + self.parked_or_queued_ns
    }

    fn of(state: State, ns: i64) -> Totals {
        let mut it = Totals::default();
        *it.slot(state) = ns;
        it
    }

    fn slot(&mut self, state: State) -> &mut i64 {
        match state {
            State::Running { .. } => &mut self.running_ns,
            State::Parked { .. } => &mut self.parked_ns,
            State::Queued { .. } => &mut self.queued_ns,
            State::ParkedOrQueued { .. } => &mut self.parked_or_queued_ns,
            State::Unrecorded => &mut self.unrecorded_ns,
        }
    }

    pub fn add(&mut self, other: Totals) {
        self.running_ns += other.running_ns;
        self.parked_ns += other.parked_ns;
        self.queued_ns += other.queued_ns;
        self.parked_or_queued_ns += other.parked_or_queued_ns;
        self.unrecorded_ns += other.unrecorded_ns;
    }

    pub fn minus(self, other: Totals) -> Totals {
        Totals {
            running_ns: self.running_ns - other.running_ns,
            parked_ns: self.parked_ns - other.parked_ns,
            queued_ns: self.queued_ns - other.queued_ns,
            parked_or_queued_ns: self.parked_or_queued_ns - other.parked_or_queued_ns,
            unrecorded_ns: self.unrecorded_ns - other.unrecorded_ns,
        }
    }
}

/// A job's timeline, or a thread's.
#[derive(Clone, Debug)]
pub struct Lane {
    pub id: LaneId,
    /// The fiber a job ran on; `None` for a thread lane.
    pub fiber: Option<u32>,
    pub begin: Begin,
    pub end: End,
    /// End to end, from its lifetime's start to its end.
    pub intervals: Vec<Interval>,
    /// The spans laid on it ([`Model::spans`]), by start.
    pub spans: Vec<usize>,
    /// `before[i]`: the totals of `intervals[..i]`.
    before: Vec<Totals>,
}

impl Lane {
    pub fn begin_ns(&self) -> i64 {
        self.intervals.first().map_or(0, |it| it.from_ns)
    }

    pub fn end_ns(&self) -> i64 {
        self.intervals.last().map_or(0, |it| it.to_ns)
    }

    pub fn totals(&self) -> Totals {
        self.before.last().copied().unwrap_or_default()
    }

    /// Its totals from its start to `t`.
    pub fn accrued(&self, t: i64) -> Totals {
        let at = self.intervals.partition_point(|it| it.from_ns <= t);
        let Some(i) = at.checked_sub(1) else { return Totals::default() };
        let interval = &self.intervals[i];
        let mut it = self.before[i];
        it.add(Totals::of(interval.state, t.min(interval.to_ns) - interval.from_ns));
        it
    }

    /// The interval in which `t` falls, its end included: what ran just before `t`.
    pub fn at_or_before(&self, t: i64) -> Option<usize> {
        let at = self.intervals.partition_point(|it| it.from_ns < t);
        let i = at.checked_sub(1)?;
        (self.intervals[i].to_ns >= t).then_some(i)
    }

    fn sum(&mut self) {
        self.before.clear();
        self.before.reserve(self.intervals.len() + 1);
        let mut running = Totals::default();
        self.before.push(running);
        for it in &self.intervals {
            running.add(Totals::of(it.state, it.dur_ns()));
            self.before.push(running);
        }
    }
}

/// Who made a parked job runnable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waker {
    /// The `ready` row said.
    Ready { lane: usize },
    /// It waited for a job's end: that job.
    Awaited { lane: usize },
    /// The wait's span named the task running the key: the job alive in that slot.
    Owner { lane: usize },
    /// The wait's span named a slot with no job on a fiber alive in it: a client thread's.
    OwnerOffFiber { slot: u64 },
    /// The record does not say: a scope's, a counter's or a deadline's wait.
    Unknown,
}

impl Waker {
    pub fn lane(self) -> Option<usize> {
        match self {
            Waker::Ready { lane } | Waker::Awaited { lane } | Waker::Owner { lane } => Some(lane),
            Waker::OwnerOffFiber { .. } | Waker::Unknown => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Waker::Ready { .. } => "ready",
            Waker::Awaited { .. } => "awaited",
            Waker::Owner { .. } => "owner",
            Waker::OwnerOffFiber { .. } => "owner_off_fiber",
            Waker::Unknown => "unknown",
        }
    }
}

/// A park, or a thread lane's wait span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wait {
    pub lane: usize,
    /// `None` for a thread lane's wait, which no switch says.
    pub on: Option<WaitedOn>,
    pub value: u64,
    /// Its `wait` span ([`Model::spans`]), naming what it waits for.
    pub span: Option<usize>,
    pub parked_ns: i64,
    pub ready_ns: Option<i64>,
    /// `None`: still waiting at the end of the record.
    pub resumed_ns: Option<i64>,
    pub waker: Waker,
}

/// A span's time, by its job's states while it was open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpanTime {
    pub lane: Option<usize>,
    pub totals: Totals,
    /// `totals` less its children's: `running_ns` is its uncovered time.
    pub own: Totals,
}

impl SpanTime {
    /// Running, under no child span.
    pub fn uncovered_ns(&self) -> i64 {
        self.own.running_ns
    }
}

/// Where the build's time bounds came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundsFrom {
    /// Its `builds` row.
    Build,
    /// Its first and last row: it has no `builds` row with a duration yet.
    Record,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub start_ns: i64,
    pub end_ns: i64,
    pub from: BoundsFrom,
}

impl Bounds {
    pub fn wall_ns(&self) -> i64 {
        self.end_ns - self.start_ns
    }
}

/// What the rows did not line up on: counted, so a report says how far to trust it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Anomalies {
    /// A span whose parent is not among the rows: taken as a root.
    pub orphans: u64,
    /// A span reaching past its parent, or overlapping a sibling on its lane.
    pub misnested: u64,
    /// A run cut short by another job's switch on its thread.
    pub runs_cut: u64,
}

/// A build's time, job by job.
#[derive(Clone, Debug)]
pub struct Model {
    pub build: u64,
    pub bounds: Bounds,
    pub lanes: Vec<Lane>,
    pub spans: Vec<SpanIn>,
    pub span_time: Vec<SpanTime>,
    /// Each span's parent among [`Model::spans`]: `None` at a root, or when the parent is not
    /// among the rows ([`Anomalies::orphans`]).
    pub parent: Vec<Option<usize>>,
    pub waits: Vec<Wait>,
    pub anomalies: Anomalies,
}

impl Model {
    /// The model of `input`.
    pub fn build(input: Input) -> Result<Model, TimeError> {
        let Input { build, build_row, mut switches, spans } = input;
        let bounds = bounds(build, build_row, &switches, &spans)?;
        switches.sort_by_key(|it| (it.process, it.t_ns, it.event.rank()));
        let threads = thread_rows(&switches);
        let mut builder = Builder {
            bounds,
            threads,
            // alloc: once a model
            lanes: Vec::new(),
            // alloc: once a model
            waits: Vec::new(),
            // alloc: once a model
            ready_by: Vec::new(),
            anomalies: Anomalies::default(),
        };
        builder.job_lanes(&switches);
        let ready_by = std::mem::take(&mut builder.ready_by);
        let mut model = builder.finish(build, spans);
        model.lay_spans();
        model.resolve_waits(&ready_by);
        model.time_spans();
        Ok(model)
    }

    /// The lane `id`, if the model has it.
    pub fn lane(&self, id: LaneId) -> Option<usize> {
        self.lanes.binary_search_by_key(&id, |it| it.id).ok()
    }

    /// What a wait waited for, as a reader says it: its span's `awaits`, else its kind and value.
    pub fn wait_label(&self, wait: &Wait) -> String {
        if let Some(awaits) = wait.span.and_then(|it| self.spans[it].awaits.as_deref()) {
            // alloc: a label
            return awaits.to_owned();
        }
        match wait.on {
            // alloc: a label
            Some(WaitedOn::Key) => format!("key {:016x}", wait.value),
            // alloc: a label
            Some(WaitedOn::Job) => format!("job slot {}", wait.value),
            // alloc: a label
            Some(WaitedOn::Counter) => format!("counter {}", wait.value),
            // alloc: a label
            Some(WaitedOn::Until) => format!("until {} ns", wait.value),
            // alloc: a label
            Some(WaitedOn::Moved) => "a move to another core".to_owned(),
            // alloc: a label
            Some(WaitedOn::Nothing) => "nothing".to_owned(),
            // alloc: a label
            None => "a wait span with no awaits".to_owned(),
        }
    }

    /// The outermost span `span` is inside, itself included.
    pub fn root_of(&self, mut span: usize) -> usize {
        let mut fuel = self.spans.len();
        while let Some(parent) = self.parent[span]
            && fuel > 0
        {
            span = parent;
            fuel -= 1;
        }
        span
    }
}

/// Each thread's switches' instants, in order, by (process, thread).
type Threads = HashMap<(u32, u32), Vec<i64>>;

fn thread_rows(switches: &[SwitchIn]) -> Threads {
    let mut threads: Threads = HashMap::default();
    for it in switches {
        // alloc: a thread's instants, once a thread a model
        threads.entry((it.process, it.thread)).or_default().push(it.t_ns);
    }
    for rows in threads.values_mut() {
        rows.sort_unstable();
    }
    threads
}

fn bounds(build: u64, row: Option<(i64, i64)>, switches: &[SwitchIn], spans: &[SpanIn]) -> Result<Bounds, TimeError> {
    let first = switches.iter().map(|it| it.t_ns).chain(spans.iter().map(|it| it.start_ns)).min();
    let last = switches.iter().map(|it| it.t_ns).chain(spans.iter().map(SpanIn::end_ns)).max();
    let (Some(first), Some(last)) = (first, last) else { return Err(TimeError::Empty { build }) };
    Ok(match row {
        Some((start, dur)) => Bounds { start_ns: start.min(first), end_ns: (start + dur).max(last), from: BoundsFrom::Build },
        None => Bounds { start_ns: first, end_ns: last, from: BoundsFrom::Record },
    })
}

struct Builder {
    bounds: Bounds,
    threads: Threads,
    lanes: Vec<Lane>,
    waits: Vec<Wait>,
    /// Each wait a `ready` row named the waker of: the wait and the waker's (process, job).
    ready_by: Vec<(usize, u64)>,
    anomalies: Anomalies,
}

/// What a job is doing between two of its switches.
#[derive(Clone, Copy)]
enum Doing {
    Running { core: u32, thread: u32 },
    Waiting { wait: usize, ready: bool },
    Unrecorded,
}

impl Builder {
    fn job_lanes(&mut self, switches: &[SwitchIn]) {
        // alloc: each job's rows, once a model
        let mut jobs: HashMap<(u32, u64), Vec<SwitchIn>> = HashMap::default();
        for it in switches.iter().filter(|it| !matches!(it.event, Event::Idle | Event::Woke)) {
            // alloc: a job's rows, once a job a model
            jobs.entry((it.process, it.job)).or_default().push(*it);
        }
        // alloc: the jobs in order, once a model
        let mut jobs: Vec<_> = jobs.into_iter().collect();
        jobs.sort_unstable_by_key(|it| it.0);
        for ((process, job), rows) in jobs {
            self.job_lane(LaneId::Job { process, job }, &rows);
        }
    }

    /// The state of a stretch of `wait`: queued after a move or a `ready` row, else parked or
    /// queued, the record not saying which.
    fn waiting(&self, wait: usize, ready: bool) -> State {
        match (self.waits[wait].on, ready) {
            (Some(WaitedOn::Moved), _) | (_, true) => State::Queued { wait },
            (_, false) => State::ParkedOrQueued { wait },
        }
    }

    /// The next switch of `thread` after `t`.
    fn thread_next(&self, process: u32, thread: u32, t: i64) -> Option<i64> {
        let rows = self.threads.get(&(process, thread))?;
        rows.get(rows.partition_point(|it| *it <= t)).copied()
    }

    fn job_lane(&mut self, id: LaneId, rows: &[SwitchIn]) {
        let lane = self.lanes.len();
        let LaneId::Job { process, .. } = id else { unreachable!("a lane of switches is a job's") };
        // alloc: a job's intervals, once a job a model
        let mut intervals: Vec<Interval> = Vec::new();
        let begin = match rows.first().map(|it| it.event) {
            Some(Event::Started) => Begin::Started,
            _ => Begin::BeforeRecord,
        };
        let mut end = End::RecordEnd;
        let mut at = rows.first().map_or(self.bounds.start_ns, |it| it.t_ns);
        // Before its first row, a job that began earlier did what the record lost.
        let mut doing = Doing::Unrecorded;
        let push = |intervals: &mut Vec<Interval>, from: i64, to: i64, state: State| {
            if to > from {
                intervals.push(Interval { from_ns: from, to_ns: to, state, inside: None });
            }
        };
        for row in rows {
            let t = row.t_ns;
            match doing {
                Doing::Running { core, thread } => {
                    // A thread runs one fiber: the run ends at its thread's next switch.
                    let cut = self.thread_next(process, thread, at).unwrap_or(t).min(t);
                    let run_end = if cut < t { RunEnd::ThreadNext } else { RunEnd::Switch };
                    push(&mut intervals, at, cut, State::Running { core: Some(core), thread, end: run_end });
                    if cut < t {
                        self.anomalies.runs_cut += 1;
                        push(&mut intervals, cut, t, State::Unrecorded);
                    }
                }
                Doing::Waiting { wait, ready } => push(&mut intervals, at, t, self.waiting(wait, ready)),
                Doing::Unrecorded => push(&mut intervals, at, t, State::Unrecorded),
            }
            at = t;
            doing = match row.event {
                Event::Started | Event::Resumed => {
                    if let Doing::Waiting { wait, .. } = doing {
                        self.waits[wait].resumed_ns = Some(t);
                    }
                    Doing::Running { core: row.core, thread: row.thread }
                }
                Event::Parked { on, value } => {
                    let wait = self.waits.len();
                    self.waits.push(Wait {
                        lane,
                        on: Some(on),
                        value,
                        span: None,
                        parked_ns: t,
                        ready_ns: None,
                        resumed_ns: None,
                        waker: Waker::Unknown,
                    });
                    Doing::Waiting { wait, ready: false }
                }
                Event::Ready { by } => match doing {
                    Doing::Waiting { wait, ready: false } => {
                        if let Some(by) = by {
                            self.ready_by.push((wait, by));
                        }
                        // The stretch so far was parked, not parked-or-queued.
                        if let Some(last) = intervals.last_mut()
                            && last.state == (State::ParkedOrQueued { wait })
                        {
                            last.state = State::Parked { wait };
                        }
                        self.waits[wait].ready_ns = Some(t);
                        Doing::Waiting { wait, ready: true }
                    }
                    other => other,
                },
                Event::Ended => {
                    // A job's number is its generation and slot, so no row of it follows its end.
                    end = End::Ended;
                    break;
                }
                Event::Idle | Event::Woke => doing,
            };
        }
        if end != End::Ended {
            match doing {
                Doing::Running { core, thread } => {
                    let (to, run_end) = match self.thread_next(process, thread, at) {
                        Some(next) => {
                            end = End::ThreadNext;
                            (next, RunEnd::ThreadNext)
                        }
                        None => (self.bounds.end_ns.max(at), RunEnd::RecordEnd),
                    };
                    push(&mut intervals, at, to, State::Running { core: Some(core), thread, end: run_end });
                }
                Doing::Waiting { wait, ready } => {
                    push(&mut intervals, at, self.bounds.end_ns.max(at), self.waiting(wait, ready));
                }
                Doing::Unrecorded => {}
            }
        }
        let fiber = rows.iter().find(|it| matches!(it.event, Event::Started | Event::Resumed)).map(|it| it.fiber);
        // alloc: a lane's sums, once a lane a model
        let mut lane = Lane { id, fiber, begin, end, intervals, spans: Vec::new(), before: Vec::new() };
        lane.sum();
        self.lanes.push(lane);
    }

    fn finish(self, build: u64, spans: Vec<SpanIn>) -> Model {
        let count = spans.len();
        Model {
            build,
            bounds: self.bounds,
            lanes: self.lanes,
            // alloc: once a model
            span_time: vec![SpanTime::default(); count],
            // alloc: once a model
            parent: vec![None; count],
            spans,
            waits: self.waits,
            anomalies: self.anomalies,
        }
    }
}
