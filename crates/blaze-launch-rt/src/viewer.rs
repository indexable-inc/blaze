//! Local loopback live telemetry viewer (`blaze-launcher --viewer`, `127.0.0.1:41937`).
//!
//! Serves `https://ix.dev/blaze/explore` (and local `/explore`) over loopback HTTP + RFC 6455
//! WebSocket with zero third-party dependencies:
//! - `GET /api/info` -> `{"version":1,"active_viewers":<n>}` (lets `blaze run` skip opening a
//!   duplicate browser tab whenever an Explore tab is already connected).
//! - `GET /api/runs`, `GET /api/builds` -> the runs and builds now.
//! - `GET /api/resources?pid=<pid>` -> a process's descriptors, IP sockets and open files, asked
//!   of the kernel in this process (`viewer/procfd.rs`: libproc on macOS, `/proc` on Linux; no
//!   `lsof`), or a typed refusal naming the call and its errno.
//! - `GET /ws` -> live stream of `runs`, `builds`, `run_started`, `run_ended` text frames and the
//!   packed binary batches of every shared-memory `*.telem` ring under `<runs_dir>`.
//! - `GET /explore` -> self-contained local fallback UI when offline.
//!
//! One thread, one wait. The thread sleeps in a single `poll` (`blaze_core::wait::readable`,
//! named, with a deadline) over the listener, every connection (its reads, and its writes while
//! bytes are queued for it), and the kernel's word on the runs directory and on each run's
//! process (`viewer/watch.rs`: kqueue `EVFILT_VNODE` and `EVFILT_PROC` on macOS, inotify and
//! pidfds on Linux). What each wake does is the sans-IO core's (`viewer/pump.rs`). Its one
//! timer, the 16 ms drain tick, is armed only while a viewer is connected and a ring may have
//! events; with nobody watching and nothing running the thread does not wake before the idle
//! expiry, when it stops (rule 70).
//!
//! Upheld invariants:
//! - Loopback-only (`127.0.0.1`), CORS + Private Network Access headers restricted to `https://ix.dev`
//!   and loopback origins.
//! - Every header, frame, queue, connection count, listing and ring drain per tick has a bound.
//! - Idle expiry (rule 70): exits cleanly once both `active_viewers == 0` and `active_runs == 0`
//!   continuously for `idle_expiry`.

#![blaze::driver]

mod procfd;
mod pump;
mod resources;
mod watch;

use core::ffi::CStr;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::time::Duration;
use std::{ffi::OsStr, fs, io::{self, Read, Write}, net::{Shutdown, SocketAddr, TcpListener, TcpStream}, os::{fd::AsRawFd, unix::ffi::OsStrExt}, path::{Path, PathBuf}, thread::{self, JoinHandle}, time::{SystemTime, UNIX_EPOCH}};

use blaze_core::wait::{self, Clock, Deadline, Monotonic, WaitError, Waited};
use blaze_own::{AString, AVec, Alloc, Arc, Share, SharedAlloc};

use crate::telemetry::{Event, MappedRing};

use self::pump::{Action, ConnId, Listed, Pump, Ring, RingStats, RunMeta};
use self::watch::{Exit, Seen, Watch, WatchError};

/// Default loopback port matching `site/explore/protocol.ts` and `blaze_run::viewer::DEFAULT_VIEWER_PORT`.
pub const DEFAULT_PORT: u16 = 41937;

/// Hosted Explore page opened by `blaze run --open` when `active_viewers == 0`.
pub const DEFAULT_EXPLORE_URL: &str = "https://ix.dev/blaze/explore";

/// Default idle expiry: 120 seconds with zero active runs and zero connected browser tabs.
pub const DEFAULT_IDLE_EXPIRY: Duration = Duration::from_secs(120);

const MAX_HTTP_HEADER_BYTES: usize = 8192;
const MAX_CLIENTS: usize = 32;
/// The most connections open at once (viewers and requests); one past it is closed unanswered.
const MOST_CONNS: usize = 64;
/// A request's headers arrive within this of its connection, and a closing connection's peer
/// closes within this of its answer; past it the connection is closed.
const REQUEST_WAIT: Duration = Duration::from_secs(2);
/// The most bytes queued for one viewer: one further behind is closed, and reported.
const MOST_QUEUED: usize = 8 << 20;
/// The longest frame a viewer may send (it sends close, ping and pong).
const MOST_CLIENT_FRAME: usize = 64 << 10;
/// A frame's header and mask at most: 2 + 8 + 4 bytes.
const MOST_FRAME_HEADER: usize = 14;
/// The reads of one connection at one wake; the descriptor stays readable for the next.
const READS_A_WAKE: usize = 16;
/// The most runs-directory entries one listing reads (rings and builds).
const MOST_LISTED: usize = 1024;
const MOST_META_BYTES: usize = 64 << 10;
const MOST_BUILD_BYTES: usize = 1 << 20;
/// The rounds of actions one wake takes; what is left is taken at once on the next.
const ROUNDS: usize = 64;
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// The viewer's one wait.
const WAITING_FOR: &str = "the viewer: its listener, its viewers, the runs directory and the runs' processes";

/// Typed error for viewer startup and loopback operations.
#[derive(Debug)]
pub enum ViewerError {
    Bind { addr: SocketAddr, io: io::Error },
    RunsDir { path: PathBuf, io: io::Error },
    Spawn { thread: &'static str, io: io::Error },
    /// The listener refused `call`.
    Listener { call: &'static str, io: io::Error },
    /// The runs directory could not be watched, or its watch refused a read.
    Watch { path: PathBuf, error: WatchError },
    /// The runs directory was deleted or moved while it was watched.
    RunsDirGone { path: PathBuf },
    /// The one wait refused.
    Wait(WaitError),
    /// The viewer's thread panicked.
    Panicked,
}

impl core::fmt::Display for ViewerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ViewerError::Bind { addr, io } => write!(f, "blaze viewer: cannot bind {addr}: {io}"),
            ViewerError::RunsDir { path, io } => write!(f, "blaze viewer: runs dir {}: {io}", path.display()),
            ViewerError::Spawn { thread, io } => write!(f, "blaze viewer: spawn {thread}: {io}"),
            ViewerError::Listener { call, io } => write!(f, "blaze viewer: {call}: {io}"),
            ViewerError::Watch { path, error } => write!(f, "blaze viewer: runs dir {}: {error}", path.display()),
            ViewerError::RunsDirGone { path } => {
                write!(f, "blaze viewer: runs dir {} was deleted or moved while watched", path.display())
            }
            ViewerError::Wait(error) => write!(f, "blaze viewer: {error}"),
            ViewerError::Panicked => write!(f, "blaze viewer: its thread panicked"),
        }
    }
}

impl core::error::Error for ViewerError {}

/// Configuration for running the viewer.
#[derive(Clone, Debug)]
pub struct ViewerConfig {
    pub runs_dir: PathBuf,
    pub port: u16,
    pub idle_expiry: Duration,
}

impl ViewerConfig {
    pub fn for_home(home: &Path) -> Self {
        Self {
            runs_dir: home.join(".blaze/v2/runs"),
            port: DEFAULT_PORT,
            idle_expiry: DEFAULT_IDLE_EXPIRY,
        }
    }
}

/// What the handle and the viewer's thread share.
struct Shared {
    active_viewers: AtomicU32,
    stop: AtomicBool,
}

/// Running local viewer server handle (for both CLI `--viewer` and tests).
pub struct ViewerHandle {
    addr: SocketAddr,
    shared: Arc<Shared, SharedAlloc<'static>>,
    thread: Option<JoinHandle<Result<(), ViewerError>>>,
}

impl ViewerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn active_viewers(&self) -> u32 {
        self.shared.active_viewers.load(Ordering::Acquire)
    }

    /// Stops the viewer and waits for its thread: the error it stopped on, if it did.
    pub fn stop(&mut self) -> Result<(), ViewerError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        self.shared.stop.store(true, Ordering::Release);
        // A connection wakes the wait, which then sees the flag. It is refused only when the
        // thread has ended already, which the join reports.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(50));
        joined(thread)
    }

    /// Blocks until the viewer stops by itself (its idle expiry, or an error, returned).
    pub fn finish(mut self) -> Result<(), ViewerError> {
        match self.thread.take() {
            Some(thread) => joined(thread),
            None => Ok(()),
        }
    }

    /// Blocks until the viewer stops (e.g. idle expiry); an error it stopped on is written to
    /// standard error.
    pub fn wait(self) {
        if let Err(error) = self.finish() {
            report(format_args!("{error}"));
        }
    }
}

impl Drop for ViewerHandle {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            report(format_args!("{error}"));
        }
    }
}

fn joined(thread: JoinHandle<Result<(), ViewerError>>) -> Result<(), ViewerError> {
    thread.join().unwrap_or(Err(ViewerError::Panicked))
}

/// [`start_in`] on the process's heap and the monotonic clock, for blaze-launcher's entry until
/// it hands its own (`start_in(cfg, shared, Monotonic::new())` with the `SharedAlloc` its
/// `main` holds); then this goes.
pub fn start(config: ViewerConfig) -> Result<ViewerHandle, ViewerError> {
    start_in(config, SharedAlloc::global(), Monotonic::new())
}

/// Starts the loopback viewer server on `127.0.0.1:<config.port>`, its memory from `alloc` and
/// its time from `clock`.
pub fn start_in<C: Clock + Send + 'static>(
    config: ViewerConfig,
    alloc: SharedAlloc<'static>,
    clock: C,
) -> Result<ViewerHandle, ViewerError> {
    if let Err(io) = fs::create_dir_all(&config.runs_dir) {
        return Err(ViewerError::RunsDir { path: config.runs_dir, io });
    }
    // Hold a shared advisory lock (`LOCK_SH`) on `<runs_dir>/viewer.lock` for the viewer's exact
    // lifetime so `blaze run` and `blaze gc` can probe non-blocking `LOCK_EX` to detect a live
    // viewer without racing a fast-exiting child's `.telem`/`.meta` cleanup.
    let viewer_lock = acquire_viewer_lock(&config.runs_dir, alloc.local());
    let bind_addr = SocketAddr::from(([127, 0, 0, 1], config.port));
    let listener = TcpListener::bind(bind_addr).map_err(|io| ViewerError::Bind { addr: bind_addr, io })?;
    let addr = listener.local_addr().map_err(|io| ViewerError::Bind { addr: bind_addr, io })?;
    listener
        .set_nonblocking(true)
        .map_err(|io| ViewerError::Listener { call: "fcntl(O_NONBLOCK) on the listener", io })?;
    // Made here, so a directory that cannot be watched is this call's error.
    let mut dir = AVec::from_slice_in(config.runs_dir.as_os_str().as_bytes(), alloc.local());
    dir.push(0);
    let watched = match CStr::from_bytes_with_nul(&dir) {
        Ok(dir) => Watch::new(dir),
        Err(_) => return Err(ViewerError::RunsDir { path: config.runs_dir, io: io::Error::from(io::ErrorKind::InvalidInput) }),
    };
    drop(dir);
    let watch = match watched {
        Ok(watch) => watch,
        Err(error) => return Err(ViewerError::Watch { path: config.runs_dir, error }),
    };
    let shared = Arc::new_in(Shared { active_viewers: AtomicU32::new(0), stop: AtomicBool::new(false) }, alloc);
    let theirs = shared.share();
    let thread = thread::Builder::new()
        .name(String::from("blaze-viewer"))
        .spawn(move || {
            let mut served = Loop::new(&config, listener, watch, viewer_lock, alloc.local(), clock.now());
            let ended = served.run(&theirs, &clock);
            drop(served);
            theirs.active_viewers.store(0, Ordering::Release);
            match ended {
                Ended::Stopped | Ended::Expired => Ok(()),
                Ended::DirGone => Err(ViewerError::RunsDirGone { path: config.runs_dir }),
                Ended::Listing(io) => Err(ViewerError::RunsDir { path: config.runs_dir, io }),
                Ended::Watch(error) => Err(ViewerError::Watch { path: config.runs_dir, error }),
                Ended::Wait(error) => Err(ViewerError::Wait(error)),
                Ended::Accept(io) => Err(ViewerError::Listener { call: "accept() on the listener", io }),
            }
        })
        .map_err(|io| ViewerError::Spawn { thread: "blaze-viewer", io })?;
    Ok(ViewerHandle { addr, shared, thread: Some(thread) })
}

/// Why the loop ended.
enum Ended {
    /// The handle asked.
    Stopped,
    /// The idle expiry passed.
    Expired,
    /// The runs directory was deleted or moved.
    DirGone,
    /// Listing the runs directory refused.
    Listing(io::Error),
    Watch(WatchError),
    Wait(WaitError),
    Accept(io::Error),
}

/// A mapped `.telem` file, as the core reads it.
struct Mapped(MappedRing);

const _: () = assert!(core::mem::size_of::<Event>() == pump::EVENT_BYTES);

/// `event`'s bytes as it lies in memory (`#[repr(C)]`, no padding: the page reads them so),
/// appended to `into`.
fn push_event(into: &mut AVec<'_, u8>, event: &Event) {
    into.push(event.kind);
    into.push(event.flags);
    into.extend_from_slice(&event.cpu.to_ne_bytes());
    for word in [event.thread, event.slot_or_op, event._pad2] {
        into.extend_from_slice(&word.to_ne_bytes());
    }
    for word in [event.ts_ns, event.a, event.b, event.c, event.d, event.e] {
        into.extend_from_slice(&word.to_ne_bytes());
    }
}

impl Ring for Mapped {
    fn drain_into(&mut self, most: usize, into: &mut AVec<'_, u8>) -> usize {
        self.0.ring().drain(most, |event| push_event(into, &event))
    }

    fn stats(&self) -> RingStats {
        let stats = self.0.ring().stats();
        RingStats {
            cap: stats.cap,
            pushed: stats.pushed,
            popped: stats.popped,
            dropped: stats.dropped,
            shadow_overflows: stats.shadow_overflows,
            unwound_spans: stats.unwound_spans,
        }
    }
}

/// Where a connection is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Its request's headers are being read.
    Reading,
    /// A viewer: frames go out, close, ping and pong come in.
    Ws,
    /// Its answer goes out; then (`shut`) our side is closed, and it ends at the peer's close.
    Closing { shut: bool },
}

struct Conn<'a> {
    id: ConnId,
    stream: TcpStream,
    phase: Phase,
    inbox: AVec<'a, u8>,
    outbox: AVec<'a, u8>,
    /// What of `outbox` is written.
    sent: usize,
    /// Closed past this unless it moves on ([`REQUEST_WAIT`]); none for a viewer.
    deadline: Option<Duration>,
    /// Over: removed, and its viewer left, at the next sweep.
    gone: bool,
}

/// Advisory shared flock on `<runs_dir>/viewer.lock`, released automatically when `OwnedFd` drops.
fn acquire_viewer_lock(runs_dir: &Path, alloc: Alloc<'_>) -> Option<blaze_sys::posix::io::OwnedFd> {
    use blaze_sys::posix::io::{FlockOp, OpenFlags, flock, open};
    let dir_bytes = runs_dir.as_os_str().as_bytes();
    let leaf = blaze_launch::telemetry::VIEWER_LOCK_LEAF.as_bytes();
    let mut path = AVec::with_capacity_in(dir_bytes.len() + 1 + leaf.len() + 1, alloc);
    path.extend_from_slice(dir_bytes);
    path.push(b'/');
    path.extend_from_slice(leaf);
    path.push(0);
    let cstr = CStr::from_bytes_with_nul(&path).ok()?;
    let sys_path = blaze_sys::Path::new(cstr);
    let owned = open(sys_path, OpenFlags::READ_WRITE | OpenFlags::CREATE | OpenFlags::CLOSE_ON_EXEC, 0o600).ok()?;
    flock(owned.fd(), FlockOp::SharedNonBlock).ok()?;
    Some(owned)
}

/// The viewer's thread: the core, and what it waits on.
struct Loop<'a> {
    alloc: Alloc<'a>,
    idle_expiry: Duration,
    /// The runs directory's path, as bytes.
    runs_dir: AVec<'a, u8>,
    listener: TcpListener,
    watch: Watch,
    /// Shared advisory flock on `<runs_dir>/viewer.lock` held for the viewer's lifetime.
    _viewer_lock: Option<blaze_sys::posix::io::OwnedFd>,
    pump: Pump<'a, Mapped>,
    conns: AVec<'a, Conn<'a>>,
    next_conn: u64,
    fds: AVec<'a, libc::pollfd>,
    seen: AVec<'a, Seen>,
}

impl<'a> Loop<'a> {
    fn new(config: &ViewerConfig, listener: TcpListener, watch: Watch, viewer_lock: Option<blaze_sys::posix::io::OwnedFd>, alloc: Alloc<'a>, now: Duration) -> Loop<'a> {
        Loop {
            alloc,
            idle_expiry: config.idle_expiry,
            runs_dir: AVec::from_slice_in(config.runs_dir.as_os_str().as_bytes(), alloc),
            listener,
            watch,
            _viewer_lock: viewer_lock,
            pump: Pump::new(pump::Config::new(config.idle_expiry), now, alloc),
            conns: AVec::new_in(alloc),
            next_conn: 0,
            fds: AVec::new_in(alloc),
            seen: AVec::new_in(alloc),
        }
    }

    fn run(&mut self, shared: &Shared, clock: &impl Clock) -> Ended {
        loop {
            if shared.stop.load(Ordering::Acquire) {
                return Ended::Stopped;
            }
            let now = clock.now();
            self.pump.advance(now);
            for conn in self.conns.iter_mut() {
                if conn.deadline.is_some_and(|at| at <= now) {
                    conn.gone = true;
                }
            }
            if let Some(ended) = self.settle() {
                return ended;
            }
            shared.active_viewers.store(u32::try_from(self.pump.viewers()).unwrap_or(u32::MAX), Ordering::Release);
            if self.pump.stopped() {
                return Ended::Expired;
            }
            self.fds.clear();
            self.fds.push(watch::pollin(self.listener.as_raw_fd()));
            self.watch.poll_fds(&mut self.fds);
            let watched = self.fds.len();
            for conn in self.conns.iter() {
                let mut events = libc::POLLIN;
                if conn.sent < conn.outbox.len() {
                    events |= libc::POLLOUT;
                }
                self.fds.push(libc::pollfd { fd: conn.stream.as_raw_fd(), events, revents: 0 });
            }
            let deadline = if self.pump.has_actions() { Deadline::PASSED } else { Deadline::at(self.next_wake(now)) };
            match wait::readable(WAITING_FOR, &mut self.fds, Some(deadline), clock) {
                Ok(Waited::Ready(_)) => {}
                Ok(Waited::Deadline) => continue,
                Err(error) => return Ended::Wait(error),
            }
            if let Some(ended) = self.take_ready(watched, clock.now()) {
                return ended;
            }
        }
    }

    /// The wait's end: the core's next deadline, a connection's, and at the latest one idle
    /// expiry from now (rule 18's bound: with a viewer and no live ring nothing else is due).
    fn next_wake(&self, now: Duration) -> Duration {
        let mut at = now.saturating_add(self.idle_expiry);
        if let Some(due) = self.pump.next_deadline() {
            at = at.min(due);
        }
        for conn in self.conns.iter() {
            if let Some(due) = conn.deadline {
                at = at.min(due);
            }
        }
        at
    }

    /// The core's actions taken, queued bytes written, and closed connections swept, until
    /// nothing is left (at most [`ROUNDS`] rounds).
    fn settle(&mut self) -> Option<Ended> {
        for _ in 0..ROUNDS {
            for action in self.pump.take_actions() {
                if let Some(ended) = self.act(action) {
                    return Some(ended);
                }
            }
            for conn in self.conns.iter_mut() {
                if conn.sent < conn.outbox.len() || conn.phase == (Phase::Closing { shut: false }) {
                    flush(conn);
                }
            }
            let pump = &mut self.pump;
            self.conns.retain(|conn| {
                if conn.gone && conn.phase == Phase::Ws {
                    pump.viewer_left(conn.id);
                }
                !conn.gone
            });
            if !self.pump.has_actions() {
                break;
            }
        }
        None
    }

    fn act(&mut self, action: Action<'a>) -> Option<Ended> {
        match action {
            Action::ListDir => {
                if let Err(error) = self.list() {
                    return Some(if error.kind() == io::ErrorKind::NotFound { Ended::DirGone } else { Ended::Listing(error) });
                }
            }
            Action::Attach { stem } => self.attach(stem.as_str()),
            Action::ReadBuild { name, modified } => self.read_build(name.as_str(), modified),
            Action::WatchExit { pid } => match self.watch.watch_exit(pid) {
                Ok(Exit::Watched) => {}
                Ok(Exit::Gone) => self.pump.exited(pid),
                Err(errno) => self.pump.watch_failed(pid, errno.raw(), errno.name()),
            },
            Action::Remove { stem } => self.remove(stem.as_str()),
            Action::SendText { to, text } => {
                if let Some(conn) = self.conns.iter_mut().find(|conn| conn.id == to) {
                    queue_frame(conn, 0x81, text.as_bytes());
                }
            }
            Action::BroadcastText { text } => {
                for conn in self.conns.iter_mut() {
                    queue_frame(conn, 0x81, text.as_bytes());
                }
            }
            Action::BroadcastBinary { bytes } => {
                for conn in self.conns.iter_mut() {
                    queue_frame(conn, 0x82, &bytes);
                }
            }
            Action::Report { error } => report(format_args!("{error}")),
            // The core says it is stopped; the loop ends after this round.
            Action::Stop => {}
        }
        None
    }

    /// The runs directory's rings and builds, to the core.
    fn list(&mut self) -> io::Result<()> {
        let entries = fs::read_dir(Path::new(OsStr::from_bytes(&self.runs_dir)))?;
        let mut names: AVec<'a, (AString<'a>, u64)> = AVec::new_in(self.alloc);
        let mut over = 0usize;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            // A name that is not UTF-8 is no run's or build's: blaze names them.
            let Some(name) = name.to_str() else {
                continue;
            };
            let build = name.ends_with(".build.json");
            if !build && !name.ends_with(".telem") {
                continue;
            }
            if names.len() >= MOST_LISTED {
                over += 1;
                continue;
            }
            let modified = if build {
                match entry.metadata() {
                    Ok(meta) => nanos_since_epoch(meta.modified()?),
                    // Deleted since the listing: not there.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                }
            } else {
                0
            };
            names.push((AString::from_str_in(name, self.alloc), modified));
        }
        if over > 0 {
            report(format_args!("blaze viewer: {over} entries of the runs directory past the bound of {MOST_LISTED}: not read"));
        }
        let mut listed = AVec::with_capacity_in(names.len(), self.alloc);
        for (name, modified) in names.iter() {
            listed.push(Listed { name: name.as_str(), modified: *modified });
        }
        self.pump.listed(&listed);
        Ok(())
    }

    fn attach(&mut self, stem: &str) {
        let telem = file_path(&self.runs_dir, stem, ".telem", self.alloc);
        let Ok(path) = CStr::from_bytes_with_nul(&telem) else {
            self.pump.attach_failed(stem, "its path holds a NUL byte");
            return;
        };
        let ring = match MappedRing::attach_file(path) {
            Ok(ring) => ring,
            Err(error) => {
                let mut why = AString::new_in(self.alloc);
                let _ = write!(why, "{error}");
                self.pump.attach_failed(stem, why.as_str());
                return;
            }
        };
        // The run's start where its meta says none: the ring file's modification time (it is
        // made as the run starts), 0 when that cannot be read; never this process's clock.
        let started_ms = fs::metadata(path_of(&telem))
            .and_then(|meta| meta.modified())
            .map_or(0, |at| nanos_since_epoch(at) / 1_000_000);
        let meta = file_path(&self.runs_dir, stem, ".meta", self.alloc);
        let bytes = match read_bounded(path_of(&meta), MOST_META_BYTES, self.alloc) {
            Ok(bytes) => Some(bytes),
            // No meta: the run is listed by its stem, with no process to watch.
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                report(format_args!("blaze viewer: run {stem}: {stem}.meta: {error}"));
                None
            }
        };
        let text = match bytes.as_deref().map(core::str::from_utf8) {
            Some(Ok(text)) => Some(text),
            Some(Err(_)) => {
                report(format_args!("blaze viewer: run {stem}: {stem}.meta is not UTF-8"));
                None
            }
            None => None,
        };
        let meta = RunMeta::parse(text, stem, started_ms, self.alloc);
        self.pump.attached(stem, Mapped(ring), meta);
    }

    fn read_build(&mut self, name: &str, modified: u64) {
        let path = file_path(&self.runs_dir, name, "", self.alloc);
        match read_bounded(path_of(&path), MOST_BUILD_BYTES, self.alloc) {
            Ok(bytes) => self.pump.build_read(name, modified, &bytes),
            // Deleted since the listing: not there.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => report(format_args!("blaze viewer: build {name}: {error}")),
        }
    }

    fn remove(&mut self, stem: &str) {
        for suffix in [".telem", ".meta"] {
            let path = file_path(&self.runs_dir, stem, suffix, self.alloc);
            match fs::remove_file(path_of(&path)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => report(format_args!("blaze viewer: run {stem}: cannot delete {stem}{suffix}: {error}")),
            }
        }
    }

    /// What the wait said is ready: the watch's events to the core, then each connection's
    /// reads and writes, then new connections. `watched` is where the connections' pollfds
    /// start.
    fn take_ready(&mut self, watched: usize, now: Duration) -> Option<Ended> {
        let listener_ready = self.fds.first().is_some_and(|fd| fd.revents != 0);
        if let Some(polled) = self.fds.get(1..watched) {
            self.seen.clear();
            if let Err(error) = self.watch.take(polled, &mut self.seen) {
                return Some(Ended::Watch(error));
            }
            for &seen in self.seen.iter() {
                match seen {
                    Seen::DirChanged => self.pump.dir_changed(),
                    Seen::DirGone => return Some(Ended::DirGone),
                    Seen::Exited(pid) => self.pump.exited(pid),
                }
            }
        }
        let polled = self.fds.get(watched..).unwrap_or(&[]);
        for (conn, fd) in self.conns.iter_mut().zip(polled) {
            if fd.revents == 0 {
                continue;
            }
            if fd.revents & libc::POLLNVAL != 0 {
                conn.gone = true;
                continue;
            }
            if fd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                read_conn(conn, &mut self.pump, self.alloc, now);
            }
            if fd.revents & libc::POLLOUT != 0 && !conn.gone {
                flush(conn);
            }
        }
        if listener_ready {
            return self.accept(now);
        }
        None
    }

    /// The listener's waiting connections taken, at most [`MOST_CONNS`] a wake.
    fn accept(&mut self, now: Duration) -> Option<Ended> {
        for _ in 0..MOST_CONNS {
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return None,
                // The peer gave up before it was taken, or a signal came: the next one.
                Err(error) if matches!(error.kind(), io::ErrorKind::ConnectionAborted | io::ErrorKind::Interrupted) => continue,
                Err(error) => return Some(Ended::Accept(error)),
            };
            if self.conns.len() >= MOST_CONNS {
                report(format_args!("blaze viewer: {MOST_CONNS} connections open: one more closed unanswered"));
                continue;
            }
            if let Err(error) = stream.set_nonblocking(true).and_then(|()| stream.set_nodelay(true)) {
                report(format_args!("blaze viewer: a connection refused its options, closed: {error}"));
                continue;
            }
            let id = ConnId(self.next_conn);
            self.next_conn = self.next_conn.wrapping_add(1);
            self.conns.push(Conn {
                id,
                stream,
                phase: Phase::Reading,
                inbox: AVec::new_in(self.alloc),
                outbox: AVec::new_in(self.alloc),
                sent: 0,
                deadline: Some(now.saturating_add(REQUEST_WAIT)),
                gone: false,
            });
        }
        None
    }
}

/// What `conn` sent, read without waiting, then taken: a request answered, a viewer's frames.
fn read_conn<'a>(conn: &mut Conn<'a>, pump: &mut Pump<'a, Mapped>, alloc: Alloc<'a>, now: Duration) {
    let room = match conn.phase {
        Phase::Reading => MAX_HTTP_HEADER_BYTES,
        Phase::Ws => MOST_CLIENT_FRAME + MOST_FRAME_HEADER,
        Phase::Closing { .. } => usize::MAX,
    };
    let mut chunk = [0u8; 4096];
    for _ in 0..READS_A_WAKE {
        if conn.inbox.len() >= room {
            break;
        }
        match conn.stream.read(&mut chunk) {
            Ok(0) => {
                conn.gone = true;
                return;
            }
            Ok(got) => {
                // A closing connection's bytes are read only to see its close.
                if !matches!(conn.phase, Phase::Closing { .. }) {
                    conn.inbox.extend_from_slice(chunk.get(..got).unwrap_or(&[]));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            // Reset by the peer: the connection is over.
            Err(_) => {
                conn.gone = true;
                return;
            }
        }
    }
    match conn.phase {
        Phase::Reading => answer(conn, pump, alloc, now),
        Phase::Ws => take_frames(conn, pump, now),
        Phase::Closing { .. } => {}
    }
}

/// `conn`'s request answered once its headers are in: a viewer's upgrade, or one answer and
/// the close.
fn answer<'a>(conn: &mut Conn<'a>, pump: &mut Pump<'a, Mapped>, alloc: Alloc<'a>, now: Duration) {
    let Some(end) = find_header_end(&conn.inbox) else {
        if conn.inbox.len() >= MAX_HTTP_HEADER_BYTES {
            conn.gone = true;
        }
        return;
    };
    let Some(Ok(head)) = conn.inbox.get(..end).map(core::str::from_utf8) else {
        conn.gone = true;
        return;
    };
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let mut origin: Option<&str> = None;
    let mut ws_key: Option<&str> = None;
    let mut upgrade_ws = false;
    for line in lines {
        let Some((name, val)) = line.split_once(':') else {
            continue;
        };
        let val = val.trim();
        if name.eq_ignore_ascii_case("origin") {
            origin = Some(val);
        } else if name.eq_ignore_ascii_case("sec-websocket-key") {
            ws_key = Some(val);
        } else if name.eq_ignore_ascii_case("upgrade") && val.eq_ignore_ascii_case("websocket") {
            upgrade_ws = true;
        }
    }
    let cors = allowed_cors_origin(origin);

    if method == "GET" && path == "/ws" && upgrade_ws {
        match ws_key {
            Some(key) if pump.viewers() < MAX_CLIENTS => {
                let accept = ws_accept_key(key);
                let mut reply = AString::new_in(alloc);
                let _ = write!(
                    reply,
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                );
                conn.outbox.extend_from_slice(reply.as_bytes());
                // What follows the request is the viewer's first frames.
                let rest = AVec::from_slice_in(conn.inbox.get(end..).unwrap_or(&[]), alloc);
                conn.inbox = rest;
                conn.phase = Phase::Ws;
                conn.deadline = None;
                // Its runs and builds are queued after the handshake.
                pump.viewer_joined(conn.id);
                return;
            }
            Some(_) => push_http(&mut conn.outbox, "503 Service Unavailable", "text/plain", cors, b"viewer client cap reached", alloc),
            None => push_http(&mut conn.outbox, "400 Bad Request", "text/plain", cors, b"missing Sec-WebSocket-Key", alloc),
        }
    } else {
        route(method, path, query, cors, &mut conn.outbox, pump, alloc);
    }
    close(conn, now);
}

/// The answer to `method path?query`, queued in `out`.
fn route<'a>(
    method: &str,
    path: &str,
    query: &str,
    cors: &str,
    out: &mut AVec<'a, u8>,
    pump: &Pump<'a, Mapped>,
    alloc: Alloc<'a>,
) {
    match (method, path) {
        ("OPTIONS", _) => {
            let mut head = AString::new_in(alloc);
            let _ = write!(
                head,
                "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: {cors}\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: content-type\r\nAccess-Control-Allow-Private-Network: true\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            out.extend_from_slice(head.as_bytes());
        }
        ("GET", "/api/info") => {
            let mut body = AString::new_in(alloc);
            let _ = write!(body, r#"{{"version":1,"active_viewers":{}}}"#, pump.viewers());
            push_http(out, "200 OK", "application/json", cors, body.as_bytes(), alloc);
        }
        ("GET", "/api/runs") => push_http(out, "200 OK", "application/json", cors, pump.runs_json().as_bytes(), alloc),
        ("GET", "/api/builds") => push_http(out, "200 OK", "application/json", cors, pump.builds_json().as_bytes(), alloc),
        ("GET", "/api/topology") => push_http(out, "200 OK", "application/json", cors, host_topology_json().as_bytes(), alloc),
        ("GET", "/api/source") => match read_source_snippet_json(query) {
            Some(body) => push_http(out, "200 OK", "application/json", cors, body.as_bytes(), alloc),
            None => push_http(out, "404 Not Found", "application/json", cors, br#"{"error":"source unavailable"}"#, alloc),
        },
        ("GET", "/api/resources") => {
            let mut body = AString::new_in(alloc);
            let status = match resources::query_pid(query).and_then(|pid| procfd::gather(pid, alloc)) {
                Ok(facts) => {
                    facts.write_json(&mut body);
                    "200 OK"
                }
                Err(error) => {
                    error.write_json(&mut body);
                    error.status()
                }
            };
            push_http(out, status, "application/json", cors, body.as_bytes(), alloc);
        }
        ("POST", "/api/toggle") => push_http(out, "200 OK", "application/json", cors, br#"{"ok":true}"#, alloc),
        ("GET", "/" | "/explore") => {
            push_http(out, "200 OK", "text/html; charset=utf-8", cors, LOCAL_EXPLORE_HTML.as_bytes(), alloc);
        }
        _ => push_http(out, "404 Not Found", "text/plain", cors, b"not found", alloc),
    }
}

/// One HTTP answer, closing its connection, queued in `out`.
fn push_http(out: &mut AVec<'_, u8>, status: &str, content_type: &str, cors_origin: &str, body: &[u8], alloc: Alloc<'_>) {
    let mut head = AString::new_in(alloc);
    let _ = write!(
        head,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nAccess-Control-Allow-Origin: {cors_origin}\r\nAccess-Control-Allow-Private-Network: true\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(body);
}

/// `conn` answered: what is queued goes out, then it closes.
fn close(conn: &mut Conn<'_>, now: Duration) {
    conn.phase = Phase::Closing { shut: false };
    conn.deadline = Some(now.saturating_add(REQUEST_WAIT));
    conn.inbox.clear();
}

/// A viewer's whole frames taken from the front of its inbox: a close answered and the
/// connection closed, a ping answered with its pong, the rest passed over.
fn take_frames<'a>(conn: &mut Conn<'a>, pump: &mut Pump<'a, Mapped>, now: Duration) {
    let mut at = 0;
    while let Some(rest) = conn.inbox.get(at..) {
        let (opcode, size, start, len, mask) = match client_frame(rest) {
            ClientFrame::Partial => break,
            ClientFrame::Refused => {
                conn.gone = true;
                return;
            }
            ClientFrame::Whole { opcode, size, start, len, mask } => (opcode, size, start, len, mask),
        };
        match opcode {
            0x8 => {
                push_ws_frame(&mut conn.outbox, 0x88, &[]);
                pump.viewer_left(conn.id);
                close(conn, now);
                return;
            }
            0x9 => {
                let mut pong = [0u8; 125];
                let wrote = unmask(rest.get(start..start.saturating_add(len)).unwrap_or(&[]), mask, &mut pong);
                push_ws_frame(&mut conn.outbox, 0x8a, pong.get(..wrote).unwrap_or(&[]));
            }
            _ => {}
        }
        at += size;
    }
    // `at` is at most the inbox's length: each frame taken was whole in it.
    let left = conn.inbox.len().saturating_sub(at);
    if at > 0 && left > 0 {
        conn.inbox.copy_within(at.., 0);
    }
    conn.inbox.truncate(left);
}

/// A client's frame at the front of what it sent (RFC 6455 section 5.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClientFrame {
    /// More bytes are needed.
    Partial,
    /// Unmasked (a client masks every frame), longer than [`MOST_CLIENT_FRAME`], or a control
    /// frame longer than 125 bytes: the connection is closed.
    Refused,
    /// `size` bytes in all; the payload is `len` bytes from `start`, masked with `mask`.
    Whole { opcode: u8, size: usize, start: usize, len: usize, mask: [u8; 4] },
}

fn client_frame(bytes: &[u8]) -> ClientFrame {
    let (Some(&first), Some(&second)) = (bytes.first(), bytes.get(1)) else {
        return ClientFrame::Partial;
    };
    if second & 0x80 == 0 {
        return ClientFrame::Refused;
    }
    let (len, at) = match second & 0x7f {
        126 => match bytes.get(2..4) {
            Some(&[high, low]) => (u64::from(u16::from_be_bytes([high, low])), 4),
            _ => return ClientFrame::Partial,
        },
        127 => match bytes.get(2..10).and_then(|word| <[u8; 8]>::try_from(word).ok()) {
            Some(word) => (u64::from_be_bytes(word), 10),
            None => return ClientFrame::Partial,
        },
        short => (u64::from(short), 2),
    };
    let opcode = first & 0x0f;
    let Ok(len) = usize::try_from(len) else {
        return ClientFrame::Refused;
    };
    if len > MOST_CLIENT_FRAME || (opcode & 0x8 != 0 && len > 125) {
        return ClientFrame::Refused;
    }
    let Some(mask) = bytes.get(at..at + 4).and_then(|mask| <[u8; 4]>::try_from(mask).ok()) else {
        return ClientFrame::Partial;
    };
    let start = at + 4;
    if bytes.len() < start + len {
        return ClientFrame::Partial;
    }
    ClientFrame::Whole { opcode, size: start + len, start, len, mask }
}

/// `masked` unmasked with `mask` into `into`, as much as fits: how many bytes.
fn unmask(masked: &[u8], mask: [u8; 4], into: &mut [u8]) -> usize {
    let wrote = into.len().min(masked.len());
    for ((out, byte), key) in into.iter_mut().zip(masked).zip(mask.iter().cycle()) {
        *out = byte ^ key;
    }
    wrote
}

/// A server-to-client (unmasked) RFC 6455 frame (`0x81` text, `0x82` binary), appended to `out`.
fn push_ws_frame(out: &mut AVec<'_, u8>, fin_opcode: u8, payload: &[u8]) {
    out.push(fin_opcode);
    let len = payload.len();
    if let Ok(short) = u8::try_from(len)
        && short <= 125
    {
        out.push(short);
    } else if let Ok(medium) = u16::try_from(len) {
        out.push(126);
        out.extend_from_slice(&medium.to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes());
    }
    out.extend_from_slice(payload);
}

/// A frame queued for viewer `conn`; one that would put it [`MOST_QUEUED`] behind closes it.
fn queue_frame(conn: &mut Conn<'_>, fin_opcode: u8, payload: &[u8]) {
    if conn.phase != Phase::Ws || conn.gone {
        return;
    }
    let queued = conn.outbox.len().saturating_sub(conn.sent);
    if queued.saturating_add(payload.len()) > MOST_QUEUED {
        report(format_args!("blaze viewer: viewer {} is {queued} bytes behind: closed", conn.id.0));
        conn.gone = true;
        return;
    }
    push_ws_frame(&mut conn.outbox, fin_opcode, payload);
}

/// What is queued for `conn` written without waiting; an answered connection's side closed.
fn flush(conn: &mut Conn<'_>) {
    // Bounded by what is queued: each write takes some of it, or the loop ends.
    while let Some(left) = conn.outbox.get(conn.sent..)
        && !left.is_empty()
    {
        match conn.stream.write(left) {
            Ok(0) => {
                conn.gone = true;
                return;
            }
            Ok(wrote) => conn.sent += wrote,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            // Reset by the peer: the connection is over.
            Err(_) => {
                conn.gone = true;
                return;
            }
        }
    }
    if conn.sent >= conn.outbox.len() {
        conn.outbox.clear();
        conn.sent = 0;
    } else if conn.sent > conn.outbox.len() / 2 {
        let left = conn.outbox.len() - conn.sent;
        conn.outbox.copy_within(conn.sent.., 0);
        conn.outbox.truncate(left);
        conn.sent = 0;
    }
    if conn.phase == (Phase::Closing { shut: false }) && conn.outbox.is_empty() {
        // All of the answer is out: our side is closed, and the peer's close ends it (closing
        // with its bytes unread could reset the answer away).
        match conn.stream.shutdown(Shutdown::Write) {
            Ok(()) => conn.phase = Phase::Closing { shut: true },
            Err(_) => conn.gone = true,
        }
    }
}

/// A line on standard error: what refused, for whoever reads the viewer's log.
fn report(what: core::fmt::Arguments<'_>) {
    // Standard error is the last place a report can go: a failed write of it has none.
    let _ = writeln!(io::stderr().lock(), "{what}");
}

/// `<dir>/<name><suffix>` and a NUL: a `CStr`, or (without the NUL, [`path_of`]) a `Path`.
fn file_path<'a>(dir: &[u8], name: &str, suffix: &str, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut path = AVec::with_capacity_in(dir.len() + name.len() + suffix.len() + 2, alloc);
    path.extend_from_slice(dir);
    path.push(b'/');
    path.extend_from_slice(name.as_bytes());
    path.extend_from_slice(suffix.as_bytes());
    path.push(0);
    path
}

fn path_of(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes.strip_suffix(&[0u8]).unwrap_or(bytes)))
}

fn nanos_since_epoch(at: SystemTime) -> u64 {
    // A time before the epoch is 0.
    at.duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_nanos()).unwrap_or(u64::MAX))
}

/// The file at `path`, refused (`FileTooLarge`) past `most` bytes.
fn read_bounded<'a, P: AsRef<Path> + ?Sized>(path: &P, most: usize, alloc: Alloc<'a>) -> io::Result<AVec<'a, u8>> {
    let mut file = fs::File::open(path)?;
    let mut bytes = AVec::new_in(alloc);
    let mut chunk = [0u8; 8192];
    // Bounded by `most`: each read adds to `bytes` or ends the loop.
    loop {
        let got = match file.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(got) => got,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if bytes.len().saturating_add(got) > most {
            return Err(io::Error::from(io::ErrorKind::FileTooLarge));
        }
        bytes.extend_from_slice(chunk.get(..got).unwrap_or(&[]));
    }
}

fn allowed_cors_origin(origin: Option<&str>) -> &str {
    match origin {
        Some(o) if o == "https://ix.dev" || o.starts_with("http://127.0.0.1:") || o.starts_with("http://localhost:") => o,
        _ => "https://ix.dev",
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Queries live host CPU, P-core/E-core, cache, memory, and NUMA topology (cached once).
pub fn host_topology_json() -> String {
    #[cfg(target_os = "macos")]
    {
        fn sysctl_u64(name: &core::ffi::CStr) -> u64 {
            let mut val: u64 = 0;
            let mut len = core::mem::size_of::<u64>();
            // SAFETY: `sysctlbyname` writes up to `len` bytes into `val`.
            let rc = unsafe {
                libc::sysctlbyname(
                    name.as_ptr(),
                    (&mut val as *mut u64).cast(),
                    &mut len,
                    core::ptr::null_mut(),
                    0,
                )
            };
            if rc == 0 { val } else { 0 }
        }
        fn sysctl_str(name: &core::ffi::CStr) -> String {
            let mut buf = [0u8; 128];
            let mut len = buf.len();
            // SAFETY: `sysctlbyname` writes a NUL-terminated string into `buf`.
            let rc = unsafe {
                libc::sysctlbyname(
                    name.as_ptr(),
                    buf.as_mut_ptr().cast(),
                    &mut len,
                    core::ptr::null_mut(),
                    0,
                )
            };
            if rc == 0 && len > 0 {
                let end = buf[..len].iter().position(|&b| b == 0).unwrap_or(len);
                String::from_utf8_lossy(&buf[..end]).into_owned()
            } else {
                String::from("Apple Silicon")
            }
        }
        let brand = sysctl_str(c"machdep.cpu.brand_string");
        let ncpu = sysctl_u64(c"hw.ncpu").max(1);
        let p_cores = sysctl_u64(c"hw.perflevel0.physicalcpu");
        let e_cores = sysctl_u64(c"hw.perflevel1.physicalcpu");
        let l1d = sysctl_u64(c"hw.perflevel0.l1dcachesize").max(sysctl_u64(c"hw.l1dcachesize"));
        let l2 = sysctl_u64(c"hw.perflevel0.l2cachesize").max(sysctl_u64(c"hw.l2cachesize"));
        let mem = sysctl_u64(c"hw.memsize");
        format!(
            r#"{{"arch":"aarch64","os":"macos","brand":"{}","ncpu":{ncpu},"p_cores":{p_cores},"e_cores":{e_cores},"numa_nodes":1,"l1d_bytes":{l1d},"l2_bytes":{l2},"mem_bytes":{mem}}}"#,
            escape_json(&brand),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let ncpu = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1);
        let numa_nodes = std::fs::read_dir("/sys/devices/system/node")
            .ok()
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_name().to_str().is_some_and(|s| s.starts_with("node")))
                    .count()
                    .max(1)
            })
            .unwrap_or(1);
        format!(
            r#"{{"arch":"{}","os":"linux","brand":"Linux Host","ncpu":{ncpu},"p_cores":{ncpu},"e_cores":0,"numa_nodes":{numa_nodes},"l1d_bytes":0,"l2_bytes":0,"mem_bytes":0}}"#,
            std::env::consts::ARCH,
        )
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads a bounded 7-line source snippet around `line` for hover code peek (`file` must be a
/// regular `.rs`/`.ts`/`.c`/`.h` source file without `..` traversal, <= 256 KiB).
fn read_source_snippet_json(query: &str) -> Option<String> {
    let mut file: Option<String> = None;
    let mut line: usize = 1;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k == "file" {
            file = Some(percent_decode(v));
        } else if k == "line" {
            if let Ok(n) = v.parse::<usize>() {
                line = n.max(1);
            }
        }
    }
    let path_str = file?;
    if path_str.contains("..") {
        return None;
    }
    let path = Path::new(&path_str);
    let ext = path.extension().and_then(|s| s.to_str())?;
    if !matches!(ext, "rs" | "ts" | "js" | "c" | "h" | "cpp" | "toml") {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 512 * 1024 {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let mut out = format!(r#"{{"file":"{}","line":{line},"full":true,"lines":["#, escape_json(&path_str));
    let mut first = true;
    for (idx, text) in content.lines().take(2500).enumerate() {
        let ln = idx + 1;
        if !first {
            out.push(',');
        }
        first = false;
        let _ = write!(out, r#"{{"n":{ln},"text":"{}"}}"#, escape_json(text));
    }
    out.push_str("]}");
    Some(out)
}

/// Probes `http://127.0.0.1:<port>/api/info` with a tight loopback timeout.
/// Returns `Some(active_viewers)` when a live viewer responded.
pub fn probe_active_viewers(port: u16) -> Option<u32> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(120)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(150)));
    stream
        .write_all(b"GET /api/info HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut resp = [0u8; 1024];
    let n = stream.read(&mut resp).ok()?;
    let text = core::str::from_utf8(&resp[..n]).ok()?;
    let key = r#""active_viewers":"#;
    let at = text.find(key)? + key.len();
    let digits: String = text[at..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().ok()
}

/// Opens `url` in the user's default browser (`open` on macOS, `xdg-open` on Linux).
pub fn open_browser_url(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let cmd = "xdg-open";
    std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// RFC 6455 `Sec-WebSocket-Accept = base64(SHA-1(key || GUID))`.
pub fn ws_accept_key(key: &str) -> String {
    let mut input = Vec::with_capacity(key.len() + WS_GUID.len());
    input.extend_from_slice(key.as_bytes());
    input.extend_from_slice(WS_GUID.as_bytes());
    let digest = sha1(&input);
    base64_encode(&digest)
}

/// Minimal FIPS 180-4 SHA-1 solely for the RFC 6455 WebSocket handshake (not security crypto).
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = usize::from(chunk[0]);
        let b1 = if chunk.len() > 1 { usize::from(chunk[1]) } else { 0 };
        let b2 = if chunk.len() > 2 { usize::from(chunk[2]) } else { 0 };
        out.push(char::from(TABLE[b0 >> 2]));
        out.push(char::from(TABLE[((b0 & 0x03) << 4) | (b1 >> 4)]));
        out.push(if chunk.len() > 1 {
            char::from(TABLE[((b1 & 0x0f) << 2) | (b2 >> 6)])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(TABLE[b2 & 0x3f])
        } else {
            '='
        });
    }
    out
}

const LOCAL_EXPLORE_HTML: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>blaze explore (local)</title>
<style>body{background:#0b0c10;color:#e6e8ef;font:13px/1.5 ui-monospace,monospace;margin:0;padding:16px}
.hdr{display:flex;gap:12px;align-items:center;border-bottom:1px solid #232734;padding-bottom:10px;margin-bottom:12px}
.pill{background:#182436;color:#7cc4ff;padding:2px 8px;border-radius:999px;font-size:11px}
pre{background:#12141c;border:1px solid #232734;border-radius:8px;padding:12px;max-height:75vh;overflow:auto}</style></head>
<body><div class="hdr"><strong>blaze explore · local fallback</strong><span id="st" class="pill">connecting…</span>
<a href="https://ix.dev/blaze/explore" style="color:#7cc4ff;margin-left:auto">Open hosted ix.dev/blaze/explore ↗</a></div>
<pre id="log"></pre><script>
const st=document.getElementById('st'),log=document.getElementById('log');
function go(){const ws=new WebSocket(`ws://${location.host}/ws`);
ws.onopen=()=>{st.textContent='live (websocket)';};
ws.onclose=()=>{st.textContent='reconnecting…';setTimeout(go,500);};
ws.onmessage=(e)=>{log.textContent=e.data+'\n'+log.textContent.slice(0,24000);};}go();
</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_launch::telemetry::{AllocOp, EventKind};
    use std::{env, ffi::CString, process};

    #[test]
    fn rfc6455_accept_key_matches_spec_vector() {
        // RFC 6455 Section 1.3 official test vector.
        assert_eq!(ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn a_client_frame_is_taken_whole_and_unmasked() {
        let mask = [0x11, 0x22, 0x33, 0x44];
        let ping = [0x89, 0x82, 0x11, 0x22, 0x33, 0x44, b'h' ^ 0x11, b'i' ^ 0x22];
        assert_eq!(client_frame(&ping), ClientFrame::Whole { opcode: 0x9, size: 8, start: 6, len: 2, mask });
        for cut in 0..ping.len() {
            assert_eq!(client_frame(&ping[..cut]), ClientFrame::Partial, "{cut} bytes of a ping");
        }
        let mut payload = [0u8; 125];
        let wrote = unmask(&ping[6..], mask, &mut payload);
        assert_eq!(&payload[..wrote], b"hi");
        // A client's frame is masked; a control frame is at most 125 bytes; a frame at most
        // MOST_CLIENT_FRAME.
        assert_eq!(client_frame(&[0x81, 0x02, b'h', b'i']), ClientFrame::Refused);
        assert_eq!(client_frame(&[0x89, 0x80 | 126, 0x00, 126]), ClientFrame::Refused);
        assert_eq!(client_frame(&[0x82, 0x80 | 127, 0, 0, 0, 0, 0, 0x10, 0, 0]), ClientFrame::Refused);
        // Each length form of a server's frame.
        let alloc = blaze_own::testing::heap().alloc();
        for (len, header) in [(0usize, 2usize), (125, 2), (126, 4), (65535, 4), (65536, 10)] {
            let mut out = AVec::new_in(alloc);
            push_ws_frame(&mut out, 0x82, &AVec::from_elem_in(7u8, len, alloc));
            assert_eq!(out.len(), header + len, "a frame of {len} bytes");
        }
    }

    #[test]
    fn viewer_tracks_active_viewers_streams_live_ring_events_and_expires_idle() {
        let dir = env::temp_dir().join(format!("blaze-viewer-test-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create runs dir");

        let config = ViewerConfig { runs_dir: dir.to_path_buf(), port: 0, idle_expiry: Duration::from_secs(30) };
        let mut handle = start_in(config, blaze_own::testing::heap().shared(), Monotonic::new()).expect("start viewer");
        let port = handle.addr().port();

        // 1. Before any tab connects: active_viewers == 0 -> `blaze run --open` would open browser.
        assert_eq!(probe_active_viewers(port), Some(0));

        // 2. Connect a real RFC 6455 WebSocket client.
        let mut ws = TcpStream::connect(handle.addr()).expect("connect ws");
        ws.set_read_timeout(Some(Duration::from_secs(3))).expect("timeout");
        ws.write_all(
            b"GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: https://ix.dev\r\n\r\n",
        )
        .expect("send upgrade");

        let mut head = [0u8; 512];
        let n = ws.read(&mut head).expect("read handshake");
        let hdr_end = find_header_end(&head[..n]).expect("complete http response header");
        let text = core::str::from_utf8(&head[..hdr_end]).expect("utf8 handshake");
        assert!(text.starts_with("HTTP/1.1 101 Switching Protocols"), "unexpected response: {text}");
        assert!(text.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "bad accept: {text}");

        // 3. While the tab is connected: active_viewers == 1 -> blaze run skips opening duplicate tab!
        assert_eq!(probe_active_viewers(port), Some(1));

        // 4. Create a shared-memory telemetry ring + build snapshot in `runs_dir` and push live Span + Alloc events.
        let telem_path = dir.join("run-live-test.telem");
        let meta_path = dir.join("run-live-test.meta");
        let build_path = dir.join("run-live-test.build.json");
        fs::write(&meta_path, format!("pid={}\npackage=demo-app\nstarted_ms=12345\n", process::id())).expect("write meta");
        fs::write(
            &build_path,
            r#"{"id":"run-live-test","package":"demo-app","command":"blaze run","status":"running","exit_code":null,"errors":0,"warnings":1,"reached":40,"found":36,"made":4,"placed":36,"resolved":4,"frontend_built":"no","recompile_reason":"Certificate changed","phases":[]}"#,
        )
        .expect("write build json");
        let telem_cpath = CString::new(telem_path.as_os_str().as_encoded_bytes()).expect("cpath");
        let ring = MappedRing::create(&telem_cpath, 64).expect("create ring");
        assert!(ring.ring().try_push(Event::span_enter(
            1,
            blaze_launch::Slot(7),
            1000,
            0x4444,
            blaze_launch::telemetry::SpanId(99),
            blaze_launch::telemetry::SpanId(0),
        )));
        assert!(ring.ring().try_push(Event::alloc(
            1,
            AllocOp::Malloc,
            1050,
            0xdeadbeef,
            512,
            [0x1000, 0x2000, 0x3000],
        )));

        // 5. Read WebSocket frames until we see both the live build snapshot and the packed
        //    binary event batch frame (`0x82`, magic `'B'`, version `1`, 2 x 64B Events). Each
        //    read waits at most the stream's 3 s timeout, and at most 256 are made.
        let mut raw_stream = AVec::new_in(blaze_own::testing::heap().alloc());
        let mut buf = [0u8; 4096];
        let mut saw_binary_events = false;
        let mut saw_builds = false;
        for _ in 0..256 {
            if saw_binary_events && saw_builds {
                break;
            }
            let k = match ws.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(k) => k,
            };
            raw_stream.extend_from_slice(&buf[..k]);
            if !saw_builds && raw_stream.windows(19).any(|w| w == br#"Certificate changed"#) {
                saw_builds = true;
            }
            // Locate binary batch header: [b'B', 1, 13, 0, b"run-live-test"...]
            if !saw_binary_events
                && let Some(pos) = raw_stream.windows(17).position(|w| {
                    w[0] == pump::BIN_EVENT_MAGIC && w[1] == pump::BIN_EVENT_VERSION && &w[4..17] == b"run-live-test"
                })
            {
                let hdr_len = (4 + 13 + 7) & !7; // 24 bytes
                if raw_stream.len() >= pos + hdr_len + 128 {
                    let ev_bytes = &raw_stream[pos + hdr_len..pos + hdr_len + 128];
                    assert_eq!(ev_bytes[0], EventKind::SpanEnter as u8, "first binary event must be SpanEnter");
                    assert_eq!(ev_bytes[64], EventKind::Alloc as u8, "second binary event must be Alloc");
                    let alloc_size = u64::from_le_bytes(ev_bytes[64 + 32..64 + 40].try_into().expect("8 bytes"));
                    assert_eq!(alloc_size, 512, "binary Alloc event size must match 512");
                    saw_binary_events = true;
                }
            }
        }
        assert!(saw_binary_events, "did not receive packed binary event batch in ws stream");
        assert!(saw_builds, "did not receive live build.json snapshot in ws stream");

        // 6. Close client -> active_viewers drops back to 0. Each probe is one round trip
        //    through the viewer's loop, which takes the close before or with it.
        drop(ws);
        let mut active = probe_active_viewers(port);
        for _ in 0..64 {
            if active == Some(0) {
                break;
            }
            active = probe_active_viewers(port);
        }
        assert_eq!(active, Some(0));
        assert_eq!(handle.active_viewers(), 0);

        handle.stop().expect("the viewer stops on its handle's word");
        let _ = fs::remove_dir_all(&dir);
    }
}
