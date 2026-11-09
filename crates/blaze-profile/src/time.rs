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
//!   running + parked + queued + parked_or_queued + unrecorded = its lifetime. It begins at its
//!   `started` or `ran` row's `since_ns` (its first row's time when the row has none) and ends
//!   at its `ended` or `ran` row.
//! - A run ends at the job's own next row. Nothing is inferred where a row is missing: a run
//!   that another job's switch on its thread cuts before that row, or one no row ends, is
//!   [`State::Unrecorded`] with the reason ([`Unrecorded`]). A thread runs one fiber at a time,
//!   so no two runs on a core overlap and the build's running time is at most cores x wall.
//! - A span belongs to the job its root `job` span names (`spans.job`), else to its thread's
//!   own lane, and its time is the overlap of its open stretch with its lane's intervals. A
//!   span's uncovered time is its running time under no child span; it is a column, never
//!   folded into another.
//! - A job's spawner is the job its `started` or `ran` row's `by` names, else the lane of the
//!   span its `job` span names as its `creator` (a job given by a thread on no fiber, the
//!   daemon's loop): the critical path crosses from a job's start to it.

pub mod cpu;
pub mod html;
pub mod load;
pub mod path;
pub mod report;
pub mod sim;
pub mod trace;

#[cfg(test)]
mod tests;

use std::fmt;

use blaze_hash::HashMap;
use std::alloc::Global;

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
    /// `switches.by`: a `started` or `ran` job's spawner, a `ready` job's waker.
    pub by: Option<u64>,
    /// `switches.since_ns`: when a `started` or `ran` job began.
    pub since_ns: Option<i64>,
    /// `switches.began_as`: a `started` job's number before it joined the graph.
    pub began_as: Option<u64>,
    /// `switches.queued_ns`: how long a `started` or `ran` job waited for a core before it began.
    pub queued_ns: Option<i64>,
    /// `switches.cpu_ns`: the thread's CPU time read at the switch ([`cpu`]); `None` when not
    /// read.
    pub cpu_ns: Option<i64>,
}

impl SwitchIn {
    /// A row of `event` and nothing past the switch's own columns.
    pub fn of(
        process: u32,
        thread: u32,
        t_ns: i64,
        event: Event,
        job: u64,
        fiber: u32,
        core: u32,
    ) -> SwitchIn {
        SwitchIn {
            process,
            thread,
            t_ns,
            event,
            job,
            fiber,
            core,
            by: None,
            since_ns: None,
            began_as: None,
            queued_ns: None,
            cpu_ns: None,
        }
    }
}

/// What a switch says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Started,
    /// Waiting for `on` and its value; [`WaitedOn::Moved`] is no wait, only a move to another
    /// core, so what follows it is queued time.
    Parked {
        on: WaitedOn,
        value: u64,
    },
    /// The parked job became runnable, woken by the row's `by`: from here to its resume it is
    /// queued.
    Ready,
    Resumed,
    Ended,
    /// A job that never joined the graph ended: its whole life, from the row's `since_ns`.
    Ran,
    /// A worker went idle or woke: a row of no job.
    Idle,
    Woke,
}

impl Event {
    /// The event a `switches` row names, with its wait columns.
    pub fn of(event: &str, wait: Option<(&str, Option<u64>)>) -> Result<Event, TimeError> {
        const STARTED: &str = Switched::Started.name();
        const PARKED: &str = Switched::Parked.name();
        const RESUMED: &str = Switched::Resumed.name();
        const ENDED: &str = Switched::Ended.name();
        const IDLE: &str = Switched::Idle.name();
        const WOKE: &str = Switched::Woke.name();
        const READY: &str = Switched::Ready.name();
        const RAN: &str = Switched::Ran.name();
        // alloc: an error
        let bad = |why: &'static str| TimeError::BadSwitch { event: event.to_owned(), why };
        Ok(match event {
            STARTED => Event::Started,
            RESUMED => Event::Resumed,
            ENDED => Event::Ended,
            IDLE => Event::Idle,
            WOKE => Event::Woke,
            READY => Event::Ready,
            RAN => Event::Ran,
            PARKED => {
                let (kind, value) = wait.ok_or_else(|| bad("a park with no wait_kind"))?;
                let on = waited_on(kind)
                    .ok_or_else(|| bad("a park whose wait_kind is none the recorder writes"))?;
                let value = match (on, value) {
                    (WaitedOn::Moved | WaitedOn::Nothing, _) => 0,
                    (_, Some(value)) => value,
                    (_, None) => {
                        return Err(bad(
                            "a park on a job, key, counter or deadline with no wait_value",
                        ));
                    }
                };
                Event::Parked { on, value }
            }
            _ => return Err(bad("no event the recorder writes")),
        })
    }

    /// The event of a ring's switch row, as `of` reads its `switches` row.
    pub fn of_row(event: Switched, waited: Option<(WaitedOn, u64)>) -> Result<Event, TimeError> {
        match (event, waited) {
            (Switched::Parked, Some((on, value))) => Ok(Event::Parked { on, value }),
            // alloc: an error
            (Switched::Parked, None) => Err(TimeError::BadSwitch {
                event: event.name().to_owned(),
                why: "a park with no wait",
            }),
            (Switched::Started, _) => Ok(Event::Started),
            (Switched::Resumed, _) => Ok(Event::Resumed),
            (Switched::Ended, _) => Ok(Event::Ended),
            (Switched::Idle, _) => Ok(Event::Idle),
            (Switched::Woke, _) => Ok(Event::Woke),
            (Switched::Ready, _) => Ok(Event::Ready),
            (Switched::Ran, _) => Ok(Event::Ran),
        }
    }

    /// Where it sorts among one job's switches at one instant: a park before the resume that
    /// ends it, a start first and an end last.
    fn rank(self) -> u8 {
        match self {
            Event::Started => 0,
            Event::Ended | Event::Ran | Event::Idle => 4,
            Event::Parked { .. } => 1,
            Event::Ready => 2,
            Event::Resumed | Event::Woke => 3,
        }
    }
}

fn waited_on(name: &str) -> Option<WaitedOn> {
    [
        WaitedOn::Job,
        WaitedOn::Key,
        WaitedOn::Counter,
        WaitedOn::Until,
        WaitedOn::Moved,
        WaitedOn::Nothing,
    ]
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
    /// For a wait on a key: the job running it, as a switch's `job` names it.
    pub owner: Option<u64>,
    /// For a `job` span: the job it is of, as a switch's `job` (or a `started` row's
    /// `began_as`) names it.
    pub job: Option<u64>,
    /// For a `job` span: the span its giver was inside as it gave the job to the pool, as
    /// `span_id` names it in the same process ([`crate::span::col::Creator`]).
    pub creator: Option<u64>,
    pub start_ns: i64,
    pub dur_ns: i64,
    /// `spans.self_cpu_ns`: its CPU while it was the innermost span its thread was inside
    /// ([`cpu`]); `None` when not counted.
    pub self_cpu_ns: Option<i64>,
    /// `spans.folded_ns`: its children too short for a row each ([`crate::flight::FOLD_BELOW`]),
    /// summed; 0 when none was. They ran inside it, so its uncovered time leaves them out.
    pub folded_ns: i64,
}

/// The name of a span that is a wait (`blaze_graph::wait`'s spans).
pub const WAIT_SPAN: &str = "wait";

/// The name of a job's root span ([`crate::span::Job`]): the span that names its job and creator.
pub const JOB_SPAN: &str = <crate::span::Job as crate::span::Span>::NAME;

/// Which of a build's spans the model read ([`load::modelled`]). One build can hold hundreds of
/// millions of spans (240 million in build 9 of a 186-minute self-build, 2026-11-04), more than
/// the model can hold, so past [`load::SPAN_BUDGET`] it reads, in a first pass, the spans at or
/// past `floor_ns` and every span the record never ended; then, in a second, the spans of the
/// lanes that first model's critical path crosses, down to their own floor ([`PathRead`]). Taken
/// by duration, the spans read keep every parent they have: a child lies inside its parent, so is
/// never longer. What the first pass left out of the root `job` spans and the `wait` spans is
/// summed in DuckDB instead ([`CutGroup`]), so a report still says what they were.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpanCut {
    /// The least duration of a span the first pass read; 0 when every span was.
    pub floor_ns: i64,
    /// The build's spans, all told.
    pub spans: u64,
    /// Of them, those read, in both passes.
    pub read: u64,
    /// The durations of those the first pass did not read, summed: a nested one's time is also
    /// its parent's.
    pub cut_ns: i64,
    /// The second pass, over the critical path's lanes; `None` when nothing was cut.
    pub path: Option<PathRead>,
    /// The root `job` spans and the `wait` spans under `floor_ns` by name, crate and subject
    /// (a job's function, a wait's `awaits`): the [`load::CUT_GROUPS`] that took the most time.
    pub groups: Vec<CutGroup>,
    /// The same spans by name alone, every one of them: `krate` and `subject` are `None`.
    pub totals: Vec<CutGroup>,
}

impl SpanCut {
    /// The spans not read.
    pub fn cut(&self) -> u64 {
        self.spans.saturating_sub(self.read)
    }
}

/// The second pass of [`SpanCut`]: the spans of the lanes the first model's critical path
/// crossed, read under the first pass's floor down to `floor_ns`, so the path's work, its phases
/// and the waits on it are named from every span its lanes hold (or every one past `floor_ns`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathRead {
    /// The least duration of a span read for the path's lanes; 0: all of them.
    pub floor_ns: i64,
    /// The lanes whose spans were read.
    pub lanes: u64,
    /// The spans the second pass added.
    pub spans: u64,
    /// Lanes on the final model's critical path whose spans the second pass did not read: the
    /// first model's path did not cross them, or they passed [`load::PATH_STRETCHES`]. Their
    /// spans are those past `SpanCut::floor_ns` alone.
    pub lanes_unread: u64,
}

/// Spans the first pass of [`SpanCut`] did not read, summed in DuckDB: how many, their time, the
/// longest and their CPU.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CutGroup {
    /// [`JOB_SPAN`] or [`WAIT_SPAN`].
    pub name: String,
    pub krate: Option<String>,
    /// A job span's function (its closure's type), a wait span's `awaits`.
    pub subject: Option<String>,
    pub spans: u64,
    pub dur_ns: i64,
    pub max_ns: i64,
    pub self_cpu_ns: i64,
}

/// The name of a pool worker's span of its own work between jobs (`blaze_graph::pool::Driving`):
/// no job's, and no thread lane's either, as a worker's idle is no thread's running.
pub const WORKER_SPAN: &str = <crate::span::Worker as crate::span::Span>::NAME;

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

/// An event as the model takes it: the columns of `events` it reads, and its rows of
/// `event_fields`. A profiled process's part keeps its events so too.
#[derive(Clone, Debug, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct EventIn {
    pub process: u32,
    pub thread: u32,
    pub t_ns: i64,
    /// The build it was part of.
    #[serde(default)]
    pub build: Option<u64>,
    /// The job running on the thread, as a switch's `job` names it.
    pub job: Option<u64>,
    pub level: String,
    pub target: String,
    pub name: String,
    /// Its message alone.
    pub message: Option<String>,
    /// The span it happened in, as a span's `span_id`.
    pub span: Option<u64>,
    /// Its id in its process, as `events.event_id`.
    #[serde(default)]
    pub event_id: u64,
    /// Its fields other than its message, in the order it recorded them: each key and value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<(String, crate::logs::Value)>,
}

impl EventIn {
    /// Its message, then each field as `key=value`: a label.
    pub fn said(&self) -> String {
        let message = self.message.as_deref().unwrap_or("(its text was let go)");
        match self.fields.is_empty() {
            // alloc: a label, once an event a report
            true => message.to_owned(),
            // alloc: a label, once an event a report
            false => format!("{message} {}", crate::logs::fields_text(&self.fields)),
        }
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
    pub events: Vec<EventIn>,
}

impl Input {
    /// What a read of the flight recorder's rings holds, all of `process`: every build in them
    /// as one, bounded by its rows (the trace `flight::dump` writes).
    pub(crate) fn of_read(read: &crate::flight::Read, process: u32) -> Result<Input, TimeError> {
        use crate::{flight, span::col};
        // alloc: a label, once a span a dump
        let text = |sym: flight::Sym| flight::name(sym).map(|it| it.to_string());
        let switches = read
            .switches
            .iter()
            .map(|it| {
                Ok(SwitchIn {
                    process,
                    thread: it.thread,
                    t_ns: it.t_ns,
                    event: Event::of_row(it.event, it.waited)?,
                    job: it.job,
                    fiber: it.fiber,
                    core: it.core,
                    by: it.by,
                    since_ns: it.since_ns,
                    began_as: it.began_as,
                    queued_ns: it.queued_ns,
                    cpu_ns: it.cpu_ns.map(|it| it as i64),
                })
            })
            // alloc: the dump's switches, once a dump
            .collect::<Result<Vec<_>, TimeError>>()?;
        // alloc: the dump's spans, once a dump
        let mut spans = Vec::with_capacity(read.spans.len());
        for it in &read.spans {
            spans.push(SpanIn {
                process,
                thread: it.thread,
                span_id: it.span_id,
                parent: (it.parent != 0).then_some(it.parent),
                depth: it.depth,
                // alloc: a span's name, once a span a dump
                name: flight::meta(it.site).map_or("?", |it| it.name()).to_owned(),
                krate: text(it.krate),
                function: text(it.function),
                unit: text(it.unit),
                awaits: it.get::<col::Awaits>().and_then(|it| it.text()).map(|it| it.to_string()),
                owner: it.get::<col::Owner>(),
                job: it.get::<col::OfJob>(),
                creator: it.get::<col::Creator>(),
                start_ns: it.start_ns,
                dur_ns: it.dur_ns,
                self_cpu_ns: it.self_cpu_ns.and_then(|it| i64::try_from(it).ok()),
                folded_ns: it.folded_ns,
            });
        }
        // alloc: the dump's fields by event, once a dump
        let mut fields: HashMap<u64, Vec<(String, crate::logs::Value)>, Global> =
            HashMap::new_in(Global);
        for it in &read.fields {
            let key = flight::meta(it.site)
                .and_then(|meta| meta.fields().iter().nth(usize::from(it.index)))
                .map_or("?", |it| it.name());
            // alloc: a field, once a field a dump
            fields
                .entry(it.event_id)
                .or_default()
                .push((key.to_owned(), crate::logs::Value::of(it.value)));
        }
        let events = read
            .events
            .iter()
            .map(|it| {
                let meta = flight::meta(it.site);
                EventIn {
                    process,
                    thread: it.thread,
                    t_ns: it.t_ns,
                    build: (it.build != 0).then_some(u64::from(it.build)),
                    job: (it.job != 0).then_some(it.job),
                    // alloc: an event's labels, once an event a dump
                    level: meta.map_or("?", |it| it.level().as_str()).to_owned(),
                    target: meta.map_or("?", |it| it.target()).to_owned(),
                    name: meta.map_or("?", |it| it.name()).to_owned(),
                    message: text(it.message),
                    span: (it.span != 0).then_some(it.span),
                    event_id: it.event_id,
                    fields: fields.remove(&it.event_id).unwrap_or_default(),
                }
            })
            // alloc: the dump's events, once a dump
            .collect();
        Ok(Input { build: 0, build_row: None, switches, spans, events })
    }
}

#[derive(Debug)]
pub enum TimeError {
    /// The history has no build that ended or whose daemon died.
    NoBuild,
    /// The build has no switch and no span.
    Empty {
        build: u64,
    },
    /// A `switches` row the model can't read.
    BadSwitch {
        event: String,
        why: &'static str,
    },
    /// A column of the rows read is missing or of a type the reader does not take.
    BadColumn {
        table: &'static str,
        column: &'static str,
        why: &'static str,
    },
    /// DuckDB's query over the history failed.
    Sql {
        doing: &'static str,
        error: crate::sql::SqlError,
    },
    /// A derived table's batch could not be made.
    Arrow {
        table: &'static str,
        error: arrow_schema::ArrowError,
    },
    /// A derived table could not be written into the history.
    History(crate::history::HistoryError),
    /// The rows of a daemon that died could not be read back from its flight recorder's
    /// mapping, or written into the history ([`crate::history::recover_dead`]).
    Postmortem(crate::history::HistoryError),
    Io {
        path: std::path::PathBuf,
        doing: &'static str,
        error: std::io::Error,
    },
    /// The rows of build `build` come from more than one process: each daemon numbered a build
    /// so in one history. Read as one build, their workers and windows would be summed.
    ManyProcesses {
        build: u64,
        processes: Vec<OfProcess>,
    },
    /// Build `build` holds more spans than the model reads ([`load::SPAN_BUDGET`]) even at the
    /// highest floor tried: `read` at `floor_ns`, of `spans`. The highest floor keeps only the
    /// spans the record never ended (a few for each thread), so this is the hard bound, not a
    /// size a build reaches: every other span is read past a floor or summed in DuckDB.
    TooManySpans {
        build: u64,
        spans: u64,
        read: u64,
        floor_ns: i64,
        budget: u64,
    },
}

/// A process's rows of a build ([`TimeError::ManyProcesses`]): how many, and from when to when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfProcess {
    pub process: u32,
    pub rows: u64,
    pub from_ns: i64,
    pub to_ns: i64,
}

impl fmt::Display for TimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimeError::NoBuild => f.write_str(
                "the history has no build that ended or whose daemon died: name one with --build",
            ),
            TimeError::Empty { build } => {
                write!(f, "build {build} has no switch and no span in the history")
            }
            TimeError::BadSwitch { event, why } => {
                write!(f, "a switches row of event `{event}` can't be read: {why}")
            }
            TimeError::BadColumn { table, column, why } => {
                write!(f, "the column {table}.{column} {why}")
            }
            TimeError::Sql { doing, error } => write!(f, "{doing}: {error}"),
            TimeError::Arrow { table, error } => {
                write!(f, "making the rows of the derived table {table}: {error}")
            }
            TimeError::History(error) => {
                write!(f, "writing a derived table into the history: {error}")
            }
            TimeError::Postmortem(error) => {
                write!(f, "reading back the last rows of a daemon that died: {error}")
            }
            TimeError::Io { path, doing, error } => {
                write!(f, "{doing} {}: {error}", path.display())
            }
            TimeError::ManyProcesses { build, processes } => {
                write!(
                    f,
                    "build {build}'s rows come from {} processes, so they are no one build:",
                    processes.len()
                )?;
                for (i, it) in processes.iter().enumerate() {
                    let secs = |ns: i64| ns as f64 / 1e9;
                    let sep = if i == 0 { " " } else { "; " };
                    write!(
                        f,
                        "{sep}pid {} wrote {} rows from {:.1} s to {:.1} s",
                        it.process,
                        it.rows,
                        secs(it.from_ns),
                        secs(it.to_ns)
                    )?;
                }
                write!(
                    f,
                    ". Each was a daemon that numbered a build {build} in this history (a blaze before 2026-11-03 named a \
                     timer's flush of a build that never ended 0, so the next daemon took its number again); a build's number \
                     is its identity (rule 21), so this one is refused rather than read as one build of all their workers"
                )
            }
            TimeError::TooManySpans { build, spans, read, floor_ns, budget } => write!(
                f,
                "build {build} has {spans} spans, and even taking only those of {:.1} s or more and every unended one, \
                 the model would hold {read}, past its bound of {budget} (time::load::SPAN_BUDGET): so many spans the \
                 record never ended is a recorder bug; read the build with `blaze debug sql` over the spans view",
                *floor_ns as f64 / 1e9
            ),
        }
    }
}

/// Each variant says its cause in its message, as blaze's CLI prints an error's message alone.
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
    /// At its `started` or `ran` row's `since_ns`, or the row itself when it has none.
    Started,
    /// Its first row is neither: it began before the record.
    BeforeRecord,
    /// A thread lane: its first span.
    FirstSpan,
}

/// How a lane's lifetime ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// At its `ended` or `ran` row.
    Ended,
    /// Still running, or waiting, at the end of the record.
    RecordEnd,
    /// Its last row is a run that no row ends ([`Unrecorded::NoEnd`]).
    Unrecorded,
    /// A thread lane: its last span's end.
    LastSpan,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEnd {
    /// At the job's own next row.
    Switch,
    /// Running at the end of a live record (a dump of the rings), which no row ends yet.
    RecordEnd,
    /// A thread lane's run, between its spans' waits.
    Spans,
}

impl RunEnd {
    /// Its name in `job_intervals.run_end`.
    pub fn name(self) -> &'static str {
        match self {
            RunEnd::Switch => "switch",
            RunEnd::RecordEnd => "record_end",
            RunEnd::Spans => "spans",
        }
    }
}

/// Why the record does not say what a lane did: each a row the recorder writes and the ring
/// lost, never a stretch the model fills with a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unrecorded {
    /// Another job switched on its thread before this job's next row: the rows between were
    /// written over.
    Cut,
    /// Its last row is a run and no `ended`, `ran` or `parked` row follows: to its thread's
    /// next switch, or the build's end when the thread has none.
    NoEnd,
    /// A thread lane between its spans.
    BetweenSpans,
}

impl Unrecorded {
    /// Its name in `job_intervals.unrecorded`.
    pub fn name(self) -> &'static str {
        match self {
            Unrecorded::Cut => "cut",
            Unrecorded::NoEnd => "no_end",
            Unrecorded::BetweenSpans => "between_spans",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// On a core: `core` is `None` for a thread lane.
    Running { core: Option<u32>, thread: u32, end: RunEnd },
    /// Waiting for [`Model::waits`]`[wait]`, until the `ready` row.
    Parked { wait: usize },
    /// Runnable, waiting for a core: after a `ready` row, or a move to another core.
    Queued { wait: usize },
    /// Parked then queued, the record not saying where one became the other: a wait with no
    /// `ready` row.
    ParkedOrQueued { wait: usize },
    /// The record lost what the job did, and why.
    Unrecorded { why: Unrecorded },
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Running { .. } => "running",
            State::Parked { .. } => "parked",
            State::Queued { .. } => "queued",
            State::ParkedOrQueued { .. } => "parked_or_queued",
            State::Unrecorded { .. } => "unrecorded",
        }
    }

    pub fn wait(self) -> Option<usize> {
        match self {
            State::Parked { wait } | State::Queued { wait } | State::ParkedOrQueued { wait } => {
                Some(wait)
            }
            State::Running { .. } | State::Unrecorded { .. } => None,
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
            State::Unrecorded { .. } => &mut self.unrecorded_ns,
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
    /// The lane of the job that gave it to the pool, when its `started` or `ran` row names one
    /// the model has; else the lane of its `creator` span.
    pub spawner: Option<usize>,
    /// The span it was given inside, when no job gave it and its `job` span names one the
    /// model has ([`SpanIn::creator`]): its giver's work stood on the path until that span's
    /// end.
    pub creator: Option<usize>,
    /// How long it waited for a core before it began, from its pool's submit: its `started` or
    /// `ran` row's `queued_ns`. `None` when the row does not say.
    pub queued_ns: Option<i64>,
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
    /// The `ready` row's `by`.
    Ready { lane: usize },
    /// No `ready` row: it waited for a job's end, that job.
    Awaited { lane: usize },
    /// No `ready` row: the wait's span named the job running the key.
    Owner { lane: usize },
    /// The `ready` row names no job: a thread on no fiber, or a timer, woke it.
    NoJob,
    /// The record names a job with no lane in the model: one on no fiber (a client thread's),
    /// or one whose rows are of no build the model read.
    NoLane { job: u64 },
    /// The record does not say: a wait with no `ready` row on a scope, a counter or a deadline.
    Unknown,
}

impl Waker {
    pub fn lane(self) -> Option<usize> {
        match self {
            Waker::Ready { lane } | Waker::Awaited { lane } | Waker::Owner { lane } => Some(lane),
            Waker::NoJob | Waker::NoLane { .. } | Waker::Unknown => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Waker::Ready { .. } => "ready",
            Waker::Awaited { .. } => "awaited",
            Waker::Owner { .. } => "owner",
            Waker::NoJob => "no_job",
            Waker::NoLane { .. } => "no_lane",
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
    /// A run another job's switch on its thread cut before its own next row
    /// ([`Unrecorded::Cut`]).
    pub runs_cut: u64,
    /// A `job` span naming a job with no lane: laid on its thread's lane.
    pub spans_of_no_lane: u64,
    /// A job given by no job whose `job` span names a creator span that is not among the
    /// rows (one that began before the build, or was lost): the critical path ends at its
    /// start.
    pub creators_unread: u64,
}

/// A pool worker: a thread whose switches put a job on it or say it idled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worker {
    pub process: u32,
    pub thread: u32,
    /// Its stretches with nothing to run, from an `idle` row to its next `woke` or `idle` row,
    /// in order. One its first row ends (a `woke`) was idle since before the build: from the
    /// build's start. One no row ends lasts to the build's end.
    pub idle: Vec<(i64, i64)>,
}

impl Worker {
    pub fn idle_ns(&self) -> i64 {
        self.idle.iter().map(|it| it.1 - it.0).sum()
    }
}

/// The workers' idle time while a job was runnable and waited for a core: at each instant, the
/// fewer of the workers idle and the jobs queued (after a `ready` row or a move, or from a pool
/// submit to the job's start), so a worker idle beside queued work counts once per job.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdleWithWork {
    pub workers: u32,
    pub wall_ns: i64,
    /// All the workers' idle time.
    pub idle_ns: i64,
    pub with_work_ns: i64,
    /// Whether any `started` or `ran` row said its job's queued time: without, the waits from
    /// a submit to a start are not in `with_work_ns`.
    pub submits_said: bool,
}

impl IdleWithWork {
    /// Of the workers x wall, from 0 to 1.
    pub fn share(&self, ns: i64) -> f64 {
        let capacity = f64::from(self.workers) * self.wall_ns as f64;
        if capacity > 0.0 { ns as f64 / capacity } else { 0.0 }
    }
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
    /// The pool workers, by (process, thread).
    pub workers: Vec<Worker>,
    pub events: Vec<EventIn>,
    /// Each event's lane: its job's, else its span's, else its thread's; `None` on a thread
    /// with no lane.
    pub event_lane: Vec<Option<usize>>,
    pub anomalies: Anomalies,
    /// Each job lane by the numbers its rows name it by: its own, and the `began_as` its
    /// `started` row gave, which its `job` span and the jobs it spawned before then name.
    jobs: HashMap<(u32, u64), usize, Global>,
}

impl Model {
    /// The model of `input`.
    pub fn build(input: Input) -> Result<Model, TimeError> {
        let Input { build, build_row, mut switches, spans, events } = input;
        Model::of(build, build_row, &mut switches, spans, events)
    }

    /// The model of a build's rows, with `switches` sorted in place and left to the caller, so a
    /// second model of the same switches and other spans needs no second read of them
    /// ([`load::modelled`]).
    pub fn of(
        build: u64,
        build_row: Option<(i64, i64)>,
        switches: &mut [SwitchIn],
        spans: Vec<SpanIn>,
        mut events: Vec<EventIn>,
    ) -> Result<Model, TimeError> {
        let bounds = bounds(build, build_row, switches, &spans)?;
        switches.sort_by_key(|it| (it.process, it.t_ns, it.event.rank()));
        let switches: &[SwitchIn] = switches;
        let threads = thread_rows(switches);
        let workers = workers(switches, bounds);
        let mut builder = Builder {
            bounds,
            threads,
            // alloc: once a model
            lanes: Vec::new(),
            // alloc: once a model
            waits: Vec::new(),
            // alloc: once a model
            ready_by: Vec::new(),
            // alloc: once a model
            spawned_by: Vec::new(),
            // alloc: once a model
            began_as: Vec::new(),
            anomalies: Anomalies::default(),
        };
        builder.job_lanes(switches);
        let ready_by = std::mem::take(&mut builder.ready_by);
        let spawned_by = std::mem::take(&mut builder.spawned_by);
        let mut model = builder.finish(build, spans, workers);
        for (lane, by) in spawned_by {
            let process = model.lanes[lane].id.process();
            model.lanes[lane].spawner = model.job_lane(process, by).filter(|&it| it != lane);
        }
        model.lay_spans();
        model.link_creators();
        model.resolve_waits(&ready_by);
        model.time_spans();
        events.sort_by_key(|it| (it.process, it.t_ns, it.thread));
        model.lay_events(events);
        Ok(model)
    }

    /// Puts each event on its lane: the job its row names, else its span's, else its thread's.
    fn lay_events(&mut self, events: Vec<EventIn>) {
        // alloc: once a model with events
        let spans: HashMap<(u32, u64), usize, Global> = if events.is_empty() {
            HashMap::new_in(Global)
        } else {
            self.spans.iter().enumerate().map(|(i, it)| ((it.process, it.span_id), i)).collect()
        };
        self.event_lane = events
            .iter()
            .map(|it| {
                it.job
                    .and_then(|job| self.job_lane(it.process, job))
                    .or_else(|| {
                        it.span
                            .and_then(|span| spans.get(&(it.process, span)))
                            .and_then(|&i| self.span_time[i].lane)
                    })
                    .or_else(|| {
                        self.lane(LaneId::Thread { process: it.process, thread: it.thread })
                    })
            })
            // alloc: once a model
            .collect();
        self.events = events;
    }

    /// Its spans, the rest let go: a second model of the same build takes them ([`load::modelled`]).
    pub fn into_spans(self) -> Vec<SpanIn> {
        self.spans
    }

    /// The lane `id`, if the model has it.
    pub fn lane(&self, id: LaneId) -> Option<usize> {
        self.lanes.binary_search_by_key(&id, |it| it.id).ok()
    }

    /// The lane of the job `process`'s rows name `job`: by its own number, or the one it had
    /// before it joined the graph.
    pub fn job_lane(&self, process: u32, job: u64) -> Option<usize> {
        self.jobs.get(&(process, job)).copied()
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
            Some(WaitedOn::Job) => format!("{}", LaneId::Job { process: 0, job: wait.value }),
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

    /// [`IdleWithWork`] over the build.
    pub fn idle_with_work(&self) -> IdleWithWork {
        let (start, end) = (self.bounds.start_ns, self.bounds.end_ns);
        // alloc: once a report
        let mut steps: Vec<(i64, i32, i32)> = Vec::new();
        let mut step = |from: i64, to: i64, idle: i32, queued: i32| {
            let (from, to) = (from.max(start), to.min(end));
            if to > from {
                steps.push((from, idle, queued));
                steps.push((to, -idle, -queued));
            }
        };
        let mut idle_ns = 0;
        for worker in &self.workers {
            idle_ns += worker.idle_ns();
            for &(from, to) in &worker.idle {
                step(from, to, 1, 0);
            }
        }
        let mut submits_said = false;
        for lane in self.lanes.iter().filter(|it| it.id.job().is_some()) {
            if let (Begin::Started, Some(queued)) = (lane.begin, lane.queued_ns) {
                submits_said = true;
                step(lane.begin_ns() - queued, lane.begin_ns(), 0, 1);
            }
            for it in lane.intervals.iter().filter(|it| matches!(it.state, State::Queued { .. })) {
                step(it.from_ns, it.to_ns, 0, 1);
            }
        }
        steps.sort_unstable();
        let (mut idle, mut queued, mut at, mut with_work_ns) = (0i64, 0i64, start, 0i64);
        for (t, d_idle, d_queued) in steps {
            with_work_ns += idle.min(queued).max(0) * (t - at);
            (idle, queued, at) = (idle + i64::from(d_idle), queued + i64::from(d_queued), t);
        }
        let workers = u32::try_from(self.workers.len()).unwrap_or(u32::MAX);
        IdleWithWork {
            workers,
            wall_ns: self.bounds.wall_ns(),
            idle_ns,
            with_work_ns,
            submits_said,
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
type Threads = HashMap<(u32, u32), Vec<i64>, Global>;

fn thread_rows(switches: &[SwitchIn]) -> Threads {
    let mut threads: Threads = HashMap::new_in(Global);
    // A ready row is written by whichever job runs on the thread, so it switches nothing there;
    // a job's `since_ns` is a switch of its thread, where it began.
    for it in switches.iter().filter(|it| it.event != Event::Ready) {
        // alloc: a thread's instants, once a thread a model
        let rows = threads.entry((it.process, it.thread)).or_default();
        rows.push(it.t_ns);
        rows.extend(it.since_ns);
    }
    for rows in threads.values_mut() {
        rows.sort_unstable();
    }
    threads
}

/// The pool workers of `switches` (sorted by time) and their idle stretches.
fn workers(switches: &[SwitchIn], bounds: Bounds) -> Vec<Worker> {
    // A BTreeMap: the workers come out in order.
    // alloc: once a model
    let mut workers: std::collections::BTreeMap<(u32, u32), (Vec<(i64, i64)>, Option<i64>, bool)> =
        std::collections::BTreeMap::new();
    for it in switches {
        let on_worker = matches!(
            it.event,
            Event::Idle | Event::Woke | Event::Started | Event::Resumed | Event::Ran
        );
        if !on_worker {
            continue;
        }
        // alloc: a worker's stretches, once a worker a model
        let (idle, open, seen) = workers.entry((it.process, it.thread)).or_default();
        match it.event {
            Event::Idle => {
                if let Some(from) = open.replace(it.t_ns) {
                    idle.push((from, it.t_ns));
                }
            }
            Event::Woke => match open.take() {
                Some(from) => idle.push((from, it.t_ns)),
                // Its first row: it slept since before the build.
                None if !*seen => idle.push((bounds.start_ns, it.t_ns)),
                None => {}
            },
            _ => {}
        }
        *seen = true;
    }
    workers
        .into_iter()
        .map(|((process, thread), (mut idle, open, _))| {
            if let Some(from) = open {
                idle.push((from, bounds.end_ns.max(from)));
            }
            idle.retain(|it| it.1 > it.0);
            Worker { process, thread, idle }
        })
        // alloc: once a model
        .collect()
}

fn bounds(
    build: u64,
    row: Option<(i64, i64)>,
    switches: &[SwitchIn],
    spans: &[SpanIn],
) -> Result<Bounds, TimeError> {
    let first = switches
        .iter()
        .map(|it| it.since_ns.unwrap_or(it.t_ns))
        .chain(spans.iter().map(|it| it.start_ns))
        .min();
    let last = switches.iter().map(|it| it.t_ns).chain(spans.iter().map(SpanIn::end_ns)).max();
    let (Some(first), Some(last)) = (first, last) else { return Err(TimeError::Empty { build }) };
    Ok(match row {
        Some((start, dur)) => Bounds {
            start_ns: start.min(first),
            end_ns: (start + dur).max(last),
            from: BoundsFrom::Build,
        },
        None => Bounds { start_ns: first, end_ns: last, from: BoundsFrom::Record },
    })
}

struct Builder {
    bounds: Bounds,
    threads: Threads,
    lanes: Vec<Lane>,
    waits: Vec<Wait>,
    /// Each wait a `ready` row ended: the wait and the row's `by`.
    ready_by: Vec<(usize, Option<u64>)>,
    /// Each lane whose `started` or `ran` row names its spawner: the lane and the spawner's job.
    spawned_by: Vec<(usize, u64)>,
    /// Each `started` row's `began_as`: the (process, number) and the lane it names.
    began_as: Vec<((u32, u64), usize)>,
    anomalies: Anomalies,
}

/// What a job is doing between two of its rows.
#[derive(Clone, Copy)]
enum Doing {
    /// Before its first row: nothing of it is in the record.
    Nothing,
    /// Running on `thread`, whose last row of this job was at `last`.
    Running {
        core: u32,
        thread: u32,
        last: i64,
    },
    Waiting {
        wait: usize,
        ready: bool,
    },
}

impl Builder {
    fn job_lanes(&mut self, switches: &[SwitchIn]) {
        // alloc: each job's rows, once a model
        let mut jobs: HashMap<(u32, u64), Vec<SwitchIn>, Global> = HashMap::new_in(Global);
        for it in switches.iter().filter(|it| !matches!(it.event, Event::Idle | Event::Woke)) {
            // alloc: a job's rows, once a job a model
            jobs.entry((it.process, it.job)).or_default().push(*it);
        }
        // alloc: the jobs in order, once a model
        let mut jobs: Vec<_> = jobs.into_iter().collect();
        jobs.sort_unstable_by_key(|it| it.0);
        for ((process, job), rows) in jobs {
            self.job_lane(process, job, &rows);
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

    fn job_lane(&mut self, process: u32, job: u64, rows: &[SwitchIn]) {
        let lane = self.lanes.len();
        let id = LaneId::Job { process, job };
        // alloc: a job's intervals, once a job a model
        let mut intervals: Vec<Interval> = Vec::new();
        let first = rows.first();
        let begin = match first.map(|it| it.event) {
            Some(Event::Started | Event::Ran) => Begin::Started,
            _ => Begin::BeforeRecord,
        };
        if let Some(first) = first.filter(|it| matches!(it.event, Event::Started | Event::Ran)) {
            if let Some(by) = first.by {
                self.spawned_by.push((lane, by));
            }
            if let Some(began_as) = first.began_as {
                self.began_as.push(((process, began_as), lane));
            }
        }
        let queued_ns = first
            .filter(|it| matches!(it.event, Event::Started | Event::Ran))
            .and_then(|it| it.queued_ns);
        let mut end = End::RecordEnd;
        let mut at = first.map_or(self.bounds.start_ns, |it| it.t_ns);
        let mut doing = Doing::Nothing;
        // A job that began before it joined the graph ran on its thread from `since_ns`.
        if let Some(first) = first
            && let (Event::Started | Event::Ran, Some(since)) = (first.event, first.since_ns)
            && since < first.t_ns
        {
            at = since;
            doing = Doing::Running { core: first.core, thread: first.thread, last: since };
        }
        let push = |intervals: &mut Vec<Interval>, from: i64, to: i64, state: State| {
            if to > from {
                intervals.push(Interval { from_ns: from, to_ns: to, state, inside: None });
            }
        };
        for row in rows {
            let t = row.t_ns;
            // Joining the graph mid-run switches nothing: the run goes on.
            if let (Event::Started, Doing::Running { core, thread, .. }) = (row.event, doing) {
                doing = Doing::Running { core, thread, last: t };
                continue;
            }
            match doing {
                Doing::Nothing => {}
                Doing::Running { core, thread, last } => {
                    // A thread runs one fiber: another job's switch on it before this job's
                    // next row means the rows between were lost, and when this run ended with them.
                    if self.thread_next(process, thread, last).is_some_and(|next| next < t) {
                        self.anomalies.runs_cut += 1;
                        push(&mut intervals, at, t, State::Unrecorded { why: Unrecorded::Cut });
                    } else {
                        push(
                            &mut intervals,
                            at,
                            t,
                            State::Running { core: Some(core), thread, end: RunEnd::Switch },
                        );
                    }
                }
                Doing::Waiting { wait, ready } => {
                    push(&mut intervals, at, t, self.waiting(wait, ready))
                }
            }
            at = t;
            doing = match row.event {
                Event::Started | Event::Resumed => {
                    if let Doing::Waiting { wait, .. } = doing {
                        self.waits[wait].resumed_ns = Some(t);
                    }
                    Doing::Running { core: row.core, thread: row.thread, last: t }
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
                Event::Ready => match doing {
                    Doing::Waiting { wait, ready: false } => {
                        self.ready_by.push((wait, row.by));
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
                Event::Ended | Event::Ran => {
                    // A job's number is its generation and slot, so no row of it follows its end.
                    end = End::Ended;
                    break;
                }
                Event::Idle | Event::Woke => doing,
            };
        }
        if end != End::Ended {
            let record_end = self.bounds.end_ns.max(at);
            match doing {
                Doing::Running { core, thread, last } => {
                    match self.thread_next(process, thread, last) {
                        // Its thread went on to other work, and no row says when this job stopped.
                        Some(next) => {
                            end = End::Unrecorded;
                            push(
                                &mut intervals,
                                at,
                                next,
                                State::Unrecorded { why: Unrecorded::NoEnd },
                            );
                        }
                        None if self.bounds.from == BoundsFrom::Record => {
                            push(
                                &mut intervals,
                                at,
                                record_end,
                                State::Running { core: Some(core), thread, end: RunEnd::RecordEnd },
                            );
                        }
                        // The build ended, and its rows hold no end of this run.
                        None => {
                            end = End::Unrecorded;
                            push(
                                &mut intervals,
                                at,
                                record_end,
                                State::Unrecorded { why: Unrecorded::NoEnd },
                            );
                        }
                    }
                }
                Doing::Waiting { wait, ready } => {
                    push(&mut intervals, at, record_end, self.waiting(wait, ready))
                }
                Doing::Nothing => {}
            }
        }
        let fiber = rows
            .iter()
            .find(|it| matches!(it.event, Event::Started | Event::Resumed | Event::Ran))
            .map(|it| it.fiber);
        // alloc: a lane's sums, once a lane a model
        let mut lane = Lane {
            id,
            fiber,
            begin,
            end,
            spawner: None,
            creator: None,
            queued_ns,
            intervals,
            spans: Vec::new(),
            before: Vec::new(),
        };
        lane.sum();
        self.lanes.push(lane);
    }

    fn finish(self, build: u64, spans: Vec<SpanIn>, workers: Vec<Worker>) -> Model {
        let count = spans.len();
        // alloc: once a model
        let mut jobs: HashMap<(u32, u64), usize, Global> = HashMap::new_in(Global);
        for (l, lane) in self.lanes.iter().enumerate() {
            if let LaneId::Job { process, job } = lane.id {
                jobs.insert((process, job), l);
            }
        }
        jobs.extend(self.began_as);
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
            workers,
            // alloc: none until laid
            events: Vec::new(),
            event_lane: Vec::new(),
            anomalies: self.anomalies,
            jobs,
        }
    }
}

/// Where a span is laid, before the lanes of threads are made.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Laid {
    Job(usize),
    Thread {
        process: u32,
        thread: u32,
    },
    /// A pool worker's own span ([`WORKER_SPAN`]) or one under it: on no lane.
    Worker,
}

impl Model {
    /// Lays each span on its lane: its parent's, else the job its row names (a `job` span's
    /// `job`), else its thread's own lane; then makes the thread lanes and marks what each
    /// interval ran inside.
    fn lay_spans(&mut self) {
        // alloc: once a model
        let mut index: HashMap<(u32, u64), usize, Global> = HashMap::new_in(Global);
        for (i, it) in self.spans.iter().enumerate() {
            index.insert((it.process, it.span_id), i);
        }
        for (i, it) in self.spans.iter().enumerate() {
            let Some(parent) = it.parent else { continue };
            match index.get(&(it.process, parent)) {
                Some(&p) if p != i => self.parent[i] = Some(p),
                _ => self.anomalies.orphans += 1,
            }
        }
        // alloc: once a model
        let mut order: Vec<usize> = (0..self.spans.len()).collect();
        // By span id on a tie, never by row order: the model must not depend on how rows came.
        order.sort_by_key(|&i| {
            (
                self.spans[i].start_ns,
                self.spans[i].depth,
                self.spans[i].process,
                self.spans[i].span_id,
            )
        });
        // alloc: once a model
        let mut laid: Vec<Option<Laid>> = vec![None; self.spans.len()];
        // alloc: once a model
        let mut placed: Vec<(usize, Laid)> = Vec::with_capacity(order.len());
        for &i in &order {
            let span = &self.spans[i];
            let thread = Laid::Thread { process: span.process, thread: span.thread };
            let it = match (self.parent[i].and_then(|p| laid[p]), span.job) {
                _ if span.name == WORKER_SPAN => Laid::Worker,
                (Some(parent), _) => parent,
                (None, Some(job)) => match self.job_lane(span.process, job) {
                    Some(lane) => Laid::Job(lane),
                    None => {
                        self.anomalies.spans_of_no_lane += 1;
                        thread
                    }
                },
                (None, None) => thread,
            };
            laid[i] = Some(it);
            placed.push((i, it));
        }
        // A BTreeMap: the thread lanes are made in order, after the jobs', so the lanes stay
        // sorted by id.
        // alloc: once a model
        let mut threads: std::collections::BTreeMap<(u32, u32), Vec<usize>> =
            std::collections::BTreeMap::new();
        for (i, it) in placed {
            match it {
                Laid::Job(lane) => {
                    self.lanes[lane].spans.push(i);
                    self.span_time[i].lane = Some(lane);
                }
                // alloc: a thread lane's spans, once a lane a model
                Laid::Thread { process, thread } => {
                    threads.entry((process, thread)).or_default().push(i)
                }
                Laid::Worker => {}
            }
        }
        for ((process, thread), spans) in threads {
            let lane = self.lanes.len();
            for &i in &spans {
                self.span_time[i].lane = Some(lane);
            }
            self.thread_lane(process, thread, spans);
        }
        let spans = &self.spans;
        for lane in &mut self.lanes {
            mark_inside(lane, spans);
        }
    }

    /// Gives each job lane no job gave (its `started` or `ran` row names no spawner the model
    /// has) the lane of the span its `job` span names as its creator: a job the daemon's loop
    /// gave, on no fiber, stands on the loop's work, which the critical path crosses into.
    fn link_creators(&mut self) {
        let wanted = |lane: &Lane| lane.spawner.is_none() && matches!(lane.id, LaneId::Job { .. });
        if !self.lanes.iter().any(wanted) {
            return;
        }
        // alloc: once a model
        let mut index: HashMap<(u32, u64), usize, Global> = HashMap::new_in(Global);
        for (i, it) in self.spans.iter().enumerate() {
            index.insert((it.process, it.span_id), i);
        }
        for l in 0..self.lanes.len() {
            if !wanted(&self.lanes[l]) {
                continue;
            }
            let named = self.lanes[l].spans.iter().find_map(|&i| {
                let span = &self.spans[i];
                // Its own `job` span, not that of a job run inside it.
                span.job.filter(|&job| self.job_lane(span.process, job) == Some(l))?;
                span.creator.map(|creator| (span.process, creator))
            });
            let Some(named) = named else { continue };
            let Some(&creator) = index.get(&named) else {
                self.anomalies.creators_unread += 1;
                continue;
            };
            let Some(lane) = self.span_time[creator].lane.filter(|&it| it != l) else { continue };
            self.lanes[l].spawner = Some(lane);
            self.lanes[l].creator = Some(creator);
        }
    }

    /// The lane of a thread no switch puts a job on: running while any of its spans is open,
    /// parked-or-queued inside its outermost `wait` spans, unrecorded between its spans.
    fn thread_lane(&mut self, process: u32, thread: u32, spans: Vec<usize>) {
        let lane = self.lanes.len();
        // alloc: a thread lane's stretches, once a lane a model
        let mut covered: Vec<(i64, i64)> = Vec::with_capacity(spans.len());
        for &i in &spans {
            let (from, to) = (self.spans[i].start_ns, self.spans[i].end_ns());
            match covered.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => covered.push((from, to)),
            }
        }
        // alloc: a thread lane's waits, once a lane a model
        let mut waits: Vec<(i64, i64, usize)> = Vec::new();
        for &i in spans.iter().filter(|&&i| self.spans[i].is_wait()) {
            let (from, to) = (self.spans[i].start_ns, self.spans[i].end_ns());
            if waits.last().is_some_and(|last| from < last.1) {
                continue;
            }
            let wait = self.waits.len();
            self.waits.push(Wait {
                lane,
                on: None,
                value: 0,
                span: Some(i),
                parked_ns: from,
                ready_ns: None,
                resumed_ns: Some(to),
                waker: Waker::Unknown,
            });
            waits.push((from, to, wait));
        }
        let running = State::Running { core: None, thread, end: RunEnd::Spans };
        // alloc: a thread lane's intervals, once a lane a model
        let mut intervals = Vec::new();
        let mut push = |from: i64, to: i64, state: State| {
            if to > from {
                intervals.push(Interval { from_ns: from, to_ns: to, state, inside: None });
            }
        };
        let mut t = covered.first().map_or(0, |it| it.0);
        let mut next = waits.iter().peekable();
        for &(from, to) in &covered {
            push(t, from, State::Unrecorded { why: Unrecorded::BetweenSpans });
            t = from;
            while let Some(&&(wait_from, wait_to, wait)) = next.peek()
                && wait_from < to
            {
                push(t, wait_from, running);
                let (wait_from, wait_to) = (wait_from.max(t), wait_to.min(to));
                push(wait_from, wait_to, State::ParkedOrQueued { wait });
                t = t.max(wait_to);
                next.next();
            }
            push(t, to, running);
            t = to;
        }
        let id = LaneId::Thread { process, thread };
        let mut lane = Lane {
            id,
            fiber: None,
            begin: Begin::FirstSpan,
            end: End::LastSpan,
            spawner: None,
            creator: None,
            queued_ns: None,
            intervals,
            spans,
            before: Vec::new(),
        };
        lane.sum();
        self.lanes.push(lane);
    }

    /// Each wait's span, and who woke it.
    fn resolve_waits(&mut self, ready_by: &[(usize, Option<u64>)]) {
        // alloc: each lane's wait spans, once a model
        let mut wait_spans: Vec<Vec<usize>> = Vec::with_capacity(self.lanes.len());
        for lane in &self.lanes {
            // alloc: a lane's wait spans, once a lane a model
            wait_spans
                .push(lane.spans.iter().copied().filter(|&i| self.spans[i].is_wait()).collect());
        }
        // alloc: once a model
        let ready: HashMap<usize, Option<u64>, Global> = ready_by.iter().copied().collect();
        let jobs = &self.jobs;
        let named = |process: u32, job: u64, found: fn(usize) -> Waker| {
            jobs.get(&(process, job)).copied().map_or(Waker::NoLane { job }, found)
        };
        for w in 0..self.waits.len() {
            let wait = &self.waits[w];
            let process = self.lanes[wait.lane].id.process();
            let span = match (wait.span, wait.on) {
                (Some(span), _) => Some(span),
                (None, Some(WaitedOn::Moved | WaitedOn::Nothing) | None) => None,
                (None, Some(_)) => {
                    let spans = &wait_spans[wait.lane];
                    let at = spans.partition_point(|&i| self.spans[i].start_ns <= wait.parked_ns);
                    at.checked_sub(1)
                        .map(|at| spans[at])
                        .filter(|&i| self.spans[i].end_ns() >= wait.parked_ns)
                }
            };
            let owner = span.and_then(|it| self.spans[it].owner);
            let waker = match (ready.get(&w), wait.on, owner) {
                (Some(&Some(by)), _, _) => named(process, by, |lane| Waker::Ready { lane }),
                (Some(&None), _, _) => Waker::NoJob,
                (None, Some(WaitedOn::Job), _) => {
                    named(process, wait.value, |lane| Waker::Awaited { lane })
                }
                (None, _, Some(owner)) => named(process, owner, |lane| Waker::Owner { lane }),
                (None, _, None) => Waker::Unknown,
            };
            let own = wait.lane;
            let wait = &mut self.waits[w];
            wait.span = span;
            // A wait on itself is the scheduler's error, never a waker to follow.
            wait.waker = if waker.lane() == Some(own) { Waker::Unknown } else { waker };
        }
    }

    /// Each span's time by its lane's states, and less its children's.
    fn time_spans(&mut self) {
        // alloc: once a model
        let mut under: Vec<Totals> = vec![Totals::default(); self.spans.len()];
        // alloc: once a model
        let mut last_child_end: HashMap<usize, i64, Global> = HashMap::new_in(Global);
        for lane in &self.lanes {
            for &i in &lane.spans {
                let span = &self.spans[i];
                let mut totals = lane.accrued(span.end_ns()).minus(lane.accrued(span.start_ns));
                // What of it fell outside its lane's lifetime, the record says nothing of.
                totals.unrecorded_ns += span.dur_ns.max(0) - totals.total();
                self.span_time[i].totals = totals;
                let Some(p) =
                    self.parent[i].filter(|&p| self.span_time[p].lane == self.span_time[i].lane)
                else {
                    continue;
                };
                under[p].add(totals);
                let parent = &self.spans[p];
                let escapes = span.start_ns < parent.start_ns || span.end_ns() > parent.end_ns();
                let overlaps = last_child_end.get(&p).is_some_and(|&end| span.start_ns < end);
                if escapes || overlaps {
                    self.anomalies.misnested += 1;
                }
                let end = last_child_end.entry(p).or_insert(span.end_ns());
                *end = (*end).max(span.end_ns());
            }
        }
        for ((it, under), span) in self.span_time.iter_mut().zip(under).zip(&self.spans) {
            it.own = it.totals.minus(under);
            // Its folded children ran inside it, under a span with no row: covered, never its
            // own. A fold neither parks nor waits ([`crate::flight::WAIT_FOLD_BELOW`]), so
            // their time is running time.
            it.own.running_ns -= span.folded_ns.clamp(0, it.own.running_ns.max(0));
        }
    }
}

/// Marks each of `lane`'s intervals with the innermost of its spans open just before it ended.
/// Linear in its spans: a span that closed leaves the stack of open ones as it reaches the top.
/// (Until 2026-11-03 each close searched the stack and removed from its middle: a thread lane
/// with millions of spans in one running stretch held them all open at once, and `blaze slow`
/// ran for more than ten minutes on a self-build whose recorder lost no rows.)
fn mark_inside(lane: &mut Lane, spans: &[SpanIn]) {
    // alloc: a lane's spans' places by end, once a lane a model
    let mut by_end: Vec<usize> = (0..lane.spans.len()).collect();
    by_end.sort_by_key(|&at| spans[lane.spans[at]].end_ns());
    // alloc: whether each of a lane's spans closed, once a lane a model
    let mut ended = vec![false; lane.spans.len()];
    // alloc: a lane's open spans' places, once a lane a model
    let mut open: Vec<usize> = Vec::new();
    let (mut opened, mut closed) = (0, 0);
    for interval in &mut lane.intervals {
        let end = interval.to_ns;
        while let Some(&i) = lane.spans.get(opened)
            && spans[i].start_ns < end
        {
            open.push(opened);
            opened += 1;
        }
        while let Some(&at) = by_end.get(closed)
            && spans[lane.spans[at]].end_ns() < end
        {
            ended[at] = true;
            closed += 1;
        }
        while open.last().is_some_and(|&at| ended[at]) {
            open.pop();
        }
        interval.inside = open.last().map(|&at| lane.spans[at]);
    }
}
