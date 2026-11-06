//! `harness` client: talks to harnessd (starting it if needed), shows the session tree, and
//! attaches to a session with raw passthrough. Ctrl-] inside a session returns to the tree.
use crate::paths;
use harness_proto::frame::{T_CBOR, T_DATA, aio};
use harness_proto::msg::{Down, Reply, Request, SessionInfo, Up};
use anyhow::{Context, Result, anyhow, bail};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use std::io::Write;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

pub const DETACH: u8 = 0x1d; // Ctrl-]

/// Request/response connection to the daemon.
pub struct Ctl {
    stream: UnixStream,
}

impl Ctl {
    pub async fn connect() -> Result<Self> {
        let mut stream = ensure_daemon().await?;
        hello(&mut stream).await?;
        Ok(Self { stream })
    }

    /// One request and its reply (a refusal as the error it names).
    pub async fn req(&mut self, r: Request) -> Result<Reply> {
        send(&mut self.stream, &Up::Req(r)).await?;
        match recv(&mut self.stream).await? {
            Down::Reply(reply) => Ok(reply),
            Down::Error { error } => bail!("{error}"),
            m => bail!("harnessd answered a request with {m:?}"),
        }
    }

    /// `list`'s sessions.
    pub async fn list(&mut self, all: bool, chats: bool) -> Result<Vec<SessionInfo>> {
        match self.req(Request::List { all, chats }).await? {
            Reply::Sessions { sessions, .. } => Ok(sessions),
            r => bail!("harnessd answered list with {r:?}"),
        }
    }

    /// A request whose reply is an open record (runs, objects).
    pub async fn record(&mut self, r: Request) -> Result<serde_json::Value> {
        match self.req(r).await? {
            Reply::Record(v) => Ok(v),
            Reply::Ok => Ok(serde_json::json!({"ok": true})),
            r => bail!("harnessd answered with {r:?} where a record belongs"),
        }
    }
}

/// One message to the daemon.
pub async fn send<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, m: &Up) -> Result<()> {
    aio::write(w, T_CBOR, &harness_proto::cbor::encode(m)).await?;
    Ok(())
}

/// The daemon's next message (a T_CBOR frame).
pub async fn recv<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<Down> {
    let (t, payload) = aio::read(r).await?.ok_or_else(|| anyhow!("harnessd closed the connection"))?;
    if t != T_CBOR {
        bail!("harnessd sent a frame of type {t} where a message (type {T_CBOR}) belongs");
    }
    decode_down(&payload)
}

fn decode_down(payload: &[u8]) -> Result<Down> {
    harness_proto::cbor::decode(payload).map_err(|e| anyhow!("harnessd sent a message that is not harness schema {}'s: {e}", harness_proto::SCHEMA))
}

/// How long a daemon has to answer the hello.
pub const HELLO_WAIT: Duration = Duration::from_secs(3);

/// A daemon that took the connection but did not answer the hello in [`HELLO_WAIT`]: most likely an older one that
/// predates the CBOR hello. Named by its pid (the socket's peer credentials) and socket.
#[derive(Debug)]
pub struct NoHello {
    pub pid: Option<i32>,
    pub sock: std::path::PathBuf,
}

impl std::fmt::Display for NoHello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pid = self.pid.map_or("unknown".to_owned(), |p| p.to_string());
        write!(
            f,
            "harnessd (pid {pid}, socket {}) did not answer the hello within {}s: it is likely an older daemon that does not speak harness schema {}; run `harness stop` and try again",
            self.sock.display(),
            HELLO_WAIT.as_secs(),
            harness_proto::SCHEMA
        )
    }
}

impl std::error::Error for NoHello {}

/// Open a connection: this client's hello, and the daemon's (an error naming both schemas when they differ), within
/// [`HELLO_WAIT`] ([`NoHello`] past it).
pub async fn hello(s: &mut UnixStream) -> Result<()> {
    send(s, &Up::Hello { schema: harness_proto::SCHEMA }).await?;
    let answer = match tokio::time::timeout(HELLO_WAIT, recv(s)).await {
        Ok(answer) => answer?,
        Err(_) => return Err(NoHello { pid: s.peer_cred().ok().and_then(|c| c.pid()), sock: paths::sock() }.into()),
    };
    match answer {
        Down::Hello { schema, .. } if schema == harness_proto::SCHEMA => Ok(()),
        Down::Hello { schema, .. } => bail!("harnessd speaks harness schema {schema}, this client {}", harness_proto::SCHEMA),
        Down::Error { error } => bail!("harnessd refused the connection: {error}"),
        m => bail!("harnessd answered the hello with {m:?}"),
    }
}

/// Stop the daemon: `shutdown` (it kills every session first). A daemon this client cannot speak to (no hello in
/// [`HELLO_WAIT`], or another schema's) is sent SIGTERM, by the pid its socket's peer credentials name, and waited for
/// a few seconds.
pub async fn stop() -> Result<()> {
    let sock = paths::sock();
    let Ok(mut s) = UnixStream::connect(&sock).await else {
        println!("harnessd is not running (nothing listens at {})", sock.display());
        return Ok(());
    };
    let refused = match hello(&mut s).await {
        Ok(()) => {
            let _ = Ctl { stream: s }.req(Request::Shutdown).await;
            return Ok(());
        }
        Err(e) => e,
    };
    let Some(pid) = s.peer_cred().ok().and_then(|c| c.pid()) else {
        return Err(refused.context(format!("stopping harnessd at {}: its pid is unknown, so it cannot be signalled", sock.display())));
    };
    // SAFETY: kill(2) with a pid and a signal number; no memory is shared.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        let e = std::io::Error::last_os_error();
        return Err(refused.context(format!("stopping harnessd pid {pid} ({}): SIGTERM failed: {e}", sock.display())));
    }
    for _ in 0..60 {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            println!("stopped harnessd pid {pid} with SIGTERM ({refused:#})");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("harnessd pid {pid} ({}) is still running 3s after SIGTERM; kill it with `kill -9 {pid}`", sock.display())
}

/// Connect to harnessd, starting it detached (own session, log file) if nothing is listening.
pub async fn ensure_daemon() -> Result<UnixStream> {
    let sock = paths::sock();
    if let Ok(s) = UnixStream::connect(&sock).await {
        return Ok(s);
    }
    std::fs::create_dir_all(paths::home())?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths::log())?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("daemon").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("start harnessd")?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(s) = UnixStream::connect(&sock).await {
            return Ok(s);
        }
    }
    bail!("harnessd did not start; see {}", paths::log().display())
}

fn term_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((120, 40))
}

/// Terminal modes a session may have switched on; undone when we leave it.
const RESET_MODES: &[u8] = b"\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[>4;0m";

enum AttachEnd {
    Detached,
    Exited(i32),
}

struct Ui {
    ctl: Ctl,
    keys: mpsc::UnboundedReceiver<Vec<u8>>,
    winch: tokio::signal::unix::Signal,
    cwd: String,
}

pub async fn run(spawn: Option<(String, Vec<String>)>) -> Result<()> {
    let mut ctl = Ctl::connect().await?;
    let cwd = std::env::current_dir()?.display().to_string();
    let mut target = match &spawn {
        Some((kind, args)) => {
            let (cols, rows) = term_size();
            let r = ctl.req(spawn_req(kind, args.clone(), &cwd, cols, rows)).await?;
            let Reply::Spawned { id } = r else { bail!("harnessd answered spawn with {r:?}") };
            Some(id)
        }
        None => match ctl.req(Request::Last).await? {
            Reply::Last { id } => id,
            r => bail!("harnessd answered last with {r:?}"),
        },
    };

    // One reader owns stdin for the whole run; tree mode and attach mode both consume its bytes.
    let (ktx, keys) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || ktx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut ui = Ui { ctl, keys, winch, cwd };

    crossterm::terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    crossterm::execute!(out, crossterm::terminal::EnterAlternateScreen)?;
    let result = async {
        let mut status = String::new();
        loop {
            if let Some(id) = target.take() {
                match attach(&mut ui, id).await {
                    Ok(AttachEnd::Detached) => status = format!("detached from #{id}"),
                    Ok(AttachEnd::Exited(code)) => status = format!("#{id} exited ({code})"),
                    Err(e) => status = format!("attach #{id}: {e:#}"),
                }
                let mut out = std::io::stdout();
                out.write_all(RESET_MODES)?;
                crossterm::execute!(out, crossterm::terminal::EnterAlternateScreen)?;
            }
            match tree(&mut ui, &mut status).await? {
                Some(id) => target = Some(id),
                None => return Ok::<_, anyhow::Error>(()),
            }
        }
    }
    .await;
    let mut out = std::io::stdout();
    let _ = out.write_all(RESET_MODES);
    let _ = crossterm::execute!(out, crossterm::terminal::LeaveAlternateScreen);
    let _ = crossterm::terminal::disable_raw_mode();
    result
}

async fn attach(ui: &mut Ui, id: u64) -> Result<AttachEnd> {
    let mut stream = UnixStream::connect(paths::sock()).await?;
    hello(&mut stream).await?;
    let (cols, rows) = term_size();
    send(&mut stream, &Up::Req(Request::Attach { id, cols: Some(cols), rows: Some(rows), events: false })).await?;
    match recv(&mut stream).await? {
        Down::Reply(Reply::Attached { .. }) => {}
        Down::Error { error } => bail!("{error}"),
        m => bail!("harnessd answered attach with {m:?}"),
    }
    let (mut r, mut w) = stream.into_split();
    let mut stdout = tokio::io::stdout();
    stdout.write_all(b"\x1b[0m\x1b[H\x1b[2J").await?;
    // Frames arrive through a channel so the select! below stays cancel-safe.
    let (tx, mut frames) = mpsc::unbounded_channel();
    let pump = tokio::spawn(async move {
        while let Ok(Some(f)) = aio::read(&mut r).await {
            if tx.send(f).is_err() {
                break;
            }
        }
    });
    let _abort = AbortOnDrop(pump);
    loop {
        tokio::select! {
            f = frames.recv() => match f {
                Some((T_DATA, b)) => { stdout.write_all(&b).await?; stdout.flush().await?; }
                Some((T_CBOR, b)) => match decode_down(&b)? {
                    Down::Exit { code } => return Ok(AttachEnd::Exited(code)),
                    m => bail!("harnessd sent {m:?} on an attach stream"),
                },
                Some((t, _)) => bail!("harnessd sent a frame of type {t} on a terminal attach stream"),
                None => bail!("daemon closed the attach stream"),
            },
            k = ui.keys.recv() => {
                let k = k.ok_or_else(|| anyhow!("stdin closed"))?;
                if let Some(pos) = k.iter().position(|&b| b == DETACH) {
                    if pos > 0 { aio::write(&mut w, T_DATA, &k[..pos]).await?; }
                    send(&mut w, &Up::Detach).await?;
                    return Ok(AttachEnd::Detached);
                }
                aio::write(&mut w, T_DATA, &k).await?;
            }
            _ = ui.winch.recv() => {
                let (cols, rows) = term_size();
                send(&mut w, &Up::Resize { cols, rows }).await?;
            }
        }
    }
}

/// A `spawn` of `kind` in its terminal UI.
fn spawn_req(kind: &str, args: Vec<String>, cwd: &str, cols: u16, rows: u16) -> Request {
    Request::Spawn {
        kind: kind.to_owned(),
        args,
        parent: None,
        cwd: Some(cwd.to_owned()),
        prompt: None,
        name: None,
        cols: Some(cols),
        rows: Some(rows),
        mode: Some("tui".to_owned()),
        model: None,
        effort: None,
    }
}

/// Sessions in tree order: (session, depth).
fn ordered(list: &[SessionInfo]) -> Vec<(&SessionInfo, usize)> {
    let ids: std::collections::HashSet<u64> = list.iter().map(|s| s.id).collect();
    let parent = |s: &SessionInfo| s.parent.filter(|p| ids.contains(p));
    fn walk<'a>(
        list: &'a [SessionInfo], p: Option<u64>, depth: usize, parent: &dyn Fn(&SessionInfo) -> Option<u64>,
        out: &mut Vec<(&'a SessionInfo, usize)>,
    ) {
        for s in list.iter().filter(|s| parent(s) == p) {
            out.push((s, depth));
            walk(list, Some(s.id), depth + 1, parent, out);
        }
    }
    let mut out = Vec::new();
    walk(list, None, 0, &parent, &mut out);
    out
}

fn age(ms: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let s = now.saturating_sub(ms) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

pub fn tree_lines(list: &[SessionInfo]) -> Vec<(u64, String, bool)> {
    ordered(list)
        .into_iter()
        .map(|(s, depth)| {
            let indent = if depth == 0 { String::new() } else { format!("{}└ ", "  ".repeat(depth - 1)) };
            let lost = s.status.as_deref() == Some("lost");
            let state = match s.exit {
                Some(c) => format!("exit {c}"),
                None if lost => "lost".into(), // the daemon stopped while it ran
                None if s.attached > 0 => "attached".into(),
                None => "running".into(),
            };
            (
                s.id,
                // What the person renamed it to, else its automatic title.
                format!("{indent}#{} {:<6} {:<9} {:>4}  {}", s.id, s.kind, state, age(s.created_ms.unwrap_or(0)), s.label.as_deref().unwrap_or(&s.title)),
                s.exit.is_some() || lost,
            )
        })
        .collect()
}

/// Tree view. Returns the session to attach to, or None to quit.
async fn tree(ui: &mut Ui, status: &mut String) -> Result<Option<u64>> {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    terminal.clear()?;
    let mut state = ListState::default().with_selected(Some(0));
    let mut tick = tokio::time::interval(Duration::from_millis(1000));
    loop {
        let list = ui.ctl.list(false, false).await?;
        let rows = tree_lines(&list);
        if let Some(sel) = state.selected() {
            if sel >= rows.len() {
                state.select(rows.len().checked_sub(1));
            }
        }
        terminal.draw(|f| {
            let [head, body, foot] = Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(3)]).areas(f.area());
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![Span::from("harness").bold(), Span::from(format!("  {} sessions", rows.len())).dim()]),
                    Line::from(""),
                ]),
                head,
            );
            let items: Vec<ListItem> = if rows.is_empty() {
                vec![ListItem::new("no sessions yet: press c for claude, x for codex, u for cursor, s for a shell").dim()]
            } else {
                rows.iter().map(|(_, l, dead)| if *dead { ListItem::new(l.clone()).dim() } else { ListItem::new(l.clone()) }).collect()
            };
            f.render_stateful_widget(List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED)).highlight_symbol("› "), body, &mut state);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(status.clone()).yellow(),
                    Line::from("enter attach · c claude · x codex · u cursor · s shell · d kill · D remove · q quit (sessions keep running)").dim(),
                    Line::from("inside a session: ctrl-] back to this tree").dim(),
                ]),
                foot,
            );
        })?;
        let key = tokio::select! {
            k = ui.keys.recv() => k.ok_or_else(|| anyhow!("stdin closed"))?,
            _ = tick.tick() => continue,
            _ = ui.winch.recv() => { terminal.autoresize()?; continue }
        };
        let sel_id = state.selected().and_then(|i| rows.get(i)).map(|r| r.0);
        let spawn = |kind: &str| spawn_req(kind, Vec::new(), &ui.cwd, term_size().0, term_size().1);
        match key.as_slice() {
            b"q" | b"\x03" => return Ok(None),
            b"j" | b"\x1b[B" | b"\x1bOB" => state.select_next(),
            b"k" | b"\x1b[A" | b"\x1bOA" => state.select_previous(),
            b"g" => state.select_first(),
            b"G" => state.select_last(),
            b"\r" | b"\n" => {
                if let Some(id) = sel_id {
                    return Ok(Some(id));
                }
            }
            b"c" | b"x" | b"u" | b"s" => {
                let kind = match key[0] { b'c' => "claude", b'x' => "codex", b'u' => "cursor", _ => "shell" };
                match ui.ctl.req(spawn(kind)).await {
                    Ok(Reply::Spawned { id }) => return Ok(Some(id)),
                    Ok(r) => *status = format!("spawn {kind}: harnessd answered {r:?}"),
                    Err(e) => *status = format!("spawn {kind}: {e:#}"),
                }
            }
            b"d" => {
                if let Some(id) = sel_id {
                    ui.ctl.req(Request::Kill { id }).await?;
                    *status = format!("killed #{id}");
                }
            }
            b"D" => {
                if let Some(id) = sel_id {
                    ui.ctl.req(Request::Forget { id }).await?;
                    *status = format!("removed #{id}");
                }
            }
            _ => {}
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
