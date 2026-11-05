//! Codex's adapter: `codex app-server` (the JSON-RPC its IDE extensions speak, one object per line on stdin and stdout,
//! without the `jsonrpc` field) read into the event model of `chat/mod.rs`. The only place Codex's shapes are known; the
//! protocol is `codex app-server generate-ts` (openai/codex `codex-rs/app-server-protocol`, v2).
//!
//! The session: `initialize` (answered), the `initialized` notification, `thread/start` in the session's cwd (answered
//! with the thread and its model), or for a resumed chat `thread/resume` of its thread (answered the same way); then each user message is a `turn/start` on that thread. Messages sent before the
//! thread exists, or while a turn is in flight, wait here and go one turn each, in order (as Claude queues them).
//!
//! What Codex sends per turn: `turn/started`, then per item `item/started` / `item/completed` (an `agentMessage`, a
//! `reasoning`, an `mcpToolCall` whose arguments come whole at its start, ...), between them `item/agentMessage/delta`
//! and `item/reasoning/summaryTextDelta` / `textDelta` (with `summaryPartAdded` between summary parts),
//! `thread/tokenUsage/updated` (the thread's running total), `error` (with `willRetry`), and `turn/completed` with the
//! turn's `status` (`completed` | `interrupted` | `failed`), `error` and `durationMs`. An interrupt is `turn/interrupt`
//! on the turn in flight; the turn then completes as `interrupted`, its items left unfinished.
//!
//! Codex asks the client too (server requests, each answered at once so a turn never hangs): approvals are approved
//! (the profile already runs with approval policy never and no sandbox, the TUI's yolo), questions for the user get no
//! answers, MCP elicitations are declined, and everything else is refused with a JSON-RPC error naming the method.
use harness_proto::event::{Body, Kind};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};

/// Notifications this client never reads, asked off at initialize so the raw record stays small.
const OPT_OUT: &[&str] = &[
    "turn/moderationMetadata",
    "account/rateLimits/updated",
    "account/updated",
    "thread/readState/changed",
    "thread/status/changed",
    "remoteControl/status/changed",
    "mcpServer/startupStatus/updated",
    "rawResponseItem/completed",
    "rawResponse/completed",
];

/// What a request of ours was, so its answer can be read.
enum Pending {
    Initialize,
    /// `thread/start`, or `thread/resume` (answered alike).
    ThreadStart,
    TurnStart,
    Interrupt,
}

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
    /// The thread to resume instead of starting one.
    resume: Option<String>,
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
    queue: VecDeque<String>,
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
}

impl Codex {
    #[must_use]
    pub fn new(cwd: &str) -> Self {
        Self::with_thread(cwd, None)
    }

    /// A chat that resumes Codex's thread `thread` (its id from the chat's log).
    #[must_use]
    pub fn resume(cwd: &str, thread: &str) -> Self {
        Self::with_thread(cwd, Some(thread.to_owned()))
    }

    fn with_thread(cwd: &str, resume: Option<String>) -> Self {
        Self {
            cwd: cwd.to_owned(),
            resume,
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
        }
    }

    fn request(&mut self, method: &str, params: Value, what: Pending) -> String {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, what);
        json!({"id": id, "method": method, "params": params}).to_string()
    }

    fn turn_start(&mut self, text: &str) -> String {
        self.in_turn = true;
        self.turn = None;
        let thread = self.thread.clone().unwrap_or_default();
        self.request(
            "turn/start",
            json!({
                "threadId": thread,
                "input": [{"type": "text", "text": text, "text_elements": []}],
                "summary": "auto",
            }),
            Pending::TurnStart,
        )
    }

    /// Start the next waiting message's turn, when the thread can take one.
    fn pump(&mut self) {
        if self.thread.is_some() && !self.in_turn {
            if let Some(text) = self.queue.pop_front() {
                let line = self.turn_start(&text);
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
                let mut params = json!({"cwd": self.cwd, "approvalPolicy": "never", "sandbox": "danger-full-access"});
                let method = match &self.resume {
                    Some(thread) => {
                        // Its turns are already in the chat's log: only the thread's state comes back.
                        params["threadId"] = json!(thread);
                        params["excludeTurns"] = json!(true);
                        "thread/resume"
                    }
                    None => "thread/start",
                };
                let line = self.request(method, params, Pending::ThreadStart);
                self.out.push(line);
                vec![]
            }
            Pending::ThreadStart => {
                if let Some(e) = err {
                    self.dead = true;
                    let what = match &self.resume {
                        Some(t) => format!("thread/resume of thread {t}"),
                        None => "thread/start".to_owned(),
                    };
                    return self.refuse_waiting(&format!("codex app-server refused {what}: {e}"));
                }
                let r = &v["result"];
                self.thread = r["thread"]["id"].as_str().map(str::to_owned);
                self.pump();
                let model = r["model"].as_str().or_else(|| r["thread"]["model"].as_str()).unwrap_or("");
                vec![Body::Session {
                    agent: "codex".to_owned(),
                    model: model.to_owned(),
                    session_id: self.thread.clone().filter(|t| !t.is_empty()),
                }]
            }
            Pending::TurnStart => {
                if let Some(e) = err {
                    let why = vec![Body::Error { text: format!("codex app-server refused turn/start: {e}") }];
                    return self.turn_over(false, "error", None, why);
                }
                // The answer names the turn, but Codex refuses to interrupt it ("no active turn") until `turn/started`.
                vec![]
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
            "contextCompaction" => vec![Body::Note { text: "Compacting the context".to_owned() }],
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
                self.total = p["tokenUsage"]["total"].clone();
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

    fn events(&mut self, line: &str) -> Vec<Body> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![Body::Error { text: format!("codex app-server wrote a line that is not JSON: {line}") }];
        };
        match (v.get("id"), v["method"].as_str()) {
            (Some(id), Some(method)) => self.server_request(id, method),
            (None, Some(method)) => self.notification(method, &v["params"]),
            (Some(id), None) => match id.as_u64() {
                Some(id) => self.answer(id, &v),
                None => vec![],
            },
            (None, None) => vec![],
        }
    }

    fn replies(&mut self) -> Vec<String> {
        std::mem::take(&mut self.out)
    }

    fn user(&mut self, text: &str) -> Vec<String> {
        if self.dead {
            // The thread never started, so nothing would answer (the error said why); the turn ends when Codex exits.
            return Vec::new();
        }
        self.queue.push_back(text.to_owned());
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
