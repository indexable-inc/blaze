//! A chat session's state from its agent-neutral event log (harness/src/chat/mod.rs has the event model): one JSON object
//! a line, `ev` naming it. This is the one reducer of that log: the daemon folds every event it records through it (the
//! chat's title, model, busy state and the agent's own session id for a resume), the daemon's restore of a past chat folds
//! the log file through it, and the app folds the events an attach sends through it to draw the transcript.
//!
//! Partials (`block_start`, `delta`, `tool_start`, `tool_delta`) grow an item, and the final (`block`, `tool_call`)
//! replaces what they grew. A `tool_detail` (what a harness REPL cell a tool call ran did: its steps and value) is kept by
//! its call's block beside the items, whole each time, whichever comes first. Nothing here knows which agent wrote it.
//!
//! [`lines`] splits a log's bytes into its events, reporting a line that is not whole JSON (the torn last line a crash
//! mid-append leaves) instead of dropping the log.
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

/// One thing the transcript shows, in the order it happened.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// What the person sent.
    User { text: String },
    /// The agent's words (markdown), streaming until `done`.
    Text { block: String, text: String, done: bool },
    /// The agent's reasoning, streaming until `done`.
    Thinking { block: String, text: String, done: bool },
    /// A tool call: its name, its input (JSON; partial text while it streams) and, once it came, its result.
    Tool { block: String, name: String, input: Value, partial: String, result: Option<ToolResult>, done: bool },
    /// Something said about the conversation (it was interrupted).
    Note { text: String },
    /// The agent failed.
    Error { text: String },
    /// A turn ended: how (`end_turn`, `interrupted`, `error`), how long it took and what it cost.
    TurnEnd { ok: bool, stop: String, duration_ms: Option<u64>, cost_usd: Option<f64> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    pub text: String,
    pub error: bool,
}

/// What a harness REPL cell a tool call ran did (`tool_detail`, harness/src/chat/mod.rs): its steps outside the REPL and
/// its value. Values (`value`, a step's `value`) stay JSON in the kernel's shape (mcp/src/shown.ts): `{t: table | record |
/// list, cols, rows, total}`, `{t: scalar, type, v}`, `{t: text, v, total}`, `{t: error, message, stack}`, or null; shared,
/// so a row that draws one holds it without a copy.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Detail {
    /// `running`, `done` or `error`.
    pub status: String,
    pub ms: Option<u64>,
    pub code: String,
    pub effects: Vec<Effect>,
    /// Steps past the kernel's cap, not kept.
    pub dropped: u64,
    pub value: Arc<Value>,
    /// What the cell draws (Svelte markup in the kernel, `mcp/src/ui/`): a `{type, props, children}` tree or a list of
    /// them, or null.
    pub ui: Arc<Value>,
    pub logs: String,
}

/// One step a cell took outside the REPL (mcp/src/effects.ts).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effect {
    /// `exec`, `read`, `write`, `delete`, `net`, `search`, `spawn` or `sql`.
    pub kind: String,
    /// Its words while it runs and once it is done.
    pub doing: String,
    pub done: String,
    pub target: String,
    /// `running`, `ok` or `error`.
    pub status: String,
    pub ms: Option<u64>,
    pub detail: String,
    pub value: Arc<Value>,
}

impl Detail {
    fn of(v: &Value) -> Self {
        let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_owned();
        let effects = v["effects"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| Effect {
                kind: s(e, "kind"),
                doing: s(e, "doing"),
                done: s(e, "done"),
                target: s(e, "target"),
                status: s(e, "status"),
                ms: e["ms"].as_u64(),
                detail: s(e, "detail"),
                value: Arc::new(e["value"].clone()),
            })
            .collect();
        Self {
            status: s(v, "status"),
            ms: v["ms"].as_u64(),
            code: s(v, "code"),
            effects,
            dropped: v["effects_dropped"].as_u64().unwrap_or(0),
            value: Arc::new(v["value"].clone()),
            ui: Arc::new(v["ui"].clone()),
            logs: s(v, "logs"),
        }
    }
}

/// A session's transcript and what the status line shows.
#[derive(Debug, Default)]
pub struct Transcript {
    pub items: Vec<Item>,
    /// A turn is in flight.
    pub busy: bool,
    pub agent: String,
    pub model: String,
    /// The agent's own id for the conversation (Claude's session id, Codex's thread id): what resumes it. Empty when the
    /// log never said (a chat recorded before `session` events carried it).
    pub session_id: String,
    /// The first user message's first line, cut to [`TITLE_CHARS`].
    pub title: String,
    /// Moves on every change (what the view redraws on).
    pub rev: u64,
    /// Each tool call's REPL cell, by the call's block.
    pub details: HashMap<String, Detail>,
    /// The item each block id is, for its deltas and its final form.
    at: HashMap<String, usize>,
    /// Bytes of a line not finished yet.
    buf: Vec<u8>,
}

/// Two transcripts are equal when they hold the same conversation: the redraw counter and a part line not finished yet
/// are bookkeeping, not state.
impl PartialEq for Transcript {
    fn eq(&self, o: &Self) -> bool {
        (&self.items, self.busy, &self.agent, &self.model, &self.session_id, &self.title, &self.details, &self.at)
            == (&o.items, o.busy, &o.agent, &o.model, &o.session_id, &o.title, &o.details, &o.at)
    }
}

/// How many characters of the first user line a title keeps.
pub const TITLE_CHARS: usize = 60;

/// A chat's title from its first user message: the first non-blank line, cut to [`TITLE_CHARS`] with an ellipsis.
#[must_use]
pub fn title_of(text: &str) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut s: String = first.chars().take(TITLE_CHARS).collect();
    if first.chars().count() > TITLE_CHARS {
        s.push('\u{2026}');
    }
    s
}

/// One line of a chat log: its number (from 1), its bytes as written, and its event (None when it is not whole JSON
/// naming an `ev`: the torn last line a crash mid-append leaves).
pub type Line<'a> = (usize, &'a [u8], Option<Value>);

/// A chat log's lines (split on `\n`, a trailing piece without one counted as a line), each parsed, blank lines left
/// out. A line that is not an event comes back with None, for the reader to report and skip; the rest of the log stands.
pub fn lines(bytes: &[u8]) -> impl Iterator<Item = Line<'_>> {
    bytes.split(|&b| b == b'\n').enumerate().filter(|(_, l)| !l.iter().all(u8::is_ascii_whitespace)).map(|(i, l)| {
        (i + 1, l, serde_json::from_slice::<Value>(l).ok().filter(|v| v.get("ev").is_some_and(Value::is_string)))
    })
}

impl Transcript {
    /// The name of tool call `block`, once its call came.
    #[must_use]
    pub fn tool_name(&self, block: &str) -> Option<&str> {
        match self.items.get(*self.at.get(block)?)? {
            Item::Tool { name, .. } => Some(name),
            _ => None,
        }
    }

    /// Bytes of the attach stream: whole lines are applied, a part line waits for the rest.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        let mut start = 0;
        while let Some(n) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let line = &self.buf[start..start + n];
            if let Ok(v) = serde_json::from_slice::<Value>(line) {
                self.apply(&v);
            }
            start += n + 1;
        }
        self.buf.drain(..start);
    }

    fn push(&mut self, block: &str, item: Item) {
        if !block.is_empty() {
            self.at.insert(block.to_owned(), self.items.len());
        }
        self.items.push(item);
    }

    fn get(&mut self, block: &str) -> Option<&mut Item> {
        let i = *self.at.get(block)?;
        self.items.get_mut(i)
    }

    /// One event.
    pub fn apply(&mut self, v: &Value) {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
        let block = s("block");
        self.rev = self.rev.wrapping_add(1);
        match v["ev"].as_str().unwrap_or("") {
            "session" => {
                self.agent = s("agent");
                self.model = s("model");
                if let Some(id) = v["session_id"].as_str().filter(|id| !id.is_empty()) {
                    self.session_id = id.to_owned();
                }
            }
            "user" => {
                if self.title.is_empty() {
                    self.title = title_of(v["text"].as_str().unwrap_or(""));
                }
                self.push("", Item::User { text: s("text") });
            }
            "busy" => self.busy = v["busy"].as_bool().unwrap_or(false),
            "block_start" => {
                let item = if v["kind"] == "thinking" {
                    Item::Thinking { block: block.clone(), text: String::new(), done: false }
                } else {
                    Item::Text { block: block.clone(), text: String::new(), done: false }
                };
                self.push(&block, item);
            }
            "delta" => match self.get(&block) {
                Some(Item::Text { text, done: false, .. } | Item::Thinking { text, done: false, .. }) => {
                    text.push_str(v["text"].as_str().unwrap_or(""));
                }
                _ => {}
            },
            "block" => {
                let (kind, text) = (s("kind"), s("text"));
                let item = if kind == "thinking" {
                    Item::Thinking { block: block.clone(), text, done: true }
                } else {
                    Item::Text { block: block.clone(), text, done: true }
                };
                match self.at.get(&block).copied() {
                    Some(i) => self.items[i] = item,
                    None => self.push(&block, item),
                }
            }
            "tool_start" => {
                let item = Item::Tool {
                    block: block.clone(),
                    name: s("name"),
                    input: Value::Null,
                    partial: String::new(),
                    result: None,
                    done: false,
                };
                self.push(&block, item);
            }
            "tool_delta" => {
                if let Some(Item::Tool { partial, done: false, .. }) = self.get(&block) {
                    partial.push_str(v["json"].as_str().unwrap_or(""));
                }
            }
            "tool_call" => {
                let (name, input) = (s("name"), v["input"].clone());
                match self.get(&block) {
                    Some(Item::Tool { name: n, input: i, partial, done, .. }) => {
                        *n = name;
                        *i = input;
                        // The whole call replaces its streamed arguments (a replay that left them out holds the same).
                        partial.clear();
                        *done = true;
                    }
                    _ => self.push(
                        &block,
                        Item::Tool { block: block.clone(), name, input, partial: String::new(), result: None, done: true },
                    ),
                }
            }
            "tool_result" => {
                let r = ToolResult { text: s("text"), error: v["error"].as_bool().unwrap_or(false) };
                match self.get(&block) {
                    Some(Item::Tool { result, .. }) => *result = Some(r),
                    _ => self.push("", Item::Note { text: r.text }),
                }
            }
            "tool_detail" => {
                self.details.insert(block, Detail::of(v));
            }
            "note" => self.push("", Item::Note { text: s("text") }),
            "error" => self.push("", Item::Error { text: s("text") }),
            "turn_end" => {
                // A turn's partials that never came whole (the agent died mid-block) stop streaming.
                for it in &mut self.items {
                    match it {
                        Item::Text { done, .. } | Item::Thinking { done, .. } | Item::Tool { done, .. } => *done = true,
                        _ => {}
                    }
                }
                self.push(
                    "",
                    Item::TurnEnd {
                        ok: v["ok"].as_bool().unwrap_or(false),
                        stop: s("stop"),
                        duration_ms: v["duration_ms"].as_u64(),
                        cost_usd: v["cost_usd"].as_f64(),
                    },
                );
            }
            _ => self.rev = self.rev.wrapping_sub(1),
        }
    }
}

/// A tool call's main argument, as one line: what a person reads first (a command, a path, the code's first line).
#[must_use]
pub fn main_arg(input: &Value) -> String {
    let first = ["command", "code", "file_path", "path", "pattern", "query", "url", "prompt", "description"]
        .iter()
        .find_map(|k| input[*k].as_str())
        .map(str::to_owned)
        .or_else(|| input.as_object().and_then(|o| o.values().find_map(|v| v.as_str().map(str::to_owned))))
        .unwrap_or_default();
    first.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partials_grow_and_the_final_replaces_them() {
        let mut t = Transcript::default();
        let lines = [
            r#"{"ev":"user","text":"hi"}"#,
            r#"{"ev":"busy","busy":true}"#,
            r#"{"ev":"block_start","block":"m:0","kind":"text"}"#,
            r#"{"ev":"delta","block":"m:0","text":"Hel"}"#,
            r#"{"ev":"delta","block":"m:0","text":"lo"}"#,
            r#"{"ev":"tool_start","block":"t1","name":"ts"}"#,
            r#"{"ev":"block","block":"m:0","kind":"text","text":"Hello!"}"#,
            r#"{"ev":"tool_call","block":"t1","name":"ts","input":{"code":"1+1"}}"#,
            r#"{"ev":"tool_detail","block":"t1","status":"done","ms":3,"code":"1+1","effects":[{"kind":"read","done":"Read a","status":"ok"}],"value":{"t":"scalar","type":"number","v":2}}"#,
            r#"{"ev":"tool_result","block":"t1","text":"2","error":false}"#,
            r#"{"ev":"turn_end","ok":true,"stop":"end_turn","duration_ms":1200,"cost_usd":0.01}"#,
            r#"{"ev":"busy","busy":false}"#,
        ];
        let all = lines.join("\n") + "\n";
        // Fed in two pieces cut mid-line.
        let (a, b) = all.as_bytes().split_at(57);
        t.feed(a);
        t.feed(b);
        assert_eq!(t.items.len(), 4);
        assert_eq!(t.items[1], Item::Text { block: "m:0".into(), text: "Hello!".into(), done: true });
        assert!(matches!(&t.items[2], Item::Tool { result: Some(r), done: true, .. } if r.text == "2"));
        assert!(!t.busy);
        assert_eq!(t.details["t1"].effects[0].done, "Read a");
        assert_eq!(main_arg(&serde_json::json!({"code": "1 +\n 1"})), "1 + 1");
    }
}
