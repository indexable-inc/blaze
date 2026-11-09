//! The daemon link: harnessd's unix socket (`~/.harness/harnessd.sock`), harness-proto's schema (deterministic-CBOR
//! messages in `[type u8][len u32 BE][payload]` frames, each connection opened by a hello naming the schema), spoken from
//! plain threads.
//!
//! - [`ensure_daemon`] connects, or starts `harness daemon` the way the CLI's `ensure_daemon` does (setsid, output appended
//!   to `~/.harness/daemon.log`) and waits for the socket.
//! - A list thread asks `list` (with every past chat) and `runs` and sends the world [`Msg::List`] and [`Msg::Runs`]: again each time the
//!   daemon's change feed (`watch`) says something changed, or every [`POLL`] against an older daemon without one.
//! - Each attached object has a thread reading its stream (`attach`: `attached`, then the snapshot and live terminal
//!   bytes, or a chat's T_EVENT frames, and the exit) into [`Msg::Data`] and [`Msg::Exit`]; its write half takes the
//!   keyboard (`T_DATA`), resizes (`Up::Resize`) and the detach (`Up::Detach`).
use harness_native_model::{LinkState, Mailbox, Msg};
use harness_proto::frame::{self, T_CBOR, T_DATA, T_EVENT};
use harness_proto::event::ModelChoice;
use harness_proto::msg::{Down, Reply, Request, RunReq, Up};
use harness_proto::peek::{GhChecks, GhItem, GhRef, PageMeta, REFS_MAX};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const POLL: Duration = Duration::from_millis(400);

/// How many of the newest shell runs the poll asks for.
const RUNS: usize = 100;

/// How many older runs one page of the Archive's history asks for.
pub const PAGE: usize = 100;

/// How long one `run_read` waits for new output.
const RUN_WAIT_MS: u64 = 1000;

pub fn home() -> PathBuf {
    std::env::var_os("HARNESS_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".harness")
    })
}

pub fn sock() -> PathBuf {
    home().join("harnessd.sock")
}

/// One message's frame.
fn up(m: &Up) -> Vec<u8> {
    frame::frame(T_CBOR, &harness_proto::cbor::encode(m))
}

fn decode_down(payload: &[u8]) -> std::io::Result<Down> {
    harness_proto::cbor::decode(payload).map_err(|e| {
        std::io::Error::other(format!("harnessd sent a message that is not harness schema {}'s: {e}", harness_proto::SCHEMA))
    })
}

/// The daemon's next message.
fn recv(r: &mut impl Read) -> std::io::Result<Down> {
    match frame::read(r)? {
        Some((T_CBOR, body)) => decode_down(&body),
        Some((t, _)) => Err(std::io::Error::other(format!("harnessd sent a frame of type {t} where a message belongs"))),
        None => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "harnessd closed the connection")),
    }
}

/// How long a daemon has to answer the hello (the CLI's `HELLO_WAIT`).
const HELLO_WAIT: Duration = Duration::from_secs(3);

/// How long a request waits for its answer once the hello is in.
const REPLY_WAIT: Duration = Duration::from_secs(10);

/// A daemon that took the connection but that this app cannot speak to (no hello in [`HELLO_WAIT`], another schema, a
/// refusal), as an io error's inner error: the list thread tells it from no daemon at all and shows it, rather than
/// trying to connect again and again.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refused(why: String) -> std::io::Error {
    std::io::Error::other(Refused(why))
}

/// The reason, when `e` is a [`Refused`].
fn refusal(e: &std::io::Error) -> Option<&str> {
    e.get_ref()?.downcast_ref::<Refused>().map(|r| r.0.as_str())
}

/// The pid at the other end of `s`: macOS's `LOCAL_PEERPID` socket option (sys/un.h).
fn peer_pid(s: &UnixStream) -> Option<i32> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `pid`, which is that size.
    let r = unsafe {
        libc::getsockopt(s.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &mut len)
    };
    (r == 0 && pid > 0).then_some(pid)
}

/// A fresh connection: the hello and `first` written together, and the daemon's hello read (an error naming both
/// schemas when they differ). The answer to `first` is the next message.
///
/// # Errors
/// The socket's (no daemon, a connection closed or reset), or a [`Refused`] naming the daemon's pid and socket: no hello
/// within [`HELLO_WAIT`] (an older daemon), another schema's, or a hello this app cannot read.
fn open(first: &Up) -> std::io::Result<UnixStream> {
    let path = sock();
    let mut s = UnixStream::connect(&path)?;
    s.set_read_timeout(Some(HELLO_WAIT))?;
    let mut out = up(&Up::Hello { schema: harness_proto::SCHEMA });
    out.extend_from_slice(&up(first));
    std::io::Write::write_all(&mut s, &out)?;
    let pid = peer_pid(&s).map_or("unknown".to_owned(), |p| p.to_string());
    let who = || format!("harnessd (pid {pid}, socket {})", path.display());
    let hello = match recv(&mut s) {
        Ok(m) => m,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
            return Err(refused(format!(
                "{} did not answer the hello within {}s: it is likely an older daemon that does not speak harness schema {}; run `harness stop`",
                who(),
                HELLO_WAIT.as_secs(),
                harness_proto::SCHEMA
            )));
        }
        Err(e) if matches!(
            e.kind(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
        ) => return Err(e),
        Err(e) => return Err(refused(format!("{} answered the hello with something this app cannot read ({e}); run `harness stop`", who()))),
    };
    match hello {
        Down::Hello { schema, .. } if schema == harness_proto::SCHEMA => {
            s.set_read_timeout(Some(REPLY_WAIT))?;
            Ok(s)
        }
        Down::Hello { schema, .. } => Err(refused(format!(
            "{} speaks harness schema {schema}, this app {}: run `harness stop` to restart it",
            who(),
            harness_proto::SCHEMA
        ))),
        Down::Error { error } => Err(refused(format!("{} refused the connection: {error}", who()))),
        m => Err(refused(format!("{} answered the hello with {m:?}", who()))),
    }
}

/// One request on a fresh connection: its reply, or the daemon's refusal (the inner error, naming why). The outer error
/// is the socket's (no daemon, a closed connection).
pub fn request(r: Request) -> std::io::Result<Result<Reply, String>> {
    request_within(r, REPLY_WAIT)
}

/// [`request`], its answer waited for up to `wait` (a request the daemon may take minutes over, an aside's).
fn request_within(r: Request, wait: Duration) -> std::io::Result<Result<Reply, String>> {
    let mut s = open(&Up::Req(r))?;
    s.set_read_timeout(Some(wait))?;
    match recv(&mut s)? {
        Down::Reply(reply) => Ok(Ok(reply)),
        Down::Error { error } => Ok(Err(error)),
        m => Err(std::io::Error::other(format!("harnessd answered a request with {m:?}"))),
    }
}

/// The thread that sends the UI's events to the daemon, one at a time so they arrive in the order they happened (a
/// slider's values while it is dragged), and the sender that feeds it.
fn ui_sender() -> Sender<UiEvent> {
    let (tx, rx) = std::sync::mpsc::channel::<UiEvent>();
    std::thread::Builder::new()
        .name("harness-ui-events".into())
        .spawn(move || {
            for UiEvent { cell, handler, value } in rx {
                let shown: String = value.to_string().chars().take(80).collect();
                let said = format!("harness-native: ui event {cell} {handler} {shown}");
                match request(Request::UiEvent { cell, handler, value }) {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => eprintln!("{said}: the daemon refused it: {e}"),
                    Err(e) => eprintln!("{said}: {e}"),
                }
            }
        })
        .expect("ui events thread");
    tx
}

/// A request whose reply is an open record (runs, objects).
fn record(r: Request) -> std::io::Result<Result<Value, String>> {
    Ok(request(r)?.and_then(|reply| match reply {
        Reply::Record(v) => Ok(v),
        Reply::Ok => Ok(serde_json::json!({"ok": true})),
        r => Err(format!("harnessd answered with {r:?} where a record belongs")),
    }))
}

/// The newest runs (`before`, `q` as `runs` takes them).
fn runs(limit: usize, before: Option<u64>, q: Option<String>) -> std::io::Result<Result<Vec<Value>, String>> {
    Ok(record(Request::Runs(RunReq::Runs { session: None, limit: Some(limit), before, q }))?
        .map(|v| v["runs"].as_array().cloned().unwrap_or_default()))
}

/// The `harness` binary to start the daemon with: `$HARNESS_BIN`, else the `harness` next to this app's own binary
/// (one build makes both, so the app never starts a daemon from another build).
fn harness_bin() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("HARNESS_BIN") {
        return Ok(p.into());
    }
    let exe = std::env::current_exe().map_err(|e| format!("harness-native: locating its own binary: {e}"))?;
    let bin = exe.with_file_name("harness");
    if bin.is_file() {
        Ok(bin)
    } else {
        Err(format!("no harness binary next to {}: build it with this app (cargo build -p harness -p harness-native) or set HARNESS_BIN", exe.display()))
    }
}

/// Connect, or start the daemon and wait for its socket (100 tries 50 ms apart, as the CLI does).
pub fn ensure_daemon() -> Result<(), String> {
    if UnixStream::connect(sock()).is_ok() {
        return Ok(());
    }
    let bin = harness_bin()?;
    std::fs::create_dir_all(home()).map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home().join("daemon.log"))
        .map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("daemon")
        .env("HARNESS_NO_OPEN", "1")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log);
    // SAFETY: setsid in the child before exec only; it touches no state of this process.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| format!("start {}: {e}", bin.display()))?;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        if UnixStream::connect(sock()).is_ok() {
            return Ok(());
        }
    }
    Err(format!("harnessd did not start; see {}", home().join("daemon.log").display()))
}

type Wake = Arc<dyn Fn() + Send + Sync>;

/// Open the daemon's change feed: `watch` answers `watching`, then sends `Down::Changed` each time its sessions or runs
/// change. None when the daemon is not there or refused it.
fn watch_feed() -> Option<UnixStream> {
    let mut s = open(&Up::Req(Request::Watch)).ok()?;
    match recv(&mut s) {
        Ok(Down::Reply(Reply::Watching)) => {}
        other => {
            eprintln!("harness-native: watch: {other:?}");
            return None;
        }
    }
    // Events come only on a change: wait for them as long as it takes.
    s.set_read_timeout(None).ok()?;
    Some(s)
}

/// Long-poll `run_read` on one `stream` of run `r{run}` from the start, handing each answer to `got`, while `live()`;
/// until the run is done and read to the end. False when the daemon refused the stream (no chunk index for
/// "ordered"), so the caller reads it another way.
fn follow(
    run: u64,
    stage: Option<u32>,
    stream: &str,
    live: &dyn Fn() -> bool,
    mut got: impl FnMut(&Value),
) -> bool {
    let mut offset = 0u64;
    while live() {
        let v = match record(Request::Runs(RunReq::RunRead {
            id: format!("r{run}"),
            stage: stage.map(|s| s as usize),
            stream: stream.to_owned(),
            offset,
            max_bytes: None,
            wait_ms: RUN_WAIT_MS,
        })) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                if stream != "ordered" {
                    eprintln!("harness-native: run_read r{run} {stream}: {e}");
                }
                return false;
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        if !live() {
            break;
        }
        got(&v);
        offset = v["next_offset"].as_u64().unwrap_or(offset);
        if v["done"].as_bool() == Some(true) && v["eof"].as_bool() == Some(true) {
            break;
        }
    }
    true
}

/// The app's side of the daemon: what it can ask from the main thread. Every call returns at once; answers arrive as
/// [`Msg`]s.
#[derive(Clone)]
pub struct Link {
    tx: Mailbox,
    wake: Wake,
    /// The write half of each attach stream, by session id.
    streams: Arc<Mutex<HashMap<u64, (u64, UnixStream)>>>,
    poke: Sender<()>,
    /// The run outputs being followed, by (run, stage): a generation each one's reader threads check, so an unwatch (or a
    /// new watch of the same output) ends the old readers.
    run_watch: Arc<Mutex<HashMap<(u64, Option<u32>), u64>>>,
    /// Trivially cheap counters F3's overlay (src/app/stats.rs) reads: the last `list` round trip (microseconds, 0 before
    /// the first), the daemon's pid (asked once each time the link comes up; 0 unknown), and the bytes each attach
    /// stream (by session id) and each followed run output (by run and stage) has carried.
    rtt_us: Arc<AtomicU64>,
    pid: Arc<AtomicU32>,
    bytes: Arc<Mutex<HashMap<u64, Arc<AtomicU64>>>>,
    run_bytes: Arc<Mutex<HashMap<(u64, Option<u32>), Arc<AtomicU64>>>>,
    /// The person's events on cells' UI (`Request::UiEvent`), in the order they happened, to the one thread that sends
    /// them ([`Link::ui_event`]).
    ui: Sender<UiEvent>,
    /// By agent kind, the models it last listed to the daemon (`list`'s `models`): what a chat whose own log has none
    /// picks from (chat/pick.rs).
    models: Arc<Mutex<Arc<BTreeMap<String, Vec<ModelChoice>>>>>,
    /// What the daemon said of the things chats' chips point at (`github_status`, `page_peek`), for the chips and their
    /// hover cards (src/app/chat/peek.rs); bumped `rev` and a wake each time an answer lands.
    peeks: Arc<Mutex<Peeks>>,
}

/// The daemon's answers about chips' targets: a pull request's or issue's state by ref, a shown pull request's checks one
/// by one, a page's card by URL, what is being asked now (asked once at a time), and a revision that moves with each
/// answer.
#[derive(Default)]
pub struct Peeks {
    pub gh: BTreeMap<GhRef, GhItem>,
    pub checks: BTreeMap<GhRef, GhChecks>,
    pub pages: BTreeMap<String, PageMeta>,
    pub asking_gh: std::collections::BTreeSet<GhRef>,
    pub asking_checks: std::collections::BTreeSet<GhRef>,
    pub asking_pages: std::collections::BTreeSet<String>,
    pub rev: u64,
}

/// One event on a cell's UI: cell `<repl>.<task>` runs handler `handler` with `value`.
struct UiEvent {
    cell: String,
    handler: String,
    value: Value,
}

impl Link {
    /// Where this link posts what the daemon says (a shot posts a recorded world there: App::shot_fixture).
    pub fn mailbox(&self) -> Mailbox {
        self.tx.clone()
    }

    pub fn start(tx: Mailbox, wake: Wake) -> Self {
        let (poke, poked) = std::sync::mpsc::channel::<()>();
        let link = Self {
            tx: tx.clone(),
            wake: wake.clone(),
            streams: Arc::default(),
            poke,
            run_watch: Arc::default(),
            rtt_us: Arc::default(),
            pid: Arc::default(),
            bytes: Arc::default(),
            run_bytes: Arc::default(),
            ui: ui_sender(),
            models: Arc::default(),
            peeks: Arc::default(),
        };
        let (rtt_us, pid, agent_models) = (link.rtt_us.clone(), link.pid.clone(), link.models.clone());
        std::thread::Builder::new()
            .name("harness-list".into())
            .spawn(move || {
                let mut up = None;
                // The daemon's change feed (`watch`), while it is open; None against a daemon without one (it is polled).
                let mut feed: Option<UnixStream> = None;
                let mut polled = false;
                // The last refusal shown (Msg::Refused), so an unchanged one is not sent again.
                let mut refused_why: Option<String> = None;
                loop {
                    let asked = std::time::Instant::now();
                    // Every chat too, from before the daemon's restarts (the Archive files them; opening one asks the
                    // daemon for its log's events, resuming one sends it a message).
                    let listed = match request(Request::List { all: false, chats: true }) {
                        Ok(Ok(Reply::Sessions { sessions, models })) => {
                            let mut kept = agent_models.lock().unwrap();
                            if **kept != models {
                                *kept = Arc::new(models);
                            }
                            Ok(sessions)
                        }
                        Ok(Ok(r)) => Err((format!("list answered {r:?}"), None)),
                        Ok(Err(e)) => Err((e, None)),
                        Err(e) => Err((e.to_string(), refusal(&e).map(str::to_owned))),
                    };
                    let state = match listed {
                        Ok(sessions) => {
                            rtt_us.store(asked.elapsed().as_micros() as u64, Ordering::Relaxed);
                            if up != Some(LinkState::Up) {
                                let p = match request(Request::Info) {
                                    Ok(Ok(Reply::Daemon { pid, .. })) => pid,
                                    _ => 0,
                                };
                                pid.store(p, Ordering::Relaxed);
                            }
                            // The model reads each row by its fields' names (the schema's SessionInfo).
                            let rows = sessions.iter().filter_map(|s| serde_json::to_value(s).ok()).collect();
                            tx.send(Msg::List(rows));
                            // The recent shell runs, beside the sessions they ran in.
                            match runs(RUNS, None, None) {
                                Ok(Ok(r)) => {
                                    tx.send(Msg::Runs(r));
                                }
                                Ok(Err(e)) => eprintln!("harness-native: runs: {e}"),
                                Err(e) => eprintln!("harness-native: runs: {e}"),
                            }
                            LinkState::Up
                        }
                        // A daemon is there but cannot be spoken to: say so in the window (asked again each POLL, so
                        // a `harness stop` and a fresh daemon bring the link up), never "connecting" forever.
                        Err((_, Some(why))) => {
                            if refused_why.as_deref() != Some(why.as_str()) {
                                eprintln!("harness-native: {why}");
                                tx.send(Msg::Refused(why.clone()));
                                refused_why = Some(why);
                            }
                            LinkState::Refused
                        }
                        Err((e, None)) => {
                            if up == Some(LinkState::Up) {
                                eprintln!("harness-native: list: {e}");
                            }
                            if up != Some(LinkState::Connecting) {
                                tx.send(Msg::Link(LinkState::Connecting));
                                wake();
                            }
                            match ensure_daemon() {
                                Ok(()) => LinkState::Connecting,
                                Err(e) => {
                                    eprintln!("harness-native: {e}");
                                    LinkState::Down
                                }
                            }
                        }
                    };
                    if state != LinkState::Refused {
                        refused_why = None;
                    }
                    if up != Some(state) {
                        tx.send(Msg::Link(state));
                        up = Some(state);
                    }
                    wake();
                    if state != LinkState::Up {
                        // A daemon that comes back may be a newer one: try its feed again.
                        (feed, polled) = (None, false);
                        let _ = poked.recv_timeout(POLL);
                        continue;
                    }
                    // The daemon pushes a change event (a session or run came, went, or changed its status, title or
                    // busyness): ask for the list again only then. Its events coalesce, so a burst asks once.
                    if feed.is_none() && !polled {
                        feed = watch_feed();
                        polled = feed.is_none();
                        if feed.is_some() {
                            // Ask once more: a change made before the feed opened is in this answer.
                            continue;
                        }
                    }
                    match feed.as_mut() {
                        Some(s) => match recv(s) {
                            Ok(Down::Changed { .. }) => {}
                            _ => feed = None,
                        },
                        None => {
                            // An older daemon: poll, woken early by a spawn or a kill so the tree follows at once.
                            let _ = poked.recv_timeout(POLL);
                        }
                    }
                    while poked.try_recv().is_ok() {}
                }
            })
            .expect("list thread");
        link
    }

    /// What wakes the main thread: for work the app runs on a thread of its own (chat/mention.rs's index).
    pub fn waker(&self) -> Wake {
        self.wake.clone()
    }

    /// The last `list` round trip to the daemon, milliseconds (None before the first answer).
    pub fn rtt_ms(&self) -> Option<f32> {
        let us = self.rtt_us.load(Ordering::Relaxed);
        (us > 0).then(|| us as f32 / 1000.0)
    }

    /// By agent kind, the models it last listed to the daemon (none before the first `list`, or for a kind that never
    /// listed).
    pub fn agent_models(&self) -> Arc<BTreeMap<String, Vec<ModelChoice>>> {
        self.models.lock().unwrap().clone()
    }

    /// The daemon's pid, as its `info` said when the link last came up.
    pub fn daemon_pid(&self) -> Option<u32> {
        Some(self.pid.load(Ordering::Relaxed)).filter(|&p| p != 0)
    }

    /// The bytes session `id`'s attach streams have carried since launch.
    pub fn session_bytes(&self, id: u64) -> u64 {
        self.bytes.lock().unwrap().get(&id).map_or(0, |c| c.load(Ordering::Relaxed))
    }

    /// The bytes run `r{run}`'s output (its stage `stage`) has carried since launch.
    pub fn run_bytes(&self, run: u64, stage: Option<u32>) -> u64 {
        self.run_bytes.lock().unwrap().get(&(run, stage)).map_or(0, |c| c.load(Ordering::Relaxed))
    }

    pub fn attached(&self, id: u64) -> bool {
        self.streams.lock().unwrap().contains_key(&id)
    }

    /// Attach to `id` at `cols` x `rows`: its stream's bytes come as `Msg::Data` (a chat session's events as whole
    /// T_EVENT frames, with `events`).
    pub fn attach(&self, id: u64, cols: u16, rows: u16, events: bool) {
        if self.attached(id) {
            return;
        }
        let s = match open(&Up::Req(Request::Attach { id, cols: Some(cols), rows: Some(rows), events })) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("harness-native: attach {id}: {e}");
                return;
            }
        };
        let _ = s.set_read_timeout(None);
        let Ok(w) = s.try_clone() else { return };
        // A generation per attach: a stream detached and attached again is not removed by the old reader as it ends.
        static GEN: AtomicU64 = AtomicU64::new(1);
        let generation = GEN.fetch_add(1, Ordering::Relaxed);
        self.streams.lock().unwrap().insert(id, (generation, w));
        let (tx, wake, streams) = (self.tx.clone(), self.wake.clone(), self.streams.clone());
        let counted = self.bytes.lock().unwrap().entry(id).or_default().clone();
        std::thread::Builder::new()
            .name(format!("harness-attach-{id}"))
            .spawn(move || {
                let mut s = std::io::BufReader::with_capacity(256 * 1024, s);
                // `attached` (or an error) first.
                match recv(&mut s) {
                    Ok(Down::Reply(Reply::Attached { .. })) => {}
                    other => eprintln!("harness-native: attach {id}: {other:?}"),
                }
                // A chat's events, as whole frames, gathered while more are already read (a history comes as one batch).
                let mut events = Vec::new();
                loop {
                    let got = frame::read(&mut s);
                    if let Ok(Some((T_EVENT, body))) = &got {
                        events.extend_from_slice(&frame::head(T_EVENT, body.len()));
                        events.extend_from_slice(body);
                        if !s.buffer().is_empty() && events.len() < 256 * 1024 {
                            continue;
                        }
                    }
                    if !events.is_empty() {
                        counted.fetch_add(events.len() as u64, Ordering::Relaxed);
                        tx.send(Msg::Data { id, bytes: std::mem::take(&mut events) });
                    }
                    match got {
                        Ok(Some((T_EVENT, _))) => {}
                        Ok(Some((T_DATA, body))) => {
                            counted.fetch_add(body.len() as u64, Ordering::Relaxed);
                            tx.send(Msg::Data { id, bytes: body });
                        }
                        Ok(Some((T_CBOR, body))) => match decode_down(&body) {
                            Ok(Down::Exit { code }) => {
                                tx.send(Msg::Exit { id, code });
                            }
                            other => eprintln!("harness-native: attach {id}: {other:?}"),
                        },
                        Ok(Some((t, _))) => eprintln!("harness-native: attach {id}: a frame of type {t}"),
                        Ok(None) => break,
                        Err(e) => {
                            eprintln!("harness-native: attach {id}: {e}");
                            break;
                        }
                    }
                    wake();
                }
                let mut map = streams.lock().unwrap();
                if map.get(&id).is_some_and(|(g, _)| *g == generation) {
                    map.remove(&id);
                    drop(map);
                    tx.send(Msg::Detached { id });
                    wake();
                }
            })
            .expect("attach thread");
    }

    /// Detach from `id` (`Up::Detach` ends the attach; the session keeps running).
    pub fn detach(&self, id: u64) {
        if let Some((_, mut s)) = self.streams.lock().unwrap().remove(&id) {
            let _ = std::io::Write::write_all(&mut s, &up(&Up::Detach));
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Keyboard bytes into `id`'s PTY.
    pub fn input(&self, id: u64, bytes: &[u8]) {
        if let Some((_, s)) = self.streams.lock().unwrap().get_mut(&id) {
            let _ = frame::write(s, T_DATA, bytes);
        }
    }

    pub fn resize(&self, id: u64, cols: u16, rows: u16) {
        if let Some((_, s)) = self.streams.lock().unwrap().get_mut(&id) {
            let _ = std::io::Write::write_all(s, &up(&Up::Resize { cols, rows }));
        }
    }

    /// A message from the person into chat session `id`, with the pictures at `images` (the daemon writes it to the agent
    /// as a user turn).
    pub fn chat_send(&self, id: u64, text: &str, images: Vec<String>) {
        let (text, poke) = (text.to_owned(), self.poke.clone());
        std::thread::spawn(move || {
            match request(Request::Send { id, text, raw: false, submit: None, images }) {
                Ok(Err(e)) => eprintln!("harness-native: send {id}: {e}"),
                Ok(Ok(_)) => {}
                Err(e) => eprintln!("harness-native: send {id}: {e}"),
            }
            let _ = poke.send(());
        });
    }

    /// An aside to chat session `id` (`Request::Aside`): `text` asked of a fork of its conversation as one turn, the fork
    /// then discarded, so the conversation never has it. `then` gets the fork's answer (markdown), or why there is none
    /// (the daemon's refusal, naming the chat, or the socket's error), on the asking thread; the main thread is woken
    /// after. The reply is waited for the daemon's own bound on the fork ([`harness_proto::msg::ASIDE_SECS`]) and a
    /// little more, never past it.
    pub fn chat_aside(&self, id: u64, text: &str, then: impl FnOnce(Result<String, String>) + Send + 'static) {
        let (text, wake) = (text.to_owned(), self.wake.clone());
        std::thread::spawn(move || {
            let wait = Duration::from_secs(harness_proto::msg::ASIDE_SECS) + REPLY_WAIT;
            let answer = match request_within(Request::Aside { id, text }, wait) {
                Ok(Ok(Reply::Aside { text })) => Ok(text),
                Ok(Ok(r)) => Err(format!("the daemon answered the aside to chat s{id} with {r:?}")),
                Ok(Err(e)) => Err(e),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    Err(format!("the daemon did not answer the aside to chat s{id} within {}s", wait.as_secs()))
                }
                Err(e) => Err(format!("asking chat s{id} aside: {e}")),
            };
            if let Err(e) = &answer {
                eprintln!("harness-native: aside {id}: {e}");
            }
            then(answer);
            wake();
        });
    }

    /// The person did something to a cell's UI: cell `cell` (`<repl>.<task>`) runs handler `handler` (`h3`) with `value`
    /// (null for a press). Sent in order, one after another, by the link's UI thread; a daemon's refusal (no kernel
    /// listens for the cell's REPL) is logged naming the cell and handler.
    pub fn ui_event(&self, cell: &str, handler: &str, value: Value) {
        let ev = UiEvent { cell: cell.to_owned(), handler: handler.to_owned(), value };
        if self.ui.send(ev).is_err() {
            eprintln!("harness-native: ui event {cell} {handler}: the link's UI thread is gone");
        }
    }

    /// Switch chat session `id`'s model and/or reasoning effort for its turns from here on (`Request::SetModel`: the daemon
    /// has its agent switch and logs the choice). A refusal (a model or effort its agent does not offer) is logged naming
    /// the chat and what was asked.
    pub fn chat_choose(&self, id: u64, model: Option<String>, effort: Option<String>) {
        let poke = self.poke.clone();
        std::thread::spawn(move || {
            let what = format!("model {model:?} effort {effort:?}");
            match request(Request::SetModel { id, model, effort }) {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!("harness-native: set-model {id} ({what}): {e}"),
                Err(e) => eprintln!("harness-native: set-model {id} ({what}): {e}"),
            }
            let _ = poke.send(());
        });
    }

    /// The answers about chips' targets so far ([`Peeks`]).
    pub fn peeks(&self) -> std::sync::MutexGuard<'_, Peeks> {
        self.peeks.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Ask the daemon after `refs` (`github_status`, in batches of [`REFS_MAX`]; `fresh` past its cache), those not
    /// already being asked. Each answer lands in [`Peeks::gh`]; a failed request lands as an item per ref whose error
    /// names it and why, its state unknown.
    pub fn github(&self, refs: Vec<GhRef>, fresh: bool) {
        let refs: Vec<GhRef> = {
            let mut p = self.peeks();
            let new: Vec<GhRef> = refs.into_iter().filter(|r| !p.asking_gh.contains(r)).collect();
            p.asking_gh.extend(new.iter().cloned());
            new
        };
        if refs.is_empty() {
            return;
        }
        let (peeks, wake) = (self.peeks.clone(), self.wake.clone());
        std::thread::spawn(move || {
            for batch in refs.chunks(REFS_MAX) {
                let failed = |why: String| -> Vec<GhItem> {
                    batch.iter().map(|r| GhItem::failed(r, format!("{}#{}: {why}", r.repo, r.n))).collect()
                };
                let items = match request(Request::GithubStatus { refs: batch.to_vec(), fresh }) {
                    Ok(Ok(Reply::Github { items })) if items.len() == batch.len() => items,
                    Ok(Ok(r)) => failed(format!("the daemon answered github_status with {r:?}")),
                    Ok(Err(e)) => failed(e),
                    Err(e) => failed(format!("no daemon: {e}")),
                };
                for it in items.iter().filter(|it| !it.error.is_empty()) {
                    eprintln!("harness-native: github: {}", it.error);
                }
                let mut p = peeks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                for (r, it) in batch.iter().zip(items) {
                    p.asking_gh.remove(r);
                    p.gh.insert(r.clone(), it);
                }
                p.rev += 1;
                drop(p);
                wake();
            }
        });
    }

    /// Ask the daemon after pull request `pr`'s checks one by one (`github_checks`) unless they are being asked now. The
    /// answer lands in [`Peeks::checks`]; a failed request as checks whose error names the pull request and why.
    pub fn checks(&self, pr: GhRef) {
        if !self.peeks().asking_checks.insert(pr.clone()) {
            return;
        }
        let (peeks, wake) = (self.peeks.clone(), self.wake.clone());
        std::thread::spawn(move || {
            let failed = |why: String| GhChecks::failed(&pr, format!("{}#{}: {why}", pr.repo, pr.n));
            let checks = match request(Request::GithubChecks { pr: pr.clone() }) {
                Ok(Ok(Reply::GithubChecks { checks })) => checks,
                Ok(Ok(r)) => failed(format!("the daemon answered github_checks with {r:?}")),
                Ok(Err(e)) => failed(e),
                Err(e) => failed(format!("no daemon: {e}")),
            };
            if !checks.error.is_empty() {
                eprintln!("harness-native: github: {}", checks.error);
            }
            let mut p = peeks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            p.asking_checks.remove(&pr);
            p.checks.insert(pr, checks);
            p.rev += 1;
            drop(p);
            wake();
        });
    }

    /// Ask the daemon for `url`'s card (`page_peek`) unless it is known or being asked; it lands in [`Peeks::pages`],
    /// a failure as a card whose error names the URL.
    pub fn page(&self, url: &str) {
        self.pages(&[url]);
    }

    /// Ask the daemon for each of `urls`' cards (their favicons too) that is neither known nor being asked, one after
    /// another on one thread, each landing in [`Peeks::pages`] as it comes (a chat's links: their chips' favicons).
    pub fn pages(&self, urls: &[&str]) {
        let urls: Vec<String> = {
            let mut p = self.peeks();
            let mut new = Vec::new(); // alloc: the URLs this call asks after, moved to its thread
            for u in urls {
                if !p.pages.contains_key(*u) && p.asking_pages.insert((*u).to_owned()) {
                    new.push((*u).to_owned());
                }
            }
            new
        };
        if urls.is_empty() {
            return;
        }
        let (peeks, wake) = (self.peeks.clone(), self.wake.clone());
        std::thread::spawn(move || {
            for url in urls {
                let failed = |why: String| PageMeta { url: url.clone(), error: format!("{url}: {why}"), ..PageMeta::default() };
                let page = match request(Request::PagePeek { url: url.clone() }) {
                    Ok(Ok(Reply::Page { page })) => page,
                    Ok(Ok(r)) => failed(format!("the daemon answered page_peek with {r:?}")),
                    Ok(Err(e)) => failed(e),
                    Err(e) => failed(format!("no daemon: {e}")),
                };
                if !page.error.is_empty() {
                    eprintln!("harness-native: page: {}", page.error);
                }
                let mut p = peeks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                p.asking_pages.remove(&url);
                p.pages.insert(url, page);
                p.rev += 1;
                drop(p);
                wake();
            }
        });
    }

    /// Interrupt chat session `id`'s turn in flight.
    pub fn chat_interrupt(&self, id: u64) {
        std::thread::spawn(move || {
            let r = Request::Rpc { id: format!("s{id}"), method: "interrupt".into(), args: Value::Null };
            match request(r) {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!("harness-native: interrupt {id}: {e}"),
                Err(e) => eprintln!("harness-native: interrupt {id}: {e}"),
            }
        });
    }

    /// Name session `id` `title` (empty: its automatic name again); the daemon keeps it, and its change feed brings the
    /// row back with it.
    pub fn rename(&self, id: u64, title: &str) {
        let title = title.to_owned();
        std::thread::spawn(move || {
            let what = format!("rename {id} to {title:?}");
            match request(Request::Rename { id, title }) {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!("harness-native: {what}: {e}"),
                Err(e) => eprintln!("harness-native: {what}: {e}"),
            }
        });
    }

    /// A new object of `kind` in `cwd`; `then` gets its id. `claude-tui` and `codex-tui` are the agent in its terminal
    /// UI (a PTY); plain `claude` and `codex` are chat sessions.
    pub fn spawn(
        &self,
        kind: &str,
        cwd: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        cols: u16,
        rows: u16,
        then: impl FnOnce(u64) + Send + 'static,
    ) {
        let (kind, poke) = (kind.to_owned(), self.poke.clone());
        std::thread::spawn(move || {
            let (k, mode) = match kind.as_str() {
                k => k.strip_suffix("-tui").map_or((k, None), |k| (k, Some("tui"))),
            };
            let r = Request::Spawn {
                kind: k.to_owned(),
                args: Vec::new(),
                parent: None,
                cwd,
                prompt: None,
                name: None,
                cols: Some(cols),
                rows: Some(rows),
                mode: mode.map(str::to_owned),
                model,
                effort,
            };
            match request(r) {
                Ok(Ok(Reply::Spawned { id })) => {
                    let _ = poke.send(());
                    then(id);
                }
                Ok(Ok(r)) => eprintln!("harness-native: spawn {kind}: answered {r:?}"),
                Ok(Err(e)) => eprintln!("harness-native: spawn {kind}: {e}"),
                Err(e) => eprintln!("harness-native: spawn {kind}: {e}"),
            }
        });
    }

    /// Follow run `r{run}`'s output (its stage `stage`, or the caller-visible streams) from the start, read-only, until
    /// the run is done and read to the end, or [`Link::unwatch_run`] (or another watch of it) ends it. Each pane that
    /// shows a run has its own watch. One thread
    /// long-polls `run_read` with stream "ordered" (stdout and stderr in the order they were written, from the daemon's
    /// chunk index); a run from before the index (or an older daemon) is read as two streams, each on its own thread.
    pub fn watch_run(&self, run: u64, stage: Option<u32>) {
        static GEN: AtomicU64 = AtomicU64::new(1);
        let generation = GEN.fetch_add(1, Ordering::Relaxed);
        self.run_watch.lock().unwrap().insert((run, stage), generation);
        let (tx, wake, watches) = (self.tx.clone(), self.wake.clone(), self.run_watch.clone());
        let counted = self.run_bytes.lock().unwrap().entry((run, stage)).or_default().clone();
        let current = move || watches.lock().unwrap().get(&(run, stage)) == Some(&generation);
        std::thread::Builder::new()
            .name(format!("harness-run-r{run}"))
            .spawn(move || {
                let live = || current();
                let send = |err: bool, data: &str| {
                    if !data.is_empty() {
                        counted.fetch_add(data.len() as u64, Ordering::Relaxed);
                        tx.send(Msg::RunData {
                            run,
                            stage,
                            err,
                            bytes: data.as_bytes().to_vec(),
                        });
                        wake();
                    }
                };
                if follow(run, stage, "ordered", &live, |v| {
                    for c in v["chunks"].as_array().into_iter().flatten() {
                        send(c["err"].as_bool() == Some(true), c["data"].as_str().unwrap_or(""));
                    }
                }) {
                    return;
                }
                let err_side = {
                    let (tx, wake, current) = (tx.clone(), wake.clone(), current.clone());
                    std::thread::spawn(move || {
                        follow(run, stage, "err", &current, |v| {
                            let data = v["data"].as_str().unwrap_or("");
                            if !data.is_empty() {
                                tx.send(Msg::RunData {
                                    run,
                                    stage,
                                    err: true,
                                    bytes: data.as_bytes().to_vec(),
                                });
                                wake();
                            }
                        });
                    })
                };
                follow(run, stage, "out", &live, |v| send(false, v["data"].as_str().unwrap_or("")));
                let _ = err_side.join();
            })
            .expect("run reader thread");
    }

    /// Stop following run `r{run}`'s output (of its stage `stage`).
    pub fn unwatch_run(&self, run: u64, stage: Option<u32>) {
        self.run_watch.lock().unwrap().remove(&(run, stage));
    }

    /// A page of [`PAGE`] runs older than `r{before}` (the Archive's history): it comes as `Msg::RunsPage`.
    pub fn runs_page(&self, before: u64) {
        let (tx, wake) = (self.tx.clone(), self.wake.clone());
        std::thread::spawn(move || {
            let runs = match runs(PAGE, Some(before), None) {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    eprintln!("harness-native: runs before r{before}: {e}");
                    Vec::new()
                }
                Err(e) => {
                    eprintln!("harness-native: runs before r{before}: {e}");
                    Vec::new()
                }
            };
            let full = runs.len() >= PAGE;
            tx.send(Msg::RunsPage { runs, full });
            wake();
        });
    }

    /// Runs from all of history whose command or id holds `q` (the palette's search; the daemon filters, newest first,
    /// up to [`PAGE`]): they come as `Msg::RunsFound`.
    pub fn runs_search(&self, q: &str) {
        let (tx, wake, q) = (self.tx.clone(), self.wake.clone(), q.to_owned());
        std::thread::spawn(move || match runs(PAGE, None, Some(q.clone())) {
            Ok(Ok(r)) => {
                tx.send(Msg::RunsFound(r));
                wake();
            }
            Ok(Err(e)) => eprintln!("harness-native: runs matching {q:?}: {e}"),
            Err(e) => eprintln!("harness-native: runs matching {q:?}: {e}"),
        });
    }

    /// Kill shell run `r{n}` (its process group).
    pub fn kill_run(&self, n: u64) {
        let poke = self.poke.clone();
        std::thread::spawn(move || {
            match request(Request::Runs(RunReq::RunKill { id: format!("r{n}") })) {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!("harness-native: run_kill r{n}: {e}"),
                Err(e) => eprintln!("harness-native: run_kill r{n}: {e}"),
            }
            let _ = poke.send(());
        });
    }

    pub fn kill(&self, id: u64) {
        let poke = self.poke.clone();
        std::thread::spawn(move || {
            match request(Request::Kill { id }) {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!("harness-native: kill {id}: {e}"),
                Err(e) => eprintln!("harness-native: kill {id}: {e}"),
            }
            // The daemon pushes the exit when it comes; an older one is polled again now.
            let _ = poke.send(());
        });
    }
}
