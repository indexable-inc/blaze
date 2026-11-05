//! `harness` client: talks to harnessd (starting it if needed), shows the session tree, and
//! attaches to a session with raw passthrough. Ctrl-] inside a session returns to the tree.
use crate::{paths, proto};
use anyhow::{Context, Result, anyhow, bail};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use serde_json::{Value, json};
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
        Ok(Self { stream: ensure_daemon().await? })
    }

    pub async fn req(&mut self, v: Value) -> Result<Value> {
        proto::write_json(&mut self.stream, &v).await?;
        let (t, payload) = proto::read_frame(&mut self.stream).await?.ok_or_else(|| anyhow!("daemon closed the connection"))?;
        if t != proto::T_JSON {
            bail!("unexpected frame type {t}");
        }
        let v: Value = serde_json::from_slice(&payload)?;
        if let Some(e) = v.get("error") {
            bail!("{}", e.as_str().unwrap_or("error"));
        }
        Ok(v)
    }
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
            Some(ctl.req(json!({"op": "spawn", "kind": kind, "args": args, "cwd": cwd, "cols": cols, "rows": rows, "mode": "tui"})).await?["id"].as_u64().unwrap())
        }
        None => ctl.req(json!({"op": "last"})).await?["id"].as_u64(),
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
    let (cols, rows) = term_size();
    proto::write_json(&mut stream, &json!({"op": "attach", "id": id, "cols": cols, "rows": rows})).await?;
    let (_, payload) = proto::read_frame(&mut stream).await?.ok_or_else(|| anyhow!("daemon closed"))?;
    let resp: Value = serde_json::from_slice(&payload)?;
    if let Some(e) = resp.get("error") {
        bail!("{e}");
    }
    let (mut r, mut w) = stream.into_split();
    let mut stdout = tokio::io::stdout();
    stdout.write_all(b"\x1b[0m\x1b[H\x1b[2J").await?;
    // Frames arrive through a channel so the select! below stays cancel-safe.
    let (tx, mut frames) = mpsc::unbounded_channel();
    let pump = tokio::spawn(async move {
        while let Ok(Some(f)) = proto::read_frame(&mut r).await {
            if tx.send(f).is_err() {
                break;
            }
        }
    });
    let _abort = AbortOnDrop(pump);
    loop {
        tokio::select! {
            f = frames.recv() => match f {
                Some((proto::T_DATA, b)) => { stdout.write_all(&b).await?; stdout.flush().await?; }
                Some((proto::T_EXIT, b)) => {
                    let code = serde_json::from_slice::<Value>(&b).ok().and_then(|v| v["code"].as_i64()).unwrap_or(-1);
                    return Ok(AttachEnd::Exited(code as i32));
                }
                Some(_) => {}
                None => bail!("daemon closed the attach stream"),
            },
            k = ui.keys.recv() => {
                let k = k.ok_or_else(|| anyhow!("stdin closed"))?;
                if let Some(pos) = k.iter().position(|&b| b == DETACH) {
                    if pos > 0 { proto::write_frame(&mut w, proto::T_DATA, &k[..pos]).await?; }
                    proto::write_json(&mut w, &json!({"op": "detach"})).await?;
                    return Ok(AttachEnd::Detached);
                }
                proto::write_frame(&mut w, proto::T_DATA, &k).await?;
            }
            _ = ui.winch.recv() => {
                let (cols, rows) = term_size();
                proto::write_frame(&mut w, proto::T_RESIZE, json!({"cols": cols, "rows": rows}).to_string().as_bytes()).await?;
            }
        }
    }
}

/// Sessions in tree order: (session, depth).
fn ordered(list: &[Value]) -> Vec<(Value, usize)> {
    let ids: std::collections::HashSet<u64> = list.iter().filter_map(|s| s["id"].as_u64()).collect();
    let parent = |s: &Value| s["parent"].as_u64().filter(|p| ids.contains(p));
    fn walk(list: &[Value], p: Option<u64>, depth: usize, parent: &dyn Fn(&Value) -> Option<u64>, out: &mut Vec<(Value, usize)>) {
        for s in list.iter().filter(|s| parent(s) == p) {
            out.push((s.clone(), depth));
            walk(list, s["id"].as_u64(), depth + 1, parent, out);
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

pub fn tree_lines(list: &[Value]) -> Vec<(u64, String, bool)> {
    ordered(list)
        .into_iter()
        .map(|(s, depth)| {
            let indent = if depth == 0 { String::new() } else { format!("{}└ ", "  ".repeat(depth - 1)) };
            let exit = s["exit"].as_i64();
            let state = match exit {
                Some(c) => format!("exit {c}"),
                None if s["status"] == "lost" => "lost".into(), // the daemon stopped while it ran
                None if s["attached"].as_u64().unwrap_or(0) > 0 => "attached".into(),
                None => "running".into(),
            };
            let title = s["title"].as_str().unwrap_or("");
            (
                s["id"].as_u64().unwrap_or(0),
                format!("{indent}#{} {:<6} {:<9} {:>4}  {}", s["id"], s["kind"].as_str().unwrap_or(""), state, age(s["created_ms"].as_u64().unwrap_or(0)), title),
                exit.is_some() || s["status"] == "lost",
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
        let list = ui.ctl.req(json!({"op": "list"})).await?["sessions"].as_array().cloned().unwrap_or_default();
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
        let spawn = |kind: &str| json!({"op": "spawn", "kind": kind, "cwd": ui.cwd, "cols": term_size().0, "rows": term_size().1, "mode": "tui"});
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
                    Ok(v) => return Ok(v["id"].as_u64()),
                    Err(e) => *status = format!("spawn {kind}: {e:#}"),
                }
            }
            b"d" => {
                if let Some(id) = sel_id {
                    ui.ctl.req(json!({"op": "kill", "id": id})).await?;
                    *status = format!("killed #{id}");
                }
            }
            b"D" => {
                if let Some(id) = sel_id {
                    ui.ctl.req(json!({"op": "forget", "id": id})).await?;
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
