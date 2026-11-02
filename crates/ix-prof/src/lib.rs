//! ix-prof: a profiler for any Rust program, in the spirit of Go's pprof.
//!
//! ```ignore
//! #[global_allocator] // optional: allocation counts and the heap profile
//! static ALLOC: ix_prof::alloc::Counting<std::alloc::System> =
//!     ix_prof::alloc::Counting::new(std::alloc::System);
//!
//! fn main() {
//!     let _prof = ix_prof::start_from_env().expect("ix-prof"); // IX_PROF=<dir> turns it on
//!     tracing_subscriber::registry().with(ix_prof::layer()).init(); // optional: spans
//!     // ...
//! }
//! ```
//!
//! On, it records CPU samples of every thread (no registration), the span tree of `tracing`
//! spans with each span's own CPU time and allocations, waits (spans with a `wait` field),
//! sampled allocations by stack with live and total bytes, and heap counters; when the guard
//! drops it writes `<dir>/<program>-<pid>.ixprof`. Off (no `IX_PROF`, no `IX_PROF_HTTP`),
//! nothing runs and each span callback and allocation costs one relaxed load.
//!
//! The format and every interface: docs/profiling.md and docs/profiling-any-program.md.
//! The aggregation is [`core::Recorder`], a sans-IO state machine; everything else here is a
//! driver (the sampler thread, the layer, the allocator, the ticker, the HTTP server).

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use parking_lot::Mutex;

pub mod alloc;
pub mod chrome;
pub mod clock;
pub mod core;
#[cfg(feature = "http")]
pub mod http;
pub mod ixprof;
pub mod json;
#[cfg(feature = "layer")]
pub mod layer;
pub mod live;
pub mod pprof;
pub mod profile;
pub mod report;
#[cfg(feature = "sampler")]
pub mod sampler;
pub mod symbols;
#[cfg(test)]
mod tests;
pub mod view;

#[cfg(feature = "layer")]
pub use layer::layer;
pub use profile::Profile;
pub use view::{Kind, View};

/// Turns the profiler on, naming the directory the profile is written to.
pub const VAR: &str = "IX_PROF";
/// CPU samples a second a thread; 0 turns the sampler off.
pub const VAR_HZ: &str = "IX_PROF_HZ";
/// Mean bytes between allocation samples, or `off`.
pub const VAR_ALLOC: &str = "IX_PROF_ALLOC";
/// Address of the `/debug/pprof` endpoint (feature `http`).
pub const VAR_HTTP: &str = "IX_PROF_HTTP";
/// Bytes of encoded live batches kept for resume and window snapshots.
pub const VAR_RETAIN: &str = "IX_PROF_RETAIN";

/// Go's default: 100 Hz; 99 so the sampler does not beat with periodic work.
pub const DEFAULT_HZ: u32 = 99;
/// Go's `MemProfileRate`.
pub const DEFAULT_ALLOC_SAMPLE: u64 = 512 * 1024;

#[derive(Clone, Debug)]
pub struct Config {
    /// Where the profile is written when the guard drops; `None`: not written.
    pub dir: Option<PathBuf>,
    /// CPU samples a second a thread; `None`: no sampler.
    pub cpu_hz: Option<u32>,
    /// Mean bytes between allocation samples; `None`: counts only (if the counting allocator
    /// is installed).
    pub alloc_sample_bytes: Option<u64>,
    /// `/debug/pprof` on this address (feature `http`).
    pub http: Option<String>,
    pub retain_bytes: usize,
    /// How often the ticker drains the allocator, writes counters and makes live batches.
    pub tick: Duration,
    /// The program's name in the manifest; the executable's file name by default.
    pub program: Option<String>,
    /// Free labels for the manifest.
    pub labels: Vec<(String, String)>,
    pub limits: core::Limits,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            dir: None,
            cpu_hz: Some(DEFAULT_HZ),
            alloc_sample_bytes: Some(DEFAULT_ALLOC_SAMPLE),
            http: None,
            retain_bytes: 64 << 20,
            tick: Duration::from_millis(250),
            program: None,
            labels: Vec::new(),
            limits: core::Limits::default(),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// A variable has a value ix-prof cannot read.
    Var { var: &'static str, value: String, why: &'static str },
    /// The sampler cannot run here, and the config asks for it.
    Sampler(String),
    AlreadyStarted,
    Http(String),
    Write { path: PathBuf, why: String },
    Format(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Var { var, value, why } => write!(f, "ix-prof: {var}={value:?}: {why}"),
            Error::Sampler(why) => write!(
                f,
                "ix-prof: no CPU sampler: {why}; set {VAR_HZ}=0 to profile without CPU samples"
            ),
            Error::AlreadyStarted => write!(f, "ix-prof: already started in this process"),
            Error::Http(why) => write!(f, "ix-prof: the /debug/pprof endpoint: {why}"),
            Error::Write { path, why } => write!(f, "ix-prof: writing {}: {why}", path.display()),
            Error::Format(why) => write!(f, "ix-prof: {why}"),
        }
    }
}

impl std::error::Error for Error {}

impl Config {
    /// The config `IX_PROF*` give, or `None` when neither `IX_PROF` nor `IX_PROF_HTTP` is set.
    pub fn from_env() -> Result<Option<Config>, Error> {
        let var = |name: &str| std::env::var(name).ok().filter(|it| !it.is_empty());
        let dir = var(VAR).map(PathBuf::from);
        let http = var(VAR_HTTP);
        if dir.is_none() && http.is_none() {
            return Ok(None);
        }
        let mut c = Config { dir, http, ..Config::default() };
        if let Some(hz) = var(VAR_HZ) {
            let n: u32 = hz.parse().map_err(|_| Error::Var {
                var: VAR_HZ,
                value: hz.clone(),
                why: "not a whole number of samples a second",
            })?;
            c.cpu_hz = (n > 0).then_some(n);
        }
        if let Some(a) = var(VAR_ALLOC) {
            c.alloc_sample_bytes = match a.as_str() {
                "off" | "0" => None,
                _ => Some(a.parse().map_err(|_| Error::Var {
                    var: VAR_ALLOC,
                    value: a.clone(),
                    why: "neither bytes nor `off`",
                })?),
            };
        }
        if let Some(r) = var(VAR_RETAIN) {
            c.retain_bytes = r.parse().map_err(|_| Error::Var {
                var: VAR_RETAIN,
                value: r.clone(),
                why: "not a number of bytes",
            })?;
        }
        Ok(Some(c))
    }
}

pub(crate) struct Session {
    pub(crate) config: Config,
    pub(crate) recorder: Mutex<core::Recorder>,
    pub(crate) log: Mutex<live::Log>,
    pub(crate) live_wanted: AtomicBool,
    stop: Arc<AtomicBool>,
    #[cfg(feature = "sampler")]
    sampler: Mutex<Option<sampler::Sampler>>,
}

static RECORDING: AtomicBool = AtomicBool::new(false);
static SESSION: Mutex<Option<Arc<Session>>> = Mutex::new(None);

/// Whether the profiler is on: what every driver checks first.
#[inline]
pub fn recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

pub(crate) fn session() -> Option<Arc<Session>> {
    SESSION.lock().clone()
}

/// Runs `f` on the recorder, when on.
pub(crate) fn with_recorder(f: impl FnOnce(&mut core::Recorder)) {
    if let Some(s) = session() {
        f(&mut s.recorder.lock());
    }
}

/// Starts the profiler; it stops, and writes its profile, when the guard drops.
pub fn start(config: Config) -> Result<Guard, Error> {
    let mut slot = SESSION.lock();
    if slot.is_some() {
        return Err(Error::AlreadyStarted);
    }
    let pid = std::process::id();
    let program = config.program.clone().unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|it| it.file_name().map(|it| it.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "program".to_owned())
    });
    let start_ns = clock::now_ns();
    let start_unix_ns = clock::unix_ns();
    let alloc_on = alloc::turn_on(config.alloc_sample_bytes);
    let meta = profile::Meta {
        program: program.clone(),
        producer: concat!("ix-prof ", env!("CARGO_PKG_VERSION")).to_owned(),
        clock: clock::CLOCK.to_owned(),
        start_ns,
        end_ns: start_ns,
        start_unix_ns,
        arch: std::env::consts::ARCH.to_owned(),
        os: std::env::consts::OS.to_owned(),
        cpu_period_ns: config.cpu_hz.map(|hz| 1_000_000_000 / u64::from(hz.max(1))),
        cpu_sampler: "off: not asked for".to_owned(),
        alloc_sample_bytes: if alloc_on { config.alloc_sample_bytes } else { None },
        alloc_counter: if alloc_on {
            "on".to_owned()
        } else {
            "off: no ix_prof::alloc::Counting global allocator in this program".to_owned()
        },
        symbolized: true,
        labels: config.labels.iter().cloned().collect(),
    };
    let mut recorder = core::Recorder::new(pid, &program, meta, config.limits.clone());
    let session_id = live::session_id(pid, start_ns, start_unix_ns);
    let log = live::Log::new(session_id, config.retain_bytes);
    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(feature = "sampler")]
    let sampler = match config.cpu_hz {
        Some(hz) => {
            let sink: sampler::Sink = Arc::new(|samples: &[sampler::Sample], names: &[(u64, String)]| {
                alloc::quiet(|| {
                    with_recorder(|r| {
                        for (tid, name) in names {
                            r.thread(*tid, Some(name));
                        }
                        for s in samples {
                            let span_id = r.innermost(s.tid);
                            r.cpu_sample(core::RawSample {
                                thread: s.tid,
                                t_ns: s.t_ns,
                                pcs: s.pcs().to_vec(),
                                span_id,
                                on_cpu: s.on_cpu,
                                weight_ns: s.weight_ns,
                            });
                        }
                    });
                });
            });
            match sampler::start(hz, sink) {
                Ok(it) => {
                    recorder.meta_mut().cpu_sampler = format!("on: {hz} Hz, {}", it.how);
                    Some(it)
                }
                Err(why) => {
                    alloc::turn_off();
                    return Err(Error::Sampler(why.0));
                }
            }
        }
        None => None,
    };
    #[cfg(not(feature = "sampler"))]
    if config.cpu_hz.is_some() {
        alloc::turn_off();
        return Err(Error::Sampler("ix-prof was built without its `sampler` feature".to_owned()));
    }
    let serving = config.http.is_some();
    let session = Arc::new(Session {
        config,
        recorder: Mutex::new(recorder),
        log: Mutex::new(log),
        // With an endpoint, batches are kept from the start so a client can resume and backfill.
        live_wanted: AtomicBool::new(serving),
        stop: Arc::clone(&stop),
        #[cfg(feature = "sampler")]
        sampler: Mutex::new(sampler),
    });
    *slot = Some(Arc::clone(&session));
    drop(slot);
    RECORDING.store(true, Ordering::Release);
    std::thread::Builder::new()
        .name("ix-prof-ticker".to_owned())
        .spawn({
            let session = Arc::clone(&session);
            move || ticker(&session)
        })
        .map_err(|why| Error::Format(format!("the ticker thread did not start: {why}")))?;
    #[cfg(feature = "http")]
    if let Some(addr) = session.config.http.clone() {
        let bound = http::serve(&addr).map_err(Error::Http)?;
        let url = format!("http://{bound}/debug/pprof/live");
        eprintln!("ix-prof: live at {url}");
        if let Some(dir) = &session.config.dir {
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(dir.join("http.addr"), bound.to_string());
        }
    }
    #[cfg(not(feature = "http"))]
    if session.config.http.is_some() {
        return Err(Error::Http(format!(
            "{VAR_HTTP} is set but ix-prof was built without its `http` feature"
        )));
    }
    Ok(Guard { on: true })
}

/// [`start`] with [`Config::from_env`]; an inert guard when the variables are unset.
pub fn start_from_env() -> Result<Guard, Error> {
    match Config::from_env()? {
        Some(config) => start(config),
        None => Ok(Guard { on: false }),
    }
}

/// Stops the profiler and writes its profile when dropped.
#[must_use = "the profiler stops when the guard drops"]
pub struct Guard {
    on: bool,
}

impl Guard {
    /// Stops now and says where the profile went.
    pub fn finish(mut self) -> Result<Option<PathBuf>, Error> {
        self.stop()
    }

    fn stop(&mut self) -> Result<Option<PathBuf>, Error> {
        if !std::mem::take(&mut self.on) {
            return Ok(None);
        }
        let Some(session) = SESSION.lock().take() else { return Ok(None) };
        session.stop.store(true, Ordering::Relaxed);
        #[cfg(feature = "sampler")]
        drop(session.sampler.lock().take());
        RECORDING.store(false, Ordering::Release);
        alloc::turn_off();
        drain_allocs(&session);
        session.log.lock().ended = Some("process exiting".to_owned());
        let Some(dir) = session.config.dir.clone() else { return Ok(None) };
        let (bytes, program) = alloc::quiet(|| {
            let mut recorder = session.recorder.lock();
            recorder.advance_clock(clock::now_ns());
            let mut symbols = symbols::Symbolizer::new();
            (recorder.snapshot(None, None, &mut symbols).to_ixprof(), recorder.meta_mut().program.clone())
        });
        let bytes = bytes.map_err(|it| Error::Format(it.to_string()))?;
        let name = format!("{program}-{}.ixprof", std::process::id());
        let path = dir.join(&name);
        // Whole or not at all: a reader of the directory (a build laying code out by it,
        // docs/v2/plan/lanes/layout/layout.md) never sees a profile half written.
        let part = dir.join(format!("{name}.part"));
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&part, bytes))
            .and_then(|()| std::fs::rename(&part, &path))
            .map_err(|why| Error::Write { path: path.clone(), why: why.to_string() })?;
        eprintln!("ix-prof: wrote {}", path.display());
        Ok(Some(path))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(why) = self.stop() {
            eprintln!("{why}");
        }
    }
}

fn drain_allocs(session: &Session) {
    let mut records = Vec::new();
    alloc::drain(&mut records);
    let (queue_full, untracked) = alloc::lost();
    let mut r = session.recorder.lock();
    for rec in records {
        if rec.free {
            r.free(rec.address, rec.t_ns, rec.tid);
        } else {
            let span_id = r.innermost(rec.tid);
            r.alloc(core::RawAlloc {
                thread: rec.tid,
                t_ns: rec.t_ns,
                address: rec.address,
                bytes: rec.bytes,
                count: rec.count,
                pcs: rec.pcs().to_vec(),
                span_id,
            });
        }
    }
    let dropped = r.dropped();
    let seen = |t: &str| dropped.get(t).map_or(0, |it| it.0);
    // The allocator's own losses, as totals since start.
    if queue_full > seen("alloc_queue") {
        let n = queue_full - seen("alloc_queue");
        r.drop_records("alloc_queue", n, "the allocator's sample queue was full");
    }
    if untracked > seen("alloc_untracked") {
        let n = untracked - seen("alloc_untracked");
        r.drop_records("alloc_untracked", n, "the table of sampled blocks was full: their frees are not seen");
    }
}

/// The ticker: drains the allocator, writes heap counters, and makes live batches when a
/// live client has asked. A driver: its sleep is its own timer.
fn ticker(session: &Session) {
    let mut symbols = symbols::Symbolizer::new();
    let mut encoder = {
        let log = session.log.lock();
        live::Encoder::new(log.session.clone(), std::process::id())
    };
    while !session.stop.load(Ordering::Relaxed) {
        #[allow(clippy::disallowed_methods, reason = "the ticker thread's own timer, a driver's: no pool thread or job waits here")]
        std::thread::sleep(session.config.tick);
        alloc::quiet(|| {
            drain_allocs(session);
            let now = clock::now_ns();
            let (allocated, freed) = alloc::totals();
            let delta = {
                let mut r = session.recorder.lock();
                r.advance_clock(now);
                if alloc::installed() {
                    r.counter("heap_alloc_bytes_total", now, allocated as f64);
                    r.counter("heap_live_bytes", now, allocated.saturating_sub(freed) as f64);
                }
                session.live_wanted.load(Ordering::Relaxed).then(|| (r.take_live(), r.dropped()))
            };
            if let Some((delta, dropped)) = delta {
                let mut dropped = dropped;
                for (table, n) in &delta.missed {
                    let e = dropped.entry(*table).or_insert((0, String::new()));
                    e.0 += n;
                    e.1 = "evicted from the recorder before the live stream took it".to_owned();
                }
                match encoder.tick(&delta, &mut symbols) {
                    Ok(tick) => session.log.lock().push(tick, now, &dropped, delta.open_spans.len()),
                    Err(why) => eprintln!("ix-prof: live batch: {why}"),
                }
            }
        });
    }
}

/// A profile of what is retained in `[since, until]`, now: the HTTP endpoint's window
/// snapshot and the explorer's backfill.
pub fn snapshot(since: Option<i64>, until: Option<i64>) -> Option<Profile> {
    let session = session()?;
    drain_allocs(&session);
    Some(alloc::quiet(|| {
        let mut symbols = symbols::Symbolizer::new();
        let mut recorder = session.recorder.lock();
        recorder.advance_clock(clock::now_ns());
        recorder.snapshot(since, until, &mut symbols)
    }))
}

/// The open spans of each thread, outermost first, as of now: `/debug/pprof/threads`.
pub fn open_spans() -> Vec<profile::Span> {
    let Some(session) = session() else { return Vec::new() };
    alloc::quiet(|| {
        let mut recorder = session.recorder.lock();
        recorder.advance_clock(clock::now_ns());
        recorder.open_spans()
    })
}
