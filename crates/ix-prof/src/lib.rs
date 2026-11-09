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
//! drops it writes `<dir>/<program>-<pid>-<session>.ixprof`. Off (no `IX_PROF`, no `IX_PROF_HTTP`),
//! nothing runs and each span callback and allocation costs one relaxed load.
//!
//! The format and every interface: docs/ix-prof.md and docs/ix-prof-format.md.
//! The aggregation is [`core::Recorder`], a sans-IO state machine; everything else here is a
//! driver (the sampler thread, the layer, the allocator, the ticker, the HTTP server).
//!
//! With the feature `explore`, `explore` serves https://ix.dev/explore over WebTransport for
//! any program that has an `explore::Source` (the blaze daemon is one).

// `#![blaze::sans_io]` marks a sans-IO core for `blaze::sans_io_core` (`explore::session`);
// inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
// `std::random`: the explorer's token (`explore::link::Token`).
#![cfg_attr(feature = "explore", feature(random))]

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use blaze_core::lock::Lock;

pub mod alloc;
pub mod chrome;
pub mod clock;
pub mod core;
pub mod endpoint;
#[cfg(feature = "explore")]
pub mod explore;
#[cfg(all(feature = "http", unix))]
pub mod http;
pub mod images;
pub mod ixprof;
pub mod jit;
pub mod json;
#[cfg(feature = "layer")]
pub mod layer;
pub mod live;
/// blaze's one `perf_event_open` reader, ix-prof's sampler's and blaze-profile's.
#[cfg(target_os = "linux")]
pub mod perf;
pub mod pprof;
pub mod profile;
pub mod report;
#[cfg(feature = "sampler")]
pub mod sampler;
pub mod symbols;
#[cfg(test)]
mod tests;
pub mod token;
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
    Var {
        var: &'static str,
        value: String,
        why: &'static str,
    },
    /// The sampler cannot run here, and the config asks for it.
    Sampler(String),
    AlreadyStarted,
    Http(String),
    Write {
        path: PathBuf,
        why: String,
    },
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
    /// Shared with the sampler's sink, which feeds it directly (not through [`SESSION`]), so
    /// the sampler's last batch lands while [`Guard::stop`] joins it.
    pub(crate) recorder: Arc<Lock<core::Recorder>>,
    pub(crate) log: Lock<live::Log>,
    pub(crate) live_wanted: AtomicBool,
    /// The live stream's session id, also in the profile's file name.
    id: String,
    stop: AtomicBool,
    #[cfg(feature = "sampler")]
    sampler: Lock<Option<sampler::Sampler>>,
    ticker: Lock<Option<std::thread::JoinHandle<()>>>,
    #[cfg(all(feature = "http", unix))]
    http: Lock<Option<http::Server>>,
}

/// Held by every test that starts the profiler or turns the allocator on: they share
/// [`SESSION`], [`RECORDING`] and the allocator's globals.
#[cfg(test)]
pub(crate) static TEST_GLOBALS: Lock<()> = Lock::new("ixprof.session.test_globals", ());

static RECORDING: AtomicBool = AtomicBool::new(false);
static SESSION: Lock<Option<Arc<Session>>> = Lock::new("ixprof.session.session", None);

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
///
/// Every step that can fail (binding the endpoint, the sampler, the ticker and server
/// threads, writing `http.endpoint`) runs before the session is published and recording turned
/// on, and a failure stops whatever had started: an `Err` leaves no thread running, nothing
/// recording and no session, so a later `start` begins clean.
pub fn start(config: Config) -> Result<Guard, Error> {
    let mut slot = SESSION.lock();
    if slot.is_some() {
        return Err(Error::AlreadyStarted);
    }
    // Checked now, not at exit: a directory that is not this user's own is refused before the
    // run, not after it, when the profile would be lost.
    if let Some(dir) = &config.dir {
        private_dir(dir).map_err(|why| Error::Write { path: dir.clone(), why })?;
    }
    #[cfg(all(feature = "http", unix))]
    let bound = config.http.as_deref().map(http::bind).transpose().map_err(Error::Http)?;
    #[cfg(all(feature = "http", not(unix)))]
    if config.http.is_some() {
        return Err(Error::Http(format!(
            "{VAR_HTTP} is set but the endpoint needs a Unix platform (its stop and token file), not {}",
            std::env::consts::OS
        )));
    }
    #[cfg(not(feature = "http"))]
    if config.http.is_some() {
        return Err(Error::Http(format!(
            "{VAR_HTTP} is set but ix-prof was built without its `http` feature"
        )));
    }
    #[cfg(not(feature = "sampler"))]
    if config.cpu_hz.is_some() {
        return Err(Error::Sampler("ix-prof was built without its `sampler` feature".to_owned()));
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
    // Turned on when the session is published; whether it is installed is known now.
    let alloc_on = alloc::installed();
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
    let recorder =
        Arc::new(Lock::unrecorded(
            "ixprof.session.recorder",
            core::Recorder::new(pid, &program, meta, config.limits.clone()),
        ));
    let id = live::session_id(pid, start_ns, start_unix_ns, nonce());
    let log = live::Log::new(id.clone(), config.retain_bytes);
    #[cfg(feature = "sampler")]
    let sampler = match config.cpu_hz {
        Some(hz) => {
            let into = Arc::clone(&recorder);
            let sink: sampler::Sink =
                Arc::new(move |samples: &[sampler::Sample], names: &[(u64, String)]| {
                    alloc::quiet(|| {
                        let mut r = into.lock();
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
            let it = sampler::start(hz, sink).map_err(|why| Error::Sampler(why.0))?;
            recorder.lock().meta_mut().cpu_sampler = format!("on: {hz} Hz, {}", it.how);
            Some(it)
        }
        None => None,
    };
    let serving = config.http.is_some();
    let session = Arc::new(Session {
        config,
        recorder,
        log: Lock::unrecorded("ixprof.session.log", log),
        // With an endpoint, batches are kept from the start so a client can resume and backfill.
        live_wanted: AtomicBool::new(serving),
        id,
        stop: AtomicBool::new(false),
        #[cfg(feature = "sampler")]
        sampler: Lock::new("ixprof.session.sampler", sampler),
        ticker: Lock::new("ixprof.session.ticker", None),
        #[cfg(all(feature = "http", unix))]
        http: Lock::new("ixprof.session.http", None),
    });
    let started = (|| -> Result<(), Error> {
        let ticking = std::thread::Builder::new()
            .name("ix-prof-ticker".to_owned())
            .spawn({
                let session = Arc::clone(&session);
                move || ticker(&session)
            })
            .map_err(|why| Error::Format(format!("the ticker thread did not start: {why}")))?;
        *session.ticker.lock() = Some(ticking);
        #[cfg(all(feature = "http", unix))]
        if let Some(bound) = bound {
            let server = bound.serve().map_err(Error::Http)?;
            let (addr, token) = (server.addr, server.token.clone());
            *session.http.lock() = Some(server);
            announce(&session, addr, &token)?;
        }
        Ok(())
    })();
    if let Err(why) = started {
        halt(&session);
        return Err(why);
    }
    alloc::turn_on(session.config.alloc_sample_bytes);
    *slot = Some(session);
    drop(slot);
    RECORDING.store(true, Ordering::Release);
    Ok(Guard { on: true })
}

/// A random number for the session id, from the OS's randomness (std's `RandomState` keys).
fn nonce() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(clock::now_ns() as u64);
    h.finish()
}

/// What `announce` prints to stderr. With a directory, where the token is, never the token: a
/// program's stderr often lands where other users can read it (a CI log, journald, a shared
/// `2>run.log`). Without one, stderr is the only place to give it.
#[cfg(all(feature = "http", unix))]
fn announcement(addr: std::net::SocketAddr, token: &str, dir: Option<&std::path::Path>) -> String {
    match dir {
        Some(dir) => format!(
            "ix-prof: live at http://{addr}/debug/pprof/live (its token is in {}; `blaze pprof --dir {}` reads it)",
            dir.join(endpoint::FILE).display(),
            dir.display()
        ),
        None => format!("ix-prof: live at http://{addr}/debug/pprof/live?token={token}"),
    }
}

/// Says where the endpoint is: with a directory, `<dir>/http.endpoint` (the address and the
/// token in one 0600 file, renamed into place whole, so a reader never pairs a new token with
/// an old address); and a line on stderr ([`announcement`]).
#[cfg(all(feature = "http", unix))]
fn announce(session: &Session, addr: std::net::SocketAddr, token: &str) -> Result<(), Error> {
    if let Some(dir) = &session.config.dir {
        let bytes = endpoint::Endpoint { addr, token: token.to_owned() }.encode();
        let part = format!("{}.{}.part", endpoint::FILE, session.id);
        private_dir(dir)
            .and_then(|d| d.write_whole(endpoint::FILE, &part, bytes.as_bytes()))
            .map_err(|why| Error::Write { path: dir.join(endpoint::FILE), why })?;
    }
    eprintln!("{}", announcement(addr, token, session.config.dir.as_deref()));
    Ok(())
}

/// Removes `http.endpoint` when it is this session's (the token in it is its own): a client
/// that reads it after the stop finds nothing, rather than a port another user may have bound
/// since. The file is read through [`endpoint::PrivateDir`]: never through a link, never a FIFO
/// or device, at most 64 bytes, so what is planted there cannot hang or bloat the stop.
#[cfg(all(feature = "http", unix))]
fn retract(session: &Session, token: &str) {
    let Some(dir) = &session.config.dir else { return };
    if std::fs::symlink_metadata(dir).is_err_and(|why| why.kind() == std::io::ErrorKind::NotFound) {
        return;
    }
    let path = dir.join(endpoint::FILE);
    let ours = endpoint::PrivateDir::open(dir).and_then(|d| {
        let Some(bytes) = d.read_small(endpoint::FILE)? else { return Ok(()) };
        if endpoint::Endpoint::parse(&bytes, &path)?.token == token {
            d.remove(endpoint::FILE)?;
        }
        Ok(())
    });
    if let Err(why) = ours {
        eprintln!("ix-prof: left {} as it is at stop: {why}", path.display());
    }
}

/// Makes `dir` and its missing parents, mode 0700, and opens it checked
/// ([`endpoint::PrivateDir::open`]): a profile, and the endpoint's token, are for this user
/// only, so a directory that is a link, someone else's, open to group or other, with an ACL
/// (on macOS, inherited from a parent another user owns), or on a volume that ignores ownership
/// or is a network one, is refused, never used and never changed.
fn private_dir(dir: &std::path::Path) -> Result<endpoint::PrivateDir, String> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir).map_err(|why| format!("making {}: {why}", dir.display()))?;
    endpoint::PrivateDir::open(dir)
}

/// Stops and joins every thread of the session: the sampler (its last batch lands in the
/// recorder first), the ticker and the HTTP server.
fn halt(session: &Session) {
    session.stop.store(true, Ordering::Relaxed);
    #[cfg(feature = "sampler")]
    {
        let sampler = session.sampler.lock().take();
        drop(sampler);
    }
    let ticking = session.ticker.lock().take();
    if let Some(ticking) = ticking {
        ticking.thread().unpark();
        let _wait = tracing::info_span!(target: "blaze::ix_prof", "join", wait = "thread", on = "ix-prof-ticker").entered();
        let _ = ticking.join();
    }
    #[cfg(all(feature = "http", unix))]
    {
        let server = session.http.lock().take();
        if let Some(server) = server {
            let token = server.token.clone();
            server.stop();
            retract(session, &token);
        }
    }
}

/// The profile's file name: the session id makes it unique to this run, so a later run with
/// the same pid (pid reuse, containers) never replaces it.
pub(crate) fn profile_name(program: &str, pid: u32, session: &str) -> String {
    format!("{program}-{pid}-{session}.ixprof")
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
        RECORDING.store(false, Ordering::Release);
        // Joined before the snapshot, so the sampler's tail is in it.
        halt(&session);
        alloc::turn_off();
        drain_allocs(&session);
        session.log.lock().ended = Some("process exiting".to_owned());
        let Some(dir) = session.config.dir.clone() else { return Ok(None) };
        let (bytes, program) = alloc::quiet(|| {
            let mut recorder = session.recorder.lock();
            recorder.advance_clock(clock::now_ns());
            let mut symbols = symbols::Symbolizer::new();
            (
                recorder.snapshot(None, None, &mut symbols).to_ixprof(),
                recorder.meta_mut().program.clone(),
            )
        });
        let bytes = bytes.map_err(|it| Error::Format(it.to_string()))?;
        let name = profile_name(&program, std::process::id(), &session.id);
        let path = dir.join(&name);
        // Whole or not at all: a reader of the directory (a build laying code out by it,
        // docs/v2/plan/design/code-layout.md) never sees a profile half written. The part
        // file is made new, so two writers never share one.
        private_dir(&dir)
            .and_then(|d| d.write_whole(&name, &format!("{name}.part"), &bytes))
            .map_err(|why| Error::Write { path: path.clone(), why })?;
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
        r.drop_records(
            "alloc_untracked",
            n,
            "the table of sampled blocks was full: their frees are not seen",
        );
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
        #[allow(
            clippy::disallowed_methods,
            reason = "the ticker thread's own timer, a driver's: no pool thread or job waits here, and `halt` unparks it"
        )]
        std::thread::park_timeout(session.config.tick);
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
                    Ok(tick) => {
                        session.log.lock().push(tick, now, &dropped, delta.open_spans.len())
                    }
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
