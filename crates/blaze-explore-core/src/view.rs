//! The page's view models, display-ready, in deterministic CBOR (blaze-protocol's codec): the
//! page decodes them and draws, with nothing left to work out.
//!
//! Ids, hashes and `_ns` times are decimal strings, so a JavaScript decoder keeps every digit
//! (a `u64` past 2^53 does not fit a double); durations are integer milliseconds beside their
//! text; counts are integers. The text is the view's: `3 of 8 busy · 5 idle`, `1m05s`, and a
//! breadcrumb `crate › step › pass › item` for each busy thread.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_protocol::{
    cbor,
    now::{self, Phase, Pressure},
};
use blaze_serde::{Deserialize, Serialize};

use crate::{
    Error,
    live::{OpenRow, Stream},
    state::{BLAZE, Busy, Session, elapsed},
    tasks::Tasks,
};

/// How long nothing may close while workers are busy before the headline says it is slow:
/// blaze-profile's `live::QUIET_WARN`, under the stall watchdog's bound.
pub const QUIET_WARN_MS: u64 = 10_000;

/// Which view to render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Which {
    /// [`RunningNow`].
    RunningNow = 1,
    /// [`Recent`].
    Recent = 2,
    /// [`Counters`].
    Counters = 3,
    /// [`SessionView`].
    Session = 4,
}

impl Which {
    pub fn from_u32(which: u32) -> Option<Which> {
        Some(match which {
            1 => Which::RunningNow,
            2 => Which::Recent,
            3 => Which::Counters,
            4 => Which::Session,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Which::RunningNow => "view running now",
            Which::Recent => "view recent",
            Which::Counters => "view counters",
            Which::Session => "view session",
        }
    }
}

/// Running now: which workers are busy, on what, for how long; and, from the one model of the
/// moment (`blaze_protocol::now::Now`), the phase, the tasks' tally, what holds the rest back,
/// the downloads, the memory and whether it is slow or stalled.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RunningNow {
    pub at_ns: String,
    pub now_ns: String,
    /// `3 of 8 busy · 5 idle`, and ` · slow: no span closed for 12s` when it is.
    pub headline: String,
    /// Whether workers are the pool's (the `blaze` layer) or every thread.
    pub layered: bool,
    pub workers: u64,
    pub busy_count: u64,
    pub idle: u64,
    /// `5 idle workers`, `none idle`: the idle in one line, not a row each.
    pub idle_line: String,
    /// Busy workers, longest first.
    pub busy: Vec<ThreadRow>,
    /// The daemon's own threads inside spans.
    pub internal: Vec<ThreadRow>,
    pub quiet_ms: Option<u64>,
    pub quiet_text: Option<String>,
    /// The phase's word (`compiling`, `linking`); `running` for a program with no tasks.
    pub phase: String,
    /// What the source says its stage waits on, or empty.
    pub stage: String,
    pub tally: TallyLine,
    /// The unfinished tasks whose needs are all done, the one holding most back first.
    pub blockers: Vec<BlockerRow>,
    /// Every download in flight, oldest first.
    pub downloads: Vec<DownloadRow>,
    pub memory: MemoryLine,
    /// The settled tasks that ran longest, longest first.
    pub slowest: Vec<TookRow>,
    /// Nothing settled, got a byte or closed for 10 s while a worker is busy or a download runs.
    pub slow: bool,
    /// Nothing moved for 10 s, no worker busy and no download in flight.
    pub stalled: bool,
    /// Since a task settled, a download got a byte or a span closed.
    pub still_ms: u64,
    pub still_text: String,
}

/// A task as a view names it: its ids as the source said them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TaskLine {
    /// The source's process and task ids; `None` for a task that has left the live set.
    pub process: Option<String>,
    pub task: Option<String>,
    pub kind: String,
    pub name: String,
}

/// How many tasks are where.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TallyLine {
    pub waiting: u64,
    pub ready: u64,
    pub running: u64,
    pub done: u64,
    pub failed: u64,
    pub total: u64,
    /// `12 of 40 done · 3 running · 5 ready · 20 waiting`, and ` · 1 failed` when any did.
    pub text: String,
}

/// An unfinished task whose needs are done, and what waits on it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BlockerRow {
    pub task: TaskLine,
    /// Running on a worker; otherwise ready and waiting for one.
    pub running: bool,
    /// Since it began running (the source's `start_ns`), or since it became ready.
    pub for_ms: u64,
    pub for_text: String,
    /// The innermost span of the worker running it: `typeck_query app::main`.
    pub doing: Option<String>,
    /// Unfinished tasks waiting on it, directly or not.
    pub holds: u64,
    /// The longest path of unfinished tasks behind it.
    pub chain: u64,
    /// The first few waiting on it directly, by name.
    pub behind: Vec<String>,
    /// `serde (codegen) · running 4.2s · holds 12, chain 3`.
    pub line: String,
}

/// A download in flight.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DownloadRow {
    pub what: String,
    pub done: u64,
    pub total: Option<u64>,
    /// Bytes a second over the last few seconds, once there are two samples.
    pub rate: Option<u64>,
    pub idle_ms: u64,
    pub elapsed_ms: u64,
    pub eta_ms: Option<u64>,
    /// `serde-1.0.219.crate 1.2 MiB of 3.4 MiB · 800 KiB/s`.
    pub text: String,
}

/// The memory the program holds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MemoryLine {
    pub held: u64,
    pub peak: u64,
    /// The ceiling it runs under; 0 for none known.
    pub limit: u64,
    /// `calm`, `rising`, `squeezed`, `over`.
    pub pressure: String,
    /// `holds 3.2 GiB, peak 4.0 GiB`, and ` of 16.0 GiB` with a ceiling.
    pub text: String,
}

/// A settled task and how long it ran.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TookRow {
    pub task: TaskLine,
    pub ms: u64,
    pub text: String,
}

/// A thread inside spans.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ThreadRow {
    /// `process:thread`, unique in the view.
    pub key: String,
    pub process: String,
    pub thread: String,
    pub thread_name: String,
    /// `crate › step › pass › item`, each part only when the chain has it.
    pub breadcrumb: String,
    /// The innermost span's label.
    pub doing: String,
    /// How long it has been inside its outermost span.
    pub for_ms: u64,
    pub for_text: String,
    /// Its open spans, outermost first, the innermost last.
    pub stack: Vec<SpanLine>,
    /// The task its innermost span carries (`open.task`), for a worker on one.
    pub task: Option<TaskLine>,
}

/// An open span of a thread's chain.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SpanLine {
    pub depth: u64,
    pub name: String,
    pub target: Option<String>,
    #[serde(rename = "crate")]
    pub krate: Option<String>,
    pub function: Option<String>,
    pub unit: Option<String>,
    pub awaits: Option<String>,
    pub owner: Option<String>,
    pub task: Option<String>,
    pub span: Option<String>,
    pub for_ms: u64,
    pub for_text: String,
    /// `typeck_query app::main`, `wait on <what>`.
    pub label: String,
}

/// The spans that closed last, newest first.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Recent {
    pub at_ns: String,
    pub spans: Vec<Closed>,
}

/// A span that closed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Closed {
    pub key: String,
    pub thread_name: Option<String>,
    pub span: String,
    pub parent: Option<String>,
    pub build: Option<String>,
    pub name: String,
    #[serde(rename = "crate")]
    pub krate: Option<String>,
    pub function: Option<String>,
    pub unit: Option<String>,
    pub label: String,
    pub start_ns: String,
    pub dur_ms: u64,
    pub dur_text: String,
    /// How long ago it closed.
    pub ago_ms: u64,
    pub ago_text: String,
}

/// Each counter's latest value, by process and name.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Counters {
    pub at_ns: String,
    pub counters: Vec<CounterLine>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CounterLine {
    pub process: String,
    pub name: String,
    pub value: f64,
    pub value_text: String,
    pub t_ns: String,
    pub ago_ms: u64,
    pub build: Option<String>,
}

/// The session: who the daemon is and what the live stream said beside rows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SessionView {
    pub daemon: String,
    pub schema: u64,
    pub clock: String,
    pub layers: Vec<String>,
    pub protocol: Option<u64>,
    pub live_every_ms: u64,
    pub certhash: String,
    pub cert_not_after_unix_s: String,
    pub at_ns: String,
    pub frames: u64,
    pub skipped: u64,
    pub lost: u64,
    pub other_frames: u64,
    pub refused: u64,
    /// Streams begun of `open`, `spans`, `counters`, `tasks` and `task_edges`.
    pub streams: Vec<u64>,
    /// The last tick the daemon could not encode: `kind: message`.
    pub refusal: Option<String>,
    /// The last answer: `op`, its rows, or its refusal.
    pub answer: Option<String>,
}

/// `which` at `now_ns` into `out` (cleared first) as one deterministic CBOR item.
pub fn render<S: Stream>(
    session: &mut Session<S>,
    which: Which,
    now_ns: i64,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    out.clear();
    let encoded = match which {
        Which::RunningNow => cbor::append(out, &running_now(session, now_ns)?),
        Which::Recent => cbor::append(out, &recent(session, now_ns)?),
        Which::Counters => cbor::append(out, &counters(session, now_ns)?),
        Which::Session => cbor::append(out, &session_view(session, now_ns)?),
    };
    // alloc: an error's text
    encoded.map_err(|why| Error::new(which.name(), "its CBOR", why.to_string()))
}

fn ms(ns: u64) -> u64 {
    ns / 1_000_000
}

/// A duration for people, integer arithmetic only: `840ms`, `4.2s`, `42s`, `3m05s`, `2h07m`.
pub fn duration_text(ms: u64) -> String {
    // alloc: a view's text
    match ms {
        0..1_000 => format!("{ms}ms"),
        1_000..10_000 => format!("{}.{}s", ms / 1_000, ms % 1_000 / 100),
        10_000..60_000 => format!("{}s", ms / 1_000),
        60_000..3_600_000 => format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1_000),
        _ => format!("{}h{:02}m", ms / 3_600_000, ms % 3_600_000 / 60_000),
    }
}

fn decimal(n: impl std::fmt::Display) -> String {
    // alloc: a view's text
    n.to_string()
}

/// A byte count for people, integer arithmetic only: `840 B`, `1.2 KiB`, `3.4 MiB`, `2.0 GiB`.
pub fn bytes_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        // alloc: a view's text
        return format!("{bytes} B");
    }
    let mut scale = 1024u64;
    let mut unit = UNITS[0];
    for (at, name) in UNITS.iter().enumerate().skip(1) {
        let next = 1u64 << (10 * (at + 1));
        if bytes < next {
            break;
        }
        scale = next;
        unit = *name;
    }
    // alloc: a view's text
    format!("{}.{} {unit}", bytes / scale, bytes % scale * 10 / scale)
}

/// The word of a memory's pressure.
fn pressure_word(pressure: Pressure) -> &'static str {
    match pressure {
        Pressure::Calm => "calm",
        Pressure::Rising => "rising",
        Pressure::Squeezed => "squeezed",
        Pressure::Over => "over",
    }
}

/// The fold's task `task` as the source names it.
fn task_line(tasks: &Tasks, task: &now::Task) -> TaskLine {
    let key = tasks.key(task.id);
    TaskLine {
        process: key.map(|(process, _)| decimal(process)),
        task: key.map(|(_, task)| decimal(task)),
        // clone: a view's text
        kind: task.kind.clone(),
        name: task.name.clone(),
    }
}

/// A span of the model as a label: its name, then its item or what it waits on.
fn span_label(span: &now::Span) -> String {
    // clone: a view's text
    let mut out = span.name.clone();
    if let Some(awaits) = &span.awaits {
        out.push_str(" on ");
        out.push_str(awaits);
    } else if let Some(item) = span.item.as_deref().filter(|it| *it != span.name) {
        out.push(' ');
        out.push_str(item);
    }
    out
}

/// A crate's text's first word: `serde` of `serde v1.0.219 (lib)`.
fn crate_name(text: &str) -> &str {
    text.split_whitespace().next().unwrap_or(text)
}

/// The spans that only carry others: the pool's own, a job's and a task's (target `blaze`).
fn wrapper(span: &OpenRow) -> bool {
    span.target.as_deref() == Some(BLAZE) && matches!(span.name.as_str(), "worker" | "job" | "task")
}

/// A unit phase's span, `unit codegen`: its phase.
fn unit_step(span: &OpenRow) -> Option<&str> {
    span.name.strip_prefix("unit ").filter(|it| !it.is_empty())
}

/// `crate › step › pass › item`: the innermost crate (its name), the outermost unit phase (or
/// the task's kind), the innermost span that is not a wrapper and its function; equal
/// neighbours once.
pub fn breadcrumb(chain: &[OpenRow]) -> String {
    let krate = chain.iter().rev().find_map(|it| it.krate.as_deref()).map(crate_name);
    let step = chain.iter().find_map(unit_step).or_else(|| {
        chain
            .iter()
            .rev()
            .find(|it| it.name == "task" && it.target.as_deref() == Some(BLAZE))
            .and_then(|it| it.function.as_deref())
    });
    let inner = chain.iter().rev().find(|it| !wrapper(it) && unit_step(it).is_none());
    let pass = inner.map(|it| it.name.as_str());
    let item = inner.and_then(|it| it.function.as_deref());
    // alloc: a view's text
    let mut out = String::new();
    let mut last: Option<&str> = None;
    for part in [krate, step, pass, item].into_iter().flatten() {
        if last == Some(part) {
            continue;
        }
        if last.is_some() {
            out.push_str(" › ");
        }
        out.push_str(part);
        last = Some(part);
    }
    if out.is_empty()
        && let Some(innermost) = chain.last()
    {
        out.push_str(&innermost.name);
    }
    out
}

/// A span's label: its name, then what it is of, each part once.
pub fn label(span: &OpenRow) -> String {
    // alloc: a view's text
    let mut out = span.name.clone();
    if let Some(awaits) = &span.awaits {
        out.push_str(" on ");
        out.push_str(awaits);
    } else if let Some(function) = span.function.as_deref().filter(|it| *it != span.name) {
        out.push(' ');
        out.push_str(function);
    }
    if let Some(unit) = &span.unit {
        out.push_str(" (");
        out.push_str(unit);
        out.push(')');
    }
    out
}

fn line(span: &OpenRow, now: i64) -> SpanLine {
    let for_ms = ms(elapsed(now, span.start_ns));
    // alloc: a view's text, every field once a view
    SpanLine {
        depth: span.depth,
        name: span.name.clone(),
        target: span.target.clone(),
        krate: span.krate.clone(),
        function: span.function.clone(),
        unit: span.unit.clone(),
        awaits: span.awaits.clone(),
        owner: span.owner.map(decimal),
        task: span.task.map(decimal),
        span: span.span.map(decimal),
        for_ms,
        for_text: duration_text(for_ms),
        label: label(span),
    }
}

fn thread_row(busy: &Busy<'_>, now: i64) -> ThreadRow {
    let for_ms = ms(busy.for_ns);
    ThreadRow {
        // alloc: a view's text
        key: format!("{}:{}", busy.key.process, busy.key.thread),
        process: decimal(busy.key.process),
        thread: decimal(busy.key.thread),
        // alloc: a view's text
        thread_name: busy.thread_name.to_owned(),
        breadcrumb: breadcrumb(busy.chain),
        // alloc: a view's text
        doing: busy.chain.last().map(label).unwrap_or_default(),
        for_ms,
        for_text: duration_text(for_ms),
        // alloc: a view's rows
        stack: busy.chain.iter().map(|it| line(it, now)).collect(),
        task: None,
    }
}

fn tally_line(tally: &now::Tally) -> TallyLine {
    // alloc: a view's text
    let mut text = format!(
        "{} of {} done · {} running · {} ready · {} waiting",
        tally.done, tally.total, tally.running, tally.ready, tally.waiting
    );
    if tally.failed > 0 {
        text.push_str(&format!(" · {} failed", tally.failed));
    }
    TallyLine {
        waiting: u64::from(tally.waiting),
        ready: u64::from(tally.ready),
        running: u64::from(tally.running),
        done: u64::from(tally.done),
        failed: u64::from(tally.failed),
        total: u64::from(tally.total),
        text,
    }
}

fn blocker_row(tasks: &Tasks, blocker: &now::Blocker, now: i64) -> BlockerRow {
    let task = task_line(tasks, &blocker.task);
    let started = tasks.key(blocker.task.id).and_then(|key| tasks.row(key)).and_then(|it| it.start_ns);
    let for_ms = match (blocker.running, started) {
        (true, Some(start)) => ms(elapsed(now, start)),
        _ => blocker.for_ms,
    };
    let for_text = duration_text(for_ms);
    // alloc: a view's text
    let line = format!(
        "{} ({}) · {} {for_text} · holds {}, chain {}",
        task.name,
        task.kind,
        if blocker.running { "running" } else { "ready" },
        blocker.holds,
        blocker.chain
    );
    BlockerRow {
        task,
        running: blocker.running,
        for_ms,
        for_text,
        doing: blocker.span.as_ref().map(span_label),
        holds: u64::from(blocker.holds),
        chain: u64::from(blocker.chain),
        // clone: a view's rows
        behind: blocker.behind.clone(),
        line,
    }
}

fn download_row(download: &now::Download) -> DownloadRow {
    // alloc: a view's text
    let mut text = format!("{} {}", download.what, bytes_text(download.done));
    if let Some(total) = download.total {
        text.push_str(" of ");
        text.push_str(&bytes_text(total));
    }
    if let Some(rate) = download.rate {
        text.push_str(" · ");
        text.push_str(&bytes_text(rate));
        text.push_str("/s");
    }
    DownloadRow {
        // clone: a view's text
        what: download.what.clone(),
        done: download.done,
        total: download.total,
        rate: download.rate,
        idle_ms: download.idle_ms,
        elapsed_ms: download.elapsed_ms,
        eta_ms: download.eta_ms(),
        text,
    }
}

fn memory_line(memory: &now::Memory) -> MemoryLine {
    // alloc: a view's text
    let mut text = format!("holds {}, peak {}", bytes_text(memory.held), bytes_text(memory.peak));
    if memory.limit > 0 {
        text.push_str(" of ");
        text.push_str(&bytes_text(memory.limit));
    }
    MemoryLine {
        held: memory.held,
        peak: memory.peak,
        limit: memory.limit,
        // alloc: a view's text
        pressure: pressure_word(memory.pressure()).to_owned(),
        text,
    }
}

pub fn running_now<S: Stream>(session: &mut Session<S>, now_ns: i64) -> Result<RunningNow, Error> {
    let model = session.look(now_ns)?;
    let running = session.running(now_ns)?;
    let tasks = session.tasks();
    let busy_count = running.busy.len() as u64;
    let quiet_ms = running.quiet_ns.map(ms);
    // alloc: a view's text
    let mut headline = format!("{busy_count} of {} busy · {} idle", running.workers, running.idle);
    if busy_count > 0
        && let Some(quiet) = quiet_ms.filter(|it| *it >= QUIET_WARN_MS)
    {
        headline.push_str(" · slow: no span closed for ");
        headline.push_str(&duration_text(quiet));
    }
    let idle_line = match running.idle {
        // alloc: a view's text
        0 => "none idle".to_owned(),
        // alloc: a view's text
        1 => if running.layered { "1 idle worker".to_owned() } else { "1 idle thread".to_owned() },
        n => format!("{n} idle {}", if running.layered { "workers" } else { "threads" }),
    };
    let now = running.now_ns;
    let busy = running
        .busy
        .iter()
        .map(|it| {
            let mut row = thread_row(it, now);
            row.task = model
                .busy
                .iter()
                .find(|busy| busy.thread == row.key)
                .and_then(|busy| busy.task.as_ref())
                .map(|task| task_line(tasks, task));
            row
        })
        // alloc: a view's rows
        .collect();
    let phase = if tasks.any() { model.phase } else { Phase::Running };
    Ok(RunningNow {
        at_ns: decimal(running.at_ns),
        now_ns: decimal(now),
        headline,
        layered: running.layered,
        workers: running.workers,
        busy_count,
        idle: running.idle,
        idle_line,
        busy,
        // alloc: a view's rows
        internal: running.internal.iter().map(|it| thread_row(it, now)).collect(),
        quiet_ms,
        quiet_text: quiet_ms.map(duration_text),
        // alloc: a view's text
        phase: phase.word().to_owned(),
        // clone: a view's text
        stage: model.stage.clone(),
        tally: tally_line(&model.tally),
        // alloc: a view's rows
        blockers: model.blockers.iter().map(|it| blocker_row(tasks, it, now)).collect(),
        // alloc: a view's rows
        downloads: model.downloads.iter().map(download_row).collect(),
        memory: memory_line(&model.memory),
        slowest: model
            .slowest
            .iter()
            .map(|it| TookRow { task: task_line(tasks, &it.task), ms: it.ms, text: duration_text(it.ms) })
            // alloc: a view's rows
            .collect(),
        slow: model.slow(),
        stalled: model.stalled(),
        still_ms: model.quiet_ms,
        still_text: duration_text(model.quiet_ms),
    })
}

pub fn recent<S: Stream>(session: &Session<S>, now_ns: i64) -> Result<Recent, Error> {
    let (_, at_ns, now) = session.clock(Which::Recent.name(), now_ns)?;
    let spans = session
        .recent()
        .iter()
        .rev()
        .map(|it| {
            let dur_ms = ms(u64::try_from(it.dur_ns).unwrap_or(0));
            let ago_ms = ms(elapsed(now, it.start_ns.saturating_add(it.dur_ns)));
            let open = OpenRow {
                process: it.process,
                thread: it.thread,
                thread_name: String::new(),
                depth: it.depth,
                span: Some(it.span),
                parent: it.parent,
                // clone: a view's text
                name: it.name.clone(),
                target: None,
                start_ns: it.start_ns,
                krate: None,
                // clone: a view's text
                function: it.function.clone(),
                unit: it.unit.clone(),
                awaits: None,
                owner: None,
                task: None,
                read_ns: None,
            };
            Closed {
                // alloc: a view's text
                key: format!("{}:{}", it.process, it.thread),
                // clone: a view's text, every field once a view
                thread_name: it.thread_name.clone(),
                span: decimal(it.span),
                parent: it.parent.map(decimal),
                build: it.build_id.map(decimal),
                name: it.name.clone(),
                krate: it.krate.clone(),
                function: it.function.clone(),
                unit: it.unit.clone(),
                label: label(&open),
                start_ns: decimal(it.start_ns),
                dur_ms,
                dur_text: duration_text(dur_ms),
                ago_ms,
                ago_text: duration_text(ago_ms),
            }
        })
        // alloc: a view's rows
        .collect();
    Ok(Recent { at_ns: decimal(at_ns), spans })
}

pub fn counters<S: Stream>(session: &Session<S>, now_ns: i64) -> Result<Counters, Error> {
    let (_, at_ns, now) = session.clock(Which::Counters.name(), now_ns)?;
    let counters = session
        .counters()
        .values()
        .map(|it| CounterLine {
            process: decimal(it.process),
            // clone: a view's text
            name: it.name.clone(),
            value: it.value,
            // Rust's shortest round-trip form: deterministic, `3` for 3.0.
            value_text: decimal(it.value),
            t_ns: decimal(it.t_ns),
            ago_ms: ms(elapsed(now, it.t_ns)),
            build: it.build_id.map(decimal),
        })
        // alloc: a view's rows
        .collect();
    Ok(Counters { at_ns: decimal(at_ns), counters })
}

pub fn session_view<S: Stream>(session: &Session<S>, now_ns: i64) -> Result<SessionView, Error> {
    let (hello, at_ns, _) = session.clock(Which::Session.name(), now_ns)?;
    let stats = session.stats();
    let (certhash, not_after) = match &session.cert {
        Some(cert) => (cert.certhash.as_str(), cert.not_after_unix_s),
        None => (hello.certhash.as_str(), hello.cert_not_after_unix_s),
    };
    let answer = session.answer.as_ref().map(|it| match (&it.refusal, it.rows) {
        // alloc: a view's text
        (Some(refusal), _) => format!("{}: refused, {refusal}", it.op),
        (None, Some(rows)) => format!("{}: {rows} rows", it.op),
        (None, None) => format!("{}: answered", it.op),
    });
    Ok(SessionView {
        // clone: a view's text, every field once a view
        daemon: hello.daemon.clone(),
        schema: u64::from(hello.schema),
        clock: hello.clock.clone(),
        layers: hello.layers.clone(),
        protocol: hello.protocol.map(u64::from),
        live_every_ms: hello.live_every_ms,
        // alloc: a view's text
        certhash: certhash.to_owned(),
        cert_not_after_unix_s: decimal(not_after),
        at_ns: decimal(at_ns),
        frames: stats.frames,
        skipped: stats.skipped,
        lost: stats.lost,
        other_frames: stats.other_frames,
        refused: stats.refused,
        streams: session.streams().to_vec(),
        refusal: session.refusal.as_ref().map(decimal),
        answer,
    })
}
