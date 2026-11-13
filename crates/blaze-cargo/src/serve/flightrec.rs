//! Flight recordings: what the flight recorder (`blaze_profile::flight`, the one always-on,
//! per-thread, lock-free ring of spans, waits, tasks, admissions and events) holds, written to
//! a file as the daemon goes down or when asked, so the client a request hung up on names a
//! file that says what the daemon was doing.
//!
//! `<state>/flightrec/<pid>-<unix ms>.cbor`, written when
//! - a thread no job's runner catches panics ([`on_panic`]: the loop, an IO thread, the guard);
//! - the daemon is stopped at its memory ceiling (`stop::passed`);
//! - a build stalls past the stall bound (`Daemon::watch`), the loop does (`stop::watch_turns`)
//!   or a stop does (`stop::guard`);
//! - `blaze debug inspect --record` asks (`inspect`).
//!
//! A fatal signal writes none: its handler may only touch what is async-signal-safe, and the
//! recorder's rows are already in `profile/flight-<pid>.map`, which the next history to open
//! reads back (`blaze_profile::flight::postmortem`).
//!
//! The file: [`MAGIC`], [`FORMAT`] as a `u32` LE, then one [`Recording`] in deterministic CBOR
//! (`blaze_protocol::cbor`; every struct here declares its fields in the order of their keys'
//! encoding, shorter names first, then bytewise) as one zstd frame (format 2; a trace is JSON
//! of a few hundred names: the four recordings of format 1 a day's work left, 12.5 MB to
//! 37.0 MB, are 0.48 MB to 1.78 MB at this level, 21 to 26 times smaller). A file of format 1, the same CBOR not compressed, is read as it is; one of
//! any other format is refused naming both formats ([`decode`]); nothing reads it as this
//! one. Its `trace` is the recorder's last
//! [`Recording::window_ms`] as the Chrome trace `blaze debug profile` writes (ui.perfetto.dev
//! opens it): every span with its name, item and `awaits`, every event, the CPU samples. It is
//! bounded: a trace past [`TRACE_MOST`] is taken again over half the window until it fits, and
//! the directory keeps the newest [`KEPT`] files.
//!
//! Nothing here blocks a recorded thread: the rings are read as every reader reads them, by
//! the thread that writes the file.

// The field order promised above, held by `blaze::cbor_field_order`.
#![blaze::canonical_cbor]

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// The directory under the daemon's state.
pub const DIR: &str = "flightrec";

/// What every recording starts with.
pub const MAGIC: [u8; 16] = *b"blaze flightrec\n";

/// The layout of [`Recording`]. A change to any field is a new number. Format 2 is format
/// 1's CBOR as one zstd frame.
pub const FORMAT: u32 = 2;

/// The format before zstd, still read: the CBOR as it is.
const FORMAT_PLAIN: u32 = 1;

/// zstd's level for a recording: written as the daemon goes down or stalls, once, so the
/// level that costs tens of milliseconds for 12 MB, not the densest.
const LEVEL: i32 = 3;

/// The most bytes a recording's CBOR is taken to be when read: the trace's bound and room for
/// the threads, so a file that claims more is refused before it is inflated.
const BODY_MOST: usize = TRACE_MOST + (64 << 20);

/// The most bytes a recording's trace holds.
pub const TRACE_MOST: usize = 64 << 20;

/// How many recordings the directory keeps, newest first: a day's work is a handful (two in
/// the day measured 2026-11-11, four of 12 MB to 37 MB in format 1), and at format 2's size
/// (a twentieth of that) sixteen of the largest seen are under 30 MB.
pub const KEPT: usize = 16;

/// How much of the recorder a recording takes: the stall watchdog's two minutes.
pub const WINDOW: Duration = Duration::from_secs(120);

/// How much one taken at the memory ceiling takes: the daemon is over its memory already.
pub const WINDOW_AT_CEILING: Duration = Duration::from_secs(5);

/// Why a recording was written.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// A panic no job's runner caught.
    Panic,
    /// The daemon's footprint passed its ceiling.
    MemoryCeiling,
    /// A build made no progress for the stall bound.
    Stall,
    /// The loop finished no turn for the stall bound.
    LoopStalled,
    /// A stop made no progress for the stall bound.
    StopStalled,
    /// The task watchdog found a task that made no progress for its bound (`serve::stuck`),
    /// or more threads stuck for good than its bound of lost workers.
    Stuck,
    /// `blaze debug inspect --record`.
    Asked,
}

/// A span a thread was inside.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Open {
    /// What it is of: a query's key, a task's kind, a function.
    pub item: Option<String>,
    pub name: String,
    /// A daemon `task` span's plane node.
    pub node: Option<u64>,
    pub unit: Option<String>,
    /// The build its thread worked for; 0 for none.
    pub build: u32,
    pub krate: Option<String>,
    /// Who ends a wait, by thread or job, when the wait says.
    pub owner: Option<u64>,
    /// What a wait waits on: a key, a lock, a channel.
    pub awaits: Option<String>,
    /// How long the thread had been inside it.
    pub for_ns: u64,
}

/// A thread and the spans it was inside, outermost first.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Thread {
    pub tid: u64,
    pub name: String,
    pub open: Vec<Open>,
    /// Spans it was inside past those named.
    pub deeper: u32,
}

impl Thread {
    pub fn of(thread: &blaze_profile::flight::ThreadNow) -> Thread {
        let text = |it: &Option<std::sync::Arc<str>>| it.as_deref().map(str::to_owned);
        Thread {
            tid: thread.tid,
            name: thread.name.clone(), // clone: a recording's copy
            // alloc: a recording, on demand or as the daemon goes down
            open: thread
                .open
                .iter()
                .map(|it| Open {
                    item: text(&it.function),
                    name: it.name.to_owned(),
                    node: it.node,
                    unit: text(&it.unit),
                    build: it.build,
                    krate: text(&it.krate),
                    owner: it.owner,
                    awaits: text(&it.awaits),
                    for_ns: it.for_ns,
                })
                .collect(),
            deeper: thread.deeper,
        }
    }
}

/// One recording.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Recording {
    pub pid: u32,
    pub why: Why,
    /// What the daemon said as it wrote this: the panic's message, the stall's line.
    pub said: String,
    /// The recorder's last `window_ms`, as a Chrome trace.
    #[serde(with = "blaze_protocol::bytes")]
    pub trace: Vec<u8>,
    /// Every thread inside a span as it was taken.
    pub threads: Vec<Thread>,
    pub window_ms: u64,
    /// When it was taken, in nanoseconds since the Unix epoch.
    pub at_unix_ns: u64,
}

/// Why a file is no recording this blaze reads.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// It does not start with [`MAGIC`] and a format.
    NotOne,
    /// It is of another format.
    Format(u32),
    /// Its body is not a [`Recording`].
    Body(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::NotOne => write!(f, "it is no flight recording (it does not start with blaze's mark)"),
            Refused::Format(format) => write!(
                f,
                "it is a flight recording of format {format}, and this blaze reads format {FORMAT}: \
                 read it with the blaze that wrote it"
            ),
            Refused::Body(why) => write!(f, "its recording does not decode: {why}"),
        }
    }
}

impl Recording {
    /// The recorder as it is now, taken by the calling thread: every thread's open spans and
    /// the last `window` as a trace, over half the window again while it is past
    /// [`TRACE_MOST`] (bounded: a window under a second is taken as it is).
    pub fn take(why: Why, said: String, window: Duration) -> Recording {
        let threads = blaze_profile::flight::threads_now().iter().map(Thread::of).collect();
        let mut window = window;
        let trace = loop {
            let trace = blaze_profile::flight::dump(window);
            if trace.len() <= TRACE_MOST || window < Duration::from_secs(1) {
                break trace;
            }
            window /= 2;
        };
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        Recording {
            pid: std::process::id(),
            why,
            said,
            trace,
            threads,
            window_ms: u64::try_from(window.as_millis()).unwrap_or(u64::MAX),
            at_unix_ns: at.map_or(0, |it| u64::try_from(it.as_nanos()).unwrap_or(u64::MAX)),
        }
    }

    /// The file's bytes.
    pub fn encode(&self) -> Vec<u8> {
        let body = blaze_protocol::cbor::to_vec(self)
            .expect("a flight recording is plain data, which CBOR encodes");
        let body = zstd::bulk::compress(&body, LEVEL)
            .expect("zstd compresses any bytes into memory it allocates");
        let mut out = Vec::with_capacity(MAGIC.len() + 4 + body.len()); // alloc: the file, once
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// The recording `bytes` are, or why they are none this blaze reads.
    pub fn decode(bytes: &[u8]) -> Result<Recording, Refused> {
        let Some((head, body)) = bytes.split_at_checked(MAGIC.len() + 4) else {
            return Err(Refused::NotOne);
        };
        let (magic, format) = head.split_at(MAGIC.len());
        if magic != MAGIC {
            return Err(Refused::NotOne);
        }
        let format = u32::from_le_bytes(format.try_into().expect("four bytes"));
        let cbor = |body: &[u8]| {
            blaze_protocol::cbor::from_slice(body).map_err(|why| Refused::Body(why.to_string()))
        };
        match format {
            FORMAT => {
                let body = zstd::bulk::decompress(body, BODY_MOST).map_err(|why| {
                    // alloc: an error
                    Refused::Body(format!("its zstd frame (at most {BODY_MOST} bytes inflated): {why}"))
                })?;
                cbor(&body)
            }
            FORMAT_PLAIN => cbor(body),
            format => Err(Refused::Format(format)),
        }
    }

    /// Its file's name: `<pid>-<unix ms>.cbor`.
    pub fn file_name(&self) -> String {
        format!("{}-{}.cbor", self.pid, self.at_unix_ns / 1_000_000) // alloc: a name, once
    }
}

/// The pid and time a recording's file name says; `None` for any other file.
fn named(name: &str) -> Option<(u32, u64)> {
    let (pid, ms) = name.strip_suffix(".cbor")?.split_once('-')?;
    Some((pid.parse().ok()?, ms.parse().ok()?))
}

/// Writes `recording` under `state`: to a hidden name, synced, then renamed, so a reader never
/// reads half of one; then the oldest recordings past [`KEPT`] are removed. The file's path, or
/// why it was not written, naming it.
pub fn write(state: &Path, recording: &Recording) -> Result<PathBuf, String> {
    let dir = state.join(DIR);
    let path = dir.join(recording.file_name());
    let failed = |doing: &str, why: std::io::Error| {
        format!("the flight recording {} was not written ({doing}): {why}", path.display())
    };
    std::fs::create_dir_all(&dir).map_err(|why| failed("making its directory", why))?;
    let hidden = dir.join(format!(".{}.{}", recording.file_name(), std::process::id()));
    let written = (|| {
        let mut file = std::fs::File::create(&hidden)?;
        file.write_all(&recording.encode())?;
        file.sync_all()?;
        std::fs::rename(&hidden, &path)
    })();
    if let Err(why) = written {
        _ = std::fs::remove_file(&hidden);
        return Err(failed("writing it", why));
    }
    prune(&dir);
    Ok(path)
}

/// Removes the recordings of `dir` past the newest [`KEPT`]. One that can't be listed or
/// removed stays: the next write tries again.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    // alloc: a directory's names, once a recording
    let mut found: Vec<(u64, u32, PathBuf)> = entries
        .flatten()
        .filter_map(|it| {
            let (pid, ms) = named(it.file_name().to_str()?)?;
            Some((ms, pid, it.path()))
        })
        .collect();
    found.sort_unstable();
    let past = found.len().saturating_sub(KEPT);
    for (_, _, path) in found.into_iter().take(past) {
        _ = std::fs::remove_file(path);
    }
}

/// The newest recording process `pid` wrote under `state`, if it wrote one: what the client
/// of a daemon that hung up names (`cli::daemon::Died`).
pub fn latest(state: &Path, pid: u32) -> Option<PathBuf> {
    std::fs::read_dir(state.join(DIR))
        .ok()?
        .flatten()
        .filter_map(|it| {
            let (of, ms) = named(it.file_name().to_str()?)?;
            (of == pid).then(|| (ms, it.path()))
        })
        .max()
        .map(|(_, path)| path)
}

#[expect(
    clippy::disallowed_types,
    reason = "where the daemon's recordings go, set once as it starts: the guard's thread and the panic hook write one with no handle to the daemon"
)]
static STATE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// A recording is being taken: a second asked for meanwhile (a panic while one is written, the
/// guard and the loop at once) is not, so none recurses and two never interleave.
static TAKING: AtomicBool = AtomicBool::new(false);

/// Says where this daemon's recordings go: `state`, its state directory. Once, as it starts.
pub fn arm(state: &Path) {
    _ = STATE.set(state.to_owned());
}

/// Takes a recording now and writes it under the daemon's state ([`arm`]), saying where on
/// the daemon's log, or why not. The path, for whoever names it.
pub fn record(why: Why, said: &str, window: Duration) -> Result<PathBuf, String> {
    let Some(state) = STATE.get() else {
        return Err("no flight recording is written: this process is no armed daemon".to_owned());
    };
    if TAKING.swap(true, Ordering::AcqRel) {
        return Err("no second flight recording is written while one is being taken".to_owned());
    }
    let written = write(state, &Recording::take(why, said.to_owned(), window));
    TAKING.store(false, Ordering::Release);
    match &written {
        Ok(path) => {
            tracing::warn!(target: "blaze", why = ?why, "flight recording written to {}", path.display());
            // logged: the event above
            eprintln!("blaze serve: pid {} flight recording: {}", std::process::id(), path.display());
        }
        Err(error) => {
            tracing::error!(target: "blaze", "{error}");
            // logged: the event above
            eprintln!("blaze serve: {error}");
        }
    }
    written
}

/// Has a panic no job's runner catches (`caught` says whether one does: `plane`'s `in_a_job`)
/// write a recording after it is printed and recorded as before: such a panic ends its thread,
/// and on the loop the daemon. Once, as the daemon starts, after `plane::record_panics`.
pub fn on_panic(caught: fn() -> bool) {
    let before = std::panic::take_hook();
    // alloc: the hook, once a daemon
    std::panic::set_hook(Box::new(move |info| {
        before(info);
        // A panic's printing is the log's largest line by far: the log is bounded after it.
        super::log::check();
        if !caught() {
            _ = record(Why::Panic, &info.to_string(), WINDOW);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(pid: u32, at_unix_ns: u64) -> Recording {
        Recording {
            pid,
            why: Why::Stall,
            said: "blaze stalled: no job ended".to_owned(),
            trace: b"{\"traceEvents\":[]}".to_vec(),
            threads: vec![Thread {
                tid: 7,
                name: "blaze-work-3".to_owned(),
                open: vec![Open {
                    item: Some("unit bodies (infer)".to_owned()),
                    name: "wait".to_owned(),
                    node: Some(12),
                    unit: None,
                    build: 4,
                    krate: Some("reth".to_owned()),
                    owner: Some(9),
                    awaits: Some("key infer(Foo)".to_owned()),
                    for_ns: 3_000_000_000,
                }],
                deeper: 0,
            }],
            window_ms: 120_000,
            at_unix_ns,
        }
    }

    fn scratch(name: &str) -> blaze_core::temp::TempDir {
        blaze_core::temp::TempDir::for_test(&format!("blaze-flightrec-{name}-"))
    }

    /// Persisted state round-trips: a recording written is read back equal, its bytes are
    /// deterministic CBOR (encoded twice they are the same, and the checker holds them), and
    /// the newest of a process is the one the client names.
    #[test]
    fn a_recording_written_reads_back_equal_and_the_newest_of_a_pid_is_found() {
        let made = scratch("round-trip");
        let state = made.path().to_owned();
        let first = recording(41, 1_700_000_000_000_000_000);
        let second = recording(41, 1_700_000_001_000_000_000);
        let other = recording(42, 1_700_000_002_000_000_000);
        let path = write(&state, &first).unwrap();
        assert_eq!(path, state.join(DIR).join("41-1700000000000.cbor"));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, first.encode(), "the same recording encodes to the same bytes");
        let body = zstd::bulk::decompress(&bytes[MAGIC.len() + 4..], BODY_MOST).unwrap();
        blaze_protocol::cbor::check(&body).unwrap();
        assert_eq!(Recording::decode(&bytes).unwrap(), first);

        let newest = write(&state, &second).unwrap();
        write(&state, &other).unwrap();
        assert_eq!(latest(&state, 41), Some(newest));
        assert_eq!(latest(&state, 43), None);
    }

    /// A file of the previous layout is handled: one of another format is refused naming both
    /// formats, never read as this one; so is a file that is no recording, and one cut short.
    #[test]
    fn a_file_of_another_format_is_refused_naming_both() {
        let mut old = recording(41, 1).encode();
        old[MAGIC.len()..MAGIC.len() + 4].copy_from_slice(&0u32.to_le_bytes());
        let refused = Recording::decode(&old).unwrap_err();
        assert_eq!(refused, Refused::Format(0));
        let said = refused.to_string();
        assert!(said.contains("format 0") && said.contains("reads format 2"), "{said}");
        let mut newer = recording(41, 1).encode();
        newer[MAGIC.len()..MAGIC.len() + 4].copy_from_slice(&(FORMAT + 1).to_le_bytes());
        assert_eq!(Recording::decode(&newer), Err(Refused::Format(FORMAT + 1)));

        assert_eq!(Recording::decode(b"{\"traceEvents\":[]}"), Err(Refused::NotOne));
        assert_eq!(Recording::decode(b"short"), Err(Refused::NotOne));
        let whole = recording(41, 1).encode();
        let cut = &whole[..whole.len() - 3];
        assert!(matches!(Recording::decode(cut), Err(Refused::Body(_))));
    }

    /// A file of the previous layout is read: format 1, the CBOR not compressed, as blaze
    /// wrote it until 2026-11-11, decodes to the recording format 2's file does.
    #[test]
    fn a_file_of_format_1_reads_as_the_recording_it_is() {
        let recording = recording(41, 9);
        let mut plain = MAGIC.to_vec();
        plain.extend_from_slice(&FORMAT_PLAIN.to_le_bytes());
        plain.extend_from_slice(&blaze_protocol::cbor::to_vec(&recording).unwrap());
        assert_eq!(Recording::decode(&plain), Ok(recording.clone()));
        assert_eq!(Recording::decode(&recording.encode()), Ok(recording));
    }

    /// What format 2 is for: a trace the size the daemon's was (12 MB of Chrome trace events,
    /// a few hundred distinct names) is a file under a quarter of it, and reads back equal.
    /// The timestamps and durations here are a generator's, with none of a real trace's
    /// order: real recordings are 21 to 26 times smaller (the module's note).
    #[test]
    fn a_trace_of_12_mb_is_a_file_under_a_quarter_of_it() {
        let mut trace = String::from("{\"traceEvents\":[");
        let mut at = 0u64;
        while trace.len() < 12 << 20 {
            // A cheap generator, so durations and timestamps are not all alike.
            at = at.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            trace.push_str(&format!(
                "{{\"name\":\"query {}\",\"ph\":\"X\",\"pid\":1,\"tid\":{},\"ts\":{},\"dur\":{},\"args\":{{\"item\":\"crate::module_{}::function_{}\"}}}},",
                at >> 56, (at >> 40) % 26, at >> 24, (at >> 8) % 100_000, (at >> 50) % 64, (at >> 44) % 512,
            ));
        }
        trace.push_str("]}");
        let recording = Recording { trace: trace.into_bytes(), ..recording(41, 9) };
        let file = recording.encode();
        assert!(file.len() * 4 < recording.trace.len(), "{} bytes of {}", file.len(), recording.trace.len());
        assert_eq!(Recording::decode(&file), Ok(recording));
    }

    /// The directory is bounded: past [`KEPT`] recordings the oldest go, whichever process
    /// wrote them, and no other file is touched.
    #[test]
    fn the_directory_keeps_the_newest_recordings() {
        let made = scratch("kept");
        let state = made.path().to_owned();
        std::fs::create_dir_all(state.join(DIR)).unwrap();
        std::fs::write(state.join(DIR).join("notes.txt"), b"mine").unwrap();
        for at in 0..KEPT as u64 + 3 {
            write(&state, &recording(50 + (at % 2) as u32, at * 1_000_000)).unwrap();
        }
        let mut left: Vec<u64> = std::fs::read_dir(state.join(DIR))
            .unwrap()
            .flatten()
            .filter_map(|it| named(it.file_name().to_str()?).map(|(_, ms)| ms))
            .collect();
        left.sort_unstable();
        assert_eq!(left, (3..KEPT as u64 + 3).collect::<Vec<_>>());
        assert!(state.join(DIR).join("notes.txt").exists());
    }
}
