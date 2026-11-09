//! Codex's adapter: `codex app-server` (the JSON-RPC its IDE extensions speak, one object per line on stdin and stdout,
//! without the `jsonrpc` field) read into the event model of `chat/mod.rs`. The only place Codex's shapes are known; the
//! protocol is `codex app-server generate-ts` (openai/codex `codex-rs/app-server-protocol`, v2).
//!
//! The session: `initialize` (answered), the `initialized` notification, `thread/start` in the session's cwd (answered
//! with the thread and its model), or for a resumed chat `thread/resume` of its thread, or for an aside `thread/fork` of it
//! (ephemeral: the fork is never written, so it goes with its process) (each answered the same way); then each user
//! message is a `turn/start` on that thread. Messages sent before the
//! thread exists, or while a turn is in flight, wait here and go one turn each, in order (as Claude queues them).
//!
//! What Codex sends per turn: `turn/started`, then per item `item/started` / `item/completed` (an `agentMessage`, a
//! `reasoning`, an `mcpToolCall` whose arguments come whole at its start, ...), between them `item/agentMessage/delta`
//! and `item/reasoning/summaryTextDelta` / `textDelta` (with `summaryPartAdded` between summary parts),
//! `thread/tokenUsage/updated` (the thread's running total), `error` (with `willRetry`), and `turn/completed` with the
//! turn's `status` (`completed` | `interrupted` | `failed`), `error` and `durationMs`. An interrupt is `turn/interrupt`
//! on the turn in flight; the turn then completes as `interrupted`, its items left unfinished.
//!
//! The model and effort: `model/list` (every page, hidden models left out) as the session starts is the `models` event;
//! a choice rides on every `turn/start` from then on as its `model` and `effort` ("for this turn and subsequent turns",
//! the way den's den-codex sends them), and on a resume's `thread/resume` as its `model`. Codex says what the thread runs
//! with in `thread/settings/updated`, read as a `session` event; it sends none for a turn's own `model` and `effort`, so
//! a `turn/start` it accepts with them is that turn's `session` event.
//!
//! How much of the person's plan is used comes as `account/rateLimits/updated` (v2 `AccountRateLimitsUpdatedNotification`:
//! `rateLimits`, a `RateLimitSnapshot` whose `primary` and `secondary` are each a `RateLimitWindow` of `usedPercent` 0 to
//! 100, `windowDurationMins` and `resetsAt` in unix seconds, or null), read as the `plan` event.
//!
//! Codex asks the client too (server requests, each answered at once so a turn never hangs): approvals are approved
//! (the profile already runs with approval policy never and no sandbox, the TUI's yolo), questions for the user get no
//! answers, MCP elicitations are declined, and everything else is refused with a JSON-RPC error naming the method.
use super::context;
use harness_proto::event::{Body, ContextCall, ContextUse, Kind, ModelChoice, PlanWindow};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};

/// A message's `turn/start` input: each picture as a `localImage` (Codex reads the file), then the words.
fn input_items(text: &str, images: &[String]) -> Value {
    let mut items: Vec<Value> = images.iter().map(|p| json!({"type": "localImage", "path": p})).collect();
    if !text.is_empty() || items.is_empty() {
        items.push(json!({"type": "text", "text": text, "text_elements": []}));
    }
    Value::Array(items)
}

/// Notifications this client never reads, asked off at initialize so the raw record stays small.
const OPT_OUT: &[&str] = &[
    "turn/moderationMetadata",
    "account/updated",
    "thread/readState/changed",
    "thread/status/changed",
    "remoteControl/status/changed",
    "mcpServer/startupStatus/updated",
    "rawResponseItem/completed",
    "rawResponse/completed",
];

/// Which thread a session runs on: a new one, a resumed chat's own, or an aside's fork of a chat's (by Codex's id for it).
enum Begin {
    New,
    Resume(String),
    Fork(String),
}

/// What a request of ours was, so its answer can be read.
enum Pending {
    Initialize,
    /// `thread/start`, `thread/resume` or `thread/fork` (answered alike).
    ThreadStart,
    TurnStart,
    Interrupt,
    /// A page of `model/list`, after the models of the pages before it.
    ModelList(Vec<ModelChoice>),
}

/// How many pages of `model/list` a session reads before it calls the list whole.
const MODEL_PAGES: usize = 16;

/// A streaming text or thinking item: its kind, whether its `block_start` went out, and its text so far.
struct Open {
    kind: Kind,
    /// When it began, so unfinished blocks close in order.
    seq: u64,
    started: bool,
    text: String,
}

pub struct Codex {
    cwd: String,
    /// The thread it starts, resumes or forks.
    begin: Begin,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    thread: Option<String>,
    /// The thread could not start: messages are refused.
    dead: bool,
    /// A turn was asked for and has not completed.
    in_turn: bool,
    /// The turn in flight, once Codex started it (only then can it be interrupted).
    turn: Option<String>,
    /// An interrupt asked for before the turn started.
    interrupt_pending: bool,
    queue: VecDeque<(String, Vec<String>)>,
    out: Vec<String>,
    open: HashMap<String, Open>,
    seq: u64,
    /// Tool calls whose `tool_call` went out.
    called: HashSet<String>,
    last_text: String,
    /// The thread's token totals at the last turn's end and now (a turn's usage is the difference).
    total_before: Value,
    total: Value,
    /// An `error` event already said why this turn failed.
    error_said: bool,
    /// Pages of `model/list` read past the first.
    pages: usize,
    /// The model and the effort chosen for its turns ([`super::Adapter::choose`], or a resumed chat's last choice): sent
    /// on every `turn/start`.
    model: Option<String>,
    effort: Option<String>,
    /// The context the thread fills, its model's window and the last call's split, from the last
    /// `thread/tokenUsage/updated`.
    context: Option<(u64, u64, ContextCall)>,
    /// The messages that went by, counted by harness: a `context` event's parts (codex splits nothing).
    ledger: context::Ledger,
    /// The model and effort the thread runs with, as Codex last said (its thread start or resume, a settings update, a
    /// turn it took with a choice).
    runs: (String, Option<String>),
}

impl Codex {
    #[must_use]
    pub fn new(cwd: &str) -> Self {
        Self::with_thread(cwd, Begin::New)
    }

    /// A chat that resumes Codex's thread `thread` (its id from the chat's log).
    #[must_use]
    pub fn resume(cwd: &str, thread: &str) -> Self {
        Self::with_thread(cwd, Begin::Resume(thread.to_owned()))
    }

    /// An aside on Codex's thread `thread` (a chat's, from its log): an ephemeral fork of it, which takes the aside's one
    /// turn and is never written, so the chat's own thread is untouched.
    #[must_use]
    pub fn fork(cwd: &str, thread: &str) -> Self {
        Self::with_thread(cwd, Begin::Fork(thread.to_owned()))
    }

    fn with_thread(cwd: &str, begin: Begin) -> Self {
        Self {
            cwd: cwd.to_owned(),
            begin,
            next_id: 0,
            pending: HashMap::new(),
            thread: None,
            dead: false,
            in_turn: false,
            turn: None,
            interrupt_pending: false,
            queue: VecDeque::new(),
            out: Vec::new(),
            open: HashMap::new(),
            seq: 0,
            called: HashSet::new(),
            last_text: String::new(),
            total_before: Value::Null,
            total: Value::Null,
            error_said: false,
            pages: 0,
            model: None,
            effort: None,
            context: None,
            ledger: context::Ledger::default(),
            runs: (String::new(), None),
        }
    }

    /// This adapter with a chat's last choice of model and effort (a resume), sent on its turns.
    #[must_use]
    pub fn with_choice(mut self, (model, effort): (Option<String>, Option<String>)) -> Self {
        self.model = model;
        self.effort = effort;
        self
    }

    fn request(&mut self, method: &str, params: Value, what: Pending) -> String {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, what);
        json!({"id": id, "method": method, "params": params}).to_string()
    }

    fn turn_start(&mut self, text: &str, images: &[String]) -> String {
        self.in_turn = true;
        self.turn = None;
        let thread = self.thread.clone().unwrap_or_default();
        let mut params = json!({
            "threadId": thread,
            "input": input_items(text, images),
            "summary": "auto",
        });
        if let Some(m) = &self.model {
            params["model"] = json!(m);
        }
        if let Some(e) = &self.effort {
            params["effort"] = json!(e);
        }
        self.request("turn/start", params, Pending::TurnStart)
    }

    /// Start the next waiting message's turn, when the thread can take one.
    fn pump(&mut self) {
        if self.thread.is_some() && !self.in_turn {
            if let Some((text, images)) = self.queue.pop_front() {
                let line = self.turn_start(&text, &images);
                self.out.push(line);
            }
        }
    }

    fn interrupt_line(&mut self) -> Option<String> {
        let (thread, turn) = (self.thread.clone()?, self.turn.clone()?);
        Some(self.request("turn/interrupt", json!({"threadId": thread, "turnId": turn}), Pending::Interrupt))
    }

    /// The turn started (`turn/started`): an interrupt asked for before it goes now.
    fn turn_started(&mut self, id: &str) {
        if self.in_turn && self.turn.is_none() {
            self.turn = Some(id.to_owned());
            if std::mem::take(&mut self.interrupt_pending) {
                if let Some(l) = self.interrupt_line() {
                    self.out.push(l);
                }
            }
        }
    }

    fn answer(&mut self, id: u64, v: &Value) -> Vec<Body> {
        let Some(what) = self.pending.remove(&id) else { return vec![] };
        let err = v.get("error").map(|e| e["message"].as_str().map_or_else(|| e.to_string(), str::to_owned));
        match what {
            Pending::Initialize => {
                if let Some(e) = err {
                    self.dead = true;
                    return self.refuse_waiting(&format!("codex app-server refused initialize: {e}"));
                }
                self.out.push(json!({"method": "initialized"}).to_string());
                // A fork answers one question and goes: the chat it forks keeps the models event, so it lists none.
                if !matches!(self.begin, Begin::Fork(_)) {
                    let list = self.request("model/list", json!({"includeHidden": false}), Pending::ModelList(Vec::new()));
                    self.out.push(list);
                }
                let mut params = json!({"cwd": self.cwd, "approvalPolicy": "never", "sandbox": "danger-full-access"});
                if let Some(m) = &self.model {
                    params["model"] = json!(m);
                }
                // The thread starts at the chosen effort (a config override; thread/start has no effort of its own), so
                // its answer's `reasoningEffort` already says it.
                if let Some(e) = &self.effort {
                    params["config"] = json!({"model_reasoning_effort": e});
                }
                let method = match &self.begin {
                    Begin::Resume(thread) => {
                        // Its turns are already in the chat's log: only the thread's state comes back.
                        params["threadId"] = json!(thread);
                        params["excludeTurns"] = json!(true);
                        "thread/resume"
                    }
                    Begin::Fork(thread) => {
                        // The v2 ThreadForkParams: loaded from disk by id, the history carried over but not sent back,
                        // and `ephemeral` so the fork is never persisted.
                        params["threadId"] = json!(thread);
                        params["excludeTurns"] = json!(true);
                        params["ephemeral"] = json!(true);
                        "thread/fork"
                    }
                    Begin::New => "thread/start",
                };
                let line = self.request(method, params, Pending::ThreadStart);
                self.out.push(line);
                vec![]
            }
            Pending::ThreadStart => {
                if let Some(e) = err {
                    self.dead = true;
                    let what = match &self.begin {
                        Begin::Resume(t) => format!("thread/resume of thread {t}"),
                        Begin::Fork(t) => format!("thread/fork of thread {t}"),
                        Begin::New => "thread/start".to_owned(),
                    };
                    return self.refuse_waiting(&format!("codex app-server refused {what}: {e}"));
                }
                let r = &v["result"];
                self.thread = r["thread"]["id"].as_str().map(str::to_owned);
                self.pump();
                let model = r["model"].as_str().or_else(|| r["thread"]["model"].as_str()).unwrap_or("");
                // The v2 ThreadStartResponse, ThreadResumeResponse and ThreadForkResponse all carry `reasoningEffort` (a string, or null
                // when the model has none): the effort the thread runs with.
                let effort = r["reasoningEffort"].as_str().filter(|e| !e.is_empty()).map(str::to_owned);
                self.runs = (model.to_owned(), effort.clone());
                vec![Body::Session {
                    agent: "codex".to_owned(),
                    model: model.to_owned(),
                    session_id: self.thread.clone().filter(|t| !t.is_empty()),
                    effort,
                }]
            }
            Pending::TurnStart => {
                if let Some(e) = err {
                    let why = vec![Body::Error { text: format!("codex app-server refused turn/start: {e}") }];
                    return self.turn_over(false, "error", None, why);
                }
                // The answer names the turn, but Codex refuses to interrupt it ("no active turn") until `turn/started`.
                // Codex sends no settings update for a turn's own model and effort: that it took the turn with them is
                // the confirmation.
                if self.model.is_none() && self.effort.is_none() {
                    return vec![];
                }
                if let Some(m) = &self.model {
                    self.runs.0.clone_from(m);
                }
                if self.effort.is_some() {
                    self.runs.1.clone_from(&self.effort);
                }
                vec![Body::Session { agent: "codex".to_owned(), model: self.runs.0.clone(), session_id: None, effort: self.runs.1.clone() }]
            }
            Pending::ModelList(mut got) => {
                if let Some(e) = err {
                    return vec![Body::Error { text: format!("codex app-server refused model/list, so its picker offers the models the last codex listed: {e}") }];
                }
                let r = &v["result"];
                got.extend(r["data"].as_array().into_iter().flatten().filter(|m| m["hidden"] != true).map(|m| {
                    let id = m["model"].as_str().or_else(|| m["id"].as_str()).unwrap_or("").to_owned();
                    ModelChoice {
                        model: id.clone(),
                        id,
                        name: m["displayName"].as_str().unwrap_or("").to_owned(),
                        about: m["description"].as_str().unwrap_or("").to_owned(),
                        efforts: m["supportedReasoningEfforts"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|e| e["reasoningEffort"].as_str())
                            .map(str::to_owned)
                            .collect(),
                        effort: m["defaultReasoningEffort"].as_str().map(str::to_owned),
                    }
                }));
                match r["nextCursor"].as_str() {
                    Some(c) if self.pages < MODEL_PAGES => {
                        self.pages += 1;
                        let line = self.request("model/list", json!({"includeHidden": false, "cursor": c}), Pending::ModelList(got));
                        self.out.push(line);
                        vec![]
                    }
                    Some(_) => vec![Body::Error { text: format!("codex's model/list went past {MODEL_PAGES} pages; its models are left unlisted, so its picker offers the models the last codex listed") }],
                    None => vec![Body::Models { models: got }],
                }
            }
            Pending::Interrupt => match err {
                Some(e) => vec![Body::Error { text: format!("codex app-server refused turn/interrupt: {e}") }],
                None => vec![],
            },
        }
    }

    /// The session can't start: every waiting message's turn ends in an error.
    fn refuse_waiting(&mut self, why: &str) -> Vec<Body> {
        let mut out = vec![Body::Error { text: why.to_owned() }];
        for _ in std::mem::take(&mut self.queue) {
            out.push(turn_end(false, "error", None, Value::Null, String::new()));
        }
        out.push(Body::Busy { busy: false });
        out
    }

    /// The turn in flight is over: unfinished blocks closed as they stand, then `why` (an interrupt's note, an error),
    /// its `turn_end`, then the next waiting turn.
    fn turn_over(&mut self, ok: bool, stop: &str, duration_ms: Option<u64>, why: Vec<Body>) -> Vec<Body> {
        let mut out = Vec::new();
        let mut open: Vec<(String, Open)> = self.open.drain().filter(|(_, o)| o.started).collect();
        open.sort_by_key(|(_, o)| o.seq);
        for (block, o) in open {
            out.push(Body::Block { block, kind: o.kind, text: o.text });
        }
        out.extend(why);
        // Its parts are the ledger's as of this place in the events (Adapter::events fills them).
        if let Some((used, window, last)) = self.context.take() {
            out.push(Body::Context(ContextUse { used, window, last: Some(last), parts: Vec::new() }));
        }
        let usage = usage(&self.total_before, &self.total);
        self.total_before = self.total.clone();
        out.push(turn_end(ok, stop, duration_ms, usage, std::mem::take(&mut self.last_text)));
        self.in_turn = false;
        self.turn = None;
        self.interrupt_pending = false;
        self.error_said = false;
        self.called.clear();
        self.pump();
        out.push(Body::Busy { busy: self.in_turn });
        out
    }

    /// A text or thinking item began; its `block_start` waits for its first text.
    fn open_item(&mut self, block: &str, kind: Kind) {
        self.seq += 1;
        self.open.insert(block.to_owned(), Open { kind, seq: self.seq, started: false, text: String::new() });
    }

    /// A streaming block's next text, its `block_start` first if it had none.
    fn more(&mut self, block: &str, kind: Kind, text: &str) -> Vec<Body> {
        let seq = &mut self.seq;
        let o = self.open.entry(block.to_owned()).or_insert_with(|| {
            *seq += 1;
            Open { kind, seq: *seq, started: false, text: String::new() }
        });
        let mut out = Vec::new();
        if !o.started {
            o.started = true;
            out.push(Body::BlockStart { block: block.to_owned(), kind: o.kind });
        }
        o.text.push_str(text);
        out.push(Body::Delta { block: block.to_owned(), text: text.to_owned() });
        out
    }

    fn item_started(&mut self, item: &Value) -> Vec<Body> {
        let id = item["id"].as_str().unwrap_or("").to_owned();
        match item["type"].as_str().unwrap_or("") {
            "agentMessage" => {
                self.open_item(&id, Kind::Text);
                vec![]
            }
            "reasoning" => {
                self.open_item(&id, Kind::Thinking);
                vec![]
            }
            "contextCompaction" => {
                self.ledger.compacted();
                vec![Body::Note { text: "Compacting the context".to_owned() }]
            }
            _ => self.call(item).into_iter().collect(),
        }
    }

    /// A tool-like item's `tool_call` (once).
    fn call(&mut self, item: &Value) -> Option<Body> {
        let id = item["id"].as_str()?.to_owned();
        let (name, input) = match item["type"].as_str()? {
            "mcpToolCall" => (tool_name(item["server"].as_str().unwrap_or(""), item["tool"].as_str().unwrap_or("")), item["arguments"].clone()),
            "dynamicToolCall" => (item["tool"].as_str().unwrap_or("").to_owned(), item["arguments"].clone()),
            "commandExecution" => ("shell".to_owned(), json!({"command": item["command"], "cwd": item["cwd"]})),
            "fileChange" => {
                let paths: Vec<&Value> = item["changes"].as_array().into_iter().flatten().map(|c| &c["path"]).collect();
                ("apply_patch".to_owned(), json!({"paths": paths}))
            }
            "webSearch" => ("web_search".to_owned(), json!({"query": item["query"]})),
            _ => return None,
        };
        if !self.called.insert(id.clone()) {
            return None;
        }
        Some(Body::ToolCall { block: id, name, input })
    }

    fn item_completed(&mut self, item: &Value) -> Vec<Body> {
        let id = item["id"].as_str().unwrap_or("").to_owned();
        let open = self.open.remove(&id);
        match item["type"].as_str().unwrap_or("") {
            "agentMessage" => {
                let text = item["text"].as_str().unwrap_or("").to_owned();
                if !text.trim().is_empty() {
                    self.last_text.clone_from(&text);
                }
                vec![Body::Block { block: id, kind: Kind::Text, text }]
            }
            "reasoning" => {
                let join = |k: &str| {
                    item[k].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("\n\n")
                };
                let text = Some(join("summary")).filter(|s| !s.trim().is_empty()).unwrap_or_else(|| join("content"));
                // A reasoning item with nothing readable (encrypted only) has no place in the conversation.
                if text.trim().is_empty() && !open.is_some_and(|o| o.started) {
                    return vec![];
                }
                vec![Body::Block { block: id, kind: Kind::Thinking, text }]
            }
            "userMessage" | "contextCompaction" | "plan" | "hookPrompt" => vec![],
            _ => {
                let mut out: Vec<Body> = self.call(item).into_iter().collect();
                if let Some(r) = tool_result(&id, item) {
                    out.push(r);
                }
                out
            }
        }
    }

    fn notification(&mut self, method: &str, p: &Value) -> Vec<Body> {
        match method {
            "turn/started" => {
                if let Some(t) = p["turn"]["id"].as_str() {
                    self.turn_started(t);
                }
                vec![Body::Busy { busy: true }]
            }
            "item/started" => self.item_started(&p["item"]),
            "item/completed" => self.item_completed(&p["item"]),
            "item/agentMessage/delta" => self.more(p["itemId"].as_str().unwrap_or(""), Kind::Text, p["delta"].as_str().unwrap_or("")),
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                self.more(p["itemId"].as_str().unwrap_or(""), Kind::Thinking, p["delta"].as_str().unwrap_or(""))
            }
            "item/reasoning/summaryPartAdded" if p["summaryIndex"].as_u64().unwrap_or(0) > 0 => {
                self.more(p["itemId"].as_str().unwrap_or(""), Kind::Thinking, "\n\n")
            }
            "thread/tokenUsage/updated" => {
                let u = &p["tokenUsage"];
                if let Some(used) = u["last"]["totalTokens"].as_u64() {
                    self.context = Some((used, u["modelContextWindow"].as_u64().unwrap_or(0), context::codex_call(&u["last"])));
                }
                self.total = u["total"].clone();
                if self.total_before.is_null() {
                    // The first figure of the session: the turn so far is all of it.
                    self.total_before = json!({});
                }
                vec![]
            }
            "turn/completed" => {
                let turn = &p["turn"];
                let status = turn["status"].as_str().unwrap_or("");
                let (ok, stop) = match status {
                    "completed" => (true, "end_turn"),
                    "interrupted" => (false, "interrupted"),
                    _ => (false, "error"),
                };
                let mut why = Vec::new();
                if status == "interrupted" {
                    why.push(Body::Note { text: "Interrupted".to_owned() });
                } else if stop == "error" && !self.error_said {
                    let msg = turn["error"]["message"].as_str().unwrap_or("the turn failed");
                    why.push(Body::Error { text: msg.to_owned() });
                }
                self.turn_over(ok, stop, turn["durationMs"].as_u64(), why)
            }
            "error" => {
                let msg = p["error"]["message"].as_str().unwrap_or("codex reported an error");
                if p["willRetry"].as_bool().unwrap_or(false) {
                    vec![Body::Note { text: format!("Retrying: {msg}") }]
                } else {
                    self.error_said = true;
                    vec![Body::Error { text: msg.to_owned() }]
                }
            }
            "warning" | "configWarning" | "deprecationNotice" | "guardianWarning" => {
                let text = ["message", "summary", "details"].iter().find_map(|k| p[*k].as_str()).map_or_else(|| p.to_string(), str::to_owned);
                vec![Body::Note { text }]
            }
            "thread/settings/updated" if self.thread.as_deref().is_some_and(|t| p["threadId"] == t) => {
                let t = &p["threadSettings"];
                self.runs = (t["model"].as_str().unwrap_or("").to_owned(), t["effort"].as_str().filter(|e| !e.is_empty()).map(str::to_owned));
                vec![Body::Session { agent: "codex".to_owned(), model: self.runs.0.clone(), session_id: None, effort: self.runs.1.clone() }]
            }
            "account/rateLimits/updated" => plan(p.get("rateLimits").unwrap_or(p)).into_iter().collect(),
            "model/rerouted" => {
                let s = |k: &str| p[k].as_str().unwrap_or("?").to_owned();
                vec![Body::Note { text: format!("Model rerouted from {} to {}", s("fromModel"), s("toModel")) }]
            }
            _ => vec![],
        }
    }

    /// Answer a request Codex made, at once.
    fn server_request(&mut self, id: &Value, method: &str) -> Vec<Body> {
        let result = match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => Some(json!({"decision": "accept"})),
            "execCommandApproval" | "applyPatchApproval" => Some(json!({"decision": "approved"})),
            "item/tool/requestUserInput" => Some(json!({"answers": {}})),
            "mcpServer/elicitation/request" => Some(json!({"action": "decline", "content": null, "_meta": null})),
            _ => None,
        };
        let (line, note) = match result {
            Some(r) => (json!({"id": id, "result": r}), None),
            None => (
                json!({"id": id, "error": {"code": -32601, "message": format!("harness does not answer {method}")}}),
                Some(Body::Note { text: format!("Refused codex's {method} request (harness does not answer it)") }),
            ),
        };
        self.out.push(line.to_string());
        let declined = matches!(method, "item/tool/requestUserInput" | "mcpServer/elicitation/request");
        match note {
            Some(n) => vec![n],
            None if declined => vec![Body::Note { text: format!("Declined codex's {method} request") }],
            None => vec![],
        }
    }
}

/// A `RateLimitSnapshot` as the `plan` event: its `primary` and `secondary` windows that say how much is used, the
/// shorter first. Read defensively: the v2 camelCase names, else the snake_case of codex's core protocol
/// (`used_percent`, `window_minutes`, `resets_at`); unknown fields are ignored and a missing one leaves its part out.
/// Codex names no warning threshold, so a window warns from [`PlanWindow::WARN`] of it used, and is limited used up.
/// None when neither window says how much is used.
fn plan(s: &Value) -> Option<Body> {
    let field = |w: &Value, names: &[&str]| names.iter().find_map(|n| w.get(*n).filter(|v| !v.is_null()).cloned());
    let mut windows: Vec<(Option<u64>, PlanWindow)> = ["primary", "secondary"]
        .iter()
        .filter_map(|k| {
            let w = &s[*k];
            let used = field(w, &["usedPercent", "used_percent"])?.as_f64()? / 100.0;
            let mins = field(w, &["windowDurationMins", "windowMinutes", "window_minutes"]).and_then(|v| v.as_u64());
            let resets_at = field(w, &["resetsAt", "resets_at"]).and_then(|v| v.as_u64());
            let name = mins.map_or_else(|| (*k).to_owned(), span_name);
            Some((mins, PlanWindow { name, used, resets_at, warn: used >= PlanWindow::WARN, limited: used >= 1.0 }))
        })
        .collect();
    windows.sort_by_key(|(mins, _)| mins.unwrap_or(u64::MAX));
    let windows: Vec<PlanWindow> = windows.into_iter().map(|(_, w)| w).collect();
    (!windows.is_empty()).then_some(Body::Plan { windows })
}

/// A window of `mins` minutes as the status bar names it: `5h`, `day`, `week`, else in days, hours or minutes.
fn span_name(mins: u64) -> String {
    match mins {
        10_080 => "week".to_owned(),
        1_440 => "day".to_owned(),
        m if m % 1_440 == 0 => format!("{}d", m / 1_440),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

/// The tool's name as a person reads it: the harness REPL is just `ts`.
fn tool_name(server: &str, tool: &str) -> String {
    if server == "harness" || server.is_empty() { tool.to_owned() } else { format!("{server}.{tool}") }
}

fn tool_result(block: &str, item: &Value) -> Option<Body> {
    let texts = |a: &Value| a.as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
    let (text, error) = match item["type"].as_str()? {
        "mcpToolCall" => {
            let failed = item["status"] == "failed" || !item["error"].is_null();
            let text = item["error"]["message"].as_str().map_or_else(|| texts(&item["result"]["content"]), str::to_owned);
            (text, failed)
        }
        "dynamicToolCall" => (texts(&item["contentItems"]), item["success"] == false),
        "commandExecution" => {
            let code = item["exitCode"].as_i64();
            (item["aggregatedOutput"].as_str().unwrap_or("").to_owned(), code.is_some_and(|c| c != 0) || item["status"] == "failed")
        }
        "fileChange" => (item["status"].as_str().unwrap_or("").to_owned(), item["status"] != "completed"),
        "webSearch" => (String::new(), false),
        _ => return None,
    };
    Some(Body::ToolResult { block: block.to_owned(), text, error })
}

/// A turn's usage in Claude's field names (what the app and the messages rows already read), from the thread's token
/// totals before and after it.
fn usage(before: &Value, after: &Value) -> Value {
    if after.is_null() {
        return Value::Null;
    }
    let d = |k: &str| after[k].as_u64().unwrap_or(0).saturating_sub(before[k].as_u64().unwrap_or(0));
    json!({
        "input_tokens": d("inputTokens").saturating_sub(d("cachedInputTokens")),
        "cache_read_input_tokens": d("cachedInputTokens"),
        "cache_creation_input_tokens": d("cacheWriteInputTokens"),
        "output_tokens": d("outputTokens"),
        "reasoning_output_tokens": d("reasoningOutputTokens"),
    })
}

/// A turn's end; Codex reports no cost.
fn turn_end(ok: bool, stop: &str, duration_ms: Option<u64>, usage: Value, text: String) -> Body {
    Body::TurnEnd { ok, stop: stop.to_owned(), duration_ms, cost_usd: None, usage, text }
}

impl super::Adapter for Codex {
    fn start(&mut self) -> Vec<String> {
        let params = json!({
            "clientInfo": {"name": "harness", "title": "harness", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": false, "requestAttestation": false, "optOutNotificationMethods": OPT_OUT},
        });
        vec![self.request("initialize", params, Pending::Initialize)]
    }

    /// Kept, and sent on every `turn/start` from the next one on: app-server has no call that switches a thread's model or
    /// effort between turns.
    fn agent(&self) -> &'static str {
        "codex"
    }

    fn choose(&mut self, model: Option<&str>, effort: Option<&str>) -> Result<Vec<String>, String> {
        if self.dead {
            return Err("its codex thread could not start, so it takes no model or effort".to_owned());
        }
        if let Some(m) = model {
            self.model = Some(m.to_owned());
        }
        if let Some(e) = effort {
            self.effort = Some(e.to_owned());
        }
        Ok(Vec::new())
    }

    fn events(&mut self, line: &str) -> Vec<Body> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![Body::Error { text: format!("codex app-server wrote a line that is not JSON: {line}") }];
        };
        let mut out = match (v.get("id"), v["method"].as_str()) {
            (Some(id), Some(method)) => self.server_request(id, method),
            (None, Some(method)) => self.notification(method, &v["params"]),
            (Some(id), None) => match id.as_u64() {
                Some(id) => self.answer(id, &v),
                None => vec![],
            },
            (None, None) => vec![],
        };
        // The ledger sees each event in order, so a `context` event's parts are the messages before it.
        for b in &mut out {
            match b {
                Body::Context(c) => c.parts = self.ledger.parts(c.used, "Codex's prompt and tools"),
                b => self.ledger.see(b),
            }
        }
        out
    }

    fn replies(&mut self) -> Vec<String> {
        std::mem::take(&mut self.out)
    }

    fn user(&mut self, text: &str, images: &[String]) -> Vec<String> {
        if self.dead {
            // The thread never started, so nothing would answer (the error said why); the turn ends when Codex exits.
            return Vec::new();
        }
        self.ledger.user(text);
        self.queue.push_back((text.to_owned(), images.to_vec()));
        self.pump();
        std::mem::take(&mut self.out)
    }

    fn interrupt(&mut self) -> Vec<String> {
        if !self.in_turn {
            // A message sent before the thread could take it is a turn in flight to the person who sent it: its turn
            // stops as soon as Codex starts it.
            self.interrupt_pending = !self.queue.is_empty();
            return Vec::new();
        }
        match self.interrupt_line() {
            Some(l) => vec![l],
            None => {
                self.interrupt_pending = true;
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Adapter;

    /// `account/rateLimits/updated` (asked for: not opted out) is the `plan` event: the five-hour window then the weekly
    /// one, `usedPercent` as a share, the weekly one warned past [`PlanWindow::WARN`]; the core protocol's snake_case and
    /// a window with nothing used read too, and a snapshot that says no use is none.
    #[test]
    fn rate_limits_are_the_plan() {
        assert!(!OPT_OUT.contains(&"account/rateLimits/updated"));
        let line = r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"codex","limitName":null,"primary":{"usedPercent":42,"windowDurationMins":300,"resetsAt":1794253800},"secondary":{"usedPercent":81.5,"windowDurationMins":10080,"resetsAt":1794470400},"credits":{"hasCredits":false,"unlimited":false,"balance":null},"planType":"plus","someFutureField":1}}}"#;
        assert_eq!(
            Codex::new("/tmp").events(line),
            vec![Body::Plan {
                windows: vec![
                    PlanWindow { name: "5h".into(), used: 0.42, resets_at: Some(1_794_253_800), warn: false, limited: false },
                    PlanWindow { name: "week".into(), used: 0.815, resets_at: Some(1_794_470_400), warn: true, limited: false },
                ],
            }]
        );
        let snake = r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":{"used_percent":100.0,"window_minutes":1440,"resets_at":null},"secondary":null}}}"#;
        assert_eq!(
            Codex::new("/tmp").events(snake),
            vec![Body::Plan { windows: vec![PlanWindow { name: "day".into(), used: 1.0, resets_at: None, warn: true, limited: true }] }]
        );
        let none = r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":null,"secondary":null,"planType":"free"}}}"#;
        assert!(Codex::new("/tmp").events(none).is_empty());
        assert_eq!((span_name(300), span_name(10_080), span_name(4_320), span_name(90)), ("5h".into(), "week".into(), "3d".into(), "90m".into()));
    }
}
