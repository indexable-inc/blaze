//! Chat mode: an agent session driven over its structured protocol instead of its TUI in a PTY, so the native app draws
//! the conversation itself (harness-native's chat pane) and agents talk to it without typing into a terminal. The agent's
//! own protocol stays in its adapter ([`claude`]: stream-json; [`codex`]: the slot for `codex app-server`); everything
//! here, and everything a client reads, is the one agent-neutral event model below.
//!
//! # The event model
//!
//! One JSON object per line, each with `ev` and `ts` (ms since the epoch). A `block` names one assistant block (text,
//! thinking) or one tool call, unique in the session:
//!
//! | `ev` | fields | |
//! | --- | --- | --- |
//! | `session` | `agent`, `model` | the agent started a turn with this model |
//! | `user` | `text` | the user's message (written by the daemon as it sends it) |
//! | `busy` | `busy` | a turn started (true) or ended (false) |
//! | `block_start` | `block`, `kind` (`text` \| `thinking`) | a block began streaming *(partial)* |
//! | `delta` | `block`, `text` | more of a streaming block *(partial)* |
//! | `block` | `block`, `kind`, `text` | a block, whole (replaces its partials) |
//! | `tool_start` | `block`, `name` | a tool call began, its arguments streaming *(partial)* |
//! | `tool_delta` | `block`, `json` | more of its arguments' JSON text *(partial)* |
//! | `tool_call` | `block`, `name`, `input` | the call, whole (replaces its partials) |
//! | `tool_result` | `block`, `text`, `error` | what the call returned |
//! | `tool_detail` | `block`, `cell`, `status`, `ms`, `code`, `effects`, `effects_dropped`, `value`, `logs` | what a harness REPL cell the call ran did, whole each time (see below) |
//! | `note` | `text` | something the agent said that is not the assistant's words (an interrupt) |
//! | `error` | `text` | the agent reported an error |
//! | `turn_end` | `ok`, `stop` (`end_turn` \| `interrupted` \| `error` \| ...), `duration_ms`, `cost_usd`, `usage`, `text` | the turn is over; `text` its last assistant words |
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
//!
//! # Storage and attach
//!
//! - The child runs with stdin and stdout piped (no PTY), stderr appended to `objects/sN/stderr`; every line it writes is
//!   appended to `objects/sN/raw` as it came (the agent's own protocol, kept whole), and every event to
//!   `objects/sN/chat`, the conversation's record (indexed as the `chat` stream).
//! - `attach` with `events: true` answers its ok (info `mode: "chat"`), then T_DATA frames of whole event lines: the
//!   history, then each event live, then T_EXIT. The history leaves out the partials of every block that came whole
//!   since, so a reattach is the finished conversation plus what is still streaming. Without `events` (the terminal
//!   client) the same events come as plain text.
//! - `send` writes the adapter's user line and records a `user` event; `interrupt` writes the adapter's interrupt.
//! - Busy (the sidebar's breathing dot) is a turn in flight, from a `user` until its `turn_end`. The title is the first
//!   user message's first line.
//! - The `messages` stream (what `Obj.reply()` reads) is written here from the events, `turn_end` on each turn's end;
//!   messages.rs leaves chat sessions alone. `screen` is [`Chat::screen`]: the conversation as plain text.
pub mod claude;
pub mod codex;

use crate::{daemon::Ev, db};
use bytes::Bytes;
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

/// An agent's protocol: how its output lines become events, and the lines that send a message and stop a turn.
pub trait Adapter: Send {
    /// The events one line of the agent's output stands for (none for what the model has no place for).
    fn events(&mut self, line: &str) -> Vec<Value>;
    /// The line that sends the user's `text`.
    fn user(&mut self, text: &str) -> String;
    /// The line that stops the turn in flight.
    fn interrupt(&mut self) -> String;
}

/// Whether event `ev` is a partial a whole block replaces.
fn partial(ev: &str) -> bool {
    matches!(ev, "block_start" | "delta" | "tool_start" | "tool_delta")
}

/// The history a reattach replays.
struct Store {
    raw: File,
    events: File,
    messages: File,
    /// Every event, a partial gone (None) once its block came whole.
    replay: Vec<Option<Arc<str>>>,
    /// Where each block's partials are in `replay`.
    partials: HashMap<String, Vec<usize>>,
    /// Where each block's latest `tool_detail` is in `replay` (an earlier one is gone once a later came).
    details: HashMap<String, usize>,
    /// Tool call (block) to tool name, for the messages rows of their results.
    tools: HashMap<String, String>,
    adapter: Box<dyn Adapter>,
}

pub struct Chat {
    oid: String,
    store: Mutex<Store>,
    title: Mutex<String>,
    model: Mutex<String>,
    /// A turn is in flight.
    pub busy: AtomicBool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Chat {
    /// A chat's record in `dir` (`objects/sN`), opened for appending, read through `adapter`.
    pub fn open(oid: &str, dir: &Path, adapter: Box<dyn Adapter>) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let open = |name: &str| std::fs::OpenOptions::new().create(true).append(true).open(dir.join(name));
        let (raw, events, messages) = (open("raw")?, open("chat")?, open("messages")?);
        db::index_stream(oid, "chat", &dir.join("chat"));
        db::index_stream(oid, "messages", &dir.join("messages"));
        Ok(Self {
            oid: oid.to_owned(),
            store: Mutex::new(Store {
                raw,
                events,
                messages,
                replay: Vec::new(),
                partials: HashMap::new(),
                details: HashMap::new(),
                tools: HashMap::new(),
                adapter,
            }),
            title: Mutex::new(String::new()),
            model: Mutex::new(String::new()),
            busy: AtomicBool::new(false),
        })
    }

    pub fn title(&self) -> String {
        self.title.lock().unwrap().clone()
    }

    pub fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    /// A line the agent wrote: kept in `raw`, and its events recorded. Whether the busy state changed.
    pub fn agent_line(&self, line: &str, tx: &broadcast::Sender<Ev>) -> bool {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return false;
        }
        let evs = {
            let mut st = self.store.lock().unwrap();
            if let Err(e) = st.raw.write_all(format!("{line}\n").as_bytes()) {
                eprintln!("{} raw: {e}", self.oid);
            }
            st.adapter.events(line)
        };
        let mut flipped = false;
        for ev in evs {
            flipped |= self.record(ev, tx);
        }
        flipped
    }

    /// The line that sends `text` to the agent, after recording its `user` event (and the turn starting).
    pub fn user_line(&self, text: &str, tx: &broadcast::Sender<Ev>) -> String {
        self.record(json!({"ev": "user", "text": text}), tx);
        self.record(json!({"ev": "busy", "busy": true}), tx);
        self.store.lock().unwrap().adapter.user(text)
    }

    /// The line that stops the turn in flight.
    pub fn interrupt_line(&self) -> String {
        self.store.lock().unwrap().adapter.interrupt()
    }

    /// The agent ended: a turn still in flight ends with it.
    pub fn ended(&self, tx: &broadcast::Sender<Ev>) {
        if self.busy.load(Ordering::Relaxed) {
            self.record(json!({"ev": "busy", "busy": false}), tx);
        }
    }

    /// Record one event: stamped, appended to the record, kept for reattaches, sent to the attached clients, its messages
    /// rows written. Whether the busy state changed.
    pub fn record(&self, mut ev: Value, tx: &broadcast::Sender<Ev>) -> bool {
        ev["ts"] = json!(now_ms());
        let kind = ev["ev"].as_str().unwrap_or("").to_owned();
        let block = ev["block"].as_str().map(str::to_owned);
        let line = ev.to_string();
        let mut st = self.store.lock().unwrap();
        if let Err(e) = st.events.write_all(format!("{line}\n").as_bytes()) {
            eprintln!("{} chat: {e}", self.oid);
        }
        let at = st.replay.len();
        st.replay.push(Some(Arc::from(line.as_str())));
        if let Some(b) = &block {
            if partial(&kind) {
                st.partials.entry(b.clone()).or_default().push(at);
            } else if kind == "tool_detail" {
                if let Some(i) = st.details.insert(b.clone(), at) {
                    st.replay[i] = None;
                }
            } else if matches!(kind.as_str(), "block" | "tool_call") {
                for i in st.partials.remove(b).unwrap_or_default() {
                    st.replay[i] = None;
                }
            }
        }
        // Sent under the lock, so a reattach's history and its live events neither miss nor double one.
        let _ = tx.send(Ev::Data(Bytes::from(format!("{line}\n"))));
        let rows = rows(&ev, &mut st.tools);
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
        match kind.as_str() {
            "session" => {
                if let Some(m) = ev["model"].as_str() {
                    *self.model.lock().unwrap() = m.to_owned();
                }
                false
            }
            "user" => {
                let mut t = self.title.lock().unwrap();
                if t.is_empty() {
                    let text = ev["text"].as_str().unwrap_or("");
                    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                    let mut s: String = first.chars().take(60).collect();
                    if first.chars().count() > 60 {
                        s.push('\u{2026}');
                    }
                    *t = s;
                    return true;
                }
                false
            }
            "busy" => {
                let b = ev["busy"].as_bool().unwrap_or(false);
                self.busy.swap(b, Ordering::Relaxed) != b
            }
            _ => false,
        }
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

/// The messages rows (messages.rs's shape) of one event.
fn rows(ev: &Value, tools: &mut HashMap<String, String>) -> Vec<Value> {
    let row = |role: &str, kind: &str, tool: Option<&str>, text: &str| {
        json!({"role": role, "kind": kind, "tool": tool, "text": text})
    };
    let s = |k: &str| ev[k].as_str().unwrap_or("");
    match ev["ev"].as_str().unwrap_or("") {
        "user" => vec![row("user", "text", None, s("text"))],
        "block" if s("kind") == "thinking" => vec![row("assistant", "thinking", None, s("text"))],
        "block" => vec![row("assistant", "text", None, s("text"))],
        "tool_call" => {
            tools.insert(s("block").to_owned(), s("name").to_owned());
            vec![row("assistant", "tool_call", Some(s("name")), &ev["input"].to_string())]
        }
        "tool_result" => {
            let name = tools.get(s("block")).cloned();
            vec![row("user", "tool_result", name.as_deref(), s("text"))]
        }
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
        "effects_dropped": task["effects_dropped"], "value": task["value"], "logs": task["logs"],
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
