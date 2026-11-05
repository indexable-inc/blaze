//! Claude's adapter: `claude -p` over stream-json (`--input-format stream-json --output-format stream-json --verbose
//! --include-partial-messages`) read into the event model of `chat/mod.rs`. The only place Claude's shapes are known.
//!
//! What Claude writes, per turn: `system/init` (model, session_id: what `--resume` takes), then for each assistant message `stream_event`s
//! (`message_start` with its id; per content block `content_block_start` at an index, `content_block_delta`s of
//! `text_delta` / `thinking_delta` / `input_json_delta`, `content_block_stop`), and one whole `assistant` line per block
//! in index order (it may come before its block's stop); `user` lines carry tool results; `result` ends the turn
//! (`subtype` success or error_*, `duration_ms`, `total_cost_usd` (since the process started), `usage`, `result`, `terminal_reason`). An interrupt
//! (`control_request` subtype interrupt) is answered by a `control_response`, the partial block whole, a `user` line
//! "[Request interrupted by user]" and a `result` with terminal_reason `aborted_streaming`.
use harness_proto::event::{Body, Kind};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// What `claude` needs, before the profile's flags, to speak stream-json on stdin and stdout.
pub const ARGS: &[&str] =
    &["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"];

#[derive(Default)]
pub struct Claude {
    /// The assistant message streaming now (its id), and how many of its blocks came whole.
    message: String,
    whole: HashMap<String, usize>,
    /// The block id of each content index of the streaming message (a tool call's is its call id).
    at: HashMap<u64, String>,
    /// Blocks that came whole: later partials of theirs are dropped.
    done: HashSet<String>,
    /// The turn's last assistant words, for its `turn_end` when Claude gives none.
    last_text: String,
    interrupted: bool,
    requests: u64,
    /// What this process had cost by the last turn's end: Claude's `total_cost_usd` counts from the process's start, a
    /// `turn_end`'s `cost_usd` is the turn's own.
    cost_before: f64,
    /// The effort its launch passed as `--effort` (profiles.rs): Claude's init does not say it, so its `session` event
    /// carries this.
    effort: Option<String>,
}

impl Claude {
    /// An adapter for a claude launched with `effort` ([`crate::profiles::Launch::effort`]).
    #[must_use]
    pub fn new(effort: Option<String>) -> Self {
        Self { effort, ..Self::default() }
    }

    fn partial(&mut self, e: &Value) -> Vec<Body> {
        match e["type"].as_str().unwrap_or("") {
            "message_start" => {
                self.message = e["message"]["id"].as_str().unwrap_or("").to_owned();
                self.at.clear();
                vec![]
            }
            "content_block_start" => {
                let i = e["index"].as_u64().unwrap_or(0);
                let b = &e["content_block"];
                match b["type"].as_str().unwrap_or("") {
                    k @ ("text" | "thinking") => {
                        let block = format!("{}:{i}", self.message);
                        self.at.insert(i, block.clone());
                        vec![Body::BlockStart { block, kind: kind(k) }]
                    }
                    "tool_use" => {
                        let block = b["id"].as_str().unwrap_or("").to_owned();
                        self.at.insert(i, block.clone());
                        vec![Body::ToolStart { block, name: tool_name(b["name"].as_str().unwrap_or("")) }]
                    }
                    _ => vec![],
                }
            }
            "content_block_delta" => {
                let Some(block) = e["index"].as_u64().and_then(|i| self.at.get(&i)).cloned() else { return vec![] };
                if self.done.contains(&block) {
                    return vec![];
                }
                let d = &e["delta"];
                let s = |k: &str| d[k].as_str().unwrap_or("").to_owned();
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => vec![Body::Delta { block, text: s("text") }],
                    "thinking_delta" => vec![Body::Delta { block, text: s("thinking") }],
                    "input_json_delta" => vec![Body::ToolDelta { block, json: s("partial_json") }],
                    _ => vec![],
                }
            }
            _ => vec![],
        }
    }

    fn assistant(&mut self, v: &Value) -> Vec<Body> {
        let id = v["message"]["id"].as_str().unwrap_or("").to_owned();
        let mut out = Vec::new();
        for b in v["message"]["content"].as_array().into_iter().flatten() {
            let n = self.whole.entry(id.clone()).or_default();
            let index = *n;
            *n += 1;
            match b["type"].as_str().unwrap_or("") {
                k @ ("text" | "thinking") => {
                    let block = format!("{id}:{index}");
                    let text = b[k].as_str().unwrap_or("").to_owned();
                    if k == "text" && !text.trim().is_empty() {
                        self.last_text.clone_from(&text);
                    }
                    self.done.insert(block.clone());
                    out.push(Body::Block { block, kind: kind(k), text });
                }
                "tool_use" => {
                    let block = b["id"].as_str().unwrap_or("").to_owned();
                    self.done.insert(block.clone());
                    out.push(Body::ToolCall {
                        block,
                        name: tool_name(b["name"].as_str().unwrap_or("")),
                        input: b["input"].clone(),
                    });
                }
                _ => {}
            }
        }
        out
    }

    fn from_user(&mut self, v: &Value) -> Vec<Body> {
        let mut out = Vec::new();
        let content = &v["message"]["content"];
        if let Some(s) = content.as_str() {
            return vec![Body::Note { text: s.to_owned() }];
        }
        for b in content.as_array().into_iter().flatten() {
            match b["type"].as_str().unwrap_or("") {
                "tool_result" => out.push(Body::ToolResult {
                    block: b["tool_use_id"].as_str().unwrap_or("").to_owned(),
                    text: result_text(&b["content"]),
                    error: b["is_error"].as_bool().unwrap_or(false),
                }),
                "text" => {
                    let t = b["text"].as_str().unwrap_or("");
                    if t.starts_with("[Request interrupted") {
                        self.interrupted = true;
                        out.push(Body::Note { text: "Interrupted".to_owned() });
                    } else {
                        out.push(Body::Note { text: t.to_owned() });
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn result(&mut self, v: &Value) -> Vec<Body> {
        let ok = v["subtype"] == "success" && !v["is_error"].as_bool().unwrap_or(false);
        let stop = if self.interrupted || v["terminal_reason"] == "aborted_streaming" {
            "interrupted".to_owned()
        } else if ok {
            v["stop_reason"].as_str().unwrap_or("end_turn").to_owned()
        } else {
            "error".to_owned()
        };
        let text = v["result"].as_str().map_or_else(|| std::mem::take(&mut self.last_text), str::to_owned);
        let cost_usd = v["total_cost_usd"].as_f64().map(|total| {
            let turn = ((total - self.cost_before) * 1e9).round() / 1e9;
            self.cost_before = total;
            turn
        });
        let mut out = Vec::new();
        if stop == "error" {
            let why = v["errors"].as_array().and_then(|e| e.first()).and_then(Value::as_str).unwrap_or("the turn failed");
            out.push(Body::Error { text: why.to_owned() });
        }
        out.push(Body::TurnEnd {
            ok,
            stop,
            duration_ms: v["duration_ms"].as_u64(),
            cost_usd,
            usage: v["usage"].clone(),
            text,
        });
        out.push(Body::Busy { busy: false });
        self.last_text.clear();
        self.interrupted = false;
        self.done.clear();
        self.whole.clear();
        out
    }
}

/// The tool's name as a person reads it: the harness REPL's MCP name is just `ts`.
fn tool_name(n: &str) -> String {
    n.strip_prefix("mcp__harness__").or_else(|| n.strip_prefix("mcp__")).unwrap_or(n).to_owned()
}

/// A content block's kind: `thinking`, else text.
fn kind(k: &str) -> Kind {
    if k == "thinking" { Kind::Thinking } else { Kind::Text }
}

fn result_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

impl super::Adapter for Claude {
    fn events(&mut self, line: &str) -> Vec<Body> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![Body::Error { text: format!("claude wrote a line that is not JSON: {line}") }];
        };
        match v["type"].as_str().unwrap_or("") {
            "system" if v["subtype"] == "init" => vec![Body::Session {
                agent: "claude".to_owned(),
                model: v["model"].as_str().unwrap_or("").to_owned(),
                session_id: v["session_id"].as_str().filter(|i| !i.is_empty()).map(str::to_owned),
                effort: self.effort.clone(),
            }],
            "stream_event" => self.partial(&v["event"]),
            "assistant" => self.assistant(&v),
            "user" => self.from_user(&v),
            "result" => self.result(&v),
            _ => vec![],
        }
    }

    fn user(&mut self, text: &str) -> Vec<String> {
        vec![json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}}).to_string()]
    }

    fn interrupt(&mut self) -> Vec<String> {
        self.requests += 1;
        vec![
            json!({"type": "control_request", "request_id": format!("harness-{}", self.requests), "request": {"subtype": "interrupt"}})
                .to_string(),
        ]
    }
}
