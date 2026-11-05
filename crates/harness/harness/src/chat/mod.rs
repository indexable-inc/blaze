//! Chat mode: an agent session driven over its structured protocol instead of its TUI in a PTY, so the native app draws
//! the conversation itself (harness-native's chat pane) and agents talk to it without typing into a terminal. The agent's
//! own protocol stays in its adapter ([`claude`]: stream-json; [`codex`]: `codex app-server`'s JSON-RPC); everything
//! here, and everything a client reads, is the one agent-neutral event model below.
//!
//! # The event model
//!
//! One JSON object per line, each with `ev` and `ts` (ms since the epoch). A `block` names one assistant block (text,
//! thinking) or one tool call, unique in the session:
//!
//! | `ev` | fields | |
//! | --- | --- | --- |
//! | `session` | `agent`, `model`, `session_id` | the agent started (or resumed) with this model; `session_id` is its own id for the conversation (Claude's session id, Codex's thread id), what a resume names |
//! | `user` | `text` | the user's message (written by the daemon as it sends it) |
//! | `busy` | `busy` | a turn started (true) or ended (false) |
//! | `block_start` | `block`, `kind` (`text` \| `thinking`) | a block began streaming *(partial)* |
//! | `delta` | `block`, `text` | more of a streaming block *(partial)* |
//! | `block` | `block`, `kind`, `text` | a block, whole (replaces its partials) |
//! | `tool_start` | `block`, `name` | a tool call began, its arguments streaming *(partial)* |
//! | `tool_delta` | `block`, `json` | more of its arguments' JSON text *(partial)* |
//! | `tool_call` | `block`, `name`, `input` | the call, whole (replaces its partials) |
//! | `tool_result` | `block`, `text`, `error` | what the call returned |
//! | `tool_detail` | `block`, `cell`, `status`, `ms`, `code`, `effects`, `effects_dropped`, `value`, `ui`, `logs` | what a harness REPL cell the call ran did, whole each time (see below) |
//! | `note` | `text` | something the agent said that is not the assistant's words (an interrupt) |
//! | `error` | `text` | the agent reported an error |
//! | `turn_end` | `ok`, `stop` (`end_turn` \| `interrupted` \| `error` \| ...), `duration_ms`, `cost_usd` (the turn's own; null when the agent says none), `usage`, `text` | the turn is over; `text` its last assistant words |
//!
//! ## `tool_detail`: a REPL cell's steps and value
//!
//! The harness REPL (`mcp/src/repl.ts`) posts each cell to the daemon as it runs (`repl_task`), with the agent's id for
//! the tool call that ran it when the agent gave one (Claude puts it in the MCP request's `_meta`). For a chat session
//! the daemon records each post as a `tool_detail` for that call's `block`: the cell's whole state, so the latest one
//! stands for all before it (a reattach replays only the latest). It may come before the call's `tool_call` or after its
//! `tool_result` (a backgrounded cell ends later). Its fields, as the kernel writes them:
//!
//! - `cell`: `<repl>.<task>` (the `repl` object it is indexed as); `status`: `running` | `done` | `error`; `ms`.
//! - `effects`: the steps the cell took outside the REPL, in order (`mcp/src/effects.ts`): `{n, kind (exec | read |
//!   write | delete | net | search | spawn | sql), doing, done (its words while running and once done), target, status
//!   (running | ok | error), started_ms, ms, detail ("exit 0", "5 results", "3.2 KB"), value}`; `effects_dropped`: how
//!   many past the kernel's cap were not kept.
//! - `value`: the cell's result as the app shows it (`mcp/src/shown.ts`), null while it runs or when it is one of its
//!   steps' values: `{t: table | record | list, cols: [{name, type}], rows: [[cell]], total}`, `{t: scalar, type, v}`,
//!   `{t: text, v, total}`, `{t: error, message, stack}`. A step's `value` has the same shape.
//! - `ui`: what the cell draws, when its value was Svelte 5 markup (`mcp/src/ui/`), else null: a tree `{type, props,
//!   children}` or a list of them, `type` one of the app's elements (Column, Row, Text, Markdown, Code, Table, Badge, Stat,
//!   Card, Divider, Spacer; harness-native `app/chat/genui.rs` draws them). The cell's `$state` changing posts the cell
//!   again with the new tree, so a `tool_detail` can follow its cell's end.
//!
//! # Storage and attach
//!
//! - The child runs with stdin and stdout piped (no PTY), stderr appended to `objects/sN/stderr`; every line it writes is
//!   appended to `objects/sN/raw` as it came (the agent's own protocol, kept whole), and every event to
//!   `objects/sN/chat`, the conversation's record (indexed as the `chat` stream).
//! - `attach` with `events: true` answers its ok (info `mode: "chat"`), then T_DATA frames of whole event lines: the
//!   history, then each event live. When the agent exits a T_EXIT says so, and the stream stays open: a chat outlives its
//!   agent's process, and a resume's events come on the same stream. The history replaces the partials of every block
//!   that came whole since by that whole event, in the place its first partial took, so a reattach folds to the same
//!   conversation the live events did (harness-chat's reducer). Without `events` (the terminal client) the same events
//!   come as plain text, and the stream ends at the agent's exit.
//! - `send` writes the adapter's user lines and records a `user` event; `interrupt` writes the adapter's interrupt. An
//!   adapter may also write lines of its own: a handshake as the child starts ([`Adapter::start`]) and answers to what
//!   the child asks or queued messages once it can take them ([`Adapter::replies`], drained after every line read).
//! - Busy (the sidebar's breathing dot) is a turn in flight, from a `user` until its `turn_end`. The title is the first
//!   user message's first line (kept in the index's meta too, so `list` names a past chat without reading its log).
//! - The `messages` stream (what `Obj.reply()` reads) is written here from the events, `turn_end` on each turn's end;
//!   messages.rs leaves chat sessions alone. `screen` is [`Chat::screen`]: the conversation as plain text.
//!
//! # Restore and resume
//!
//! The log is the chat: one NDJSON file per session, appended to forever (never compacted). A chat from before this
//! daemon started is loaded from it on its first attach, send or screen ([`Chat::open`] folds the file through the same
//! [`Store::absorb`] a live event goes through), and is then a session like any other, its agent not running. A message
//! to a chat whose agent is not running resumes it: the agent starts again on the same log with its own session id from
//! the log (Claude: `--resume <session_id>`; Codex: `thread/resume`). A chat that cannot be resumed (no session id in its
//! log, an agent kind with no resume) gets an `error` event naming why, and the message is not sent; it never starts a
//! fresh conversation in its place.
pub mod claude;
pub mod codex;

use crate::daemon::Ev;
use bytes::Bytes;
use harness_chat::Transcript;
#[cfg(test)]
use harness_chat::Item;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// How many lines of the conversation `screen` returns (its tail).
const SCREEN_LINES: usize = 200;

/// An agent's protocol: how its output lines become events, and the lines that start it, send a message, stop a turn
/// and answer what it asks.
pub trait Adapter: Send {
    /// The lines to write as the child starts, before any message (a handshake).
    fn start(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// The events one line of the agent's output stands for (none for what the model has no place for).
    fn events(&mut self, line: &str) -> Vec<Value>;
    /// The lines to write now that [`Adapter::events`] read a line: answers to the child's requests, messages it can
    /// take now. Drained.
    fn replies(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// The lines that send the user's `text` (none while the adapter holds it until the agent can take it).
    fn user(&mut self, text: &str) -> Vec<String>;
    /// The lines that stop the turn in flight (none when there is nothing to stop yet).
    fn interrupt(&mut self) -> Vec<String>;
}

/// The adapter of a chat whose agent is not running (restored from its log, before a resume): it reads and writes
/// nothing.
pub struct NoAdapter;

impl Adapter for NoAdapter {
    fn events(&mut self, _: &str) -> Vec<Value> {
        Vec::new()
    }
    fn user(&mut self, _: &str) -> Vec<String> {
        Vec::new()
    }
    fn interrupt(&mut self) -> Vec<String> {
        Vec::new()
    }
}

/// Startup: chats indexed before the index kept titles get theirs, from the first `user` event of their log.
pub fn backfill_titles() {
    for oid in crate::db::chats_without_title() {
        let Ok(log) = std::fs::read(crate::db::object_dir(&oid).join("chat")) else { continue };
        let first = harness_chat::lines(&log).find_map(|(_, _, ev)| ev.filter(|e| e["ev"] == "user"));
        if let Some(t) = first.map(|e| harness_chat::title_of(e["text"].as_str().unwrap_or(""))).filter(|t| !t.is_empty()) {
            crate::db::set_meta(&oid, "title", &json!(t));
        }
    }
}

/// Whether event `ev` is a partial a whole block replaces.
fn partial(ev: &str) -> bool {
    matches!(ev, "block_start" | "delta" | "tool_start" | "tool_delta")
}

/// Whether event `ev` starts a block's partials (the place in the conversation its whole form takes).
fn starts(ev: &str) -> bool {
    matches!(ev, "block_start" | "tool_start")
}

/// Whether event `ev` is a block that came whole (it replaces its partials).
fn whole(ev: &str) -> bool {
    matches!(ev, "block" | "tool_call")
}

/// A chat's state, folded from its events one at a time: the same fold for an event recorded live and for a line of its
/// log read back, so a restored chat is the chat it was.
struct Store {
    raw: Option<File>,
    events: File,
    messages: File,
    /// The history a reattach replays: every event, a block's partials replaced by its whole form in the place its first
    /// partial took (so the conversation's order is the live one), every block's `tool_detail` but its latest gone.
    replay: Vec<Option<Arc<str>>>,
    /// Where each block's partials are in `replay`.
    partials: HashMap<String, Vec<usize>>,
    /// Where each block's latest `tool_detail` is in `replay`.
    details: HashMap<String, usize>,
    /// The conversation (the one reducer, harness-chat): title, model, busy, the agent's session id, tool names.
    transcript: Transcript,
    adapter: Box<dyn Adapter>,
}

impl Store {
    /// Fold one event (its line as written, and parsed).
    fn absorb(&mut self, line: Arc<str>, ev: &Value) {
        let kind = ev["ev"].as_str().unwrap_or("");
        let at = self.replay.len();
        let mut slot = None;
        if let Some(b) = ev["block"].as_str() {
            if partial(kind) {
                self.partials.entry(b.to_owned()).or_default().push(at);
            } else if kind == "tool_detail" {
                if let Some(i) = self.details.insert(b.to_owned(), at) {
                    self.replay[i] = None;
                }
            } else if whole(kind) {
                for i in self.partials.remove(b).unwrap_or_default() {
                    let first = self.replay[i].as_deref().and_then(|l| serde_json::from_str::<Value>(l).ok());
                    if slot.is_none() && first.is_some_and(|f| starts(f["ev"].as_str().unwrap_or(""))) {
                        slot = Some(i);
                    }
                    self.replay[i] = None;
                }
            }
        }
        self.transcript.apply(ev);
        match slot {
            Some(i) => self.replay[i] = Some(line),
            None => self.replay.push(Some(line)),
        }
    }
}

pub struct Chat {
    oid: String,
    store: Mutex<Store>,
    /// A turn is in flight.
    pub busy: AtomicBool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Chat {
    /// Chat `oid`'s record in `dir` (`objects/sN`), opened for appending, its log folded back in (a past chat restored, a
    /// chat resumed; nothing for a new one). A line of the log that is not whole JSON (torn by a crash mid-append) is
    /// reported, to the daemon's log and as an `error` in the history a reattach replays, and skipped; the rest of the log
    /// stands. Read through `adapter` from here on.
    pub fn open(oid: &str, dir: &Path, adapter: Box<dyn Adapter>) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let open = |name: &str| std::fs::OpenOptions::new().create(true).append(true).open(dir.join(name));
        let log = std::fs::read(dir.join("chat")).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(Vec::new()) } else { Err(e) })?;
        let mut events = open("chat")?;
        // A torn last line stays one bad line: what is appended next starts a line of its own.
        if log.last().is_some_and(|&b| b != b'\n') {
            events.write_all(b"\n")?;
        }
        let mut store = Store {
            raw: None,
            events,
            messages: open("messages")?,
            replay: Vec::new(),
            partials: HashMap::new(),
            details: HashMap::new(),
            transcript: Transcript::default(),
            adapter,
        };
        for (n, bytes, ev) in harness_chat::lines(&log) {
            match ev {
                Some(ev) => store.absorb(Arc::from(String::from_utf8_lossy(bytes).as_ref()), &ev),
                None => {
                    let why = format!(
                        "line {n} of this chat's log ({} bytes) is not a whole event, likely torn by a crash mid-append; skipped",
                        bytes.len()
                    );
                    eprintln!("{oid} chat: {why}");
                    let ev = json!({"ev": "error", "text": why});
                    store.replay.push(Some(Arc::from(ev.to_string())));
                }
            }
        }
        let busy = AtomicBool::new(store.transcript.busy);
        Ok(Self { oid: oid.to_owned(), store: Mutex::new(store), busy })
    }

    /// The agent's process (re)starts: its protocol from here on, its raw lines appended to `objects/sN/raw`.
    pub fn set_adapter(&self, dir: &Path, adapter: Box<dyn Adapter>) -> anyhow::Result<()> {
        let raw = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("raw"))?;
        let mut st = self.store.lock().unwrap();
        st.adapter = adapter;
        st.raw = Some(raw);
        Ok(())
    }

    pub fn title(&self) -> String {
        self.store.lock().unwrap().transcript.title.clone()
    }

    pub fn model(&self) -> String {
        self.store.lock().unwrap().transcript.model.clone()
    }

    /// The agent's own id for this conversation (what resumes it), when its log said one.
    pub fn session_id(&self) -> Option<String> {
        Some(self.store.lock().unwrap().transcript.session_id.clone()).filter(|s| !s.is_empty())
    }

    /// The lines to write to the child as it starts.
    pub fn start_lines(&self) -> Vec<String> {
        self.store.lock().unwrap().adapter.start()
    }

    /// A line the agent wrote: kept in `raw`, and its events recorded. Whether the busy state changed, and the lines to
    /// write back to the agent.
    pub fn agent_line(&self, line: &str, tx: &broadcast::Sender<Ev>) -> (bool, Vec<String>) {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return (false, Vec::new());
        }
        let (evs, replies) = {
            let mut st = self.store.lock().unwrap();
            if let Some(raw) = st.raw.as_mut()
                && let Err(e) = raw.write_all(format!("{line}\n").as_bytes())
            {
                eprintln!("{} raw: {e}", self.oid);
            }
            let evs = st.adapter.events(line);
            (evs, st.adapter.replies())
        };
        let mut flipped = false;
        for ev in evs {
            flipped |= self.record(ev, tx);
        }
        (flipped, replies)
    }

    /// The lines that send `text` to the agent, after recording its `user` event (and the turn starting).
    pub fn user_lines(&self, text: &str, tx: &broadcast::Sender<Ev>) -> Vec<String> {
        self.record(json!({"ev": "user", "text": text}), tx);
        self.record(json!({"ev": "busy", "busy": true}), tx);
        self.store.lock().unwrap().adapter.user(text)
    }

    /// The lines that stop the turn in flight.
    pub fn interrupt_lines(&self) -> Vec<String> {
        self.store.lock().unwrap().adapter.interrupt()
    }

    /// The agent ended: a turn still in flight ends with it.
    pub fn ended(&self, tx: &broadcast::Sender<Ev>) {
        if self.busy.load(Ordering::Relaxed) {
            self.record(json!({"ev": "busy", "busy": false}), tx);
        }
    }

    /// Record one event: stamped, appended to the log, folded into the chat's state, sent to the attached clients, its
    /// messages rows written. Whether the busy state or the title changed (what `list` says of it).
    pub fn record(&self, mut ev: Value, tx: &broadcast::Sender<Ev>) -> bool {
        ev["ts"] = json!(now_ms());
        let line = ev.to_string();
        let mut st = self.store.lock().unwrap();
        if let Err(e) = st.events.write_all(format!("{line}\n").as_bytes()) {
            eprintln!("{} chat: {e}", self.oid);
        }
        let title = st.transcript.title.is_empty();
        st.absorb(Arc::from(line.as_str()), &ev);
        let retitled = title && !st.transcript.title.is_empty();
        let busy = st.transcript.busy;
        // Sent under the lock, so a reattach's history and its live events neither miss nor double one.
        let _ = tx.send(Ev::Data(Bytes::from(format!("{line}\n"))));
        let rows = rows(&ev, &st.transcript);
        if !rows.is_empty() {
            let mut out = Vec::new();
            for mut r in rows {
                r["object"] = json!(self.oid);
                r["ts_ms"] = ev["ts"].clone();
                out.extend(r.to_string().into_bytes());
                out.push(b'\n');
            }
            if let Err(e) = st.messages.write_all(&out) {
                eprintln!("{} messages: {e}", self.oid);
            }
        }
        drop(st);
        let flipped = self.busy.swap(busy, Ordering::Relaxed) != busy;
        flipped || retitled
    }

    /// The history a reattach starts with, and a receiver for every event after it.
    pub fn history_and_subscribe(&self, tx: &broadcast::Sender<Ev>) -> (Vec<Arc<str>>, broadcast::Receiver<Ev>) {
        let st = self.store.lock().unwrap();
        (st.replay.iter().flatten().cloned().collect(), tx.subscribe())
    }

    /// The conversation as plain text, its last [`SCREEN_LINES`] lines.
    pub fn screen(&self) -> String {
        let history: Vec<Arc<str>> = self.store.lock().unwrap().replay.iter().flatten().cloned().collect();
        let mut out = String::new();
        for l in &history {
            out.push_str(&text_of(&serde_json::from_str(l).unwrap_or(Value::Null)));
        }
        let lines: Vec<&str> = out.lines().collect();
        lines[lines.len().saturating_sub(SCREEN_LINES)..].join("\n") + "\n"
    }
}

/// The messages rows (messages.rs's shape) of one event, `t` the chat with it folded in.
fn rows(ev: &Value, t: &Transcript) -> Vec<Value> {
    let row = |role: &str, kind: &str, tool: Option<&str>, text: &str| {
        json!({"role": role, "kind": kind, "tool": tool, "text": text})
    };
    let s = |k: &str| ev[k].as_str().unwrap_or("");
    match ev["ev"].as_str().unwrap_or("") {
        "user" => vec![row("user", "text", None, s("text"))],
        "block" if s("kind") == "thinking" => vec![row("assistant", "thinking", None, s("text"))],
        "block" => vec![row("assistant", "text", None, s("text"))],
        "tool_call" => vec![row("assistant", "tool_call", Some(s("name")), &ev["input"].to_string())],
        "tool_result" => vec![row("user", "tool_result", t.tool_name(s("block")), s("text"))],
        "turn_end" => vec![row("system", "turn_end", None, s("text"))],
        _ => vec![],
    }
}

/// The `tool_detail` event of tool call `block` from a REPL cell's post (`repl_task`).
#[must_use]
pub fn tool_detail(block: &str, task: &Value) -> Value {
    let ms = match (task["started_ms"].as_u64(), task["ended_ms"].as_u64()) {
        (Some(a), Some(b)) => json!(b.saturating_sub(a)),
        _ => Value::Null,
    };
    json!({
        "ev": "tool_detail", "block": block,
        "cell": format!("{}.{}", task["repl"].as_str().unwrap_or("repl"), task["id"].as_str().unwrap_or("t?")),
        "status": task["status"], "ms": ms, "code": task["code"], "effects": task["effects"],
        "effects_dropped": task["effects_dropped"], "value": task["value"], "ui": task["ui"], "logs": task["logs"],
    })
}

/// A tool call's main argument: the first string field of its input, on one line.
#[must_use]
pub fn main_arg(input: &Value) -> String {
    let s = input.as_object().and_then(|o| o.values().find_map(|v| v.as_str().map(str::to_owned))).unwrap_or_default();
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut: String = one.chars().take(80).collect();
    if one.chars().count() > 80 {
        cut.push('\u{2026}');
    }
    cut
}

/// One event as plain text (empty for partials and what a reader skips).
#[must_use]
pub fn text_of(ev: &Value) -> String {
    let s = |k: &str| ev[k].as_str().unwrap_or("");
    match s("ev") {
        "user" => {
            let mut out = String::from("\n");
            for l in s("text").lines() {
                out.push_str("> ");
                out.push_str(l);
                out.push('\n');
            }
            out + "\n"
        }
        "block" if s("kind") == "thinking" => "(thinking)\n".into(),
        "block" => format!("{}\n", s("text")),
        "tool_call" => format!("\u{25CF} {}({})\n", s("name"), main_arg(&ev["input"])),
        "tool_result" => {
            let t = s("text");
            let first: String = t.lines().next().unwrap_or("").chars().take(100).collect();
            let more = t.lines().count().saturating_sub(1);
            let tail = if more > 0 { format!(" (+{more} lines)") } else { String::new() };
            format!("  \u{23BF} {first}{tail}\n")
        }
        "note" => format!("({})\n", s("text")),
        "error" => format!("error: {}\n", s("text")),
        "turn_end" => {
            let secs = ev["duration_ms"].as_f64().unwrap_or(0.0) / 1000.0;
            let what = if s("stop") == "end_turn" { String::new() } else { format!("{} \u{00B7} ", s("stop")) };
            format!("({what}{secs:.1}s)\n")
        }
        _ => String::new(),
    }
}

/// Ends a chat child: SIGTERM to its process (portable-pty's killer, for a child without a PTY).
#[derive(Debug, Clone, Copy)]
pub struct PidKiller(pub u32);

impl portable_pty::ChildKiller for PidKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        // SAFETY: kill(2) on a pid this daemon started; one already reaped fails with ESRCH, reported as an error.
        if unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGTERM) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(*self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Quiet;
    impl Adapter for Quiet {
        fn events(&mut self, _: &str) -> Vec<Value> {
            Vec::new()
        }
        fn user(&mut self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn interrupt(&mut self) -> Vec<String> {
            Vec::new()
        }
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("harness-chat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Two turns as the adapters write them: partials growing blocks and calls (a text block and a tool call streaming at
    /// once, the call whole before the text), a REPL cell's detail before its call and again after, a result, and a
    /// second turn cut off with a block still streaming.
    fn events() -> Vec<Value> {
        vec![
            json!({"ev": "session", "agent": "claude", "model": "opus", "session_id": "9f2c"}),
            json!({"ev": "user", "text": "count the files\nin src"}),
            json!({"ev": "busy", "busy": true}),
            json!({"ev": "block_start", "block": "m1:0", "kind": "thinking"}),
            json!({"ev": "delta", "block": "m1:0", "text": "list "}),
            json!({"ev": "block", "block": "m1:0", "kind": "thinking", "text": "list them"}),
            json!({"ev": "block_start", "block": "m1:1", "kind": "text"}),
            json!({"ev": "delta", "block": "m1:1", "text": "Count"}),
            json!({"ev": "tool_start", "block": "t1", "name": "ts"}),
            json!({"ev": "tool_delta", "block": "t1", "json": "{\"code\":"}),
            json!({"ev": "tool_detail", "block": "t1", "cell": "r.t1", "status": "running", "effects": [], "value": null}),
            json!({"ev": "tool_call", "block": "t1", "name": "ts", "input": {"code": "await glob('src/*')"}}),
            json!({"ev": "delta", "block": "m1:1", "text": "ing"}),
            json!({"ev": "block", "block": "m1:1", "kind": "text", "text": "Counting."}),
            json!({"ev": "tool_detail", "block": "t1", "cell": "r.t1", "status": "done", "ms": 4,
                "effects": [{"kind": "search", "done": "Found 3", "status": "ok"}], "value": {"t": "scalar", "type": "number", "v": 3},
                "ui": {"type": "Text", "props": {"text": "3"}, "children": []}}),
            json!({"ev": "tool_result", "block": "t1", "text": "3", "error": false}),
            json!({"ev": "turn_end", "ok": true, "stop": "end_turn", "duration_ms": 900, "cost_usd": 0.02, "text": "Counting."}),
            json!({"ev": "busy", "busy": false}),
            json!({"ev": "user", "text": "and tests?"}),
            json!({"ev": "busy", "busy": true}),
            json!({"ev": "block_start", "block": "m2:0", "kind": "text"}),
            json!({"ev": "delta", "block": "m2:0", "text": "Look"}),
        ]
    }

    /// persisted-state-round-trips: a chat restored from its log (a fresh daemon's `Chat::open`, then the history a
    /// reattach sends) folds, through the one reducer, to exactly the state the same events gave fed live, and the
    /// daemon's own fold of the file equals its fold of the live events. Resume appends to the same log, so a chat
    /// restored again after more events still matches.
    #[test]
    fn a_restored_chat_is_the_chat_it_was() {
        let d = dir("roundtrip");
        let (tx, mut rx) = broadcast::channel(1024);
        let chat = Chat::open("s1", &d, Box::new(Quiet)).unwrap();
        let mut live = Transcript::default();
        let check = |chat: &Chat, live: &Transcript| {
            let back = Chat::open("s1", &d, Box::new(Quiet)).unwrap();
            let (history, _) = back.history_and_subscribe(&tx);
            let mut restored = Transcript::default();
            restored.feed((history.iter().map(|l| format!("{l}\n")).collect::<String>()).as_bytes());
            assert_eq!(&restored, live, "restored from the log vs fed live");
            assert_eq!(back.store.lock().unwrap().transcript, *live, "the daemon's fold of the file");
            assert_eq!(chat.store.lock().unwrap().transcript, *live, "the daemon's fold of the live events");
            let (again, _) = chat.history_and_subscribe(&tx);
            assert_eq!(history, again, "a reattach to the live chat sends the restore's history, byte for byte");
            back
        };
        let evs = events();
        let (first, second) = evs.split_at(18);
        for ev in first {
            chat.record(ev.clone(), &tx);
        }
        while let Ok(Ev::Data(b)) = rx.try_recv() {
            live.feed(&b);
        }
        assert_eq!(live.items.len(), 5, "{:#?}", live.items);
        assert!(matches!(&live.items[2], Item::Text { text, .. } if text == "Counting."), "the text before the call it streamed beside");
        assert_eq!((live.title.as_str(), live.session_id.as_str(), live.busy), ("count the files", "9f2c", false));
        let back = check(&chat, &live);
        // Resumed: the restored chat takes the next events, appended to the same log.
        drop(chat);
        for ev in second {
            back.record(ev.clone(), &tx);
        }
        while let Ok(Ev::Data(b)) = rx.try_recv() {
            live.feed(&b);
        }
        assert!(live.busy);
        check(&back, &live);
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// A crash mid-append leaves a torn last line: a restore reports it (an `error` in the history, naming the line) and
    /// keeps every whole event before it, and what is recorded next starts a line of its own.
    #[test]
    fn a_torn_last_line_is_reported_and_skipped() {
        let d = dir("torn");
        let (tx, _rx) = broadcast::channel(1024);
        let chat = Chat::open("s1", &d, Box::new(Quiet)).unwrap();
        for ev in &events()[..6] {
            chat.record(ev.clone(), &tx);
        }
        drop(chat);
        let mut f = std::fs::OpenOptions::new().append(true).open(d.join("chat")).unwrap();
        f.write_all(br#"{"ev":"block","block":"m1:1","kind":"te"#).unwrap();
        drop(f);
        let back = Chat::open("s1", &d, Box::new(Quiet)).unwrap();
        let (history, _) = back.history_and_subscribe(&tx);
        let mut t = Transcript::default();
        t.feed((history.iter().map(|l| format!("{l}\n")).collect::<String>()).as_bytes());
        assert_eq!(t.title, "count the files");
        assert!(matches!(t.items.last(), Some(Item::Error { text }) if text.starts_with("line 7 of this chat's log")), "{:#?}", t.items);
        assert_eq!(t.items.len(), 3, "user, thinking, the report: {:#?}", t.items);
        back.record(json!({"ev": "note", "text": "after the crash"}), &tx);
        drop(back);
        let log = std::fs::read(d.join("chat")).unwrap();
        let bad: Vec<usize> = harness_chat::lines(&log).filter(|l| l.2.is_none()).map(|l| l.0).collect();
        assert_eq!(bad, [7], "only the torn line is bad; the next event is whole on its own line");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
