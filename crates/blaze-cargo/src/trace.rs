//! Where the time of a build goes: spans of the client, the daemon and the worker on one
//! clock, always recorded. The worker sends its own with its answer, the daemon adds its
//! own, and the client puts them together. `BLAZE_STATS` prints a line and a table that
//! adds up to the client's wall time; `BLAZE_TRACE=<path>` writes them as a Chrome trace
//! (ui.perfetto.dev opens it).
//!
//! A span with a `level` is a step of the build: one level is inside the one above it
//! (client 0, daemon 1, the daemon's question to the worker 2, the worker 3), and the table
//! gives each moment to the deepest step under way. One without is detail for the trace:
//! a rust-analyzer query or a step of code generation, from [`install`].
//!
//! A worker started directly (not by the daemon) with `BLAZE_TRACE=<path>` writes its first
//! build, the one before it starts watching, as a trace of its own ([`start_cold`]).
//! `BLAZE_TRACE_CONST=crate::module::NAME` adds each request for that module's constant and
//! each run of its query, however short.

use std::{
    cell::Cell,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde_derive::{Deserialize, Serialize};

/// One span: what it was, the process and thread, when it began (microseconds since the
/// Unix epoch) and how long it took.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Span {
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "p")]
    pub pid: u32,
    #[serde(rename = "t")]
    pub tid: u64,
    #[serde(rename = "h", default, skip_serializing_if = "String::is_empty")]
    pub thread: String,
    #[serde(rename = "s")]
    pub start_us: u64,
    #[serde(rename = "d")]
    pub dur_us: u64,
    #[serde(rename = "l", default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
}

pub const CLIENT: u8 = 0;
pub const DAEMON: u8 = 1;
pub const ASKED: u8 = 2;
pub const WORKER: u8 = 3;

/// Microseconds since the Unix epoch of `at`: the wall clock now, less how long ago it was.
pub fn epoch_us(at: Instant) -> u64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    now.saturating_sub(at.elapsed()).as_micros() as u64
}

/// When this process began, as near as it can tell: the first call, which `main` makes.
pub fn began() -> Instant {
    static BEGAN: OnceLock<Instant> = OnceLock::new();
    *BEGAN.get_or_init(Instant::now)
}

thread_local! {
    static TID: Cell<u64> = const { Cell::new(0) };
}

/// A number for this thread, small and the same for as long as it lives.
fn tid() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    TID.with(|it| {
        if it.get() == 0 {
            it.set(NEXT.fetch_add(1, Ordering::Relaxed));
        }
        it.get()
    })
}

/// The span from `start` to `end` on this thread.
pub fn span(name: impl Into<String>, level: Option<u8>, start: Instant, end: Instant) -> Span {
    let thread = std::thread::current().name().unwrap_or_default().to_owned();
    Span {
        name: name.into(),
        pid: std::process::id(),
        tid: tid(),
        thread,
        start_us: epoch_us(start),
        dur_us: end.saturating_duration_since(start).as_micros() as u64,
        level,
    }
}

/// The spans this process noted for the build it is in: the client's steps.
static NOTED: Mutex<Vec<Span>> = Mutex::new(Vec::new());

fn noted() -> std::sync::MutexGuard<'static, Vec<Span>> {
    NOTED.lock().unwrap_or_else(|it| it.into_inner())
}

/// Notes a step from `start` to now.
pub fn note(name: &str, level: u8, start: Instant) {
    noted().push(span(name, Some(level), start, Instant::now()));
}

pub fn note_all(spans: impl IntoIterator<Item = Span>) {
    noted().extend(spans);
}

/// What the answer line of a worker has after the spans it sends.
const SPANS: &str = " spans=";

/// The answer `line` with `spans` after it, for the daemon to hand on.
pub fn with_spans(line: String, spans: &[Span]) -> String {
    match serde_json::to_string(spans) {
        Ok(json) => format!("{line}{SPANS}{json}"),
        Err(_) => line,
    }
}

/// A worker's answer and the spans it sent.
pub fn split(line: &str) -> (&str, Vec<Span>) {
    match line.find(SPANS) {
        Some(at) => {
            let spans = serde_json::from_str(&line[at + SPANS.len()..]).unwrap_or_default();
            (&line[..at], spans)
        }
        None => (line, Vec::new()),
    }
}

/// The answer without its spans, for a person or an editor.
pub fn without_spans(line: &str) -> &str {
    line.find(SPANS).map_or(line, |at| &line[..at])
}

/// When the worker last read a request.
static ARRIVED: Mutex<Option<Instant>> = Mutex::new(None);

pub fn arrived() {
    *ARRIVED.lock().unwrap_or_else(|it| it.into_inner()) = Some(Instant::now());
}

pub fn last_arrival() -> Option<Instant> {
    *ARRIVED.lock().unwrap_or_else(|it| it.into_inner())
}

/// Detail spans shorter than this are not kept.
const SHORTEST_US: u64 = 200;
/// Nor more than this many between two builds.
const MOST: usize = 4000;

static DETAIL: Mutex<Vec<Span>> = Mutex::new(Vec::new());
static DROPPED: AtomicUsize = AtomicUsize::new(0);

/// The first build of a worker started directly, traced: where it is written, the constant
/// followed, and the spans kept apart from [`DETAIL`].
struct Cold {
    path: PathBuf,
    selected: Option<String>,
    active: AtomicBool,
    batch: AtomicUsize,
    events: Mutex<ColdEvents>,
}

/// Each kind has room of its own, so a full buffer of queries drops none of these, nor do
/// many requests for the constant drop its runs.
#[derive(Default)]
struct ColdEvents {
    spans: [Vec<Span>; 3],
    dropped: [usize; 3],
}

#[derive(Clone, Copy)]
enum ColdEvent {
    Parallel,
    Request,
    Execution,
}

static COLD: OnceLock<Cold> = OnceLock::new();

/// Traces the first build of a worker started directly, when `BLAZE_TRACE` says where to;
/// the constant `BLAZE_TRACE_CONST` names, if any, for the caller to follow.
pub fn start_cold() -> Option<&'static str> {
    let path = std::env::var_os("BLAZE_TRACE").filter(|it| !it.is_empty())?;
    let selected = std::env::var("BLAZE_TRACE_CONST").ok().filter(|it| !it.is_empty());
    _ = COLD.set(Cold {
        path: path.into(),
        selected,
        active: AtomicBool::new(true),
        batch: AtomicUsize::new(1),
        events: Mutex::default(),
    });
    COLD.get()?.selected.as_deref()
}

fn cold() -> Option<&'static Cold> {
    COLD.get().filter(|it| it.active.load(Ordering::Relaxed))
}

impl Cold {
    fn record(&self, kind: ColdEvent, start: Instant, name: impl FnOnce() -> String) {
        let end = Instant::now();
        let mut events = self.events.lock().unwrap_or_else(|it| it.into_inner());
        let index = kind as usize;
        if events.spans[index].len() < MOST {
            events.spans[index].push(span(name(), None, start, end));
        } else {
            events.dropped[index] += 1;
        }
    }
}

/// The number of a parallel dispatch and how many items it has, for each of its threads.
pub fn cold_batch(count: usize) -> Option<(usize, usize)> {
    Some((cold()?.batch.fetch_add(1, Ordering::Relaxed), count))
}

/// A span of a parallel dispatch of the first build on this thread, until it is dropped.
pub fn cold_span(name: &'static str, batch: Option<(usize, usize)>) -> Option<impl Drop> {
    struct Phase(&'static Cold, &'static str, usize, usize, Instant);
    impl Drop for Phase {
        fn drop(&mut self) {
            let Self(cold, name, id, count, start) = *self;
            cold.record(ColdEvent::Parallel, start, || {
                format!("parallel {id} {name} ({count} items)")
            });
        }
    }
    let (id, count) = batch?;
    Some(Phase(cold()?, name, id, count, Instant::now()))
}

/// Runs the linker `command`, and in the first build's trace a span for it, each attempt of
/// its own, saying how a failed one ended.
pub fn link_output(
    command: &mut std::process::Command,
    name: &str,
) -> std::io::Result<std::process::Output> {
    let start = cold().map(|_| Instant::now());
    let output = command.output();
    if let Some(start) = start {
        let name = match &output {
            Ok(it) if it.status.success() => name.to_owned(),
            Ok(it) => format!("{name} ({})", it.status),
            Err(e) => format!("{name} ({e})"),
        };
        noted().push(span(name, Some(DAEMON), start, Instant::now()));
    }
    output
}

/// The constant the first build follows, while it is traced.
pub fn cold_const() -> Option<&'static str> {
    cold()?.selected.as_deref()
}

/// Notes a request for the followed constant from `start` to now: the caller's wait for it.
pub fn const_request(start: Instant) {
    record_const(ColdEvent::Request, "request", start);
}

/// Notes a run of the followed constant's query from `start` to now: its evaluation.
pub fn const_execution(start: Instant) {
    record_const(ColdEvent::Execution, "eval", start);
}

fn record_const(kind: ColdEvent, name: &str, start: Instant) {
    if let Some(cold) = cold()
        && let Some(selected) = &cold.selected
    {
        cold.record(kind, start, || format!("const {name} {selected}"));
    }
}

/// Writes the first build's trace, once: when the worker starts watching or its one build
/// ends, `outcome` saying which.
pub fn finish_cold(outcome: &str) {
    let Some(cold) = COLD.get() else { return };
    if !cold.active.swap(false, Ordering::Relaxed) {
        return;
    }
    let root = span("worker startup", Some(CLIENT), began(), Instant::now());
    let (start, end) = (root.start_us, root.start_us + root.dur_us);
    let mut spans = vec![root];
    spans.append(&mut *noted());
    let detail_dropped = DROPPED.load(Ordering::Relaxed);
    spans.extend(take_detail(began()));
    let events = std::mem::take(&mut *cold.events.lock().unwrap_or_else(|it| it.into_inner()));
    spans.extend(events.spans.into_iter().flatten());
    let [parallel, requests, executions] = events.dropped;
    let info = serde_json::json!({"outcome": outcome, "constFilter": cold.selected, "dropped": {
        "detail": detail_dropped, "parallel": parallel, "constRequests": requests,
        "constExecutions": executions}});
    if let Err(why) = write_trace(&cold.path, &spans, start, end, Some(info)) {
        eprintln!("blaze: the trace is not written to {}: {why:#}", cold.path.display());
    }
}

/// The detail spans that ended since `since`, taken; older ones are dropped.
pub fn take_detail(since: Instant) -> Vec<Span> {
    let since = epoch_us(since);
    let mut taken = std::mem::take(&mut *DETAIL.lock().unwrap_or_else(|it| it.into_inner()));
    DROPPED.store(0, Ordering::Relaxed);
    taken.retain(|it| it.start_us + it.dur_us >= since);
    taken
}

/// Records the spans of rust-analyzer's queries (the `*_query` spans of the queries that
/// ran) and of code generation (target `blaze`), each as long as [`SHORTEST_US`] or longer.
/// Every other callsite is told it is never wanted, so it costs what a disabled one does.
pub fn install() {
    _ = tracing::subscriber::set_global_default(Recorder);
}

/// The callsites of the spans [`Recorder`] keeps, by the number in their span ids.
static CALLSITES: Mutex<Vec<&'static tracing::Metadata<'static>>> = Mutex::new(Vec::new());

struct Recorder;

/// A span id holds its callsite in the low byte and its start, in nanoseconds since the
/// process began, above it: nothing is kept per open span.
const SITE_BITS: u32 = 8;

impl Recorder {
    fn wanted(meta: &tracing::Metadata<'_>) -> bool {
        meta.is_span()
            && *meta.level() <= tracing::Level::INFO
            && (meta.target().starts_with("blaze") || meta.name().ends_with("_query"))
    }

    fn site(meta: &'static tracing::Metadata<'static>) -> Option<u64> {
        let mut sites = CALLSITES.lock().unwrap_or_else(|it| it.into_inner());
        let found = sites.iter().position(|it| std::ptr::eq(*it, meta));
        let at = match found {
            Some(at) => at,
            None if sites.len() < (1 << SITE_BITS) - 1 => {
                sites.push(meta);
                sites.len() - 1
            }
            None => return None,
        };
        Some(at as u64 + 1)
    }
}

impl tracing::Subscriber for Recorder {
    fn register_callsite(
        &self,
        meta: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        if Self::wanted(meta) && Self::site(meta).is_some() {
            tracing::subscriber::Interest::always()
        } else {
            tracing::subscriber::Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::INFO)
    }

    fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
        Self::wanted(meta)
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let site = Self::site(span.metadata()).unwrap_or(0);
        let nanos = began().elapsed().as_nanos() as u64;
        tracing::span::Id::from_u64((nanos << SITE_BITS | site).max(1))
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}

    fn try_close(&self, id: tracing::span::Id) -> bool {
        let id = id.into_u64();
        let site = (id & ((1 << SITE_BITS) - 1)) as usize;
        let start = began() + std::time::Duration::from_nanos(id >> SITE_BITS);
        let end = Instant::now();
        if site == 0 || end.saturating_duration_since(start).as_micros() < SHORTEST_US as u128 {
            return true;
        }
        let Some(meta) =
            CALLSITES.lock().unwrap_or_else(|it| it.into_inner()).get(site - 1).copied()
        else {
            return true;
        };
        let mut detail = DETAIL.lock().unwrap_or_else(|it| it.into_inner());
        if detail.len() < MOST {
            detail.push(span(meta.name(), None, start, end));
        } else {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        true
    }
}

/// Where the time from `start_us` to `end_us` went: each moment is the deepest step's under
/// way then, and a moment no step has is unaccounted. In the order the steps first had
/// time; the rows add up to the whole.
pub fn breakdown(spans: &[Span], start_us: u64, end_us: u64) -> Vec<(String, u64)> {
    let steps: Vec<&Span> = spans.iter().filter(|it| it.level.is_some()).collect();
    let mut cuts: Vec<u64> = vec![start_us, end_us];
    for it in &steps {
        cuts.push(it.start_us.clamp(start_us, end_us));
        cuts.push((it.start_us + it.dur_us).clamp(start_us, end_us));
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut rows: Vec<(String, u64)> = Vec::new();
    for pair in cuts.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let covers = |it: &&&Span| it.start_us <= from && it.start_us + it.dur_us >= to;
        // The deepest level that is inside one of each level above it; at a level, the step
        // that began last.
        let mut owner: Option<&Span> = None;
        for level in 0..=u8::MAX {
            let here = steps.iter().filter(|it| it.level == Some(level)).filter(covers);
            match here.max_by_key(|it| it.start_us) {
                Some(found) => owner = Some(found),
                None => break,
            }
        }
        let name = owner.map_or("unaccounted", |it| &it.name);
        match rows.iter_mut().find(|it| it.0 == name) {
            Some(row) => row.1 += to - from,
            None => rows.push((name.to_owned(), to - from)),
        }
    }
    rows
}

/// What the client says at the end of a build the daemon made, with all the spans of it:
/// a line and a table with `BLAZE_STATS`, a Chrome trace with `BLAZE_TRACE`.
pub fn finish() {
    let stats = std::env::var_os("BLAZE_STATS").is_some();
    let trace = std::env::var_os("BLAZE_TRACE").filter(|it| !it.is_empty());
    if !stats && trace.is_none() {
        return;
    }
    let spans = std::mem::take(&mut *noted());
    let (start, end) = (epoch_us(began()), epoch_us(Instant::now()));
    if stats {
        eprint!("{}", table(&spans, start, end));
    }
    if let Some(trace) = trace {
        let written = write_trace(Path::new(&trace), &spans, start, end, None);
        if let Err(why) = written {
            eprintln!(
                "blaze: the trace is not written to {}: {why:#}",
                Path::new(&trace).display()
            );
        }
    }
}

/// `blaze time: 402.1 ms: check 285.5 (71%), ...` and the table of [`breakdown`].
pub fn table(spans: &[Span], start: u64, end: u64) -> String {
    let rows = breakdown(spans, start, end);
    let total = (end - start).max(1);
    let ms = |us: u64| us as f64 / 1000.0;
    let pct = |us: u64| us as f64 * 100.0 / total as f64;
    let mut top: Vec<&(String, u64)> = rows.iter().collect();
    top.sort_by_key(|it| std::cmp::Reverse(it.1));
    let top: Vec<String> = top
        .iter()
        .take(6)
        .map(|(name, us)| format!("{name} {:.1} ({:.0}%)", ms(*us), pct(*us)))
        .collect();
    let mut out = format!("blaze time: {:.1} ms: {}\n", ms(total), top.join(", "));
    out.push_str("blaze where the time went:\n");
    for (name, us) in &rows {
        let _ = writeln!(out, "blaze    {name:<28} {:>9.1} ms {:>5.1}%", ms(*us), pct(*us));
    }
    let _ = writeln!(out, "blaze    {:<28} {:>9.1} ms {:>5.1}%", "total", ms(total), 100.0);
    out
}

/// Writes `spans` as a Chrome trace: to `path` when it ends in `.json`, else to a file of
/// its own in the directory `path`. `startup` is what [`finish_cold`] says of the first build.
fn write_trace(
    path: &Path,
    spans: &[Span],
    start: u64,
    end: u64,
    startup: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    let path = if path.extension().is_some_and(|it| it == "json") {
        path.to_owned()
    } else {
        std::fs::create_dir_all(path)?;
        let now = epoch_us(Instant::now());
        path.join(format!("blaze-{now}-{}.json", std::process::id()))
    };
    std::fs::write(&path, chrome(spans, start, end, startup))?;
    eprintln!("blaze: trace written to {}", path.display());
    Ok(())
}

/// The Chrome trace event format: complete events, with names for the processes and
/// threads. Key `blaze` has the client's wall time and [`breakdown`] of it, for a harness,
/// and, for a worker's first build, `startup`: all of its steps are the worker's.
fn chrome(spans: &[Span], start: u64, end: u64, startup: Option<serde_json::Value>) -> String {
    use serde_json::json;
    let mut events = Vec::new();
    let mut named = rustc_hash::FxHashSet::default();
    for it in spans {
        let process = match it.level {
            Some(_) if startup.is_some() => Some("worker"),
            Some(CLIENT) => Some("client"),
            Some(DAEMON | ASKED) => Some("daemon"),
            Some(_) => Some("worker"),
            None => None,
        };
        if let Some(process) = process
            && named.insert((it.pid, 0))
        {
            events.push(json!({"ph": "M", "name": "process_name", "pid": it.pid, "tid": 0,
                "args": {"name": format!("blaze {process} ({})", it.pid)}}));
        }
        if named.insert((it.pid, it.tid)) {
            let thread =
                if it.thread.is_empty() { format!("thread {}", it.tid) } else { it.thread.clone() };
            events.push(json!({"ph": "M", "name": "thread_name", "pid": it.pid, "tid": it.tid,
                "args": {"name": thread}}));
        }
        let cat = if it.level.is_some() { "step" } else { "detail" };
        events.push(json!({"ph": "X", "name": it.name, "cat": cat, "pid": it.pid, "tid": it.tid,
            "ts": it.start_us, "dur": it.dur_us}));
    }
    let rows = breakdown(spans, start, end);
    let breakdown: Vec<_> = rows.iter().map(|(name, us)| json!([name, us])).collect();
    let mut ours = json!({"start_us": start, "total_us": end - start, "breakdown": breakdown});
    if let Some(startup) = startup {
        ours["startup"] = startup;
    }
    json!({"traceEvents": events, "displayTimeUnit": "ms", "blaze": ours}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(name: &str, level: u8, start_us: u64, dur_us: u64) -> Span {
        Span {
            name: name.to_owned(),
            pid: 1,
            tid: 1,
            thread: String::new(),
            start_us,
            dur_us,
            level: Some(level),
        }
    }

    #[test]
    fn the_rows_add_up_to_the_whole() {
        let spans = [
            at("client start", CLIENT, 0, 10),
            at("client↔daemon", CLIENT, 10, 80),
            at("daemon", DAEMON, 15, 70),
            at("daemon↔worker", ASKED, 20, 60),
            at("check", WORKER, 25, 40),
            // A worker step outside of any question of the daemon's is not the build's.
            at("stray", WORKER, 92, 5),
            at("client write state", CLIENT, 95, 3),
        ];
        let rows = breakdown(&spans, 0, 100);
        let want = [
            ("client start", 10),
            ("client↔daemon", 10),
            ("daemon", 10),
            ("daemon↔worker", 20),
            ("check", 40),
            ("unaccounted", 7),
            ("client write state", 3),
        ];
        assert_eq!(rows, want.map(|(n, us)| (n.to_owned(), us)));
        assert_eq!(rows.iter().map(|it| it.1).sum::<u64>(), 100);
        let joined = with_spans("ok patch=x ms=3".to_owned(), &spans);
        let (line, back) = split(&joined);
        assert_eq!((line, back.as_slice()), ("ok patch=x ms=3", &spans[..]));
    }

    #[test]
    fn a_first_build_trace_is_the_workers_and_its_detail_is_not_a_step() {
        use serde_json::json;
        let mut query = at("const_eval_query", CLIENT, 20, 60);
        query.level = None;
        let spans = [at("worker startup", CLIENT, 0, 100), query, at("link", DAEMON, 80, 20)];
        let startup = Some(json!({"outcome": "watch ready"}));
        let trace: serde_json::Value =
            serde_json::from_str(&chrome(&spans, 0, 100, startup)).unwrap();
        assert_eq!(trace["blaze"]["breakdown"], json!([["worker startup", 80], ["link", 20]]));
        assert_eq!(trace["blaze"]["startup"]["outcome"], "watch ready");
        assert_eq!(trace["traceEvents"][0]["args"]["name"], "blaze worker (1)");
    }

    #[test]
    fn a_full_buffer_of_requests_drops_no_evaluation() {
        let cold = Cold {
            path: PathBuf::new(),
            selected: None,
            active: AtomicBool::new(true),
            batch: AtomicUsize::new(1),
            events: Mutex::default(),
        };
        let request = ColdEvent::Request as usize;
        cold.events.lock().unwrap().spans[request].resize(MOST, at("const request", CLIENT, 0, 10));
        cold.record(ColdEvent::Request, Instant::now(), || unreachable!("a dropped span's name"));
        cold.record(ColdEvent::Execution, Instant::now(), || "const eval".into());
        let kept = cold.events.into_inner().unwrap();
        assert_eq!(kept.dropped[request], 1);
        assert_eq!(kept.spans[ColdEvent::Execution as usize][0].name, "const eval");
    }
}
