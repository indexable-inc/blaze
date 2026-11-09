//! Chat mode: an agent session driven over its structured protocol instead of its TUI in a PTY, so the native app draws
//! the conversation itself (harness-native's chat pane) and agents talk to it without typing into a terminal. The agent's
//! own protocol stays in its adapter ([`claude`]: stream-json; [`codex`]: `codex app-server`'s JSON-RPC); everything
//! here, and everything a client reads, is the one agent-neutral event model below.
//!
//! # The event model
//!
//! Typed once, in harness-proto's `event` (`Event`, `Body`): each event a deterministic-CBOR map with `ev` naming it and
//! `ts` (ms since the epoch), carried as one T_EVENT frame (harness-proto's `frame`). A `block` names one assistant block
//! (text, thinking) or one tool call, unique in the session:
//!
//! | `ev` | fields | |
//! | --- | --- | --- |
//! | `session` | `agent`, `model`, `session_id`, `effort` | the agent started (or resumed) with this model and effort, or a switch to them took; `session_id` is its own id for the conversation (Claude's session id, Codex's thread id), what a resume names |
//! | `models` | `models` (`id`, `model`, `name`, `about`, `efforts`, `effort`) | the models the agent offers, from its own list, as it starts |
//! | `choice` | `model`, `effort` | the person switched the model and/or effort for the turns from here on (`set_model`); a resume starts with the latest |
//! | `context` | `used`, `window`, `last` (`input`, `cache_read`, `cache_write`, `output`, `thinking`), `parts` (`id`, `name`, `tokens`, `of`, `estimate`) | tokens of the model's context window the conversation fills, after a turn; the last call's exact split and what the context holds part by part, each the agent's count or harness's named estimate ([`context`]) |
//! | `plan` | `windows` (`name`, `used`, `resets_at`, `warn`, `limited`) | how much of the person's plan with the agent's maker is used, a window each (`5h`, `week`): the share used (0 to 1), when it resets (unix seconds), whether the agent warns of it or refuses for it; the account's, not the chat's (claude's `rate_limit_event`, codex's `account/rateLimits/updated`) |
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
//! | `error` | `text` | the agent reported an error, or the daemon did about this chat (a torn tail cut off, a resume refused) |
//! | `turn_end` | `ok`, `stop` (`end_turn` \| `interrupted` \| `error` \| ...), `duration_ms`, `cost_usd` (the turn's own; null when the agent says none), `usage`, `text` | the turn is over; `text` its last assistant words |
//!
//! ## `tool_detail`: a REPL cell's steps and value
//!
//! The harness REPL (`mcp/src/repl.ts`) posts each cell to the daemon as it runs (`repl_task`), with the agent's id for
//! the tool call that ran it when the agent gave one (in the MCP request's `_meta`: Claude as `claudecode/toolUseId`, Codex
//! as `callId`, the id of its `mcpToolCall` item). For a chat session
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
//!   `objects/sN/chat`, the conversation's record (indexed as the `chat` stream): harness-proto's `log`, its magic and
//!   then each event's T_EVENT frame, the same bytes a chat attach is sent.
//! - `attach` with `events: true` answers `attached` (info `mode: "chat"`), then T_EVENT frames: the history, then each
//!   event live, each frame the bytes its record in the log holds. When the agent exits a `Down::Exit` says so, and the
//!   stream stays open: a chat outlives its agent's process, and a resume's events come on the same stream. The history
//!   replaces the partials of every block that came whole since by that whole event, in the place its first partial
//!   took, so a reattach folds to the same conversation the live events did (harness-chat's reducer). Without `events`
//!   (the terminal client) the same events come as plain text in T_DATA frames, and the stream ends at the agent's exit.
//! - `send` writes the adapter's user lines and records a `user` event; `interrupt` writes the adapter's interrupt. An
//!   adapter may also write lines of its own: a handshake as the child starts ([`Adapter::start`]) and answers to what
//!   the child asks or queued messages once it can take them ([`Adapter::replies`], drained after every line read).
//! - Busy (the sidebar's breathing dot) is a turn in flight, from a `user` until its `turn_end`. The title is the first
//!   user message's first line (kept in the index's meta too, so `list` names a past chat without reading its log).
//! - A chat writes no `messages` stream: the kernel's `messages` view projects a chat's rows from its log
//!   (`mcp/src/sql.ts`), and messages.rs leaves chat sessions alone. `screen` is [`Chat::screen`]: the conversation as
//!   plain text. `harness log <id>` prints a log one event per line.
//!
//! # Restore and resume
//!
//! The log is the chat: one file per session, appended to forever (never compacted). A chat from before this daemon
//! started is loaded from it on its first attach, send or screen ([`Chat::open`] folds each record through the same
//! [`Store::absorb`] a live event goes through, its frame's bytes kept as they are), and is then a session like any
//! other, its agent not running. A crash mid-append leaves a torn last frame: the open cuts it off and records an
//! `error` naming it, so the next frame is reachable; a whole frame that does not decode is reported (stderr and an
//! `error` in the history) and skipped. A message to a chat whose agent is not running resumes it: the agent starts
//! again on the same log with its own session id from the log (Claude: `--resume <session_id>`; Codex:
//! `thread/resume`). A chat that cannot be resumed (no session id in its log, an agent kind with no resume) gets an
//! `error` event naming why, and the message is not sent; it never starts a fresh conversation in its place.
//!
//! A log from before harness schema 1 (NDJSON) is converted once, at daemon start before it listens ([`migrate_logs`],
//! harness-proto's `migrate`), and marked in the index (`meta.chat_log`); [`Chat::open`] refuses one it finds unconverted.
pub mod aside;
pub mod claude;
pub mod codex;
pub mod context;
pub mod models;

use crate::daemon::Ev;
use bytes::Bytes;
#[cfg(test)]
use harness_chat::Item;
use harness_chat::Transcript;
use harness_proto::event::{Body, Detail, Event, Kind};
use harness_proto::{log, migrate};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use parking_lot::Mutex;
use std::sync::Arc;
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
    fn events(&mut self, line: &str) -> Vec<Body>;
    /// The lines to write now that [`Adapter::events`] read a line: answers to the child's requests, messages it can
    /// take now. Drained.
    fn replies(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// The lines that send the user's `text` and the pictures at `images` (absolute paths of PNG or JPEG files) with it
    /// (none while the adapter holds it until the agent can take it).
    fn user(&mut self, text: &str, images: &[String]) -> Vec<String>;
    /// The lines that stop the turn in flight (none when there is nothing to stop yet).
    fn interrupt(&mut self) -> Vec<String>;
    /// The lines that switch the model (a [`harness_proto::event::ModelChoice::id`]) and/or the effort for the turns from
    /// here on (none when the adapter carries them on its next turn itself), or why this agent cannot.
    fn choose(&mut self, model: Option<&str>, effort: Option<&str>) -> Result<Vec<String>, String>;
    /// The agent kind it speaks for (`claude`, `codex`), whose `models` events the daemon keeps ([`models`]); empty for
    /// none.
    fn agent(&self) -> &'static str {
        ""
    }
}

/// The media type of a picture an agent takes, from its first bytes: PNG, JPEG, GIF or WebP (None for anything else).
#[must_use]
pub fn picture_type(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Check that each of `images` is a picture an agent takes (an absolute path to a PNG, JPEG, GIF or WebP file), so a
/// message with one that is not is refused before it is recorded. The error names the path and what is wrong with it.
pub fn check_pictures(images: &[String]) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Read;
    for p in images {
        if !p.starts_with('/') {
            anyhow::bail!("picture {p:?} is not an absolute path");
        }
        let mut head = [0u8; 12];
        let n = std::fs::File::open(p).and_then(|mut f| f.read(&mut head)).with_context(|| format!("reading picture {p}"))?;
        if picture_type(&head[..n]).is_none() {
            anyhow::bail!("{p} is not a PNG, JPEG, GIF or WebP picture");
        }
    }
    Ok(())
}

/// The adapter of a chat whose agent is not running (restored from its log, before a resume): it reads and writes
/// nothing.
pub struct NoAdapter;

impl Adapter for NoAdapter {
    fn events(&mut self, _: &str) -> Vec<Body> {
        Vec::new()
    }
    fn user(&mut self, _: &str, _: &[String]) -> Vec<String> {
        Vec::new()
    }
    fn interrupt(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// Nothing runs to ask: the choice is in the log, and the resume starts the agent with it.
    fn choose(&mut self, _: Option<&str>, _: Option<&str>) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

/// Startup: chats indexed before the index kept titles get theirs, from the first `user` event of their log.
pub fn backfill_titles() {
    for oid in crate::db::chats_without_title() {
        let Ok(bytes) = std::fs::read(crate::db::object_dir(&oid).join("chat")) else { continue };
        let Ok(l) = log::read(&bytes) else { continue };
        let first = l.records.iter().find_map(|r| match &r.event {
            Ok(Event { body: Body::User { text, .. }, .. }) => Some(harness_chat::title_of(text)),
            _ => None,
        });
        if let Some(t) = first.filter(|t| !t.is_empty()) {
            crate::db::set_meta(&oid, "title", &serde_json::json!(t));
        }
    }
}

/// Startup, before the daemon listens: every chat log not yet marked as this schema's is converted from NDJSON if it is
/// one (harness-proto's `migrate`: written beside, synced, renamed over, the directory synced, then marked), so a crash
/// anywhere leaves the old log or the new one. A chat's `messages` file, which chats no longer write (the kernel
/// projects it from the log), is removed with its index row. A log that is neither format is reported and left alone;
/// [`Chat::open`] refuses it, naming it.
pub fn migrate_logs() {
    for oid in crate::db::chats_unmarked(harness_proto::SCHEMA) {
        if let Err(e) = migrate_one(&oid) {
            eprintln!("{oid} chat: migrating its log to harness schema {}: {e:#}", harness_proto::SCHEMA);
        }
    }
}

fn migrate_one(oid: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    let dir = crate::db::object_dir(oid);
    let (chat, tmp) = (dir.join("chat"), dir.join(migrate::TMP));
    let bytes = match std::fs::read(&chat) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", chat.display())),
    };
    let steps = migrate::plan(false, bytes.as_deref()).map_err(|e| anyhow::anyhow!("{}: {e}", chat.display()))?;
    let converts = steps.iter().any(|s| matches!(s, migrate::Step::WriteTmp(_)));
    for step in steps {
        match step {
            migrate::Step::WriteTmp(b) => std::fs::write(&tmp, b).with_context(|| format!("writing {}", tmp.display()))?,
            migrate::Step::SyncTmp => File::open(&tmp)?.sync_all().with_context(|| format!("fsync {}", tmp.display()))?,
            migrate::Step::Rename => std::fs::rename(&tmp, &chat).with_context(|| format!("renaming {} over {}", tmp.display(), chat.display()))?,
            migrate::Step::SyncDir => File::open(&dir)?.sync_all().with_context(|| format!("fsync {}", dir.display()))?,
            migrate::Step::Mark => crate::db::set_meta(oid, "chat_log", &serde_json::json!(harness_proto::SCHEMA)),
        }
    }
    if converts {
        eprintln!("{oid} chat: its NDJSON log was converted to harness schema {}", harness_proto::SCHEMA);
    }
    crate::db::drop_stream(oid, "messages");
    match std::fs::remove_file(dir.join("messages")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => eprintln!("{oid}: removing its old messages file: {e}"),
        _ => {}
    }
    Ok(())
}

/// A chat's state, folded from its events one at a time: the same fold for an event recorded live and for a record of
/// its log read back, so a restored chat is the chat it was.
struct Store {
    raw: Option<File>,
    log: File,
    /// The history a reattach replays, as frames: every event, a block's partials replaced by its whole form in the place
    /// its first partial took (so the conversation's order is the live one), every block's `tool_detail` but its latest
    /// gone. Each frame is the bytes the log holds for it.
    replay: Vec<Option<Arc<[u8]>>>,
    /// Where each block's partials are in `replay`, and whether each starts the block.
    partials: HashMap<String, Vec<(usize, bool)>>,
    /// Where each block's latest `tool_detail` is in `replay`.
    details: HashMap<String, usize>,
    /// The conversation (the one reducer, harness-chat): title, model, busy, the agent's session id, tool names.
    transcript: Transcript,
    adapter: Box<dyn Adapter>,
}

impl Store {
    /// Fold one event (its frame as written, and decoded).
    fn absorb(&mut self, frame: Arc<[u8]>, ev: &Event) {
        let at = self.replay.len();
        let mut slot = None;
        if let Some(b) = ev.body.block() {
            if ev.body.partial() {
                self.partials.entry(b.to_owned()).or_default().push((at, ev.body.starts()));
            } else if matches!(ev.body, Body::ToolDetail(_)) {
                if let Some(i) = self.details.insert(b.to_owned(), at) {
                    self.replay[i] = None;
                }
            } else if ev.body.whole() {
                for (i, starts) in self.partials.remove(b).unwrap_or_default() {
                    if slot.is_none() && starts {
                        slot = Some(i);
                    }
                    self.replay[i] = None;
                }
            }
        }
        self.transcript.apply(ev);
        match slot {
            Some(i) => self.replay[i] = Some(frame),
            None => self.replay.push(Some(frame)),
        }
    }

    /// Append one event to the log and fold it: its frame.
    fn append(&mut self, oid: &str, ev: &Event) -> Arc<[u8]> {
        let frame: Arc<[u8]> = ev.frame().into();
        if let Err(e) = self.log.write_all(&frame) {
            eprintln!("{oid} chat: {e}");
        }
        self.absorb(frame.clone(), ev);
        frame
    }
}

pub struct Chat {
    oid: String,
    store: Mutex<Store>,
    /// A turn is in flight.
    pub busy: AtomicBool,
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Chat {
    /// Chat `oid`'s record in `dir` (`objects/sN`), opened for appending, its log folded back in (a past chat restored, a
    /// chat resumed; nothing for a new one). A torn last frame (a crash mid-append) is cut off and an `error` recorded
    /// naming it; a whole frame that does not decode is reported, to the daemon's log and as an `error` in the history a
    /// reattach replays, and skipped; the rest of the log stands. Read through `adapter` from here on.
    ///
    /// # Errors
    /// The file's IO, or a log that is not this schema's (an NDJSON one the start's migration did not convert), named.
    pub fn open(oid: &str, dir: &Path, adapter: Box<dyn Adapter>) -> anyhow::Result<Self> {
        use anyhow::Context;
        std::fs::create_dir_all(dir)?;
        let path = dir.join("chat");
        let bytes = std::fs::read(&path).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(Vec::new()) } else { Err(e) })?;
        if log::unstarted(&bytes) {
            // New, or cut inside its magic by a crash: nothing recorded yet.
            std::fs::write(&path, log::MAGIC).with_context(|| format!("starting {}", path.display()))?;
        }
        let l = log::read(&bytes).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let file = std::fs::OpenOptions::new().append(true).open(&path)?;
        if let Some((at, len)) = l.torn {
            file.set_len(l.end as u64).with_context(|| format!("cutting the torn tail off {}", path.display()))?;
            eprintln!("{oid} chat: {len} bytes at {at} of its log are a torn frame (a crash mid-append); cut off");
        }
        let mut store = Store {
            raw: None,
            log: file,
            replay: Vec::new(),
            partials: HashMap::new(),
            details: HashMap::new(),
            transcript: Transcript::default(),
            adapter,
        };
        for r in &l.records {
            match &r.event {
                Ok(ev) => store.absorb(r.frame.into(), ev),
                Err(e) => {
                    let why = format!("record {} of this chat's log ({} bytes) is not an event ({e}); skipped", r.n, r.frame.len());
                    eprintln!("{oid} chat: {why}");
                    let ev = Event { ts: 0, body: Body::Error { text: why } };
                    store.replay.push(Some(ev.frame().into()));
                }
            }
        }
        if let Some((at, len)) = l.torn {
            let text = format!(
                "the last {len} bytes of this chat's log (from byte {at}) were a torn frame, likely a crash mid-append; cut off"
            );
            store.append(oid, &Event { ts: now_ms(), body: Body::Error { text } });
        }
        let busy = AtomicBool::new(store.transcript.busy);
        Ok(Self { oid: oid.to_owned(), store: Mutex::new(store), busy })
    }

    /// The agent's process (re)starts: its protocol from here on, its raw lines appended to `objects/sN/raw`.
    pub fn set_adapter(&self, dir: &Path, adapter: Box<dyn Adapter>) -> anyhow::Result<()> {
        let raw = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("raw"))?;
        let mut st = self.store.lock();
        st.adapter = adapter;
        st.raw = Some(raw);
        Ok(())
    }

    pub fn title(&self) -> String {
        self.store.lock().transcript.title.clone()
    }

    pub fn model(&self) -> String {
        self.store.lock().transcript.model.clone()
    }

    /// The reasoning effort its agent runs with; empty when its `session` event did not say.
    pub fn effort(&self) -> String {
        self.store.lock().transcript.effort.clone()
    }

    /// The person's latest choice of model and of effort (`choice` events), each None when never chosen: what a resume
    /// starts the agent with.
    pub fn chosen(&self) -> (Option<String>, Option<String>) {
        let st = self.store.lock();
        let some = |s: &String| Some(s.clone()).filter(|s| !s.is_empty());
        (some(&st.transcript.chosen_model), some(&st.transcript.chosen_effort))
    }

    /// Switch to `model` and/or `effort` for the turns from here on: each checked against the models the agent listed in
    /// this chat's log, else the list its kind `kind` last gave the daemon ([`models`]; a chat from before chats listed
    /// them, or whose agent is not running), the effort against the model it will run (the one chosen now or the one it
    /// runs), and the choice recorded (a `choice` event, what a resume keeps). While its agent runs (`live`) the adapter
    /// is asked for it and the lines to write to the agent come back; otherwise only the choice is recorded.
    ///
    /// # Errors
    /// Nothing to choose; no agent of its kind has listed models yet; a model the list lacks or an effort that model does
    /// not take (each naming the chat, the value and what it offers); the adapter's refusal.
    pub fn choose(
        &self, kind: &str, model: Option<&str>, effort: Option<&str>, live: bool, tx: &broadcast::Sender<Ev>,
    ) -> anyhow::Result<Vec<String>> {
        let oid = &self.oid;
        let (model, effort) = (model.filter(|m| !m.is_empty()), effort.filter(|e| !e.is_empty()));
        anyhow::ensure!(model.is_some() || effort.is_some(), "set_model for chat {oid} names neither a model nor an effort");
        let lines = {
            let mut guard = self.store.lock();
            let st = &mut *guard;
            let t = &st.transcript;
            let kind = if kind.is_empty() { t.agent.as_str() } else { kind };
            let kept;
            let listed: &[harness_proto::event::ModelChoice] = if t.models.is_empty() {
                kept = models::snapshot();
                kept.get(kind).map_or(&[], Vec::as_slice)
            } else {
                &t.models
            };
            if listed.is_empty() {
                anyhow::bail!("chat {oid}: no {kind} has listed its models to harness yet; the list arrives when a {kind} chat starts");
            }
            let ids = || listed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(", ");
            let target = match model {
                Some(m) => Some(listed.iter().find(|c| c.id == m).ok_or_else(|| anyhow::anyhow!("chat {oid}'s agent offers no model {m:?} (it offers {})", ids()))?),
                None => [t.chosen_model.as_str(), t.model.as_str()].into_iter().find_map(|m| listed.iter().find(|c| c.is(m))),
            };
            if let Some(e) = effort {
                let Some(target) = target else {
                    anyhow::bail!("chat {oid} runs model {:?}, which is not among the models its agent listed ({}), so its effort is not known to take {e:?}", t.model, ids());
                };
                if !target.efforts.iter().any(|x| x == e) {
                    let takes = if target.efforts.is_empty() { "takes no effort".to_owned() } else { format!("takes {}", target.efforts.join(", ")) };
                    anyhow::bail!("chat {oid}'s model {} {takes}, not {e:?}", target.id);
                }
            }
            if live { st.adapter.choose(model, effort).map_err(|why| anyhow::anyhow!("chat {oid}: {why}"))? } else { Vec::new() }
        };
        self.record(Body::Choice { model: model.map(str::to_owned), effort: effort.map(str::to_owned) }, tx);
        Ok(lines)
    }

    /// Whether the agent ever said or did anything in this conversation (words, a thought, a call). False for a chat
    /// no turn of which reached its agent: Claude writes a conversation only once a turn runs, so there is none to
    /// resume and a relaunch starts it instead.
    pub fn agent_spoke(&self) -> bool {
        use harness_chat::Item;
        self.store.lock().transcript.items.iter().any(|i| matches!(i, Item::Text { .. } | Item::Thinking { .. } | Item::Tool { .. }))
    }

    /// The agent's own id for this conversation (what resumes it), when its log said one.
    pub fn session_id(&self) -> Option<String> {
        Some(self.store.lock().transcript.session_id.clone()).filter(|s| !s.is_empty())
    }

    /// The lines to write to the child as it starts.
    pub fn start_lines(&self) -> Vec<String> {
        self.store.lock().adapter.start()
    }

    /// A line the agent wrote: kept in `raw`, and its events recorded. Whether the busy state changed, and the lines to
    /// write back to the agent.
    pub fn agent_line(&self, line: &str, tx: &broadcast::Sender<Ev>) -> (bool, Vec<String>) {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return (false, Vec::new());
        }
        let (evs, replies, agent) = {
            let mut st = self.store.lock();
            if let Some(raw) = st.raw.as_mut()
                && let Err(e) = raw.write_all(format!("{line}\n").as_bytes())
            {
                eprintln!("{} raw: {e}", self.oid);
            }
            let evs = st.adapter.events(line);
            (evs, st.adapter.replies(), st.adapter.agent())
        };
        let mut flipped = false;
        for ev in evs {
            if let Body::Models { models: listed } = &ev {
                models::learned(agent, listed);
            }
            flipped |= self.record(ev, tx);
        }
        (flipped, replies)
    }

    /// The lines that send `text` and the pictures at `images` to the agent, after recording its `user` event (and the turn
    /// starting).
    pub fn user_lines(&self, text: &str, images: &[String], tx: &broadcast::Sender<Ev>) -> Vec<String> {
        self.record(Body::User { text: text.to_owned(), images: images.to_vec() }, tx);
        self.record(Body::Busy { busy: true }, tx);
        self.store.lock().adapter.user(text, images)
    }

    /// The lines that stop the turn in flight.
    pub fn interrupt_lines(&self) -> Vec<String> {
        self.store.lock().adapter.interrupt()
    }

    /// The agent ended: a turn still in flight ends with it.
    pub fn ended(&self, tx: &broadcast::Sender<Ev>) {
        if self.busy.load(Ordering::Relaxed) {
            self.record(Body::Busy { busy: false }, tx);
        }
    }

    /// Record one event: stamped, its frame appended to the log, folded into the chat's state, and the same frame sent
    /// to the attached clients. Whether the busy state or the title changed (what `list` says of it).
    pub fn record(&self, body: Body, tx: &broadcast::Sender<Ev>) -> bool {
        let ev = Event { ts: now_ms(), body };
        let mut st = self.store.lock();
        let title = st.transcript.title.is_empty();
        let frame = st.append(&self.oid, &ev);
        let retitled = title && !st.transcript.title.is_empty();
        let busy = st.transcript.busy;
        // Sent under the lock, so a reattach's history and its live events neither miss nor double one.
        let _ = tx.send(Ev::Event(Bytes::from_owner(frame)));
        drop(st);
        let flipped = self.busy.swap(busy, Ordering::Relaxed) != busy;
        flipped || retitled
    }

    /// The history a reattach starts with (frames), and a receiver for every event after it.
    pub fn history_and_subscribe(&self, tx: &broadcast::Sender<Ev>) -> (Vec<Arc<[u8]>>, broadcast::Receiver<Ev>) {
        let st = self.store.lock();
        (st.replay.iter().flatten().cloned().collect(), tx.subscribe())
    }

    /// The conversation as plain text, its last [`SCREEN_LINES`] lines.
    pub fn screen(&self) -> String {
        let history: Vec<Arc<[u8]>> = self.store.lock().replay.iter().flatten().cloned().collect();
        let out = text_of_frames(history.iter().map(|f| &f[..]));
        let lines: Vec<&str> = out.lines().collect();
        lines[lines.len().saturating_sub(SCREEN_LINES)..].join("\n") + "\n"
    }
}

/// Whole T_EVENT frames as the conversation's plain text.
pub fn text_of_frames<'a>(frames: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut out = String::new();
    for f in frames {
        let mut rest = f;
        while let Ok(Some((_, payload, used))) = harness_proto::frame::split(rest) {
            if let Ok(ev) = harness_proto::cbor::decode::<Event>(payload) {
                out.push_str(&text_of(&ev));
            }
            rest = &rest[used..];
        }
    }
    out
}

/// The `tool_detail` event of tool call `block` from a REPL cell's post (`repl_task`).
#[must_use]
pub fn tool_detail(block: &str, task: &serde_json::Value) -> Body {
    Body::ToolDetail(Detail::of_task(block, task))
}

/// One event as plain text (empty for partials and what a reader skips).
#[must_use]
pub fn text_of(ev: &Event) -> String {
    match &ev.body {
        Body::User { text, images } => {
            let mut out = String::from("\n");
            for l in text.lines() {
                out.push_str("> ");
                out.push_str(l);
                out.push('\n');
            }
            for p in images {
                out.push_str(&format!("> ![image]({p})\n"));
            }
            out + "\n"
        }
        Body::Block { kind: Kind::Thinking, .. } => "(thinking)\n".into(),
        Body::Block { text, .. } => format!("{text}\n"),
        Body::ToolCall { name, input, .. } => format!("\u{25CF} {name}({})\n", harness_chat::main_arg(input)),
        Body::ToolResult { text, .. } => {
            let first: String = text.lines().next().unwrap_or("").chars().take(100).collect();
            let more = text.lines().count().saturating_sub(1);
            let tail = if more > 0 { format!(" (+{more} lines)") } else { String::new() };
            format!("  \u{23BF} {first}{tail}\n")
        }
        Body::Note { text } => format!("({text})\n"),
        Body::Error { text } => format!("error: {text}\n"),
        Body::TurnEnd { stop, duration_ms, .. } => {
            let secs = duration_ms.unwrap_or(0) as f64 / 1000.0;
            let what = if stop == "end_turn" { String::new() } else { format!("{stop} \u{00B7} ") };
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
    use serde_json::json;

    struct Quiet;
    impl Adapter for Quiet {
        fn events(&mut self, _: &str) -> Vec<Body> {
            Vec::new()
        }
        fn user(&mut self, _: &str, _: &[String]) -> Vec<String> {
            Vec::new()
        }
        fn interrupt(&mut self) -> Vec<String> {
            Vec::new()
        }
        fn choose(&mut self, _: Option<&str>, _: Option<&str>) -> Result<Vec<String>, String> {
            Ok(Vec::new())
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
    fn events() -> Vec<Body> {
        [
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
        .iter()
        .map(|v| Event::from_json(v).expect("a test event").body)
        .collect()
    }

    /// Every frame the broadcast carried since the last drain, concatenated: the live feed's bytes.
    fn drain(rx: &mut broadcast::Receiver<Ev>) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Ev::Event(b) = ev {
                out.extend_from_slice(&b);
            }
        }
        out
    }

    /// persisted-state-round-trips and deterministic-output: the log is the live feed, byte for byte (its magic, then
    /// exactly the frames the attached clients were sent); a chat restored from it (a fresh daemon's `Chat::open`, then
    /// the history a reattach sends) folds, through the one reducer, to exactly the state the live feed gave, each frame
    /// of that history is byte for byte a frame of the live feed, and a reattach to the live chat sends the same bytes as
    /// one to the restored chat. Resume appends to the same log, so a chat restored again after more events still
    /// matches.
    #[test]
    fn a_restored_chat_is_the_chat_it_was() {
        let d = dir("roundtrip");
        let (tx, mut rx) = broadcast::channel(1024);
        let chat = Chat::open("s1", &d, Box::new(Quiet)).unwrap();
        let mut live = Transcript::default();
        let mut feed = Vec::new();
        let check = |chat: &Chat, live: &Transcript, feed: &[u8]| {
            let log = std::fs::read(d.join("chat")).unwrap();
            assert_eq!(&log[log::MAGIC.len()..], feed, "the log is the live feed's bytes");
            let back = Chat::open("s1", &d, Box::new(Quiet)).unwrap();
            let (history, _) = back.history_and_subscribe(&tx);
            let live_frames: std::collections::HashSet<&[u8]> = log::read(&log).unwrap().records.iter().map(|r| r.frame).collect();
            assert!(history.iter().all(|f| live_frames.contains(&f[..])), "every replayed frame is a live frame as it was sent");
            let mut restored = Transcript::default();
            restored.feed(&history.concat());
            assert_eq!(&restored, live, "restored from the log vs fed live");
            assert_eq!(back.store.lock().transcript, *live, "the daemon's fold of the file");
            assert_eq!(chat.store.lock().transcript, *live, "the daemon's fold of the live events");
            let (again, _) = chat.history_and_subscribe(&tx);
            assert_eq!(history, again, "a reattach to the live chat sends the restore's history, byte for byte");
            back
        };
        let evs = events();
        let (first, second) = evs.split_at(18);
        for ev in first {
            chat.record(ev.clone(), &tx);
        }
        let got = drain(&mut rx);
        live.feed(&got);
        feed.extend_from_slice(&got);
        assert_eq!(live.items.len(), 5, "{:#?}", live.items);
        assert!(matches!(&live.items[2], Item::Text { text, .. } if text == "Counting."), "the text before the call it streamed beside");
        assert_eq!((live.title.as_str(), live.session_id.as_str(), live.busy), ("count the files", "9f2c", false));
        let back = check(&chat, &live, &feed);
        // Resumed: the restored chat takes the next events, appended to the same log.
        drop(chat);
        for ev in second {
            back.record(ev.clone(), &tx);
        }
        let got = drain(&mut rx);
        live.feed(&got);
        feed.extend_from_slice(&got);
        assert!(live.busy);
        check(&back, &live, &feed);
        // Without partials to replace, the replay is the live feed itself.
        let d2 = dir("whole");
        let (tx2, mut rx2) = broadcast::channel(1024);
        let whole = Chat::open("s2", &d2, Box::new(Quiet)).unwrap();
        for ev in evs.iter().filter(|e| !e.partial() && !matches!(e, Body::ToolDetail(_))) {
            whole.record(ev.clone(), &tx2);
        }
        let sent = drain(&mut rx2);
        let (history, _) = Chat::open("s2", &d2, Box::new(Quiet)).unwrap().history_and_subscribe(&tx2);
        assert_eq!(history.concat(), sent, "a replay of the log is the live feed, byte for byte");
        std::fs::remove_dir_all(&d).unwrap();
        std::fs::remove_dir_all(&d2).unwrap();
    }

    /// A crash mid-append leaves a torn last frame: a restore cuts it off and records an `error` naming it, keeping every
    /// whole event before it, so what is recorded next is reachable. A whole frame whose payload is not an event is
    /// reported in the history and skipped, and the records after it stand.
    #[test]
    fn a_torn_last_frame_is_cut_and_reported() {
        let d = dir("torn");
        let (tx, _rx) = broadcast::channel(1024);
        let chat = Chat::open("s1", &d, Box::new(Quiet)).unwrap();
        for ev in &events()[..3] {
            chat.record(ev.clone(), &tx);
        }
        drop(chat);
        let mut f = std::fs::OpenOptions::new().append(true).open(d.join("chat")).unwrap();
        // A whole frame that is not an event (CBOR 0xff is a break: never a value), then the next event, then half of one.
        f.write_all(&harness_proto::frame::frame(harness_proto::frame::T_EVENT, &[0xff])).unwrap();
        let next = events()[5].clone();
        f.write_all(&Event { ts: 3, body: next }.frame()).unwrap();
        let torn = Event { ts: 4, body: Body::Note { text: "never whole".into() } }.frame();
        f.write_all(&torn[..torn.len() - 3]).unwrap();
        drop(f);
        let back = Chat::open("s1", &d, Box::new(Quiet)).unwrap();
        let (history, _) = back.history_and_subscribe(&tx);
        let mut t = Transcript::default();
        t.feed(&history.concat());
        assert_eq!(t.title, "count the files");
        let errors: Vec<&str> = t.items.iter().filter_map(|i| if let Item::Error { text } = i { Some(text.as_str()) } else { None }).collect();
        assert_eq!(errors.len(), 2, "{:#?}", t.items);
        assert!(errors[0].starts_with("record 4 of this chat's log"), "{errors:?}");
        assert!(errors[1].contains("torn frame"), "{errors:?}");
        assert!(t.items.iter().any(|i| matches!(i, Item::Thinking { text, .. } if text == "list them")), "the event after the bad frame stands: {:#?}", t.items);
        back.record(Body::Note { text: "after the crash".into() }, &tx);
        drop(back);
        let bytes = std::fs::read(d.join("chat")).unwrap();
        let l = log::read(&bytes).unwrap();
        assert_eq!(l.torn, None, "the torn tail was cut off");
        let bad: Vec<usize> = l.records.iter().filter(|r| r.event.is_err()).map(|r| r.n).collect();
        assert_eq!(bad, [4], "only the bad frame is bad; the report and the next event are whole");
        assert!(matches!(&l.records.last().unwrap().event, Ok(Event { body: Body::Note { text }, .. }) if text == "after the crash"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// An NDJSON log from before schema 1 is converted at start: every event, in order, then marked so; a second start
    /// leaves it alone, and `Chat::open` reads it. `Chat::open` refuses an unconverted one, naming it.
    #[test]
    fn an_ndjson_log_is_converted_once() {
        let d = dir("ndjson");
        let lines: String = events().iter().map(|b| format!("{}\n", serde_json::to_string(&Event { ts: 7, body: b.clone() }).unwrap())).collect();
        std::fs::write(d.join("chat"), &lines).unwrap();
        let err = Chat::open("s1", &d, Box::new(Quiet)).err().expect("an NDJSON log is refused");
        assert!(format!("{err:#}").contains("NDJSON"), "{err:#}");
        let steps = migrate::plan(false, Some(lines.as_bytes())).unwrap();
        assert_eq!(steps.len(), 5);
        std::fs::write(d.join("chat"), migrate::convert(lines.as_bytes())).unwrap();
        let back = Chat::open("s1", &d, Box::new(Quiet)).unwrap();
        assert_eq!(back.title(), "count the files");
        let bytes = std::fs::read(d.join("chat")).unwrap();
        assert_eq!(log::read(&bytes).unwrap().records.len(), events().len());
        assert_eq!(migrate::plan(false, Some(&bytes)).unwrap(), [migrate::Step::Mark], "a converted log is only marked");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
