//! harnessd: owns every agent PTY, mirrors each into a libghostty terminal (harness-vt) (so a re-attaching client or
//! the web view can redraw the current screen), records the parent/child tree, and serves the
//! socket API plus the web UI. Survives client exit like a tmux server.
use crate::{archive, chat, db, messages, paths, profiles, queries, runs};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use harness_proto::event::Body;
use harness_proto::frame::{T_CBOR, T_DATA, aio};
use harness_proto::msg::{Down, Reply, Request as Req, SessionInfo, Up};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use parking_lot::Mutex;
use std::sync::Arc;
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
    /// A terminal session's output.
    Data(Bytes),
    /// A chat session's event: its whole T_EVENT frame, the bytes its log holds (chat/mod.rs).
    Event(Bytes),
    Exit(i32),
}

pub struct Session {
    pub id: u64,
    pub kind: String,
    /// Unique among running objects; how a get-or-create spawn finds it again.
    pub name: Option<String>,
    /// What the person renamed it to (`rename`), kept in its index row's meta as `label`.
    label: Mutex<Option<String>>,
    pub parent: Option<u64>,
    pub cwd: String,
    pub argv: Vec<String>,
    pub created_ms: u64,
    /// The agent process (for `signal`); a chat's changes when it is resumed, and is None while its agent is not running.
    pid: Mutex<Option<u32>>,
    pub term: Mutex<harness_vt::Term>,
    /// The PTY's input, or a chat child's stdin.
    writer: Mutex<Box<dyn Write + Send>>,
    /// None for a chat session (no PTY).
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
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
    /// A chat session's conversation (chat/mod.rs): it has no PTY, and its terminal mirror stays blank.
    pub chat: Option<Arc<chat::Chat>>,
    /// Held while a chat's agent is being resumed, so two messages at once start it once.
    resuming: Mutex<()>,
}

/// The stand-in for a chat's agent while none runs (a chat restored from its log, before a resume): writing to it is an
/// error naming why, and there is nothing to kill.
#[derive(Debug, Clone, Copy)]
struct NoAgent;

impl Write for NoAgent {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("this chat's agent is not running (a message resumes it)"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ChildKiller for NoAgent {
    fn kill(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(*self)
    }
}

/// When this daemon started (ms since the epoch): a session that ended before it is past (`list`'s `past`).
static STARTED_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Each kernel's UI feed (ui_feed.rs): where the app's `ui_event`s go.
static FEEDS: std::sync::LazyLock<Mutex<crate::ui_feed::Feeds<FeedSink>>> = std::sync::LazyLock::new(|| Mutex::new(crate::ui_feed::Feeds::default()));

/// A `ui_listen` connection's queue, [`crate::ui_feed::FEED_CAP`] deep.
struct FeedSink(tokio::sync::mpsc::Sender<crate::ui_feed::UiEv>);

impl crate::ui_feed::Sink for FeedSink {
    fn offer(&self, ev: crate::ui_feed::UiEv) -> Result<(), crate::ui_feed::Refused> {
        use tokio::sync::mpsc::error::TrySendError;
        self.0.try_send(ev).map_err(|e| match e {
            TrySendError::Full(_) => crate::ui_feed::Refused::Full,
            TrySendError::Closed(_) => crate::ui_feed::Refused::Closed,
        })
    }
}

impl Session {
    /// A chat session around `chat`, its agent not running yet (a new chat starts it next; a restored one when resumed).
    #[allow(clippy::too_many_arguments)]
    fn chat(
        id: u64, kind: &str, name: Option<String>, label: Option<String>, parent: Option<u64>, cwd: String, argv: Vec<String>,
        created_ms: u64, chat: Arc<chat::Chat>, ended: Option<(&'static str, u64)>, exit: Option<i32>,
    ) -> Self {
        let (tx, _) = broadcast::channel(16384);
        Self {
            id, kind: kind.into(), name, label: Mutex::new(label), parent, cwd, argv, created_ms, pid: Mutex::new(None),
            term: Mutex::new(harness_vt::Term::new(80, 24, 0)),
            writer: Mutex::new(Box::new(NoAgent)), master: Mutex::new(None), killer: Mutex::new(Box::new(NoAgent)),
            tx, exit: Mutex::new(exit), ended: Mutex::new(ended), killed: AtomicBool::new(false), attached: AtomicUsize::new(0),
            last_out: AtomicU64::new(0), busy: AtomicBool::new(chat.busy.load(Ordering::Relaxed)), chat: Some(chat),
            resuming: Mutex::new(()),
        }
    }

    pub fn pid(&self) -> Option<u32> {
        *self.pid.lock()
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut w = self.writer.lock();
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        if cols == 0 || rows == 0 {
            return Ok(());
        }
        let master = self.master.lock();
        let Some(master) = master.as_ref() else { return Ok(()) };
        master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).map_err(|e| anyhow!("{e}"))?;
        self.term.lock().resize(cols, rows);
        Ok(())
    }

    /// Escape codes that redraw the current screen on a fresh terminal, plus a receiver for
    /// everything after it. Taken under the emulator lock so nothing is lost or doubled.
    pub fn snapshot_and_subscribe(&self) -> (Vec<u8>, broadcast::Receiver<Ev>) {
        let mut term = self.term.lock();
        let rx = self.tx.subscribe();
        (term.vt_snapshot(), rx)
    }

    pub fn screen_text(&self) -> String {
        if let Some(c) = &self.chat {
            return c.screen();
        }
        self.term.lock().text()
    }

    pub fn size(&self) -> (u16, u16) {
        self.term.lock().size()
    }

    pub fn info(&self) -> SessionInfo {
        let (cols, rows) = self.size();
        let title = match &self.chat {
            Some(c) => c.title(),
            None => self.term.lock().title(),
        };
        let ended = *self.ended.lock();
        SessionInfo {
            id: self.id,
            object: format!("s{}", self.id),
            kind: self.kind.clone(),
            name: self.name.clone(),
            label: self.label.lock().clone(),
            parent: self.parent,
            title,
            cwd: Some(self.cwd.clone()),
            argv: self.argv.clone(),
            created_ms: Some(self.created_ms),
            ended_ms: ended.map(|(_, ms)| ms),
            exit: self.exit.lock().map(i64::from),
            status: Some(ended.map_or("running", |(s, _)| s).to_owned()),
            attached: self.attached.load(Ordering::Relaxed) as u64,
            cols: Some(cols),
            rows: Some(rows),
            busy: self.busy.load(Ordering::Relaxed),
            mode: if self.chat.is_some() { "chat" } else { "tui" }.to_owned(),
            model: self.chat.as_ref().map(|c| c.model()),
            effort: self.chat.as_ref().map(|c| c.effort()).filter(|e| !e.is_empty()),
            past: false,
        }
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
        if self.chat.is_none() && self.busy.load(Ordering::Relaxed) && now.saturating_sub(self.last_out.load(Ordering::Relaxed)) >= BUSY_MS && self.busy.swap(false, Ordering::Relaxed) {
            changed();
        }
    }

    /// A chat session's message, its words and the pictures at `images`: its user lines to the agent (Err: not a chat
    /// session, or one of `images` is not a picture an agent takes).
    pub fn chat_send(&self, text: &str, images: &[String]) -> Result<()> {
        let c = self.chat.as_ref().ok_or_else(|| anyhow!("session {} is not a chat session", self.id))?;
        chat::check_pictures(images).with_context(|| format!("sending to chat s{}", self.id))?;
        let untitled = c.title().is_empty();
        let lines = c.user_lines(text, images, &self.tx);
        if untitled && !c.title().is_empty() {
            // The index keeps the title, so `list` names a past chat without reading its log.
            db::set_meta(&format!("s{}", self.id), "title", &json!(c.title()));
        }
        self.busy.store(true, Ordering::Relaxed);
        changed();
        self.write_lines(&lines)
    }

    /// Switch a chat session's model and/or effort for its turns from here on ([`chat::Chat::choose`]): recorded, and asked
    /// of its agent when it runs (else its resume starts with it).
    pub fn chat_choose(&self, model: Option<&str>, effort: Option<&str>) -> Result<()> {
        let c = self.chat.as_ref().ok_or_else(|| anyhow!("session {} is not a chat session, so it has no model to set", self.id))?;
        // An ended chat only records the choice (nothing runs to tell); its resume starts the agent with it.
        let live = self.ended.lock().is_none();
        let lines = c.choose(&self.kind, model, effort, live, &self.tx)?;
        changed();
        self.write_lines(&lines)
    }

    /// Name it `title` (trimmed; empty: its name taken away), in its index row too, so a restart and `list`'s past rows
    /// keep it. Refused, naming the session, for a title over a line.
    pub fn rename(&self, title: &str) -> Result<()> {
        let title = title.trim();
        if title.contains(['\n', '\r']) {
            anyhow::bail!("session {}: {title:?} is not a name: a name is one line", self.id);
        }
        let label = (!title.is_empty()).then(|| title.to_owned());
        db::set_meta(&format!("s{}", self.id), "label", &json!(label));
        *self.label.lock() = label;
        changed();
        Ok(())
    }

    /// Stop a chat session's turn in flight.
    pub fn chat_interrupt(&self) -> Result<()> {
        let c = self.chat.as_ref().ok_or_else(|| anyhow!("session {} is not a chat session", self.id))?;
        self.write_lines(&c.interrupt_lines())
    }

    /// Protocol lines to a chat child's stdin, each with its newline.
    fn write_lines(&self, lines: &[String]) -> Result<()> {
        for l in lines {
            self.write(format!("{l}\n").as_bytes())?;
        }
        Ok(())
    }

    pub fn kill(&self) {
        self.killed.store(true, Ordering::Relaxed);
        let _ = self.killer.lock().kill();
    }
}

pub struct Daemon {
    pub sessions: Mutex<BTreeMap<u64, Arc<Session>>>,
    next: AtomicU64,
    pub last: Mutex<Option<u64>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

impl Daemon {
    /// Session `id`: a live one, else a chat from before this daemon started, restored from its log (chat/mod.rs) and kept
    /// from then on as a session whose agent is not running.
    pub fn get(&self, id: u64) -> Result<Arc<Session>> {
        if let Some(s) = self.sessions.lock().get(&id).cloned() {
            return Ok(s);
        }
        let row = db::session(id).ok_or_else(|| anyhow!("no session {id}"))?;
        if row.mode != "chat" {
            anyhow::bail!("no session {id}: it ended before this daemon started, and only a chat can be opened again");
        }
        let s = Arc::new(Self::restore_chat(id, &row)?);
        Ok(self.sessions.lock().entry(id).or_insert(s).clone())
    }

    /// A past chat (`row` its index row) as a session: its conversation folded back from its log.
    fn restore_chat(id: u64, row: &SessionInfo) -> Result<Session> {
        let oid = format!("s{id}");
        let dir = db::object_dir(&oid);
        let c = chat::Chat::open(&oid, &dir, Box::new(chat::NoAdapter))
            .with_context(|| format!("restoring chat {oid} from {}", dir.join("chat").display()))?;
        let status: &'static str = match row.status.as_deref() {
            Some("killed") => "killed",
            Some("exited") => "exited",
            _ => "lost",
        };
        let ended = row.ended_ms.or(row.created_ms).unwrap_or(0);
        Ok(Session::chat(
            id, &row.kind, row.name.clone(), row.label.clone(), row.parent, row.cwd.clone().unwrap_or_else(|| "/".to_owned()),
            row.argv.clone(), row.created_ms.unwrap_or(0), Arc::new(c), Some((status, ended)), row.exit.map(|c| c as i32),
        ))
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions.lock().values().map(|s| s.info()).collect()
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
        self.spawn_full(kind, args, parent, cwd, prompt, name, env, cols, rows, None)
    }

    /// [`Self::spawn_named`], a Codex chat starting on `codex_choice` (its model and effort, on its thread/start and every
    /// turn; recorded as the chat's choice, so a resume keeps them).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_full(
        self: &Arc<Self>, kind: &str, args: &[String], parent: Option<u64>, cwd: Option<String>, prompt: Option<&str>,
        name: Option<String>, env: &BTreeMap<String, String>, cols: u16, rows: u16,
        codex_choice: Option<(Option<String>, Option<String>)>,
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
        // Claude and Codex run as a chat (stream-json, app-server; drawn by the app) unless `--tui` asks for their
        // terminal UI.
        let tui = args.iter().position(|a| a == "--tui").map(|i| args.remove(i)).is_some();
        let chat = matches!(kind, "claude" | "codex") && !tui;
        if chat {
            meta["mode"] = json!("chat");
            // Its log is written in this schema from the start: nothing for a later start to convert.
            meta["chat_log"] = json!(harness_proto::SCHEMA);
            // What the person asked for (a model, flags): a resume launches with them again.
            meta["user_args"] = json!(args);
        }
        let resumes = args.iter().any(|a| matches!(a.as_str(), "-r" | "--resume" | "-c" | "--continue" | "--session-id"));
        if kind == "claude" && !resumes {
            let uuid = uuid_v4();
            args.splice(0..0, ["--session-id".to_string(), uuid.clone()]);
            meta["claude_session"] = json!(uuid);
        }
        let launch = profiles::launch(
            kind, &profiles::Ctx { session: id, cwd: &cwd, sock: &sock, user_args: &args, prompt: if chat { None } else { prompt }, chat },
        )?;
        if chat {
            return self.spawn_chat(id, kind, launch, parent, cwd, prompt, name, env, meta, created_ms, codex_choice);
        }
        let mut cmd = CommandBuilder::new(&launch.program);
        cmd.args(&launch.args);
        cmd.cwd(&cwd);
        // The profile's own variables are set below, over the daemon's less another agent's markers.
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if agent_marker(&k) {
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
            id, kind, name: name.as_deref(), parent, cwd: &cwd, argv: &argv, created_ms, meta, stream: "pty",
        });
        let session = Arc::new(Session {
            id, kind: kind.into(), name, label: Mutex::new(None), parent, cwd, argv, created_ms, pid: Mutex::new(pid),
            term: Mutex::new(harness_vt::Term::new(if cols == 0 { 80 } else { cols }, if rows == 0 { 24 } else { rows }, 10_000)),
            writer: Mutex::new(writer), master: Mutex::new(Some(pair.master)), killer: Mutex::new(killer),
            tx, exit: Mutex::new(None), ended: Mutex::new(None), killed: AtomicBool::new(false), attached: AtomicUsize::new(0),
            last_out: AtomicU64::new(0), busy: AtomicBool::new(false), chat: None, resuming: Mutex::new(()),
        });
        self.sessions.lock().insert(id, session.clone());
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
                    let mut term = s.term.lock();
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
            *s.ended.lock() = Some((status, ended));
            *s.exit.lock() = Some(code);
            changed();
            let _ = s.tx.send(Ev::Exit(code));
            db::session_end(s.id, code, status, ended);
            archive::compact_bg(format!("s{}", s.id));
        })?;
        eprintln!("spawned session {id}: {kind} (parent {parent:?})");
        Ok(id)
    }

    /// A chat session (chat/mod.rs): its log opened, its agent started ([`Daemon::start_agent`]); `prompt`, if any, sent
    /// as its first message.
    #[allow(clippy::too_many_arguments)]
    fn spawn_chat(
        self: &Arc<Self>, id: u64, kind: &str, launch: profiles::Launch, parent: Option<u64>, cwd: String,
        prompt: Option<&str>, name: Option<String>, env: &BTreeMap<String, String>, meta: Value, created_ms: u64,
        codex_choice: Option<(Option<String>, Option<String>)>,
    ) -> Result<u64> {
        let adapter: Box<dyn chat::Adapter> = match kind {
            "claude" => Box::new(chat::claude::Claude::new(&launch)),
            "codex" => Box::new(chat::codex::Codex::new(&cwd).with_choice(codex_choice.clone().unwrap_or_default())),
            other => anyhow::bail!("{other} has no chat adapter (chat/mod.rs); spawn it with mode \"tui\""),
        };
        let oid = format!("s{id}");
        let mut argv = vec![launch.program.clone()];
        argv.extend(launch.args.iter().cloned());
        db::session_start(&db::SessionRow {
            id, kind, name: name.as_deref(), parent, cwd: &cwd, argv: &argv, created_ms, meta, stream: "chat",
        });
        let c = Arc::new(chat::Chat::open(&oid, &db::object_dir(&oid), Box::new(chat::NoAdapter))?);
        let session = Arc::new(Session::chat(id, kind, name, None, parent, cwd, argv, created_ms, c, None, None));
        if let (Some((model, effort)), Some(c)) = (codex_choice, &session.chat) {
            c.record(Body::Choice { model, effort }, &session.tx);
        }
        self.sessions.lock().insert(id, session.clone());
        self.start_agent(&session, &launch, adapter, env)?;
        changed();
        if let Some(p) = prompt.filter(|p| !p.trim().is_empty()) {
            session.chat_send(p, &[])?;
        }
        eprintln!("spawned chat session {id}: {kind} (parent {parent:?})");
        Ok(id)
    }

    /// Start chat `s`'s agent: `launch` run with stdin and stdout piped, each line it writes recorded through `adapter`
    /// (chat/claude.rs, chat/codex.rs) into the chat's log, until it exits. The same for a new chat and a resumed one.
    fn start_agent(
        &self, s: &Arc<Session>, launch: &profiles::Launch, adapter: Box<dyn chat::Adapter>, env: &BTreeMap<String, String>,
    ) -> Result<()> {
        use std::io::BufRead;
        let c = s.chat.clone().ok_or_else(|| anyhow!("session {} is not a chat session", s.id))?;
        let (id, oid) = (s.id, format!("s{}", s.id));
        let dir = db::object_dir(&oid);
        db::index_stream(&oid, "chat", &dir.join("chat"));
        let mut cmd = agent_command(s, launch, env);
        let stderr = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("stderr"))?;
        cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(stderr);
        let mut child = cmd.spawn().with_context(|| format!("{oid}: spawn {}", launch.program))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("{oid}: no stdin pipe"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("{oid}: no stdout pipe"))?;
        let pid = child.id();
        c.set_adapter(&dir, adapter)?;
        *s.writer.lock() = Box::new(stdin);
        *s.killer.lock() = Box::new(chat::PidKiller(pid));
        *s.pid.lock() = Some(pid);
        s.killed.store(false, Ordering::Relaxed);
        *s.exit.lock() = None;
        *s.ended.lock() = None;
        let reader = {
            let (s, c) = (s.clone(), c.clone());
            std::thread::Builder::new().name(format!("chat-read-{id}")).spawn(move || {
                let mut r = std::io::BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let (flipped, replies) = c.agent_line(&line, &s.tx);
                    if flipped {
                        s.busy.store(c.busy.load(Ordering::Relaxed), Ordering::Relaxed);
                        changed();
                    }
                    if let Err(e) = s.write_lines(&replies) {
                        eprintln!("chat session {id}: writing to the agent: {e}");
                    }
                }
            })?
        };
        let s2 = s.clone();
        std::thread::Builder::new().name(format!("chat-wait-{id}")).spawn(move || {
            let s = s2;
            let st = child.wait();
            // Every line it wrote is recorded before the chat says it ended (and before a resume can start another).
            let _ = reader.join();
            let code = st.as_ref().ok().and_then(std::process::ExitStatus::code).unwrap_or(-1);
            let signalled = st.as_ref().is_ok_and(|st| std::os::unix::process::ExitStatusExt::signal(st).is_some());
            let status = if signalled || s.killed.load(Ordering::Relaxed) { "killed" } else { "exited" };
            let ended = now_ms();
            if let Some(c) = &s.chat {
                c.ended(&s.tx);
            }
            *s.writer.lock() = Box::new(NoAgent);
            *s.killer.lock() = Box::new(NoAgent);
            *s.pid.lock() = None;
            s.busy.store(false, Ordering::Relaxed);
            *s.ended.lock() = Some((status, ended));
            *s.exit.lock() = Some(code);
            changed();
            let _ = s.tx.send(Ev::Exit(code));
            db::session_end(s.id, code, status, ended);
        })?;
        s.write_lines(&c.start_lines())?;
        Ok(())
    }

    /// A message to chat `s`: sent to its agent, which is resumed first when it is not running.
    fn chat_send(self: &Arc<Self>, s: &Arc<Session>, text: &str, images: &[String]) -> Result<()> {
        {
            let _one = s.resuming.lock();
            if s.ended.lock().is_some() {
                if let Err(e) = self.resume(s) {
                    if let Some(c) = &s.chat {
                        let why = format!("Can't resume this chat: {e:#}. The message was not sent.");
                        c.record(Body::Error { text: why }, &s.tx);
                    }
                    return Err(e.context(format!("resuming chat s{}", s.id)));
                }
            }
        }
        s.chat_send(text, images)
    }

    /// Start chat `s`'s agent again on its log, continuing its conversation by the agent's own id for it (from the log):
    /// Claude with `--resume`, Codex with `thread/resume`. An error names why it cannot; it never starts a fresh one.
    fn resume(self: &Arc<Self>, s: &Arc<Session>) -> Result<()> {
        if let Some(n) = &s.name {
            db::check_name(n)?;
        }
        let Relaunch { launch, session, chosen, meta } = relaunch(s, false)?;
        // Claude's adapter says the effort its launch passed, so it is made after the launch.
        let adapter: Box<dyn chat::Adapter> = if s.kind == "claude" {
            Box::new(chat::claude::Claude::new(&launch))
        } else {
            Box::new(chat::codex::Codex::resume(&s.cwd, &session).with_choice(chosen))
        };
        let mut argv = vec![launch.program.clone()];
        argv.extend(launch.args.iter().cloned());
        db::session_start(&db::SessionRow {
            id: s.id, kind: &s.kind, name: s.name.as_deref(), parent: s.parent, cwd: &s.cwd, argv: &argv, created_ms: s.created_ms,
            meta, stream: "chat",
        });
        self.start_agent(s, &launch, adapter, &BTreeMap::new())?;
        changed();
        eprintln!("resumed chat session {}: {} {session}", s.id, s.kind);
        Ok(())
    }

    fn handle(self: &Arc<Self>, req: Req) -> Result<Reply> {
        #[cfg(test)]
        if tests::PANIC_NEXT.swap(false, Ordering::SeqCst) {
            // The planted panic of `tests::a_panicking_request_fails_only_itself`, holding the lock every request takes.
            let _held = self.sessions.lock();
            panic!("planted panic");
        }
        Ok(match req {
            Req::List { all, chats } => {
                let mut v = self.list();
                if all || chats {
                    let live: std::collections::HashSet<u64> = v.iter().map(|s| s.id).collect();
                    v.extend(db::sessions().into_iter().filter(|s| !live.contains(&s.id) && (all || s.mode == "chat")));
                    v.sort_by_key(|s| s.id);
                }
                let started = STARTED_MS.get().copied().unwrap_or(0);
                for s in &mut v {
                    s.past = s.ended_ms.is_some_and(|e| e < started);
                }
                Reply::Sessions { sessions: v, models: (*chat::models::snapshot()).clone() }
            }
            Req::ReplTask { task } => {
                db::repl_task(&task)?;
                // A cell a chat session's tool call ran: that call's card (chat/mod.rs `tool_detail`).
                if let (Some(call), Some(sid)) = (task["call"].as_str(), task["session"].as_u64())
                    && let Ok(s) = self.get(sid)
                    && let Some(c) = &s.chat
                {
                    c.record(chat::tool_detail(call, &task), &s.tx);
                }
                changed();
                Reply::Ok
            }
            // The app's chips and hover cards (github.rs, page.rs): gh's token and curl stay in their own processes.
            Req::GithubStatus { refs, fresh } => Reply::Github { items: crate::github::status(&refs, fresh)? },
            Req::GithubChecks { pr } => Reply::GithubChecks { checks: crate::github::checks(&pr) },
            Req::PagePeek { url } => Reply::Page { page: crate::page::peek(&url) },
            Req::Info => Reply::Daemon { pid: std::process::id(), sock: paths::sock().display().to_string() },
            Req::Last => {
                let last = *self.last.lock();
                let alive = last.and_then(|id| self.get(id).ok()).filter(|s| s.exit.lock().is_none()).map(|s| s.id);
                Reply::Last { id: alive }
            }
            Req::SpawnObject { opts } => Reply::Record(crate::object::spawn(self, &opts)?),
            Req::Object { id } => Reply::Record(crate::object::info(self, &id)?),
            Req::Rpc { id, method, args } => crate::object::rpc(self, &id, &method, &args, |r| self.handle(r))?,
            Req::Spawn { kind, mut args, parent, cwd, prompt, name, cols, rows, mode, model, effort } => {
                let tui = mode.as_deref() == Some("tui");
                let choice = launch_choice(&kind, &mut args, model, effort, tui)?;
                let id = self.spawn_full(&kind, &args, parent, cwd, prompt.as_deref(), name, &BTreeMap::new(), cols.unwrap_or(120), rows.unwrap_or(40), choice)?;
                Reply::Spawned { id }
            }
            Req::Send { id, text, raw, submit, images } => {
                let s = self.get(id)?;
                if !images.is_empty() && (raw || s.chat.is_none()) {
                    anyhow::bail!("session {id} takes no pictures: only a chat message carries them");
                }
                if raw {
                    s.write(text.as_bytes())?;
                } else if s.chat.is_some() {
                    self.chat_send(&s, &text, &images)?;
                } else {
                    crate::inject::send(&s, &text, submit.unwrap_or(true))?;
                }
                Reply::Ok
            }
            Req::SetModel { id, model, effort } => {
                self.get(id)?.chat_choose(model.as_deref(), effort.as_deref())?;
                Reply::Ok
            }
            Req::Rename { id, title } => {
                self.get(id)?.rename(&title)?;
                Reply::Ok
            }
            // Its one turn may take minutes: it holds only this request's connection and blocking thread (each request
            // has its own) and no lock, so every other request goes on.
            Req::Aside { id, text } => {
                let s = self.get(id)?;
                Reply::Aside { text: aside(&s, &text)? }
            }
            Req::Screen { id } => {
                let s = self.get(id)?;
                Reply::Screen { text: s.screen_text(), info: s.info() }
            }
            Req::Resize { id, cols, rows } => {
                self.get(id)?.resize(cols, rows)?;
                Reply::Ok
            }
            Req::Kill { id } => {
                self.get(id)?.kill();
                Reply::Ok
            }
            Req::Forget { id } => {
                let s = self.get(id)?;
                if s.exit.lock().is_none() {
                    s.kill();
                }
                self.sessions.lock().remove(&id);
                changed();
                Reply::Ok
            }
            Req::Shutdown => {
                for s in self.sessions.lock().values() {
                    s.kill();
                }
                crate::browser::stop();
                let _ = std::fs::remove_file(paths::sock());
                std::process::exit(0);
            }
            Req::Runs(r) => Reply::Record(runs::handle(r)?),
            Req::Browser => crate::browser::request()?,
            Req::UiEvent { cell, handler, value } => {
                FEEDS.lock().deliver(crate::ui_feed::UiEv { cell, handler, value }).map_err(anyhow::Error::msg)?;
                Reply::Ok
            }
            Req::Attach { .. } | Req::Watch | Req::UiListen { .. } => {
                anyhow::bail!("attach, watch and ui_listen hold the connection: send them as a connection's request, not through rpc")
            }
        })
    }
}

/// A variable another agent sets in its sessions (Claude Code's, Codex's, an agent marker): the daemon inherits whatever
/// environment first started it (often another agent's shell), and never passes these on to an agent it starts.
fn agent_marker(k: &str) -> bool {
    k == "CLAUDECODE" || k == "AI_AGENT" || k.starts_with("CLAUDE_CODE_") || k.starts_with("CODEX_")
}

/// The command that runs `launch` for chat `s` in its directory: the daemon's environment less [`agent_marker`]s, the
/// profile's variables and `env` over it, its parent named. A chat's agent and an aside's fork start alike.
fn agent_command(s: &Session, launch: &profiles::Launch, env: &BTreeMap<String, String>) -> std::process::Command {
    let mut cmd = std::process::Command::new(&launch.program);
    cmd.args(&launch.args).current_dir(&s.cwd);
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if agent_marker(&k) {
            cmd.env_remove(k.as_ref());
        }
    }
    cmd.envs(launch.env.iter().chain(env));
    if let Some(p) = s.parent {
        cmd.env("HARNESS_PARENT", p.to_string());
    }
    cmd
}

/// How chat `s`'s agent starts again on its conversation, for a resume or an aside's fork ([`relaunch`]): the launch,
/// the agent's own id for the conversation, the model and effort the chat chose since it started (each None when never
/// chosen), and its index row's meta.
struct Relaunch {
    launch: profiles::Launch,
    session: String,
    chosen: (Option<String>, Option<String>),
    meta: Value,
}

/// [`Relaunch`] for chat `s`, the one place its launch is built again: the person's own args from its first launch (a
/// model, flags), less any that chose a conversation and less a model or effort the chat has chosen since (`choice`
/// events), which it runs with instead. Claude takes `--resume <id>` and the choice as flags, and with `fork` also
/// `--fork-session --no-session-persistence` (a new conversation from the old one's history, never written: an aside);
/// Codex's choice and thread ride on its adapter (chat/codex.rs `resume`, `fork`, `with_choice`).
///
/// The flags that pick Claude's conversation `session` on a relaunch. One the agent spoke in is on disk: `--resume`, and
/// for an aside (`fork`) a copy never written. One no turn ever reached (a new chat left empty, then lost with its
/// daemon) was never written, and `--resume` of it fails with "No conversation found with session ID": it starts under
/// the same id instead, and an aside of it starts from nothing, unwritten.
fn claude_conversation(session: &str, spoke: bool, fork: bool) -> Vec<String> {
    let flags: &[&str] = match (spoke, fork) {
        (true, false) => &["--resume", session],
        (true, true) => &["--resume", session, "--fork-session", "--no-session-persistence"],
        (false, false) => &["--session-id", session],
        (false, true) => &["--no-session-persistence"],
    };
    flags.iter().map(|f| (*f).to_owned()).collect()
}

/// # Errors
/// Not a chat; its log never named the agent's id for the conversation; a kind with no chat adapter; the launch's.
fn relaunch(s: &Session, fork: bool) -> Result<Relaunch> {
    let c = s.chat.clone().ok_or_else(|| anyhow!("session {} is not a chat session", s.id))?;
    let oid = format!("s{}", s.id);
    let meta = db::object_kind_meta(&oid).map(|(_, m)| m).unwrap_or_else(|| json!({}));
    // A Claude chat recorded before `session` events carried its id was started with `--session-id` set to the id
    // the index keeps (spawn_named): the same id, from where it was written then.
    let session = c.session_id().or_else(|| (s.kind == "claude").then(|| meta["claude_session"].as_str().map(str::to_owned)).flatten());
    let Some(session) = session else {
        anyhow::bail!("its log never named {}'s id for the conversation (it predates harness recording it), so there is nothing to resume or fork", s.kind);
    };
    let chosen = c.chosen();
    let mut args = Vec::new();
    let mut user = meta["user_args"].as_array().into_iter().flatten().filter_map(Value::as_str);
    while let Some(a) = user.next() {
        match a {
            "-r" | "--resume" | "--session-id" => {
                let _ = user.next();
            }
            "-m" | "--model" if chosen.0.is_some() => {
                let _ = user.next();
            }
            "--effort" if chosen.1.is_some() => {
                let _ = user.next();
            }
            a if chosen.0.is_some() && a.starts_with("--model=") => {}
            a if chosen.1.is_some() && a.starts_with("--effort=") => {}
            "-c" | "--continue" | "--fork-session" | "--no-session-persistence" => {}
            a => args.push(a.to_owned()),
        }
    }
    match s.kind.as_str() {
        "claude" => {
            args.extend(claude_conversation(&session, c.agent_spoke(), fork));
            if let Some(m) = &chosen.0 {
                args.extend(["--model".to_owned(), m.clone()]);
            }
            if let Some(e) = &chosen.1 {
                args.extend(["--effort".to_owned(), e.clone()]);
            }
        }
        // Codex's choice rides on its thread/resume (or thread/fork) and every turn/start (chat/codex.rs `with_choice`).
        "codex" => {}
        other => anyhow::bail!("{other} has no chat adapter to resume with"),
    }
    let launch = profiles::launch(
        &s.kind, &profiles::Ctx { session: s.id, cwd: &s.cwd, sock: &paths::sock(), user_args: &args, prompt: None, chat: true },
    )?;
    Ok(Relaunch { launch, session, chosen, meta })
}

/// An aside's fork driven through its turn: the adapter's opening lines and `text` as its one message written, then each
/// line it writes read into `turn` (the adapter's replies written back), until the turn ends (its answer) or its stdout
/// does (None: it exited, or the aside's watch stopped it).
fn ask_fork(
    adapter: &mut dyn chat::Adapter, turn: &mut chat::aside::Turn, stdin: &mut impl Write, stdout: impl Read, text: &str,
) -> std::io::Result<Option<chat::aside::Answer>> {
    use std::io::BufRead;
    send_lines(stdin, &adapter.start())?;
    send_lines(stdin, &adapter.user(text, &[]))?;
    let mut r = std::io::BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            continue;
        }
        for b in adapter.events(l) {
            if let Some(a) = turn.see(b) {
                return Ok(Some(a));
            }
        }
        send_lines(stdin, &adapter.replies())?;
    }
}

/// Protocol lines to an agent's stdin, each with its newline, flushed.
fn send_lines(w: &mut impl Write, lines: &[String]) -> std::io::Result<()> {
    for l in lines {
        w.write_all(l.as_bytes())?;
        w.write_all(b"\n")?;
    }
    w.flush()
}

/// An aside to chat `s` (`Request::Aside`): `text` (trimmed) asked of a fork of its conversation as one turn, and the
/// fork's last words. The fork is a one-shot process of its agent, launched as a resume is ([`relaunch`], so with the
/// chat's model and effort) but forked (claude's `--fork-session`, never saved; codex's ephemeral `thread/fork`), read
/// through the chat's own adapter into a [`chat::aside::Turn`] and stopped once the turn ends. Nothing it says reaches
/// the chat's log, and the chat's agent, running or not, is not touched. Its stderr goes to `aside.stderr` in the chat's
/// object directory, written anew each aside.
///
/// # Errors
/// Each names the chat: an aside with no words; the [`relaunch`]'s; the fork failing to start, failing its turn (the
/// error it reported), exiting before answering (naming its stderr), or not answering within
/// [`harness_proto::msg::ASIDE_SECS`] (it is stopped).
fn aside(s: &Session, text: &str) -> Result<String> {
    use harness_proto::msg::ASIDE_SECS;
    let oid = format!("s{}", s.id);
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "an aside to chat {oid} asks nothing");
    let r = relaunch(s, true).with_context(|| format!("forking chat {oid} for an aside"))?;
    let mut adapter: Box<dyn chat::Adapter> = if s.kind == "claude" {
        Box::new(chat::claude::Claude::new(&r.launch))
    } else {
        Box::new(chat::codex::Codex::fork(&s.cwd, &r.session).with_choice(r.chosen))
    };
    let dir = db::object_dir(&oid);
    std::fs::create_dir_all(&dir).with_context(|| format!("{oid}: making {}", dir.display()))?;
    let err_path = dir.join("aside.stderr");
    let stderr = std::fs::File::create(&err_path).with_context(|| format!("{oid}: opening {} for an aside", err_path.display()))?;
    let mut cmd = agent_command(s, &r.launch, &BTreeMap::new());
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(stderr);
    let mut child = cmd.spawn().with_context(|| format!("{oid}: spawning {} for an aside", r.launch.program))?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("{oid}: the aside's fork has no stdin pipe"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("{oid}: the aside's fork has no stdout pipe"))?;
    let pid = child.id();
    // The bound: past ASIDE_SECS the fork is stopped, its stdout ends, and the aside fails saying so. The watch ends as
    // soon as `done` goes (the aside is over).
    let late = Arc::new(AtomicBool::new(false));
    let (done, over) = std::sync::mpsc::channel::<()>();
    let watch = {
        let late = late.clone();
        std::thread::Builder::new().name(format!("aside-wait-{}", s.id)).spawn(move || {
            if over.recv_timeout(std::time::Duration::from_secs(ASIDE_SECS)) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                late.store(true, Ordering::Relaxed);
                let _ = chat::PidKiller(pid).kill();
            }
        })?
    };
    let mut turn = chat::aside::Turn::default();
    let said = ask_fork(adapter.as_mut(), &mut turn, &mut stdin, stdout, text);
    // Done with it either way: stopped (one that already exited refuses the signal, which is fine), reaped, the watch ended.
    let _ = chat::PidKiller(pid).kill();
    let _ = child.wait();
    drop(done);
    let _ = watch.join();
    let kind = &s.kind;
    if late.load(Ordering::Relaxed) {
        anyhow::bail!("{kind}'s fork of chat {oid} did not answer the aside within {ASIDE_SECS}s, so it was stopped");
    }
    let why = |what: String| match turn.error() {
        Some(e) => format!("{what} (it last said: {e}); its stderr is in {}", err_path.display()),
        None => format!("{what}; its stderr is in {}", err_path.display()),
    };
    match said {
        Ok(Some(Ok(words))) => Ok(words),
        Ok(Some(Err(e))) => anyhow::bail!("{kind}'s fork of chat {oid} could not answer the aside: {e}"),
        Ok(None) => anyhow::bail!("{}", why(format!("{kind}'s fork of chat {oid} exited before answering the aside"))),
        Err(e) => anyhow::bail!("{}", why(format!("talking to {kind}'s fork of chat {oid} for an aside: {e}"))),
    }
}

pub async fn run() -> Result<()> {
    // Every agent's MCP server is the kernel: without it no spawn can work, so say where it was looked for now.
    let kernel = paths::root().context("harnessd refuses to start")?;
    eprintln!("harnessd kernel {}", kernel.display());
    std::fs::create_dir_all(paths::home())?;
    let sock = paths::sock();
    if UnixStream::connect(&sock).await.is_ok() {
        anyhow::bail!("harnessd already running at {}", sock.display());
    }
    // Old chat logs move to this schema before anyone can attach to one (chat/mod.rs `migrate_logs`).
    chat::migrate_logs();
    // The models each agent kind last listed (chat/models.rs): what a chat with none of its own picks from.
    chat::models::restore();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    // The object index: session ids continue after the last one ever used; sessions the last daemon
    // left running are lost; older runs get indexed, finished objects compacted, transcripts mirrored.
    STARTED_MS.get_or_init(now_ms);
    db::mark_lost_sessions();
    let next = db::max_session_id() + 1;
    std::thread::spawn(|| {
        runs::backfill();
        archive::compact_pending();
        chat::backfill_titles();
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
                let sessions: Vec<Arc<Session>> = d.sessions.lock().values().cloned().collect();
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
        tokio::spawn(guarded(d, stream));
    }
}

/// One connection in its own task: a panic in it (outside a request, which [`blocking`] already turns into that
/// request's error) ends only this connection, and its client gets an error frame naming the panic.
async fn guarded(d: Arc<Daemon>, stream: UnixStream) {
    use std::os::fd::AsFd;
    // A second handle on the socket, to report a panic on once the task (and its handle) is gone.
    let report = stream.as_fd().try_clone_to_owned();
    match tokio::spawn(conn(d, stream)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("connection error: {e:#}"),
        Err(e) if e.is_panic() => {
            let error = format!("harnessd panicked serving this connection: {}; it keeps serving others", panic_text(&*e.into_panic()));
            eprintln!("{error}");
            if let Ok(fd) = report {
                let _ = tokio::task::spawn_blocking(move || report_panic(fd, &error)).await;
            }
        }
        Err(e) => eprintln!("connection task cancelled: {e}"),
    }
}

/// An error frame to a client whose connection task panicked, on a blocking copy of its socket (bounded by a write
/// timeout, so a client that reads nothing cannot hold the thread).
fn report_panic(fd: std::os::fd::OwnedFd, error: &str) {
    let mut s = std::os::unix::net::UnixStream::from(fd);
    let sent = s
        .set_nonblocking(false)
        .and_then(|()| s.set_write_timeout(Some(std::time::Duration::from_secs(1))))
        .and_then(|()| harness_proto::frame::write(&mut s, T_CBOR, &harness_proto::cbor::encode(&Down::Error { error: error.to_owned() })));
    if let Err(e) = sent {
        eprintln!("reporting a connection's panic to its client: {e}");
    }
}

/// What a panic said.
fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| (*s).to_owned()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "a panic with no message".to_owned())
}

/// `f` on the blocking pool; a panic in it is the error of `what` (the request it served), never the connection's or
/// the daemon's (rule panics-fail-one-job: parking_lot's locks do not poison, so the next request finds them usable).
async fn blocking<T: Send + 'static>(what: &str, f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) if e.is_panic() => {
            let error = anyhow!("harnessd panicked handling {what}: {}; it keeps serving", panic_text(&*e.into_panic()));
            eprintln!("{error}");
            Err(error)
        }
        Err(e) => Err(anyhow!("harnessd cancelled {what}: {e}")),
    }
}

/// A request, named for an error: its first 120 characters.
fn request_name(req: &Req) -> String {
    format!("{req:?}").chars().take(120).collect()
}

/// One message to a client.
async fn send<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, m: &Down) -> std::io::Result<()> {
    aio::write(w, T_CBOR, &harness_proto::cbor::encode(m)).await
}

/// The client's next message: None when it closed the connection.
///
/// # Errors
/// The socket's, or a frame that is not a T_CBOR [`Up`] (named).
async fn recv<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<Option<Up>> {
    let Some((t, payload)) = aio::read(r).await? else { return Ok(None) };
    if t != T_CBOR {
        anyhow::bail!("a frame of type {t} where a message (type {T_CBOR}) belongs");
    }
    Ok(Some(harness_proto::cbor::decode(&payload).map_err(|e| anyhow!("a message that is not harness schema {}'s: {e}", harness_proto::SCHEMA))?))
}

/// A connection: the client's hello (its schema must be this daemon's, else an error naming both and the connection
/// closes), then one answer per request, until an `attach` or `watch` turns it into a stream.
async fn conn(d: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let (mut r, mut w) = stream.into_split();
    match recv(&mut r).await {
        Ok(Some(Up::Hello { schema })) if schema == harness_proto::SCHEMA => {
            send(&mut w, &Down::Hello { schema, pid: std::process::id() }).await?;
        }
        Ok(None) => return Ok(()),
        other => {
            let error = match other {
                Ok(Some(Up::Hello { schema })) => {
                    format!("this client speaks harness schema {schema}, this daemon {}: update the older side", harness_proto::SCHEMA)
                }
                Ok(Some(m)) => format!("the connection must start with a hello naming the schema (harness schema {}), not {m:?}", harness_proto::SCHEMA),
                Err(e) => format!("{e:#}"),
                Ok(None) => unreachable!(),
            };
            send(&mut w, &Down::Error { error: error.clone() }).await?;
            anyhow::bail!("refused a client: {error}");
        }
    }
    loop {
        let req = match recv(&mut r).await {
            Ok(Some(Up::Req(req))) => req,
            Ok(None) => return Ok(()),
            Ok(Some(m)) => {
                send(&mut w, &Down::Error { error: format!("{m:?} belongs on an attach or watch stream, not as a request") }).await?;
                continue;
            }
            Err(e) => {
                send(&mut w, &Down::Error { error: format!("bad request: {e:#}") }).await?;
                return Err(e);
            }
        };
        if let Req::Attach { id, cols, rows, events } = req {
            let got = {
                let d = d.clone();
                blocking(&format!("attach to s{id}"), move || d.get(id)).await
            };
            let s = match got {
                Ok(s) => s,
                Err(e) => {
                    send(&mut w, &Down::Error { error: format!("{e:#}") }).await?;
                    continue;
                }
            };
            if let (Some(c), Some(rw)) = (cols, rows) {
                s.resize(c, rw)?;
            }
            *d.last.lock() = Some(id);
            send(&mut w, &Down::Reply(Reply::Attached { info: s.info() })).await?;
            if let Some(c) = s.chat.clone() {
                return attach_chat(s, c, events, r, w).await;
            }
            return attach(s, r, w).await;
        }
        if let Req::Watch = req {
            send(&mut w, &Down::Reply(Reply::Watching)).await?;
            return watch(r, w).await;
        }
        if let Req::UiListen { repl } = req {
            send(&mut w, &Down::Reply(Reply::Listening)).await?;
            return ui_listen(repl, r, w).await;
        }
        let d2 = d.clone();
        let what = request_name(&req);
        let resp = blocking(&what, move || d2.handle(req)).await;
        let m = match resp {
            Ok(reply) => Down::Reply(reply),
            Err(e) => Down::Error { error: format!("{e:#}") },
        };
        send(&mut w, &m).await?;
    }
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
                send(&mut w, &Down::Changed { generation }).await?;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            _ = aio::read(&mut r) => return Ok(()),
        }
    }
}

/// A `ui_listen` connection: kernel `repl`'s UI feed, a [`Down::Ui`] per event the app sends for one of its cells, until
/// the kernel sends anything or goes, or a newer listener for `repl` replaces it (its queue's sender is dropped).
async fn ui_listen(repl: String, mut r: tokio::net::unix::OwnedReadHalf, mut w: tokio::net::unix::OwnedWriteHalf) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(crate::ui_feed::FEED_CAP);
    let id = FEEDS.lock().listen(&repl, FeedSink(tx));
    let out = async {
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(crate::ui_feed::UiEv { cell, handler, value }) => send(&mut w, &Down::Ui { cell, handler, value }).await?,
                    None => return Ok::<(), anyhow::Error>(()),
                },
                _ = aio::read(&mut r) => return Ok(()),
            }
        }
    }
    .await;
    FEEDS.lock().gone(&repl, id);
    out
}

async fn attach(s: Arc<Session>, mut r: tokio::net::unix::OwnedReadHalf, mut w: tokio::net::unix::OwnedWriteHalf) -> Result<()> {
    s.attached.fetch_add(1, Ordering::Relaxed);
    let (snap, mut rx) = s.snapshot_and_subscribe();
    let exited = *s.exit.lock();
    let mut out = {
        let s = s.clone();
        tokio::spawn(async move {
            aio::write(&mut w, T_DATA, &snap).await?;
            if let Some(code) = exited {
                send(&mut w, &Down::Exit { code }).await?;
                return Ok::<_, anyhow::Error>(());
            }
            loop {
                match rx.recv().await {
                    Ok(Ev::Data(b)) => aio::write(&mut w, T_DATA, &b).await?,
                    Ok(Ev::Event(_)) => {}
                    Ok(Ev::Exit(code)) => {
                        send(&mut w, &Down::Exit { code }).await?;
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let (snap, new_rx) = s.snapshot_and_subscribe();
                        rx = new_rx;
                        aio::write(&mut w, T_DATA, &snap).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        })
    };
    let input = async {
        while let Some((t, payload)) = aio::read(&mut r).await? {
            match t {
                T_DATA => {
                    let s = s.clone();
                    blocking("keys to an attached session", move || s.write(&payload)).await?;
                }
                T_CBOR => match harness_proto::cbor::decode::<Up>(&payload) {
                    Ok(Up::Resize { cols, rows }) => s.resize(cols, rows)?,
                    Ok(Up::Detach) => break,
                    Ok(m) => anyhow::bail!("{m:?} on an attach stream (it takes keys, resize and detach)"),
                    Err(e) => anyhow::bail!("a message on an attach stream that is not harness schema {}'s: {e}", harness_proto::SCHEMA),
                },
                t => anyhow::bail!("a frame of type {t} on an attach stream (it takes keys and messages)"),
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

/// A chat session's attach: its history, then its events live; as T_EVENT frames exactly as its log holds them
/// (`events`, the app), or as the conversation's text in T_DATA frames (the terminal client). Its agent's exit is a
/// `Down::Exit`: the terminal client's stream ends there, the app's stays open (the chat outlives its agent, and a
/// resume's events come on it). Detached by `Up::Detach` (or the client going); a client that falls behind the broadcast
/// is dropped, and attaches again for a fresh history.
async fn attach_chat(
    s: Arc<Session>, c: Arc<chat::Chat>, events: bool, mut r: tokio::net::unix::OwnedReadHalf,
    mut w: tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    s.attached.fetch_add(1, Ordering::Relaxed);
    let (history, mut rx) = c.history_and_subscribe(&s.tx);
    let exited = *s.exit.lock();
    let sid = s.id;
    let mut out = tokio::spawn(async move {
        send_history(&mut w, &history, events).await?;
        if let Some(code) = exited {
            send(&mut w, &Down::Exit { code }).await?;
            if !events {
                return Ok::<_, anyhow::Error>(());
            }
        }
        loop {
            match rx.recv().await {
                Ok(Ev::Event(f)) => send_events(&mut w, &f, events).await?,
                Ok(Ev::Data(_)) => {}
                Ok(Ev::Exit(code)) => {
                    send(&mut w, &Down::Exit { code }).await?;
                    if !events {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    return Err(anyhow!("chat attach to s{sid} fell {n} events behind; attach again"));
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    });
    let input = async {
        while let Some((t, payload)) = aio::read(&mut r).await? {
            if t == T_CBOR && matches!(harness_proto::cbor::decode::<Up>(&payload), Ok(Up::Detach)) {
                break;
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

/// Whole T_EVENT frames to a chat attach: as they are (`events`), or as their text in one T_DATA frame.
async fn send_events(w: &mut tokio::net::unix::OwnedWriteHalf, frames: &[u8], events: bool) -> Result<()> {
    if events {
        aio::write_frames(w, frames).await?;
    } else {
        let text = chat::text_of_frames([frames]).replace('\n', "\r\n");
        if !text.is_empty() {
            aio::write(w, T_DATA, text.as_bytes()).await?;
        }
    }
    Ok(())
}

/// A chat's history to an attach, about 256 KB of frames at a time.
async fn send_history(w: &mut tokio::net::unix::OwnedWriteHalf, history: &[Arc<[u8]>], events: bool) -> Result<()> {
    let mut buf = Vec::new();
    for f in history {
        buf.extend_from_slice(f);
        if buf.len() >= 256 * 1024 {
            send_events(w, &buf, events).await?;
            buf.clear();
        }
    }
    if !buf.is_empty() {
        send_events(w, &buf, events).await?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{recv, send};

    /// A Claude chat no turn ever reached has no conversation on disk, so its relaunch starts it under its id; `--resume`
    /// of it is what failed with "No conversation found with session ID". One the agent spoke in resumes.
    #[test]
    fn a_chat_never_spoken_in_starts_instead_of_resuming() {
        assert_eq!(claude_conversation("id", false, false), ["--session-id", "id"]);
        assert_eq!(claude_conversation("id", true, false), ["--resume", "id"]);
        assert_eq!(claude_conversation("id", true, true), ["--resume", "id", "--fork-session", "--no-session-persistence"]);
        assert_eq!(claude_conversation("id", false, true), ["--no-session-persistence"]);
    }

    /// Set: the next request panics while holding the sessions lock.
    pub(super) static PANIC_NEXT: AtomicBool = AtomicBool::new(false);

    /// A request that panics fails alone: its client gets an error frame naming the panic, and the next request on the
    /// same connection (and on a new one) is answered. Remove the cause (no planted panic) and the first answer is a
    /// reply; with std's poisoning locks the second request panicked too and the connection closed.
    #[tokio::test]
    async fn a_panicking_request_fails_only_itself() {
        let d = Arc::new(Daemon { sessions: Mutex::new(BTreeMap::new()), next: AtomicU64::new(1), last: Mutex::new(None) });
        let list = || Up::Req(Req::List { all: false, chats: false });
        let open = |d: Arc<Daemon>| async move {
            let (client, server) = UnixStream::pair().expect("socket pair");
            tokio::spawn(guarded(d, server));
            let mut client = client;
            send(&mut client, &Up::Hello { schema: harness_proto::SCHEMA }).await.expect("hello");
            assert!(matches!(recv(&mut client).await.expect("hello back"), Down::Hello { .. }));
            client
        };
        let mut c = open(d.clone()).await;
        PANIC_NEXT.store(true, Ordering::SeqCst);
        send(&mut c, &list()).await.expect("send");
        match recv(&mut c).await.expect("an answer to the panicking request") {
            Down::Error { error } => assert!(error.contains("panicked") && error.contains("planted panic"), "{error}"),
            m => panic!("the planted panic was not reported: {m:?}"),
        }
        send(&mut c, &list()).await.expect("send");
        assert!(matches!(recv(&mut c).await.expect("the next request answered"), Down::Reply(Reply::Sessions { .. })));
        let mut c2 = open(d).await;
        send(&mut c2, &list()).await.expect("send");
        assert!(matches!(recv(&mut c2).await.expect("a new connection answered"), Down::Reply(Reply::Sessions { .. })));
    }
}

/// What a launch of `kind` starts on: `model` and `effort` checked against its kind's list, then its launch's own flags
/// (claude's and cursor's `--model`, claude's `--effort`), or for a Codex chat the choice its thread/start carries
/// (returned). `tui` adds `--tui`. The one home for both spawn requests (`spawn` and `spawn_object`).
///
/// # Errors
/// A model or effort its kind does not list, or an effort for a kind that takes none at launch.
pub fn launch_choice(
    kind: &str, args: &mut Vec<String>, model: Option<String>, effort: Option<String>, tui: bool,
) -> Result<Option<(Option<String>, Option<String>)>> {
    let codex_chat = kind == "codex" && !tui;
    chat::models::check(kind, model.as_deref(), effort.as_deref())?;
    if !codex_chat {
        if let Some(m) = &model {
            args.extend(["--model".to_owned(), m.clone()]);
        }
        if let Some(e) = &effort {
            anyhow::ensure!(kind == "claude", "{kind} takes no effort at launch, so effort {e:?} is refused");
            args.extend(["--effort".to_owned(), e.clone()]);
        }
    }
    if tui {
        args.push("--tui".into());
    }
    Ok(if codex_chat && (model.is_some() || effort.is_some()) { Some((model, effort)) } else { None })
}
