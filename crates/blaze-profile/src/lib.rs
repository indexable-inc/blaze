//! Where the time of a build goes.
//!
//! The steps of a build: spans with a `level`, of the client, the daemon and the worker on
//! one clock, a few for each build and always noted. The worker sends its own with its
//! answer, the daemon adds its own, and the client puts them together. `BLAZE_STATS` prints a
//! line and a table of them that adds up to the client's wall time. A step is inside the one
//! above it (client 0, daemon 1, the daemon's question to the worker 2, the worker 3), and
//! the table gives each moment to the deepest step under way.
//!
//! `BLAZE_PROFILE=<dir>` ([`VAR`]) profiles the whole build, in every process of it: the
//! client, the worker, and each build script, proc macro and wrapper they start. Each
//! records, on every thread, the target `blaze` spans (with the CPU time each took itself,
//! not counting the `blaze` spans under it), rust-analyzer's queries, the steps the build
//! says it is at ([`step`]) and the time of each pass of the code generator ([`pass`]). A
//! process writes what it recorded to `<dir>/parts/` as it exits, a worker at the end of
//! each build it answers too ([`write_part`]), and the client puts the parts together at the
//! end of the build ([`finish`]): `trace.json`, a Chrome trace (ui.perfetto.dev opens it),
//! with `phases.tsv` (each span's count, wall and own CPU time), `functions.tsv` (the same
//! for each function compiled and each other span that names what it is of) and
//! `passes.tsv`. Off, nothing is recorded: every callsite is told it is never wanted, and a
//! [`pass`] is `None`.

use std::{
    cell::{Cell, RefCell},
    fmt::Write as _,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rustc_hash::FxHashMap;
use serde_derive::{Deserialize, Serialize};

/// The variable that turns profiling on, naming the directory the profile is written to.
pub const VAR: &str = "BLAZE_PROFILE";

/// One step: what it was, the process and thread, when it began (microseconds since the
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|it| it.into_inner())
}

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

/// Microseconds since the Unix epoch of [`began`], read once, so that every span of the
/// process is on one clock.
fn began_us() -> u64 {
    static AT: OnceLock<u64> = OnceLock::new();
    *AT.get_or_init(|| epoch_us(began()))
}

fn us_of(at: Instant) -> u64 {
    began_us() + at.saturating_duration_since(began()).as_micros() as u64
}

thread_local! {
    static TID: Cell<u64> = const { Cell::new(0) };
}

/// A number for this thread, small and the same for as long as it lives. 0 is the track
/// of the process's [`step`]s.
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

/// The steps this process noted for the build it is in: the client's.
static NOTED: Mutex<Vec<Span>> = Mutex::new(Vec::new());

/// Notes a step from `start` to now.
pub fn note(name: &str, level: u8, start: Instant) {
    lock(&NOTED).push(span(name, Some(level), start, Instant::now()));
}

pub fn note_all(spans: impl IntoIterator<Item = Span>) {
    lock(&NOTED).extend(spans);
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
    *lock(&ARRIVED) = Some(Instant::now());
}

pub fn last_arrival() -> Option<Instant> {
    *lock(&ARRIVED)
}

static ON: AtomicBool = AtomicBool::new(false);
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Whether this process records a profile.
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Where this process writes its profile, when it records one.
pub fn dir() -> Option<PathBuf> {
    lock(&DIR).clone()
}

/// Records a profile into `dir` from now on, until the process ends. What the process
/// starts after this is profiled when it is given [`VAR`] ([`child_env`]).
pub fn start(dir: &Path) {
    *lock(&DIR) = Some(dir.to_owned());
    if !ON.swap(true, Ordering::SeqCst) {
        tracing::callsite::rebuild_interest_cache();
        #[cfg(unix)]
        {
            extern "C" fn at_exit() {
                write_part();
            }
            static AT_EXIT: std::sync::Once = std::sync::Once::new();
            // SAFETY: `at_exit` is a function the process may call as it exits; a panic in it
            // aborts rather than unwinds out of it.
            AT_EXIT.call_once(|| unsafe {
                libc::atexit(at_exit);
            });
        }
    }
}

/// [`start`] with the directory [`VAR`] names, when it names one.
pub fn from_env() {
    if let Some(dir) = std::env::var_os(VAR).filter(|it| !it.is_empty()) {
        start(Path::new(&dir));
    }
}

/// The variable to give a process this one starts, so that it is profiled too.
pub fn child_env() -> Option<(&'static str, PathBuf)> {
    dir().filter(|_| on()).map(|dir| (VAR, dir))
}

/// Has every span of the target `blaze` and every rust-analyzer query recorded once a
/// profile is [`start`]ed, from the variables of the process ([`from_env`]) or later.
pub fn install() {
    _ = tracing::subscriber::set_global_default(Recorder);
    from_env();
}

/// Spans shorter than this are left out of the timeline; their time still counts in the
/// tables.
const SHORTEST_US: u64 = 50;

/// What one thread recorded: kept by [`THREADS`] once the thread has ended, until written.
#[derive(Default)]
struct Recorded {
    tid: u64,
    thread: String,
    events: Vec<Event>,
    phases: FxHashMap<&'static str, Cost>,
    subjects: FxHashMap<(&'static str, Box<str>), Cost>,
    passes: FxHashMap<&'static str, Cost>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Event {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    tid: u64,
    start_us: u64,
    dur_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu_us: Option<u64>,
}

/// How often something ran, its wall time (each run's whole, the runs under it too) and its
/// own CPU time (without the spans of target `blaze` under it), in nanoseconds.
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq)]
struct Cost {
    count: u64,
    wall_ns: u64,
    cpu_ns: u64,
}

impl Cost {
    fn add(&mut self, wall_ns: u64, cpu_ns: u64) {
        self.count += 1;
        self.wall_ns += wall_ns;
        self.cpu_ns += cpu_ns;
    }

    fn merge(&mut self, other: Cost) {
        self.count += other.count;
        self.wall_ns += other.wall_ns;
        self.cpu_ns += other.cpu_ns;
    }
}

/// Every thread's [`Recorded`].
static THREADS: Mutex<Vec<Arc<Mutex<Recorded>>>> = Mutex::new(Vec::new());

/// A target `blaze` span open on this thread: its id, the thread's CPU time when it began,
/// the CPU time of the `blaze` spans under it, and what it is of.
struct Open {
    id: u64,
    cpu_ns: u64,
    under_ns: u64,
    subject: Option<Box<str>>,
}

thread_local! {
    static RECORDED: Arc<Mutex<Recorded>> = {
        let recorded = Arc::new(Mutex::new(Recorded {
            tid: tid(),
            thread: std::thread::current().name().unwrap_or_default().to_owned(),
            ..Recorded::default()
        }));
        lock(&THREADS).push(recorded.clone());
        recorded
    };
    static OPEN: RefCell<Vec<Open>> = const { RefCell::new(Vec::new()) };
    /// For each [`Pass`] under way on this thread, the time of the passes under it.
    static PASSES: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

fn recorded<T>(work: impl FnOnce(&mut Recorded) -> T) -> Option<T> {
    RECORDED.try_with(|it| work(&mut lock(it))).ok()
}

/// The CPU time this thread has had, in nanoseconds.
fn thread_cpu_ns() -> u64 {
    #[cfg(unix)]
    {
        let mut at = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `at` is a timespec to write.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut at) } == 0 {
            return at.tv_sec as u64 * 1_000_000_000 + at.tv_nsec as u64;
        }
    }
    0
}

/// The callsites of the spans [`Recorder`] keeps, by the number in their span ids.
static CALLSITES: Mutex<Vec<&'static tracing::Metadata<'static>>> = Mutex::new(Vec::new());

struct Recorder;

/// A span id holds its callsite in the low byte and its start, in nanoseconds since the
/// process began, above it: nothing is kept per open span but a `blaze` one's CPU time.
const SITE_BITS: u32 = 8;

impl Recorder {
    fn wanted(meta: &tracing::Metadata<'_>) -> bool {
        meta.is_span()
            && *meta.level() <= tracing::Level::INFO
            && (ours(meta) || meta.name().ends_with("_query"))
    }

    fn site(meta: &'static tracing::Metadata<'static>) -> Option<u64> {
        let mut sites = lock(&CALLSITES);
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

fn ours(meta: &tracing::Metadata<'_>) -> bool {
    meta.target().starts_with("blaze")
}

/// The field `subject` of a span: what it is of, a function or a crate.
struct Subject(Option<Box<str>>);

impl tracing::field::Visit for Subject {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "subject" {
            self.0 = Some(value.into());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "subject" {
            self.0 = Some(format!("{value:?}").into());
        }
    }
}

impl tracing::Subscriber for Recorder {
    fn register_callsite(
        &self,
        meta: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        if on() && Self::wanted(meta) && Self::site(meta).is_some() {
            tracing::subscriber::Interest::always()
        } else {
            tracing::subscriber::Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(match on() {
            true => tracing::level_filters::LevelFilter::INFO,
            false => tracing::level_filters::LevelFilter::OFF,
        })
    }

    fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
        on() && Self::wanted(meta)
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let meta = span.metadata();
        let site = Self::site(meta).unwrap_or(0);
        let nanos = began().elapsed().as_nanos() as u64;
        let id = (nanos << SITE_BITS | site).max(1);
        if site != 0 && ours(meta) {
            let mut subject = Subject(None);
            span.record(&mut subject);
            let open = Open { id, cpu_ns: thread_cpu_ns(), under_ns: 0, subject: subject.0 };
            _ = OPEN.try_with(|it| it.borrow_mut().push(open));
        }
        tracing::span::Id::from_u64(id)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}

    fn try_close(&self, id: tracing::span::Id) -> bool {
        let id = id.into_u64();
        let site = (id & ((1 << SITE_BITS) - 1)) as usize;
        let start = began() + Duration::from_nanos(id >> SITE_BITS);
        let end = Instant::now();
        let Some(meta) = site.checked_sub(1).and_then(|at| lock(&CALLSITES).get(at).copied())
        else {
            return true;
        };
        let wall_ns = end.saturating_duration_since(start).as_nanos() as u64;
        // A `blaze` span closed on the thread it was opened on has its own CPU time.
        let own = ours(meta)
            .then(|| {
                OPEN.try_with(|open| {
                    let mut open = open.borrow_mut();
                    let at = open.iter().rposition(|it| it.id == id)?;
                    let closed = open.remove(at);
                    let whole = thread_cpu_ns().saturating_sub(closed.cpu_ns);
                    if let Some(parent) = at.checked_sub(1).and_then(|at| open.get_mut(at)) {
                        parent.under_ns += whole;
                    }
                    Some((whole.saturating_sub(closed.under_ns), closed.subject))
                })
                .ok()
                .flatten()
            })
            .flatten();
        recorded(|it| {
            let (cpu_ns, subject) = match own {
                Some((cpu_ns, subject)) => (Some(cpu_ns), subject),
                None => (None, None),
            };
            if ours(meta) {
                it.phases.entry(meta.name()).or_default().add(wall_ns, cpu_ns.unwrap_or(0));
                if let Some(subject) = &subject {
                    let cost = it.subjects.entry((meta.name(), subject.clone())).or_default();
                    cost.add(wall_ns, cpu_ns.unwrap_or(0));
                }
            }
            if wall_ns / 1000 >= SHORTEST_US {
                let event = Event {
                    name: meta.name().to_owned(),
                    subject: subject.map(Into::into),
                    tid: it.tid,
                    start_us: us_of(start),
                    dur_us: wall_ns / 1000,
                    cpu_us: cpu_ns.map(|it| it / 1000),
                };
                it.events.push(event);
            }
        });
        true
    }
}

/// The variable naming one module-level constant, `crate::module::NAME`, whose every request
/// and every run of its query the profile of a worker started directly keeps on its
/// timeline, however short ([`const_request`], [`const_execution`]).
pub const CONST_VAR: &str = "BLAZE_PROFILE_CONST";

/// The constant [`CONST_VAR`] names, while profiling.
pub fn followed_const() -> Option<&'static str> {
    static FOLLOWED: OnceLock<Option<String>> = OnceLock::new();
    let followed =
        FOLLOWED.get_or_init(|| std::env::var(CONST_VAR).ok().filter(|it| !it.is_empty()));
    followed.as_deref().filter(|_| on())
}

/// Notes a request for the followed constant from `start` to now: the caller's wait for it.
pub fn const_request(start: Instant) {
    noted_const("const request", start);
}

/// Notes a run of the followed constant's query from `start` to now: its evaluation.
pub fn const_execution(start: Instant) {
    noted_const("const eval", start);
}

fn noted_const(name: &'static str, start: Instant) {
    let Some(followed) = followed_const() else { return };
    let wall_ns = start.elapsed().as_nanos() as u64;
    recorded(|it| {
        it.phases.entry(name).or_default().add(wall_ns, 0);
        it.subjects.entry((name, followed.into())).or_default().add(wall_ns, 0);
        let event = Event {
            name: name.to_owned(),
            subject: Some(followed.to_owned()),
            tid: it.tid,
            start_us: us_of(start),
            dur_us: wall_ns / 1000,
            cpu_us: None,
        };
        it.events.push(event);
    });
}

/// Runs the linker `command` as a span `link` of `name`, each attempt a span of its own.
pub fn link_output(
    command: &mut std::process::Command,
    name: &str,
) -> std::io::Result<std::process::Output> {
    let _span = tracing::info_span!(target: "blaze", "link", subject = name).entered();
    command.output()
}

/// A pass of the code generator under way: its time, less that of the passes under it, is
/// added to the pass's row of `passes.tsv` when it is dropped.
pub struct Pass {
    name: &'static str,
    start: Instant,
}

/// Times the pass `name` from now until the result is dropped, when profiling. Passes nest:
/// each is given the time of the ones under it on its thread.
pub fn pass(name: &'static str) -> Option<Pass> {
    if !on() {
        return None;
    }
    _ = PASSES.try_with(|it| it.borrow_mut().push(0));
    Some(Pass { name, start: Instant::now() })
}

impl Drop for Pass {
    fn drop(&mut self) {
        let whole = self.start.elapsed().as_nanos() as u64;
        let under = PASSES
            .try_with(|it| {
                let mut passes = it.borrow_mut();
                let under = passes.pop().unwrap_or(0);
                if let Some(parent) = passes.last_mut() {
                    *parent += whole;
                }
                under
            })
            .unwrap_or(0);
        recorded(|it| it.passes.entry(self.name).or_default().add(whole.saturating_sub(under), 0));
    }
}

/// The step of the build the process is at, from when it began, on a track of its own.
static STEP: Mutex<Option<(String, Instant)>> = Mutex::new(None);
static STEPS: Mutex<Vec<Event>> = Mutex::new(Vec::new());

/// Says the process is at the step `name` (`None`: waiting), done with the one before.
pub fn step(name: Option<String>) {
    if !on() {
        return;
    }
    let now = Instant::now();
    let ended = std::mem::replace(&mut *lock(&STEP), name.map(|it| (it, now)));
    if let Some((name, start)) = ended {
        let dur_us = now.saturating_duration_since(start).as_micros() as u64;
        let event =
            Event { name, subject: None, tid: 0, start_us: us_of(start), dur_us, cpu_us: None };
        lock(&STEPS).push(event);
    }
}

/// What a process recorded, as it writes it to `<dir>/parts/`.
#[derive(Serialize, Deserialize, Default)]
struct Part {
    pid: u32,
    command: String,
    threads: Vec<(u64, String)>,
    events: Vec<Event>,
    phases: Vec<(String, Cost)>,
    subjects: Vec<(String, String, Cost)>,
    passes: Vec<(String, Cost)>,
}

/// Takes what every thread recorded since the last call.
fn take() -> Part {
    let threads = std::mem::take(&mut *lock(&THREADS));
    let mut part = Part {
        pid: std::process::id(),
        command: command(),
        events: std::mem::take(&mut *lock(&STEPS)),
        ..Part::default()
    };
    if !part.events.is_empty() {
        part.threads.push((0, "steps".to_owned()));
    }
    let mut kept = Vec::new();
    for thread in threads {
        let mut it = lock(&thread);
        if !it.events.is_empty() || !it.phases.is_empty() || !it.passes.is_empty() {
            let name = match it.thread.is_empty() {
                true => format!("thread {}", it.tid),
                false => it.thread.clone(),
            };
            part.threads.push((it.tid, name));
        }
        part.events.append(&mut it.events);
        part.phases.extend(it.phases.drain().map(|(name, cost)| (name.to_owned(), cost)));
        let subjects = it.subjects.drain();
        part.subjects.extend(subjects.map(|((name, of), cost)| (name.to_owned(), of.into(), cost)));
        part.passes.extend(it.passes.drain().map(|(name, cost)| (name.to_owned(), cost)));
        drop(it);
        // A thread that has ended holds it no more.
        if Arc::strong_count(&thread) > 1 {
            kept.push(thread);
        }
    }
    lock(&THREADS).extend(kept);
    part
}

/// The command line of the process, short.
fn command() -> String {
    let mut args = std::env::args();
    let program = args.next().unwrap_or_default();
    let program = Path::new(&program).file_name().map(|it| it.to_string_lossy().into_owned());
    let mut line = program.unwrap_or_default();
    for arg in args {
        if line.len() > 160 {
            line.push_str(" ...");
            break;
        }
        line.push(' ');
        line.push_str(&arg);
    }
    line
}

/// Writes what the process recorded since the last call to `<dir>/parts/`, when profiling
/// and it recorded anything. Says on stderr why when it can't.
pub fn write_part() {
    let Some(dir) = dir().filter(|_| on()) else { return };
    let part = take();
    if part.events.is_empty() && part.phases.is_empty() && part.passes.is_empty() {
        return;
    }
    static WRITTEN: AtomicU64 = AtomicU64::new(0);
    let parts = dir.join("parts");
    let n = WRITTEN.fetch_add(1, Ordering::Relaxed);
    let file = parts.join(format!("{}-{n}.json", part.pid));
    let written = std::fs::create_dir_all(&parts).and_then(|()| {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&file)?);
        serde_json::to_writer(&mut out, &part)?;
        out.flush()
    });
    if let Err(why) = written {
        eprintln!("blaze: the profile is not written to {}: {why}", file.display());
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

/// What the client says at the end of a build the daemon made, with all the steps of it:
/// a line and a table with `BLAZE_STATS` or [`VAR`], and with [`VAR`] the profile of every
/// process of the build, put together.
pub fn finish() {
    let stats = std::env::var_os("BLAZE_STATS").is_some();
    let profile = dir().filter(|_| on());
    if !stats && profile.is_none() {
        return;
    }
    let spans = std::mem::take(&mut *lock(&NOTED));
    let (start, end) = (epoch_us(began()), epoch_us(Instant::now()));
    eprint!("{}", table(&spans, start, end));
    if let Some(dir) = profile {
        write_part();
        match put_together(&dir, &spans, start, end) {
            Ok(said) => eprint!("{said}"),
            Err(why) => {
                eprintln!("blaze: the profile in {} is not put together: {why}", dir.display())
            }
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

/// Puts the parts in `<dir>/parts/` and the client's steps `spans` together into the files
/// of the profile, and removes the parts. What it says of them is the result.
fn put_together(dir: &Path, spans: &[Span], start: u64, end: u64) -> std::io::Result<String> {
    let parts_dir = dir.join("parts");
    let mut parts = Vec::new();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&parts_dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|it| it == "json") {
            let part: Part = serde_json::from_slice(&std::fs::read(&path)?)?;
            parts.push(part);
            files.push(path);
        }
    }
    let trace = dir.join("trace.json");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&trace)?);
    chrome(&mut out, spans, &parts, start, end)?;
    out.flush()?;
    let mut phases: FxHashMap<&str, Cost> = FxHashMap::default();
    let mut subjects: FxHashMap<(&str, &str), Cost> = FxHashMap::default();
    let mut passes: FxHashMap<&str, Cost> = FxHashMap::default();
    for part in &parts {
        for (name, cost) in &part.phases {
            phases.entry(name.as_str()).or_default().merge(*cost);
        }
        for (name, of, cost) in &part.subjects {
            subjects.entry((name.as_str(), of.as_str())).or_default().merge(*cost);
        }
        for (name, cost) in &part.passes {
            passes.entry(name.as_str()).or_default().merge(*cost);
        }
    }
    let ms = |ns: u64| ns as f64 / 1e6;
    let mut phases: Vec<_> = phases.into_iter().collect();
    phases.sort_by_key(|it| std::cmp::Reverse(it.1.cpu_ns));
    let mut text = String::from("phase\tcount\twall_ms\tcpu_ms\n");
    for (name, cost) in &phases {
        let _ = writeln!(
            text,
            "{name}\t{}\t{:.3}\t{:.3}",
            cost.count,
            ms(cost.wall_ns),
            ms(cost.cpu_ns)
        );
    }
    std::fs::write(dir.join("phases.tsv"), text)?;
    let mut subjects: Vec<_> = subjects.into_iter().collect();
    subjects.sort_by_key(|it| std::cmp::Reverse(it.1.cpu_ns));
    let mut text = String::from("cpu_ms\twall_ms\tcount\tspan\tof\n");
    for ((name, of), cost) in &subjects {
        let of = of.replace(['\t', '\n'], " ");
        let _ = writeln!(
            text,
            "{:.3}\t{:.3}\t{}\t{name}\t{of}",
            ms(cost.cpu_ns),
            ms(cost.wall_ns),
            cost.count
        );
    }
    std::fs::write(dir.join("functions.tsv"), text)?;
    let mut passes: Vec<_> = passes.into_iter().collect();
    passes.sort_by_key(|it| std::cmp::Reverse(it.1.wall_ns));
    let mut text = String::from("pass\tcount\tms\n");
    for (name, cost) in &passes {
        let _ = writeln!(text, "{name}\t{}\t{:.3}", cost.count, ms(cost.wall_ns));
    }
    std::fs::write(dir.join("passes.tsv"), text)?;
    for file in files {
        _ = std::fs::remove_file(file);
    }
    let processes = parts.iter().map(|it| it.pid).collect::<rustc_hash::FxHashSet<_>>().len();
    let mut said = format!(
        "blaze: profile of {processes} processes in {}: trace.json (ui.perfetto.dev), \
         phases.tsv, functions.tsv, passes.tsv\n",
        dir.display()
    );
    for (name, cost) in phases.iter().take(8) {
        let _ = writeln!(
            said,
            "blaze    {name:<28} {:>9.1} ms CPU {:>9.1} ms wall, {} times",
            ms(cost.cpu_ns),
            ms(cost.wall_ns),
            cost.count
        );
    }
    Ok(said)
}

/// The Chrome trace event format: complete events, with names for the processes and
/// threads. Key `blaze` has the client's wall time and [`breakdown`] of it, for a harness.
fn chrome(
    out: &mut dyn std::io::Write,
    spans: &[Span],
    parts: &[Part],
    start: u64,
    end: u64,
) -> std::io::Result<()> {
    use serde_json::json;
    let mut first = true;
    let mut put = |out: &mut dyn std::io::Write, event: serde_json::Value| {
        let comma = if std::mem::take(&mut first) { "" } else { ",\n" };
        write!(out, "{comma}{event}")
    };
    out.write_all(b"{\"traceEvents\":[\n")?;
    let mut named = rustc_hash::FxHashSet::default();
    for part in parts {
        if named.insert((part.pid, u64::MAX)) {
            put(
                &mut *out,
                json!({"ph": "M", "name": "process_name", "pid": part.pid, "tid": 0,
                "args": {"name": format!("{} ({})", part.command, part.pid)}}),
            )?;
        }
        for (tid, thread) in &part.threads {
            if named.insert((part.pid, *tid)) {
                put(
                    &mut *out,
                    json!({"ph": "M", "name": "thread_name", "pid": part.pid, "tid": tid,
                    "args": {"name": thread}}),
                )?;
            }
        }
        for it in &part.events {
            let mut args = serde_json::Map::new();
            if let Some(cpu) = it.cpu_us {
                args.insert("cpu_us".into(), cpu.into());
            }
            let name = match &it.subject {
                Some(of) => {
                    args.insert("span".into(), it.name.clone().into());
                    of.as_str()
                }
                None => it.name.as_str(),
            };
            put(
                &mut *out,
                json!({"ph": "X", "name": name, "cat": it.name, "pid": part.pid,
                "tid": it.tid, "ts": it.start_us, "dur": it.dur_us, "args": args}),
            )?;
        }
    }
    for it in spans {
        let process = match it.level {
            Some(CLIENT) => "client",
            Some(DAEMON | ASKED) => "daemon",
            _ => "worker",
        };
        if named.insert((it.pid, u64::MAX)) {
            put(
                &mut *out,
                json!({"ph": "M", "name": "process_name", "pid": it.pid, "tid": 0,
                "args": {"name": format!("blaze {process} ({})", it.pid)}}),
            )?;
        }
        // The steps of a process are on a track of their own beside its threads.
        let tid = it.tid + (1 << 32);
        if named.insert((it.pid, tid)) {
            let thread = match it.thread.is_empty() {
                true => format!("{process} steps"),
                false => format!("{} ({process} steps)", it.thread),
            };
            put(
                &mut *out,
                json!({"ph": "M", "name": "thread_name", "pid": it.pid, "tid": tid,
                "args": {"name": thread}}),
            )?;
        }
        put(
            &mut *out,
            json!({"ph": "X", "name": it.name, "cat": "step", "pid": it.pid, "tid": tid,
            "ts": it.start_us, "dur": it.dur_us}),
        )?;
    }
    let rows = breakdown(spans, start, end);
    let breakdown: Vec<_> = rows.iter().map(|(name, us)| json!([name, us])).collect();
    let ours = json!({"start_us": start, "total_us": end - start, "breakdown": breakdown});
    write!(out, "\n],\"displayTimeUnit\":\"ms\",\"blaze\":{ours}}}")
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

    /// One process's spans, steps and passes, written as a part and put together: the
    /// own CPU time of a span leaves out the `blaze` span under it, and a pass the one under
    /// it.
    #[test]
    fn a_profile_is_put_together() {
        let dir = std::env::temp_dir().join(format!("blaze-profile-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        install();
        start(&dir);
        step(Some("Codegen".to_owned()));
        {
            let _outer =
                tracing::info_span!(target: "blaze", "outer", subject = "app::main").entered();
            let spin = |ms| {
                let until = Instant::now() + Duration::from_millis(ms);
                while Instant::now() < until {}
            };
            spin(2);
            let _inner = tracing::info_span!(target: "blaze", "inner").entered();
            spin(8);
        }
        {
            let _whole = pass("compile");
            std::thread::sleep(Duration::from_millis(2));
            let _part = pass("regalloc");
            std::thread::sleep(Duration::from_millis(8));
        }
        step(None);
        write_part();
        let said = put_together(&dir, &[], 0, 1).unwrap();
        assert!(said.contains("profile of 1 processes"), "{said}");
        let phases = std::fs::read_to_string(dir.join("phases.tsv")).unwrap();
        let cpu_ms = |name: &str| -> f64 {
            let row = phases.lines().find(|it| it.starts_with(&format!("{name}\t"))).unwrap();
            row.split('\t').nth(3).unwrap().parse().unwrap()
        };
        assert!(cpu_ms("outer") > 0.5 && cpu_ms("outer") < cpu_ms("inner"), "{phases}");
        let functions = std::fs::read_to_string(dir.join("functions.tsv")).unwrap();
        assert!(functions.lines().any(|it| it.ends_with("\touter\tapp::main")), "{functions}");
        let passes = std::fs::read_to_string(dir.join("passes.tsv")).unwrap();
        let ms = |name: &str| -> f64 {
            let row = passes.lines().find(|it| it.starts_with(&format!("{name}\t"))).unwrap();
            row.split('\t').nth(2).unwrap().parse().unwrap()
        };
        assert!(ms("compile") >= 1.5 && ms("compile") < ms("regalloc"), "{passes}");
        let trace: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("trace.json")).unwrap()).unwrap();
        let names: Vec<&str> = trace["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|it| it["ph"] == "X")
            .filter_map(|it| it["name"].as_str())
            .collect();
        assert!(names.contains(&"app::main") && names.contains(&"inner"), "{names:?}");
        assert!(names.contains(&"Codegen"), "{names:?}");
        assert!(!dir.join("parts").read_dir().unwrap().any(|_| true));
        _ = std::fs::remove_dir_all(&dir);
    }
}
