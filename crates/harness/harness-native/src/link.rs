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
use harness_proto::msg::{Down, Reply, Request, RunReq, Up};
use serde_json::Value;
use std::collections::HashMap;
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
        None => Err(std::io::Error::other("harnessd closed the connection")),
    }
}

/// A fresh connection: the hello and `first` written together, and the daemon's hello read (an error naming both
/// schemas when they differ). The answer to `first` is the next message.
fn open(first: &Up) -> std::io::Result<UnixStream> {
    let mut s = UnixStream::connect(sock())?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut out = up(&Up::Hello { schema: harness_proto::SCHEMA });
    out.extend_from_slice(&up(first));
    std::io::Write::write_all(&mut s, &out)?;
    match recv(&mut s)? {
        Down::Hello { schema, .. } if schema == harness_proto::SCHEMA => Ok(s),
        Down::Hello { schema, .. } => Err(std::io::Error::other(format!(
            "harnessd speaks harness schema {schema}, this app {}: restart the daemon (harness stop)",
            harness_proto::SCHEMA
        ))),
        Down::Error { error } => Err(std::io::Error::other(format!("harnessd refused the connection: {error}"))),
        m => Err(std::io::Error::other(format!("harnessd answered the hello with {m:?}"))),
    }
}

/// One request on a fresh connection: its reply, or the daemon's refusal (the inner error, naming why). The outer error
/// is the socket's (no daemon, a closed connection).
pub fn request(r: Request) -> std::io::Result<Result<Reply, String>> {
    let mut s = open(&Up::Req(r))?;
    match recv(&mut s)? {
        Down::Reply(reply) => Ok(Ok(reply)),
        Down::Error { error } => Ok(Err(error)),
        m => Err(std::io::Error::other(format!("harnessd answered a request with {m:?}"))),
    }
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

/// The `harness` binary to start the daemon with: `$HARNESS_BIN`, else this workspace's `target/release/harness` (or
/// debug), else `harness` on PATH.
fn harness_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HARNESS_BIN") {
        return Some(p.into());
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    for p in ["target/release/harness", "target/debug/harness"] {
        let p = root.join(p);
        if p.is_file() {
            return Some(p);
        }
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).map(|d| d.join("harness")).find(|p| p.is_file())
    })
}

/// Connect, or start the daemon and wait for its socket (100 tries 50 ms apart, as the CLI does).
pub fn ensure_daemon() -> Result<(), String> {
    if UnixStream::connect(sock()).is_ok() {
        return Ok(());
    }
    let bin = harness_bin().ok_or(
        "no harness binary: build it (cargo build -p harness --release) or set HARNESS_BIN",
    )?;
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
}

impl Link {
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
        };
        let (rtt_us, pid) = (link.rtt_us.clone(), link.pid.clone());
        std::thread::Builder::new()
            .name("harness-list".into())
            .spawn(move || {
                let mut up = None;
                // The daemon's change feed (`watch`), while it is open; None against a daemon without one (it is polled).
                let mut feed: Option<UnixStream> = None;
                let mut polled = false;
                loop {
                    let asked = std::time::Instant::now();
                    // Every chat too, from before the daemon's restarts (the Archive files them; opening one asks the
                    // daemon for its log's events, resuming one sends it a message).
                    let listed = match request(Request::List { all: false, chats: true }) {
                        Ok(Ok(Reply::Sessions { sessions })) => Ok(sessions),
                        Ok(Ok(r)) => Err(format!("list answered {r:?}")),
                        Ok(Err(e)) => Err(e),
                        Err(e) => Err(e.to_string()),
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
                        Err(e) => {
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

    /// The last `list` round trip to the daemon, milliseconds (None before the first answer).
    pub fn rtt_ms(&self) -> Option<f32> {
        let us = self.rtt_us.load(Ordering::Relaxed);
        (us > 0).then(|| us as f32 / 1000.0)
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

    /// A message from the person into chat session `id` (the daemon writes it to the agent as a user turn).
    pub fn chat_send(&self, id: u64, text: &str) {
        let (text, poke) = (text.to_owned(), self.poke.clone());
        std::thread::spawn(move || {
            match request(Request::Send { id, text, raw: false, submit: None }) {
                Ok(Err(e)) => eprintln!("harness-native: send {id}: {e}"),
                Ok(Ok(_)) => {}
                Err(e) => eprintln!("harness-native: send {id}: {e}"),
            }
            let _ = poke.send(());
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

    /// A new object of `kind` in `cwd`; `then` gets its id. `claude-tui` and `codex-tui` are the agent in its terminal
    /// UI (a PTY); plain `claude` and `codex` are chat sessions.
    pub fn spawn(
        &self,
        kind: &str,
        cwd: Option<String>,
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
