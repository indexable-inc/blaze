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
//!
//! The model and effort change live, between turns, over the same control requests (claude 2.1's stream-json; den's
//! den-claude does the same): `list_models` as the process starts (its models, each with `supportedEffortLevels`: the
//! `models` event), `set_model` (answered with the model it resolved to), and `apply_flag_settings` with `effortLevel`
//! (answered with success even for a level it ignores, so it is followed by `get_settings` and the effort its effective
//! settings read is what the `session` event says, an `error` when it is not the one asked for).
//!
//! Around each call to the model Claude says how much of the person's plan is used: a `rate_limit_event` whose
//! `rate_limit_info` carries `unifiedWindows` (`five_hour`, `seven_day`, ...: each a `utilization` 0 to 1, `resetsAt` in
//! unix seconds, a `status` of `allowed`, `allowed_warning` or `rejected`, and a `surpassedThreshold` once warned),
//! read as the `plan` event.
use super::context;
use harness_proto::event::{Body, ContextCall, ContextUse, Kind, ModelChoice, PlanWindow};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// The content block that gives Claude the picture at `path`: its bytes, base64, under the media type its header names.
/// A picture that cannot be read by now (the daemon checked it as the message came) is a text block saying so, naming it.
fn image_block(path: &str) -> Value {
    use base64::Engine as _;
    match std::fs::read(path).map_err(|e| e.to_string()).and_then(|b| super::picture_type(&b).map(|t| (t, b)).ok_or_else(|| "not a PNG, JPEG, GIF or WebP picture".to_owned())) {
        Ok((media, bytes)) => json!({"type": "image", "source": {"type": "base64", "media_type": media, "data": base64::engine::general_purpose::STANDARD.encode(bytes)}}),
        Err(e) => json!({"type": "text", "text": format!("[the picture at {path} could not be sent: {e}]")}),
    }
}

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
    /// The effort its launch passed as `--effort` (profiles.rs), then as `get_settings` confirmed each change: Claude's
    /// init does not say it, so its `session` event carries this.
    effort: Option<String>,
    /// The model it runs as: what `list_models` resolves its launch's `--model` to (claude's `default` without one), then
    /// its init's, then each `set_model` answer's.
    model: String,
    /// The `--model` its launch passed, if any.
    launched: Option<String>,
    /// The context the last main-loop assistant message of the turn filled (its usage's input, cache and output tokens),
    /// and that usage's split.
    filled: Option<(u64, ContextCall)>,
    /// Control requests of ours not answered yet, by request id.
    asked: HashMap<String, Asked>,
    /// Lines to write back ([`super::Adapter::replies`]).
    out: Vec<String>,
}

/// What a control request of ours was, so its `control_response` can be read.
enum Asked {
    Interrupt,
    ListModels,
    SetModel(String),
    /// `apply_flag_settings` of this effort; `get_settings` follows it.
    Effort(String),
    /// `get_settings` after asking for this effort.
    ReadEffort(String),
    /// `get_context_usage` after a turn that filled `used` of `window`, its last call `last`: the `context` event waits
    /// for its parts.
    ContextUsage { used: u64, window: u64, last: ContextCall },
}

impl Claude {
    /// An adapter for claude launched with `launch` (its `--effort`, [`crate::profiles::Launch::effort`], and its
    /// `--model`, if any).
    #[must_use]
    pub fn new(launch: &crate::profiles::Launch) -> Self {
        let mut it = launch.args.iter();
        let mut launched = None;
        while let Some(a) = it.next() {
            if let Some(m) = a.strip_prefix("--model=") {
                launched = Some(m.to_owned());
            } else if a == "--model" {
                launched = it.next().cloned();
            }
        }
        Self { effort: launch.effort.clone(), launched, ..Self::default() }
    }

    /// A control request's line, noted so its answer is read as `what`.
    fn ask(&mut self, request: Value, what: Asked) -> String {
        self.requests += 1;
        let id = format!("harness-{}", self.requests);
        self.asked.insert(id.clone(), what);
        json!({"type": "control_request", "request_id": id, "request": request}).to_string()
    }

    /// The `session` event that says what it runs with now.
    fn session(&self) -> Body {
        Body::Session { agent: "claude".to_owned(), model: self.model.clone(), session_id: None, effort: self.effort.clone() }
    }

    /// The answer to a control request of ours.
    fn answered(&mut self, r: &Value) -> Vec<Body> {
        let Some(what) = r["request_id"].as_str().and_then(|id| self.asked.remove(id)) else { return vec![] };
        let error = (r["subtype"] != "success").then(|| r["error"].as_str().map_or_else(|| r.to_string(), str::to_owned));
        let a = &r["response"];
        match (what, error) {
            (Asked::Interrupt, None) => vec![],
            (Asked::Interrupt, Some(e)) => vec![Body::Error { text: format!("claude refused the interrupt: {e}") }],
            (Asked::ListModels, Some(e)) => vec![Body::Error { text: format!("claude refused list_models, so its picker offers the models the last claude listed: {e}") }],
            (Asked::ListModels, None) => {
                let mut out = vec![Body::Models { models: models(a) }];
                // Claude says the model it runs only in its init, which waits for the first message: until then it is
                // what its own list resolves the launch's model to.
                let wanted = self.launched.clone().unwrap_or_else(|| "default".to_owned());
                let resolved = a["models"].as_array().into_iter().flatten().find(|m| m["value"] == wanted.as_str() || m["resolvedModel"] == wanted.as_str());
                if self.model.is_empty()
                    && let Some(r) = resolved.and_then(|m| m["resolvedModel"].as_str()).filter(|r| !r.is_empty())
                {
                    self.model = r.to_owned();
                    out.push(self.session());
                }
                out
            }
            (Asked::SetModel(m), Some(e)) => vec![Body::Error { text: format!("claude refused set_model {m}: {e}") }],
            (Asked::SetModel(m), None) => {
                self.model = a["model"].as_str().filter(|r| !r.is_empty()).unwrap_or(&m).to_owned();
                vec![self.session()]
            }
            (Asked::Effort(l), Some(e)) => vec![Body::Error { text: format!("claude refused effort {l} (apply_flag_settings): {e}") }],
            (Asked::Effort(l), None) => {
                let line = self.ask(json!({"subtype": "get_settings"}), Asked::ReadEffort(l));
                self.out.push(line);
                vec![]
            }
            (Asked::ReadEffort(l), Some(e)) => {
                vec![Body::Error { text: format!("claude took effort {l} but refused get_settings, so it is not known to run with it: {e}") }]
            }
            (Asked::ReadEffort(l), None) => match a["effective"]["effortLevel"].as_str() {
                Some(now) if now == l => {
                    self.effort = Some(l);
                    vec![self.session()]
                }
                now => {
                    let now = now.unwrap_or("none");
                    vec![Body::Error { text: format!("claude took effort {l} (apply_flag_settings) but its settings say it runs with {now}") }]
                }
            },
            (Asked::ContextUsage { used, window, last }, None) => {
                vec![Body::Context(ContextUse { used, window, last: Some(last), parts: context::claude_parts(a) })]
            }
            // The meter still has the turn's exact figures; only the popover's parts are missing, and the error says so.
            (Asked::ContextUsage { used, window, last }, Some(e)) => vec![
                Body::Error { text: format!("claude refused get_context_usage, so this turn's context has no parts: {e}") },
                Body::Context(ContextUse { used, window, last: Some(last), parts: Vec::new() }),
            ],
        }
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
        let u = &v["message"]["usage"];
        if v["parent_tool_use_id"].is_null() && u.is_object() {
            let c = context::claude_call(u, None);
            self.filled = Some((c.input + c.cache_read + c.cache_write + c.output, c));
        }
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
        // What claude replays of its session as it resumes (`isReplay`, such as a `set_model`'s "Set model to ..." note) is
        // history: the chat's log has it already, a model switch as its `session` event.
        if v["isReplay"] == true {
            return vec![];
        }
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
        // The `context` event comes with claude's answer to get_context_usage (its parts), a moment after the turn's end.
        if let Some((used, last)) = self.filled.take() {
            let last = ContextCall { thinking: v["usage"]["output_tokens_details"]["thinking_tokens"].as_u64(), ..last };
            let ask = json!({"subtype": "get_context_usage", "detail": "full"});
            let line = self.ask(ask, Asked::ContextUsage { used, window: window(v, &self.model), last });
            self.out.push(line);
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

/// `list_models`' answer as the models event's list: each model's alias (`value`), what it resolves to, its name and
/// line, and its effort levels (none when `supportsEffort` is false). `default` is left out when another entry runs as the
/// same model: it is claude's alias for that one, and two rows for one model would tick both.
fn models(a: &Value) -> Vec<ModelChoice> {
    let all: Vec<ModelChoice> = a["models"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| ModelChoice {
            id: m["value"].as_str().unwrap_or("").to_owned(),
            model: m["resolvedModel"].as_str().unwrap_or("").to_owned(),
            name: m["displayName"].as_str().unwrap_or("").to_owned(),
            about: m["description"].as_str().unwrap_or("").to_owned(),
            efforts: if m["supportsEffort"].as_bool().unwrap_or(false) {
                m["supportedEffortLevels"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
            } else {
                Vec::new()
            },
            effort: None,
        })
        .filter(|m| !m.id.is_empty())
        .collect();
    let aliased = |m: &ModelChoice| m.id == "default" && all.iter().any(|o| o.id != "default" && !m.model.is_empty() && o.model == m.model);
    let mut kept: Vec<ModelChoice> = all.iter().filter(|m| !aliased(m)).cloned().collect();
    for &(model, name, about) in OFFERED {
        if !kept.iter().any(|m| m.model == model || m.id == model) {
            kept.push(ModelChoice {
                id: model.to_owned(),
                model: model.to_owned(),
                name: name.to_owned(),
                about: about.to_owned(),
                efforts: ["low", "medium", "high", "xhigh", "max"].map(str::to_owned).to_vec(),
                effort: None,
            });
        }
    }
    kept
}

/// Models the picker and `spawn` offer for claude whether or not this Claude Code lists them yet (model id, name, line):
/// a model released after the installed Claude Code, which `set_model` and `--model` take by its full id all the same.
/// Once Claude Code lists one itself, its own entry is the one kept; drop it from here then.
const OFFERED: &[(&str, &str, &str)] =
    &[("claude-fable-5-2", "Fable 5.2", "The most capable; the default")];

/// The context window a turn's `result` says its model has (den-claude's reading): `modelUsage[model].contextWindow`,
/// else the widest window it names; 0 when it names none. What the turn filled is its last main-loop assistant message's
/// usage (`Claude::filled`): a result's own `usage` sums every call of the turn.
fn window(v: &Value, model: &str) -> u64 {
    let usage = &v["modelUsage"];
    usage[model]["contextWindow"]
        .as_u64()
        .or_else(|| usage.as_object().into_iter().flat_map(|o| o.values()).filter_map(|m| m["contextWindow"].as_u64()).max())
        .unwrap_or(0)
}

/// A `rate_limit_event`'s `rate_limit_info` as the `plan` event: a window each of its `unifiedWindows`, shortest first
/// (with none, the one window it is about, `rateLimitType` with its own `utilization`). A window warns once its status
/// is `allowed_warning` or it is past its `surpassedThreshold` (else [`PlanWindow::WARN`]), and is limited once
/// `rejected` or used up. None when it names no window with a utilization (an `allowed` line before any use).
fn plan(info: &Value) -> Option<Body> {
    let window = |key: &str, w: &Value| {
        let used = w["utilization"].as_f64()?;
        let status = w["status"].as_str().unwrap_or("");
        let threshold = w["surpassedThreshold"].as_f64().unwrap_or(PlanWindow::WARN);
        Some(PlanWindow {
            name: window_name(key),
            used,
            resets_at: w["resetsAt"].as_u64(),
            warn: status == "allowed_warning" || used >= threshold,
            limited: status == "rejected" || used >= 1.0,
        })
    };
    let mut keyed: Vec<(&str, PlanWindow)> =
        info["unifiedWindows"].as_object().into_iter().flatten().filter_map(|(k, w)| Some((k.as_str(), window(k.as_str(), w)?))).collect();
    if keyed.is_empty()
        && let Some(k) = info["rateLimitType"].as_str()
    {
        keyed.extend(window(k, info).map(|w| (k, w)));
    }
    keyed.sort_by_key(|&(k, _)| (window_rank(k), k));
    let windows: Vec<PlanWindow> = keyed.into_iter().map(|(_, w)| w).collect();
    (!windows.is_empty()).then_some(Body::Plan { windows })
}

/// Where Claude's window `key` sorts among a plan's: the five-hour one, then the weekly ones, then any other.
fn window_rank(key: &str) -> u8 {
    match key {
        "five_hour" => 0,
        "seven_day" => 1,
        k if k.starts_with("seven_day") => 2,
        _ => 3,
    }
}

/// Claude's window `key` as the status bar names it: `five_hour` is `5h`, `seven_day` `week`, a model's weekly
/// window (`seven_day_opus`) `Opus week`, any other its key in words.
fn window_name(key: &str) -> String {
    match key {
        "five_hour" => "5h".to_owned(),
        "seven_day" => "week".to_owned(),
        k => match k.strip_prefix("seven_day_") {
            Some(m) => {
                let mut c = m.chars();
                let model: String = c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default();
                format!("{} week", model.replace('_', " "))
            }
            None => k.replace('_', " "),
        },
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
            "system" if v["subtype"] == "init" => {
                self.model = v["model"].as_str().unwrap_or("").to_owned();
                vec![Body::Session {
                    agent: "claude".to_owned(),
                    model: self.model.clone(),
                    session_id: v["session_id"].as_str().filter(|i| !i.is_empty()).map(str::to_owned),
                    effort: self.effort.clone(),
                }]
            }
            "control_response" => self.answered(&v["response"]),
            "stream_event" => self.partial(&v["event"]),
            "assistant" => self.assistant(&v),
            "user" => self.from_user(&v),
            "result" => self.result(&v),
            "rate_limit_event" => plan(&v["rate_limit_info"]).into_iter().collect(),
            _ => vec![],
        }
    }

    fn start(&mut self) -> Vec<String> {
        vec![self.ask(json!({"subtype": "list_models"}), Asked::ListModels)]
    }

    fn replies(&mut self) -> Vec<String> {
        std::mem::take(&mut self.out)
    }

    fn user(&mut self, text: &str, images: &[String]) -> Vec<String> {
        // Each picture before the words, as Claude Code's own paste puts them: an image block of its bytes.
        let mut content: Vec<Value> = images.iter().map(|p| image_block(p)).collect();
        if !text.is_empty() || content.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
        vec![json!({"type": "user", "message": {"role": "user", "content": content}}).to_string()]
    }

    fn interrupt(&mut self) -> Vec<String> {
        vec![self.ask(json!({"subtype": "interrupt"}), Asked::Interrupt)]
    }

    fn agent(&self) -> &'static str {
        "claude"
    }

    fn choose(&mut self, model: Option<&str>, effort: Option<&str>) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        if let Some(m) = model {
            lines.push(self.ask(json!({"subtype": "set_model", "model": m}), Asked::SetModel(m.to_owned())));
        }
        if let Some(e) = effort {
            lines.push(self.ask(json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": e}}), Asked::Effort(e.to_owned())));
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Adapter;

    /// Claude's `rate_limit_event` lines (as a real session wrote them) are `plan` events: both windows, five-hour first,
    /// the weekly one warned past its threshold; a line that names no window's use is none; a rejected window is limited.
    #[test]
    fn rate_limit_events_are_the_plan() {
        let warned = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1794470400,"rateLimitType":"seven_day","utilization":0.78,"overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false,"surpassedThreshold":0.75,"unifiedWindows":{"seven_day":{"utilization":0.78,"resetsAt":1794470400,"status":"allowed_warning","surpassedThreshold":0.75},"five_hour":{"utilization":0.05,"resetsAt":1794253800,"status":"allowed"}},"overageEnabled":false},"uuid":"2b7f270b-d730-4f58-9f26-5aa2a2df9756","session_id":"f088ac1a-54ed-4adf-bd6d-ca98cd560259"}"#;
        let out = Claude::default().events(warned);
        assert_eq!(
            out,
            vec![Body::Plan {
                windows: vec![
                    PlanWindow { name: "5h".into(), used: 0.05, resets_at: Some(1_794_253_800), warn: false, limited: false },
                    PlanWindow { name: "week".into(), used: 0.78, resets_at: Some(1_794_470_400), warn: true, limited: false },
                ],
            }]
        );
        // An `allowed` line: no top-level utilization, its windows' own (an integer 0 among them).
        let allowed = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1793884800,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.01,"resetsAt":1793884800,"status":"allowed"},"seven_day":{"utilization":0,"resetsAt":1794470400,"status":"allowed"}}},"uuid":"fc0a6cc1-c225-4c68-935e-0271756090f5","session_id":"fe41695e-d6e4-4b78-ab47-6e966dfd33f6"}"#;
        let Body::Plan { windows } = &Claude::default().events(allowed)[0] else { panic!("not a plan event") };
        assert_eq!(windows.iter().map(|w| (w.name.as_str(), w.used, w.warn)).collect::<Vec<_>>(), [("5h", 0.01, false), ("week", 0.0, false)]);
        // Without windows, the one it is about; with no use named, nothing.
        let one = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1794253800,"rateLimitType":"five_hour","utilization":1.0}}"#;
        let Body::Plan { windows } = &Claude::default().events(one)[0] else { panic!("not a plan event") };
        assert_eq!((windows.len(), windows[0].name.as_str(), windows[0].limited, windows[0].warn), (1, "5h", true, true));
        assert!(Claude::default().events(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#).is_empty());
        assert_eq!(window_name("seven_day_opus"), "Opus week");
    }

    /// A message with a picture is one user line whose content is the picture's bytes as an image block, then the words.
    #[test]
    fn a_picture_rides_as_an_image_block() {
        let dir = std::env::temp_dir().join(format!("harness-claude-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("shot.png");
        let bytes = b"\x89PNG\r\n\x1a\nrest of the picture";
        std::fs::write(&png, bytes).unwrap();
        let path = png.to_string_lossy().into_owned();
        let lines = Claude::default().user("what is this?", std::slice::from_ref(&path));
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        let content = &v["message"]["content"];
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        use base64::Engine as _;
        assert_eq!(content[0]["source"]["data"], base64::engine::general_purpose::STANDARD.encode(bytes));
        assert_eq!(content[1], json!({"type": "text", "text": "what is this?"}));
        // A picture gone by the time it is sent says so, naming it, rather than vanishing from the message.
        std::fs::remove_file(&png).unwrap();
        let gone: Value = serde_json::from_str(&Claude::default().user("", std::slice::from_ref(&path))[0]).unwrap();
        assert!(gone["message"]["content"][0]["text"].as_str().unwrap().contains(&path));
        assert_eq!(gone["message"]["content"].as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
