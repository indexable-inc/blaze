//! harnessd: owns every agent PTY, mirrors each into a libghostty terminal (harness-vt) (so a re-attaching client or
//! the web view can redraw the current screen), records the parent/child tree, and serves the
//! socket API plus the web UI. Survives client exit like a tmux server.
use crate::{archive, db, messages, paths, profiles, proto, queries, runs};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

/// The tree's generation: bumped whenever a session or run comes, goes, or changes what `list`/`runs` say of it that a
/// client draws (status, title, busy). `watch` clients get an event per change (coalesced), instead of polling.
static CHANGES: std::sync::LazyLock<tokio::sync::watch::Sender<u64>> = std::sync::LazyLock::new(|| tokio::sync::watch::channel(0).0);

/// Something `list` or `runs` reports changed: tell the `watch` clients.
pub fn changed() {
    CHANGES.send_modify(|g| *g = g.wrapping_add(1));
}

/// A session is busy while its program wrote output in the last this many milliseconds.
const BUSY_MS: u64 = 1500;

#[derive(Clone, Debug)]
pub enum Ev {
    Data(Bytes),
    Exit(i32),
}

pub struct Session {
    pub id: u64,
    pub kind: String,
    /// Unique among running objects; how a get-or-create spawn finds it again.
    pub name: Option<String>,
    pub parent: Option<u64>,
    pub cwd: String,
    pub argv: Vec<String>,
    pub created_ms: u64,
    /// The agent process (for `signal`).
    pub pid: Option<u32>,
    pub term: Mutex<harness_vt::Term>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pub tx: broadcast::Sender<Ev>,
    pub exit: Mutex<Option<i32>>,
    /// How and when it ended, once it has: `exited` or `killed` (by a signal), ms since the epoch.
    pub ended: Mutex<Option<(&'static str, u64)>>,
    /// `kill` was asked for (a shell that catches SIGHUP exits with a plain code, not by the signal).
    killed: AtomicBool,
    pub attached: AtomicUsize,
    /// When its program last wrote output (ms since the epoch), and whether `list` last said it was busy.
    last_out: AtomicU64,
    busy: AtomicBool,
}

impl Session {
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        if cols == 0 || rows == 0 {
            return Ok(());
        }
        self.master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).map_err(|e| anyhow!("{e}"))?;
        self.term.lock().unwrap().resize(cols, rows);
        Ok(())
    }

    /// Escape codes that redraw the current screen on a fresh terminal, plus a receiver for
    /// everything after it. Taken under the emulator lock so nothing is lost or doubled.
    pub fn snapshot_and_subscribe(&self) -> (Vec<u8>, broadcast::Receiver<Ev>) {
        let mut term = self.term.lock().unwrap();
        let rx = self.tx.subscribe();
        (term.vt_snapshot(), rx)
    }

    pub fn screen_text(&self) -> String {
        self.term.lock().unwrap().text()
    }

    pub fn size(&self) -> (u16, u16) {
        self.term.lock().unwrap().size()
    }

    pub fn info(&self) -> Value {
        let (cols, rows) = self.size();
        let title = self.term.lock().unwrap().title();
        let ended = *self.ended.lock().unwrap();
        json!({
            "id": self.id, "object": format!("s{}", self.id), "kind": self.kind, "name": self.name, "parent": self.parent, "title": title, "cwd": self.cwd,
            "argv": self.argv, "created_ms": self.created_ms, "exit": *self.exit.lock().unwrap(),
            "status": ended.map_or("running", |(s, _)| s), "ended_ms": ended.map(|(_, ms)| ms),
            "attached": self.attached.load(Ordering::Relaxed), "cols": cols, "rows": rows,
            "busy": self.busy.load(Ordering::Relaxed),
        })
    }

    /// Output came: busy now (a change when it was not).
    fn wrote(&self) {
        self.last_out.store(now_ms(), Ordering::Relaxed);
        if !self.busy.swap(true, Ordering::Relaxed) {
            changed();
        }
    }

    /// No output for [`BUSY_MS`]: not busy any more (a change when it was).
    fn settle(&self, now: u64) {
        if self.busy.load(Ordering::Relaxed) && now.saturating_sub(self.last_out.load(Ordering::Relaxed)) >= BUSY_MS && self.busy.swap(false, Ordering::Relaxed) {
            changed();
        }
    }

    pub fn kill(&self) {
        self.killed.store(true, Ordering::Relaxed);
        let _ = self.killer.lock().unwrap().kill();
    }
}

pub struct Daemon {
    pub sessions: Mutex<BTreeMap<u64, Arc<Session>>>,
    next: AtomicU64,
    pub last: Mutex<Option<u64>>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Req {
    /// Live sessions; with all: true also past ones from the index (status exited | lost).
    List {
        #[serde(default)]
        all: bool,
    },
    Info,
    Last,
    Spawn {
        kind: String,
        #[serde(default)]
        args: Vec<String>,
        parent: Option<u64>,
        cwd: Option<String>,
        prompt: Option<String>,
        name: Option<String>,
        cols: Option<u16>,
        rows: Option<u16>,
    },
    Attach { id: u64, cols: Option<u16>, rows: Option<u16> },
    /// The change feed: an ok, then `{"ev":"changed","gen":N}` each time `list` or `runs` would answer differently
    /// (sessions or runs come, go, or change status, title or busy), coalesced to at most 20 a second. Like `attach`
    /// it holds the connection; any frame from the client ends it.
    Watch,
    /// Type into a session. `submit` (default true) presses Enter afterwards; text goes in as a
    /// bracketed paste so multi-line messages arrive as one input. `raw` writes bytes verbatim.
    Send {
        id: u64,
        text: String,
        #[serde(default)]
        raw: bool,
        submit: Option<bool>,
    },
    Screen { id: u64 },
    Resize { id: u64, cols: u16, rows: u16 },
    Kill { id: u64 },
    Forget { id: u64 },
    Shutdown,
    /// A REPL cell started or finished (posted by the MCP process): indexed as a `repl` object.
    ReplTask {
        #[serde(flatten)]
        task: Value,
    },
    /// The object API (object.rs): get-or-create any kind, one object's record, and RPC to it.
    SpawnObject {
        #[serde(flatten)]
        opts: Value,
    },
    Object { id: String },
    Rpc {
        id: String,
        method: String,
        #[serde(default)]
        args: Value,
    },
    /// Shell runs with every pipeline stage's output stored: run, run_status, runs, run_read, run_kill, run_wait.
    #[serde(untagged)]
    Runs(runs::RunReq),
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

impl Daemon {
    pub fn get(&self, id: u64) -> Result<Arc<Session>> {
        self.sessions.lock().unwrap().get(&id).cloned().ok_or_else(|| anyhow!("no session {id}"))
    }

    pub fn list(&self) -> Vec<Value> {
        self.sessions.lock().unwrap().values().map(|s| s.info()).collect()
    }

    pub fn spawn(
        self: &Arc<Self>, kind: &str, args: &[String], parent: Option<u64>, cwd: Option<String>, prompt: Option<&str>,
        cols: u16, rows: u16,
    ) -> Result<u64> {
        self.spawn_named(kind, args, parent, cwd, prompt, None, &BTreeMap::new(), cols, rows)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_named(
        self: &Arc<Self>, kind: &str, args: &[String], parent: Option<u64>, cwd: Option<String>, prompt: Option<&str>,
        name: Option<String>, env: &BTreeMap<String, String>, cols: u16, rows: u16,
    ) -> Result<u64> {
        if let Some(n) = &name {
            db::check_name(n)?;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let sock = paths::sock();
        let cwd = cwd.unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()));
        let created_ms = now_ms();
        // A known Claude session id ties this object to its transcript (the messages stream).
        let mut args = args.to_vec();
        let mut meta = json!({ "session": id, "cwd": cwd, "created_ms": created_ms });
        let resumes = args.iter().any(|a| matches!(a.as_str(), "-r" | "--resume" | "-c" | "--continue" | "--session-id"));
        if kind == "claude" && !resumes {
            let uuid = uuid_v4();
            args.splice(0..0, ["--session-id".to_string(), uuid.clone()]);
            meta["claude_session"] = json!(uuid);
        }
        let launch = profiles::launch(kind, &profiles::Ctx { session: id, cwd: &cwd, sock: &sock, user_args: &args, prompt })?;
        let mut cmd = CommandBuilder::new(&launch.program);
        cmd.args(&launch.args);
        cmd.cwd(&cwd);
        // The daemon inherits whatever environment first started it (often another agent's shell);
        // don't pass that agent's session markers on. The profile's own variables are set below.
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if k == "CLAUDECODE" || k == "AI_AGENT" || k.starts_with("CLAUDE_CODE_") || k.starts_with("CODEX_") {
                cmd.env_remove(k.as_ref());
            }
        }
        for (k, v) in launch.env.iter().chain(env) {
            cmd.env(k, v);
        }
        if let Some(p) = parent {
            cmd.env("HARNESS_PARENT", p.to_string());
        }
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| anyhow!("openpty: {e}"))?;
        let mut child = pair.slave.spawn_command(cmd).map_err(|e| anyhow!("spawn {}: {e}", launch.program))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| anyhow!("{e}"))?;
        let writer = pair.master.take_writer().map_err(|e| anyhow!("{e}"))?;
        let killer = child.clone_killer();
        let pid = child.process_id();
        let (tx, _) = broadcast::channel(4096);
        let mut argv = vec![launch.program.clone()];
        argv.extend(launch.args.iter().cloned());
        let raw_path = db::object_dir(&format!("s{id}")).join("pty");
        std::fs::create_dir_all(raw_path.parent().unwrap())?;
        let mut raw = std::fs::OpenOptions::new().create(true).append(true).open(&raw_path)?;
        db::session_start(&db::SessionRow {
            id, kind, name: name.as_deref(), parent, cwd: &cwd, argv: &argv, created_ms, meta,
        });
        let session = Arc::new(Session {
            id, kind: kind.into(), name, parent, cwd, argv, created_ms, pid,
            term: Mutex::new(harness_vt::Term::new(if cols == 0 { 80 } else { cols }, if rows == 0 { 24 } else { rows }, 10_000)),
            writer: Mutex::new(writer), master: Mutex::new(pair.master), killer: Mutex::new(killer),
            tx, exit: Mutex::new(None), ended: Mutex::new(None), killed: AtomicBool::new(false), attached: AtomicUsize::new(0),
            last_out: AtomicU64::new(0), busy: AtomicBool::new(false),
        });
        self.sessions.lock().unwrap().insert(id, session.clone());
        changed();

        let s = session.clone();
        std::thread::Builder::new().name(format!("pty-read-{id}")).spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                // Every byte the PTY produced, so the transcript outlives the daemon.
                if let Err(e) = raw.write_all(&buf[..n]) {
                    eprintln!("session {id} pty stream: {e}");
                }
                // An OSC in the chunk may set the title `list` reports: compare it around the feed (only then).
                let osc = buf[..n].windows(2).any(|w| w == b"\x1b]");
                let (f, retitled) = {
                    let mut term = s.term.lock().unwrap();
                    let before = osc.then(|| term.title());
                    let f = queries::filter(&buf[..n], &mut term);
                    if !f.out.is_empty() {
                        let _ = s.tx.send(Ev::Data(Bytes::from(f.out.clone())));
                    }
                    let retitled = before.is_some_and(|b| b != term.title());
                    (f, retitled)
                };
                s.wrote();
                if retitled {
                    changed();
                }
                if !f.replies.is_empty() {
                    let _ = s.write(&f.replies);
                }
            }
        })?;
        let s = session.clone();
        std::thread::Builder::new().name(format!("pty-wait-{id}")).spawn(move || {
            let st = child.wait();
            let code = st.as_ref().map(|st| st.exit_code() as i32).unwrap_or(-1);
            let signalled = st.as_ref().is_ok_and(|st| st.signal().is_some());
            let status = if signalled || s.killed.load(Ordering::Relaxed) { "killed" } else { "exited" };
            let ended = now_ms();
            *s.ended.lock().unwrap() = Some((status, ended));
            *s.exit.lock().unwrap() = Some(code);
            changed();
            let _ = s.tx.send(Ev::Exit(code));
            db::session_end(s.id, code, status, ended);
            archive::compact_bg(format!("s{}", s.id));
        })?;
        eprintln!("spawned session {id}: {kind} (parent {parent:?})");
        Ok(id)
    }

    fn handle(self: &Arc<Self>, req: Req) -> Result<Value> {
        Ok(match req {
            Req::List { all } => {
                let mut v = self.list();
                if all {
                    let live: std::collections::HashSet<u64> = v.iter().filter_map(|s| s["id"].as_u64()).collect();
                    v.extend(db::sessions().into_iter().filter(|s| s["id"].as_u64().is_some_and(|i| !live.contains(&i))));
                    v.sort_by_key(|s| s["id"].as_u64());
                }
                json!({ "sessions": v })
            }
            Req::ReplTask { task } => {
                db::repl_task(&task)?;
                changed();
                json!({ "ok": true })
            }
            Req::Info => json!({ "pid": std::process::id(), "sock": paths::sock() }),
            Req::Last => {
                let last = *self.last.lock().unwrap();
                let alive = last.and_then(|id| self.get(id).ok()).filter(|s| s.exit.lock().unwrap().is_none()).map(|s| s.id);
                json!({ "id": alive })
            }
            Req::SpawnObject { opts } => crate::object::spawn(self, &opts)?,
            Req::Object { id } => crate::object::info(self, &id)?,
            Req::Rpc { id, method, args } => crate::object::rpc(self, &id, &method, &args, |r| self.handle(r))?,
            Req::Spawn { kind, args, parent, cwd, prompt, name, cols, rows } => {
                let id = self.spawn_named(&kind, &args, parent, cwd, prompt.as_deref(), name, &BTreeMap::new(), cols.unwrap_or(120), rows.unwrap_or(40))?;
                json!({ "id": id })
            }
            Req::Send { id, text, raw, submit } => {
                let s = self.get(id)?;
                if raw {
                    s.write(text.as_bytes())?;
                } else {
                    crate::inject::send(&s, &text, submit.unwrap_or(true))?;
                }
                json!({ "ok": true })
            }
            Req::Screen { id } => {
                let s = self.get(id)?;
                json!({ "text": s.screen_text(), "info": s.info() })
            }
            Req::Resize { id, cols, rows } => {
                self.get(id)?.resize(cols, rows)?;
                json!({ "ok": true })
            }
            Req::Kill { id } => {
                self.get(id)?.kill();
                json!({ "ok": true })
            }
            Req::Forget { id } => {
                let s = self.get(id)?;
                if s.exit.lock().unwrap().is_none() {
                    s.kill();
                }
                self.sessions.lock().unwrap().remove(&id);
                changed();
                json!({ "ok": true })
            }
            Req::Shutdown => {
                for s in self.sessions.lock().unwrap().values() {
                    s.kill();
                }
                let _ = std::fs::remove_file(paths::sock());
                std::process::exit(0);
            }
            Req::Runs(r) => runs::handle(r)?,
            Req::Attach { .. } | Req::Watch => unreachable!("attach and watch are handled by the connection loop"),
        })
    }
}

pub async fn run() -> Result<()> {
    std::fs::create_dir_all(paths::home())?;
    let sock = paths::sock();
    if UnixStream::connect(&sock).await.is_ok() {
        anyhow::bail!("harnessd already running at {}", sock.display());
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    // The object index: session ids continue after the last one ever used; sessions the last daemon
    // left running are lost; older runs get indexed, finished objects compacted, transcripts mirrored.
    db::mark_lost_sessions();
    let next = db::max_session_id() + 1;
    std::thread::spawn(|| {
        runs::backfill();
        archive::compact_pending();
    });
    std::thread::spawn(messages::poll);
    let d = Arc::new(Daemon { sessions: Mutex::new(BTreeMap::new()), next: AtomicU64::new(next), last: Mutex::new(None) });
    // What no event marks: a session going quiet (busy after BUSY_MS without output), and a live run's pipeline stages
    // starting and ending (their tee helpers write the stage files, not this process): looked at in memory here, and
    // `watch` clients told only when something did change (stages: while a run is live).
    {
        let d = d.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
            let mut n = 0u64;
            loop {
                tick.tick().await;
                n += 1;
                let now = now_ms();
                let sessions: Vec<Arc<Session>> = d.sessions.lock().unwrap().values().cloned().collect();
                for s in sessions {
                    s.settle(now);
                }
                if n % 2 == 0 && runs::any_live() {
                    changed();
                }
            }
        });
    }
    std::fs::write(paths::info(), json!({ "pid": std::process::id(), "sock": sock }).to_string())?;
    eprintln!("harnessd pid {} socket {}", std::process::id(), sock.display());
    loop {
        let (stream, _) = listener.accept().await?;
        let d = d.clone();
        tokio::spawn(async move {
            if let Err(e) = conn(d, stream).await {
                eprintln!("connection error: {e:#}");
            }
        });
    }
}

async fn conn(d: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let (mut r, mut w) = stream.into_split();
    while let Some((t, payload)) = proto::read_frame(&mut r).await? {
        if t != proto::T_JSON {
            continue;
        }
        let req: Req = match serde_json::from_slice(&payload) {
            Ok(req) => req,
            Err(e) => {
                proto::write_json(&mut w, &json!({ "error": format!("bad request: {e}") })).await?;
                continue;
            }
        };
        if let Req::Attach { id, cols, rows } = req {
            let s = match d.get(id) {
                Ok(s) => s,
                Err(e) => {
                    proto::write_json(&mut w, &json!({ "error": e.to_string() })).await?;
                    continue;
                }
            };
            if let (Some(c), Some(rw)) = (cols, rows) {
                s.resize(c, rw)?;
            }
            *d.last.lock().unwrap() = Some(id);
            proto::write_json(&mut w, &json!({ "ok": true, "info": s.info() })).await?;
            return attach(s, r, w).await;
        }
        if let Req::Watch = req {
            proto::write_json(&mut w, &json!({ "ok": true })).await?;
            return watch(r, w).await;
        }
        let d2 = d.clone();
        let resp = tokio::task::spawn_blocking(move || d2.handle(req)).await?;
        let v = resp.unwrap_or_else(|e| json!({ "error": format!("{e:#}") }));
        proto::write_json(&mut w, &v).await?;
    }
    Ok(())
}

/// A `watch` connection: an event per change of the tree's generation, at most one each 50 ms (a burst of changes is
/// one event, sent after the burst's first), until the client sends anything or goes.
async fn watch(mut r: tokio::net::unix::OwnedReadHalf, mut w: tokio::net::unix::OwnedWriteHalf) -> Result<()> {
    let mut rx = CHANGES.subscribe();
    rx.mark_unchanged();
    loop {
        tokio::select! {
            c = rx.changed() => {
                if c.is_err() {
                    return Ok(());
                }
                let generation = *rx.borrow_and_update();
                proto::write_json(&mut w, &json!({ "ev": "changed", "gen": generation })).await?;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            _ = proto::read_frame(&mut r) => return Ok(()),
        }
    }
}

async fn attach(s: Arc<Session>, mut r: tokio::net::unix::OwnedReadHalf, mut w: tokio::net::unix::OwnedWriteHalf) -> Result<()> {
    s.attached.fetch_add(1, Ordering::Relaxed);
    let (snap, mut rx) = s.snapshot_and_subscribe();
    let exited = *s.exit.lock().unwrap();
    let mut out = {
        let s = s.clone();
        tokio::spawn(async move {
            proto::write_frame(&mut w, proto::T_DATA, &snap).await?;
            if let Some(code) = exited {
                proto::write_frame(&mut w, proto::T_EXIT, json!({ "code": code }).to_string().as_bytes()).await?;
                return Ok::<_, anyhow::Error>(());
            }
            loop {
                match rx.recv().await {
                    Ok(Ev::Data(b)) => proto::write_frame(&mut w, proto::T_DATA, &b).await?,
                    Ok(Ev::Exit(code)) => {
                        proto::write_frame(&mut w, proto::T_EXIT, json!({ "code": code }).to_string().as_bytes()).await?;
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let (snap, new_rx) = s.snapshot_and_subscribe();
                        rx = new_rx;
                        proto::write_frame(&mut w, proto::T_DATA, &snap).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        })
    };
    let input = async {
        while let Some((t, payload)) = proto::read_frame(&mut r).await? {
            match t {
                proto::T_DATA => {
                    let s = s.clone();
                    tokio::task::spawn_blocking(move || s.write(&payload)).await??;
                }
                proto::T_RESIZE => {
                    let v: Value = serde_json::from_slice(&payload)?;
                    if let (Some(c), Some(rw)) = (v["cols"].as_u64(), v["rows"].as_u64()) {
                        s.resize(c as u16, rw as u16)?;
                    }
                }
                proto::T_JSON => break, // detach
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let res = tokio::select! {
        r = input => r,
        r = &mut out => r.map_err(anyhow::Error::from).and_then(|x| x),
    };
    out.abort();
    s.attached.fetch_sub(1, Ordering::Relaxed);
    res
}

/// Random RFC 4122 v4 UUID (for Claude's --session-id).
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}
