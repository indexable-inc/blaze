//! harnessd: owns every agent PTY, mirrors each into a vt100 emulator (so a re-attaching client or
//! the web view can redraw the current screen), records the parent/child tree, and serves the
//! socket API plus the web UI. Survives client exit like a tmux server.
use crate::{paths, profiles, proto, queries, runs, web};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

#[derive(Clone, Debug)]
pub enum Ev {
    Data(Bytes),
    Exit(i32),
}

#[derive(Default)]
pub struct TitleCb {
    pub title: String,
}
impl vt100::Callbacks for TitleCb {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
}

pub struct Session {
    pub id: u64,
    pub kind: String,
    pub parent: Option<u64>,
    pub cwd: String,
    pub argv: Vec<String>,
    pub created_ms: u64,
    pub term: Mutex<vt100::Parser<TitleCb>>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pub tx: broadcast::Sender<Ev>,
    pub exit: Mutex<Option<i32>>,
    pub attached: AtomicUsize,
}

impl Session {
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let (Some(c), Some(r)) = (NonZeroU16::new(cols), NonZeroU16::new(rows)) else { return Ok(()) };
        self.master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).map_err(|e| anyhow!("{e}"))?;
        self.term.lock().unwrap().set_size(r, c);
        Ok(())
    }

    /// Escape codes that redraw the current screen on a fresh terminal, plus a receiver for
    /// everything after it. Taken under the emulator lock so nothing is lost or doubled.
    pub fn snapshot_and_subscribe(&self) -> (Vec<u8>, broadcast::Receiver<Ev>) {
        let term = self.term.lock().unwrap();
        let rx = self.tx.subscribe();
        let mut snap = b"\x1b[0m\x1b[H\x1b[2J".to_vec();
        snap.extend(term.screen().state_formatted().into_bytes());
        (snap, rx)
    }

    pub fn screen_text(&self) -> String {
        self.term.lock().unwrap().screen().contents()
    }

    pub fn size(&self) -> (u16, u16) {
        let t = self.term.lock().unwrap();
        let (r, c) = t.screen().size();
        (c.get(), r.get())
    }

    pub fn info(&self) -> Value {
        let (cols, rows) = self.size();
        let title = self.term.lock().unwrap().callbacks().title.clone();
        json!({
            "id": self.id, "kind": self.kind, "parent": self.parent, "title": title, "cwd": self.cwd,
            "argv": self.argv, "created_ms": self.created_ms, "exit": *self.exit.lock().unwrap(),
            "attached": self.attached.load(Ordering::Relaxed), "cols": cols, "rows": rows,
        })
    }

    pub fn kill(&self) {
        let _ = self.killer.lock().unwrap().kill();
    }
}

pub struct Daemon {
    pub sessions: Mutex<BTreeMap<u64, Arc<Session>>>,
    next: AtomicU64,
    pub last: Mutex<Option<u64>>,
    pub web_port: u16,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Req {
    List,
    Info,
    Last,
    Spawn {
        kind: String,
        #[serde(default)]
        args: Vec<String>,
        parent: Option<u64>,
        cwd: Option<String>,
        prompt: Option<String>,
        cols: Option<u16>,
        rows: Option<u16>,
    },
    Attach { id: u64, cols: Option<u16>, rows: Option<u16> },
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
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let sock = paths::sock();
        let cwd = cwd.unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()));
        let launch = profiles::launch(kind, &profiles::Ctx { session: id, cwd: &cwd, sock: &sock, user_args: args, prompt })?;
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
        for (k, v) in &launch.env {
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
        let (tx, _) = broadcast::channel(4096);
        let mut argv = vec![launch.program.clone()];
        argv.extend(launch.args.iter().cloned());
        let session = Arc::new(Session {
            id, kind: kind.into(), parent, cwd, argv, created_ms: now_ms(),
            term: Mutex::new(vt100::Parser::new_with_callbacks(
                NonZeroU16::new(rows).unwrap_or(NonZeroU16::new(24).unwrap()),
                NonZeroU16::new(cols).unwrap_or(NonZeroU16::new(80).unwrap()),
                10_000, TitleCb::default(),
            )),
            writer: Mutex::new(writer), master: Mutex::new(pair.master), killer: Mutex::new(killer),
            tx, exit: Mutex::new(None), attached: AtomicUsize::new(0),
        });
        self.sessions.lock().unwrap().insert(id, session.clone());

        let s = session.clone();
        std::thread::Builder::new().name(format!("pty-read-{id}")).spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let f = {
                    let mut term = s.term.lock().unwrap();
                    let f = queries::filter(&buf[..n], &mut term);
                    if !f.out.is_empty() {
                        let _ = s.tx.send(Ev::Data(Bytes::from(f.out.clone())));
                    }
                    f
                };
                if !f.replies.is_empty() {
                    let _ = s.write(&f.replies);
                }
            }
        })?;
        let s = session.clone();
        std::thread::Builder::new().name(format!("pty-wait-{id}")).spawn(move || {
            let code = child.wait().map(|st| st.exit_code() as i32).unwrap_or(-1);
            *s.exit.lock().unwrap() = Some(code);
            let _ = s.tx.send(Ev::Exit(code));
        })?;
        eprintln!("spawned session {id}: {kind} (parent {parent:?})");
        Ok(id)
    }

    fn handle(self: &Arc<Self>, req: Req) -> Result<Value> {
        Ok(match req {
            Req::List => json!({ "sessions": self.list() }),
            Req::Info => json!({ "pid": std::process::id(), "web": format!("http://127.0.0.1:{}", self.web_port), "sock": paths::sock() }),
            Req::Last => {
                let last = *self.last.lock().unwrap();
                let alive = last.and_then(|id| self.get(id).ok()).filter(|s| s.exit.lock().unwrap().is_none()).map(|s| s.id);
                json!({ "id": alive })
            }
            Req::Spawn { kind, args, parent, cwd, prompt, cols, rows } => {
                let id = self.spawn(&kind, &args, parent, cwd, prompt.as_deref(), cols.unwrap_or(120), rows.unwrap_or(40))?;
                json!({ "id": id })
            }
            Req::Send { id, text, raw, submit } => {
                let s = self.get(id)?;
                if raw {
                    s.write(text.as_bytes())?;
                } else {
                    s.write(format!("\x1b[200~{text}\x1b[201~").as_bytes())?;
                    if submit.unwrap_or(true) {
                        // Let the TUI finish handling the paste before Enter, or it may land inside it.
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        s.write(b"\r")?;
                    }
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
            Req::Attach { .. } => unreachable!("attach is handled by the connection loop"),
        })
    }
}

pub async fn run(web_port: u16) -> Result<()> {
    std::fs::create_dir_all(paths::home())?;
    let sock = paths::sock();
    if UnixStream::connect(&sock).await.is_ok() {
        anyhow::bail!("harnessd already running at {}", sock.display());
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    let web = tokio::net::TcpListener::bind(("127.0.0.1", web_port)).await.with_context(|| format!("bind web port {web_port}"))?;
    let web_port = web.local_addr()?.port();
    let d = Arc::new(Daemon { sessions: Mutex::new(BTreeMap::new()), next: AtomicU64::new(1), last: Mutex::new(None), web_port });
    std::fs::write(paths::info(), json!({ "pid": std::process::id(), "web": format!("http://127.0.0.1:{web_port}") }).to_string())?;
    eprintln!("harnessd pid {} socket {} web http://127.0.0.1:{web_port}", std::process::id(), sock.display());
    tokio::spawn(web::serve(d.clone(), web));
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
        let d2 = d.clone();
        let resp = tokio::task::spawn_blocking(move || d2.handle(req)).await?;
        let v = resp.unwrap_or_else(|e| json!({ "error": format!("{e:#}") }));
        proto::write_json(&mut w, &v).await?;
    }
    Ok(())
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
