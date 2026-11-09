//! A chat session's state from its agent-neutral events (harness-proto's `event`; harness/src/chat/mod.rs has the table of
//! what each means). This is the one reducer of a chat's events: the daemon folds every event it records through it (the
//! chat's title, model, busy state and the agent's own session id for a resume), the daemon's restore of a past chat folds
//! the log file through it, and the app folds the events an attach sends through it to draw the transcript.
//!
//! Partials (`block_start`, `delta`, `tool_start`, `tool_delta`) grow an item, and the final (`block`, `tool_call`)
//! replaces what they grew. A `tool_detail` (what a harness REPL cell a tool call ran did: its steps and value) is kept by
//! its call's block beside the items, whole each time, whichever comes first. Nothing here knows which agent wrote it.
//!
//! [`Transcript::feed`] takes the bytes of a chat attach stream (its event frames, harness-proto's `T_EVENT`) in any
//! pieces; [`Transcript::apply`] one typed event.
pub mod group;

use std::collections::HashMap;
use std::sync::Arc;

use harness_proto::event::{self as ev, Body, Event, Kind};
use harness_proto::frame;
use serde_json::Value;

/// One thing the transcript shows, in the order it happened.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// What the person sent: their words and the pictures with them (absolute paths).
    User { text: String, images: Vec<String> },
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
    /// The cell, `<repl>.<task>`: what an event from the UI it draws names (`Request::UiEvent`).
    pub cell: String,
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
    /// When it started (ms since the epoch), when the kernel said.
    pub started_ms: Option<u64>,
    pub ms: Option<u64>,
    pub detail: String,
    pub value: Arc<Value>,
}

impl Detail {
    fn of(d: &ev::Detail) -> Self {
        Self {
            cell: d.cell.clone(),
            status: d.status.clone(),
            ms: d.ms,
            code: d.code.clone(),
            effects: d
                .effects
                .iter()
                .map(|e| Effect {
                    kind: e.kind.clone(),
                    doing: e.doing.clone(),
                    done: e.done.clone(),
                    target: e.target.clone(),
                    status: e.status.clone(),
                    started_ms: e.started_ms,
                    ms: e.ms,
                    detail: e.detail.clone(),
                    value: Arc::new(e.value.clone()),
                })
                .collect(),
            dropped: d.effects_dropped,
            value: Arc::new(d.value.clone()),
            ui: Arc::new(d.ui.clone()),
            logs: d.logs.clone(),
        }
    }
}

/// A session's transcript and what the status line shows.
#[derive(Debug, Default)]
pub struct Transcript {
    pub items: Vec<Item>,
    /// A turn is in flight.
    pub busy: bool,
    /// When the turn in flight began, and when its newest item (a block, a call, a message) began (ms since the epoch,
    /// the events' own stamps): how long it and its current step have run. None outside a turn.
    pub turn_since: Option<u64>,
    pub step_since: Option<u64>,
    pub agent: String,
    pub model: String,
    /// The reasoning effort it runs with; empty when its `session` event did not say.
    pub effort: String,
    /// The agent's own id for the conversation (Claude's session id, Codex's thread id): what resumes it. Empty when the
    /// log never said (a chat recorded before `session` events carried it).
    pub session_id: String,
    /// The models the agent offers (its latest `models` event); empty when it never listed them.
    pub models: Vec<ev::ModelChoice>,
    /// The person's latest choice of model (a [`ev::ModelChoice::id`]) and of effort (`choice` events); empty when they
    /// never chose. What a resume starts the agent with.
    pub chosen_model: String,
    pub chosen_effort: String,
    /// How much of the model's context window the conversation fills, as the agent last said (its `context` event: the
    /// tokens, the window (0: unknown), the last call's split and the parts); None before it said.
    pub context: Option<ev::ContextUse>,
    /// How much of the person's plan is used, window by window, as the agent last said in this chat (its latest `plan`
    /// event), and when it said it (the event's stamp, ms since the epoch): the account's, so the app shows the latest of
    /// any chat of the same agent kind. Empty before it said.
    pub plan: Vec<ev::PlanWindow>,
    pub plan_at: u64,
    /// What the conversation has cost so far (the sum of its turns' `cost_usd`); None while no turn said.
    pub cost_usd: Option<f64>,
    /// The first user message's first line, cut to [`TITLE_CHARS`].
    pub title: String,
    /// Moves on every change (what the view redraws on).
    pub rev: u64,
    /// Each tool call's REPL cell, by the call's block.
    pub details: HashMap<String, Detail>,
    /// When each tool call began (its first event's `ts`, ms since the epoch), by its block.
    pub started: HashMap<String, u64>,
    /// The person's messages sent while a turn was in flight that the agent has not taken yet (their items' indices,
    /// oldest first): Claude takes what waits with its next tool result, or as its turn ends; Codex one a turn, as the
    /// turn before it ends (the daemon's adapter queues them).
    pub queued: Vec<usize>,
    /// The item each block id is, for its deltas and its final form.
    at: HashMap<String, usize>,
    /// Bytes of a frame not finished yet.
    buf: Vec<u8>,
}

/// Two transcripts are equal when they hold the same conversation: the redraw counter, a part frame not finished yet and
/// when things began (a reattach replays a call's whole event in place of its partials, so its stamps differ) are
/// bookkeeping, not state.
impl PartialEq for Transcript {
    fn eq(&self, o: &Self) -> bool {
        (&self.models, &self.chosen_model, &self.chosen_effort, &self.context, &self.plan, self.plan_at, self.cost_usd)
            == (&o.models, &o.chosen_model, &o.chosen_effort, &o.context, &o.plan, o.plan_at, o.cost_usd)
            && (&self.items, self.busy, &self.agent, &self.model, &self.effort, &self.session_id, &self.title, &self.details, &self.at)
                == (&o.items, o.busy, &o.agent, &o.model, &o.effort, &o.session_id, &o.title, &o.details, &o.at)
            && self.queued == o.queued
    }
}

/// How many characters of the first user line a title keeps.
pub const TITLE_CHARS: usize = 60;

/// A chat's title from its first user message: its first line with words on it (a heading's or a quote's marks taken
/// off), its whitespace folded to single spaces, cut at the last word that fits in [`TITLE_CHARS`] with an ellipsis (a
/// first word longer than that is cut inside it), and no comma or colon left before the ellipsis.
#[must_use]
pub fn title_of(text: &str) -> String {
    let first = text
        .lines()
        .map(|l| l.trim().trim_start_matches(['#', '>']).trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut s = String::new();
    let mut n = 0;
    for word in first.split_whitespace() {
        let len = word.chars().count();
        let gap = usize::from(n > 0);
        if n + gap + len > TITLE_CHARS {
            if n == 0 {
                s.extend(word.chars().take(TITLE_CHARS));
            }
            let kept = s.trim_end_matches([',', ';', ':', '-', '.', ' ']).len();
            s.truncate(kept);
            s.push('\u{2026}');
            return s;
        }
        if gap == 1 {
            s.push(' ');
        }
        s.push_str(word);
        n += gap + len;
    }
    s
}

impl Transcript {
    /// Whether item `n` is a message of the person's the agent has not taken yet ([`Transcript::queued`]).
    #[must_use]
    pub fn is_queued(&self, n: usize) -> bool {
        self.queued.contains(&n)
    }

    /// The name of tool call `block`, once its call came.
    #[must_use]
    pub fn tool_name(&self, block: &str) -> Option<&str> {
        match self.items.get(*self.at.get(block)?)? {
            Item::Tool { name, .. } => Some(name),
            _ => None,
        }
    }

    /// The model and effort chat was spawned on (cmd+N's remembered choice), as its choice until its log says one: what
    /// its picker shows from its first frame, before its agent is up to say what it runs (Claude names its model only in
    /// its `init`, and its log has no `choice` for a launch's `--model`). A `choice` in the log replaces it; a `session`
    /// event leaves it, as it leaves any choice.
    pub fn start_on(&mut self, model: Option<&str>, effort: Option<&str>) {
        self.rev = self.rev.wrapping_add(1);
        if let Some(m) = model.filter(|m| self.chosen_model.is_empty() && !m.is_empty()) {
            m.clone_into(&mut self.chosen_model);
        }
        if let Some(e) = effort.filter(|e| self.chosen_effort.is_empty() && !e.is_empty()) {
            e.clone_into(&mut self.chosen_effort);
        }
    }

    /// Bytes of a chat attach stream: its whole event frames are applied, a part frame waits for the rest. A frame that
    /// is not an event (a schema bug, never a torn write: the socket carries whole frames) is skipped, and reported.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        let mut start = 0;
        loop {
            match frame::split(&self.buf[start..]) {
                Ok(Some((t, payload, took))) => {
                    if t == frame::T_EVENT {
                        match harness_proto::cbor::decode::<Event>(payload) {
                            Ok(e) => self.apply(&e),
                            Err(e) => eprintln!("harness-chat: an event frame that is not an event: {e}"),
                        }
                    }
                    start += took;
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("harness-chat: {e}; the rest of the stream is dropped");
                    start = self.buf.len();
                    break;
                }
            }
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
    pub fn apply(&mut self, e: &Event) {
        self.rev = self.rev.wrapping_add(1);
        let had = self.items.len();
        self.apply_body(e);
        if self.items.len() != had {
            self.step_since = Some(e.ts);
        }
    }

    fn apply_body(&mut self, e: &Event) {
        match &e.body {
            Body::Session { agent, model, session_id, effort } => {
                self.agent.clone_from(agent);
                self.model.clone_from(model);
                self.effort = effort.clone().unwrap_or_default();
                if let Some(id) = session_id.as_ref().filter(|id| !id.is_empty()) {
                    self.session_id.clone_from(id);
                }
            }
            Body::Models { models } => self.models.clone_from(models),
            Body::Choice { model, effort } => {
                // Shown at once (the next turn runs with it); the agent's `session` event confirms it, naming the model
                // as it runs.
                if let Some(m) = model {
                    self.chosen_model.clone_from(m);
                    self.model = self.models.iter().find(|c| c.is(m)).map_or_else(|| m.clone(), |c| if c.model.is_empty() { c.id.clone() } else { c.model.clone() });
                }
                if let Some(e) = effort {
                    self.chosen_effort.clone_from(e);
                    self.effort.clone_from(e);
                }
            }
            Body::Context(c) => self.context = Some(c.clone()),
            Body::Plan { windows } => {
                self.plan.clone_from(windows);
                self.plan_at = e.ts;
            }
            Body::User { text, images } => {
                if self.title.is_empty() {
                    self.title = title_of(text);
                }
                // Sent while a turn runs, it waits until the agent takes it.
                if self.busy {
                    self.queued.push(self.items.len());
                }
                self.push("", Item::User { text: text.clone(), images: images.clone() });
            }
            Body::Busy { busy } => {
                if *busy && !self.busy {
                    self.turn_since = Some(e.ts);
                } else if !*busy {
                    (self.turn_since, self.step_since) = (None, None);
                }
                self.busy = *busy;
            }
            Body::BlockStart { block, kind } => {
                let item = match kind {
                    Kind::Thinking => Item::Thinking { block: block.clone(), text: String::new(), done: false },
                    Kind::Text => Item::Text { block: block.clone(), text: String::new(), done: false },
                };
                self.push(block, item);
            }
            Body::Delta { block, text: more } => {
                if let Some(Item::Text { text, done: false, .. } | Item::Thinking { text, done: false, .. }) = self.get(block) {
                    text.push_str(more);
                }
            }
            Body::Block { block, kind, text } => {
                let item = match kind {
                    Kind::Thinking => Item::Thinking { block: block.clone(), text: text.clone(), done: true },
                    Kind::Text => Item::Text { block: block.clone(), text: text.clone(), done: true },
                };
                match self.at.get(block).copied() {
                    Some(i) => self.items[i] = item,
                    None => self.push(block, item),
                }
            }
            Body::ToolStart { block, name } => {
                self.started.entry(block.clone()).or_insert(e.ts);
                let item = Item::Tool {
                    block: block.clone(),
                    name: name.clone(),
                    input: Value::Null,
                    partial: String::new(),
                    result: None,
                    done: false,
                };
                self.push(block, item);
            }
            Body::ToolDelta { block, json } => {
                if let Some(Item::Tool { partial, done: false, .. }) = self.get(block) {
                    partial.push_str(json);
                }
            }
            Body::ToolCall { block, name, input } => match {
                self.started.entry(block.clone()).or_insert(e.ts);
                self.get(block)
            } {
                Some(Item::Tool { name: n, input: i, partial, done, .. }) => {
                    n.clone_from(name);
                    i.clone_from(input);
                    // The whole call replaces its streamed arguments (a replay that left them out holds the same).
                    partial.clear();
                    *done = true;
                }
                _ => self.push(
                    block,
                    Item::Tool {
                        block: block.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        partial: String::new(),
                        result: None,
                        done: true,
                    },
                ),
            },
            Body::ToolResult { block, text, error } => {
                let r = ToolResult { text: text.clone(), error: *error };
                match self.get(block) {
                    Some(Item::Tool { result, .. }) => *result = Some(r),
                    _ => self.push("", Item::Note { text: r.text }),
                }
                // Claude hands what the person sent meanwhile to the model with a tool's result.
                if self.agent == "claude" {
                    self.queued.clear();
                }
            }
            Body::ToolDetail(d) => {
                self.details.insert(d.block.clone(), Detail::of(d));
            }
            Body::Note { text } => self.push("", Item::Note { text: text.clone() }),
            Body::Error { text } => self.push("", Item::Error { text: text.clone() }),
            Body::TurnEnd { ok, stop, duration_ms, cost_usd, .. } => {
                if let Some(c) = cost_usd {
                    self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + c);
                }
                // A turn's partials that never came whole (the agent died mid-block) stop streaming.
                for it in &mut self.items {
                    match it {
                        Item::Text { done, .. } | Item::Thinking { done, .. } | Item::Tool { done, .. } => *done = true,
                        _ => {}
                    }
                }
                self.push(
                    "",
                    Item::TurnEnd { ok: *ok, stop: stop.clone(), duration_ms: *duration_ms, cost_usd: *cost_usd },
                );
                // What waited is taken now: all of it by Claude, the next one by Codex (one a turn).
                if self.agent == "codex" {
                    if !self.queued.is_empty() {
                        self.queued.remove(0);
                    }
                } else {
                    self.queued.clear();
                }
            }
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

    /// A title is the first line with words, its spaces folded, cut between words with an ellipsis and never mid-word
    /// (but a single word too long for it), no comma left hanging before the ellipsis.
    #[test]
    fn a_title_is_cut_between_words() {
        assert_eq!(title_of("\n\n  ## fix   the\tbuild  \nmore"), "fix the build");
        let long = "please look at why the sidebar rows read claude and a number, then make them say what they are";
        let t = title_of(long);
        assert_eq!(t, "please look at why the sidebar rows read claude and a\u{2026}");
        assert!(t.chars().count() <= TITLE_CHARS + 1 && long.starts_with(t.trim_end_matches('\u{2026}')));
        assert_eq!(title_of("one two three four five six seven eight nine ten eleven, twelve thirteen"), "one two three four five six seven eight nine ten eleven\u{2026}");
        let word = "x".repeat(TITLE_CHARS + 5);
        assert_eq!(title_of(&word), format!("{}\u{2026}", "x".repeat(TITLE_CHARS)));
        assert_eq!(title_of("   "), "");
    }

    #[test]
    fn partials_grow_and_the_final_replaces_them() {
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
        let all: Vec<u8> = lines
            .iter()
            .flat_map(|l| Event::from_json(&serde_json::from_str(l).unwrap()).unwrap().frame())
            .collect();
        // Fed in two pieces cut mid-frame.
        let mut t = Transcript::default();
        let (a, b) = all.split_at(57);
        t.feed(a);
        t.feed(b);
        assert_eq!(t.items.len(), 4);
        assert_eq!(t.items[1], Item::Text { block: "m:0".into(), text: "Hello!".into(), done: true });
        assert!(matches!(&t.items[2], Item::Tool { result: Some(r), done: true, .. } if r.text == "2"));
        assert!(!t.busy);
        assert_eq!(t.details["t1"].effects[0].done, "Read a");
        assert_eq!(main_arg(&serde_json::json!({"code": "1 +\n 1"})), "1 + 1");
    }

    fn fed(lines: &[&str]) -> Transcript {
        let mut t = Transcript::default();
        for l in lines {
            t.apply(&Event::from_json(&serde_json::from_str(l).unwrap()).unwrap());
        }
        t
    }

    /// A message sent mid-turn waits until the agent takes it: Claude with its next tool result, Codex as the turn ends;
    /// when the turn and each call began is kept.
    #[test]
    fn mid_turn_messages_wait_until_taken() {
        let start = [
            r#"{"ev":"session","agent":"claude","model":"m"}"#,
            r#"{"ev":"user","text":"go","ts":10}"#,
            r#"{"ev":"busy","busy":true,"ts":11}"#,
            r#"{"ev":"tool_start","block":"t1","name":"ts","ts":20}"#,
            r#"{"ev":"tool_call","block":"t1","name":"ts","input":{},"ts":25}"#,
            r#"{"ev":"user","text":"also this","ts":30}"#,
            r#"{"ev":"busy","busy":true,"ts":31}"#,
        ];
        let t = fed(&start);
        assert_eq!((t.queued.clone(), t.turn_since, t.started["t1"]), (vec![2], Some(11), 20));
        assert!(t.is_queued(2) && !t.is_queued(0));
        let t = fed(&[&start[..], &[r#"{"ev":"tool_result","block":"t1","text":"ok","error":false}"#]].concat());
        assert!(t.queued.is_empty(), "claude takes it with the tool's result");
        let codex = start.map(|l| l.replace("claude", "codex"));
        let codex: Vec<&str> = codex.iter().map(String::as_str).collect();
        let t = fed(&[&codex[..], &[r#"{"ev":"tool_result","block":"t1","text":"ok","error":false}"#]].concat());
        assert_eq!(t.queued, vec![2], "codex takes it only as the turn ends");
        let t = fed(&[&codex[..], &[r#"{"ev":"turn_end","ok":true,"stop":"end_turn"}"#, r#"{"ev":"busy","busy":false}"#]].concat());
        assert!(t.queued.is_empty() && t.turn_since.is_none());
    }
}
