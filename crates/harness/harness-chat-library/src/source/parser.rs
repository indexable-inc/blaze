//! Byte-oriented provider normalization; no filesystem, clock or environment access.
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use super::{MAX_RECORD, MAX_SNAPSHOT, Parsed, Session, Source};
use harness_proto::event::{Body, Event, Kind};
use serde_json::Value;

/// Parse a snapshot. A torn final record is expected while another process appends; malformed complete records fail.
pub fn parse(source: &Source, bytes: &[u8]) -> Result<Parsed, String> {
    if bytes.len() as u64 > MAX_SNAPSHOT {
        return Err(format!("chat source {} exceeds the {MAX_SNAPSHOT}-byte in-memory snapshot limit; use its streaming source reader", source.path.display()));
    }
    let mut state = State::new(source);
    match source.agent.as_str() {
        "harness" => state.harness(bytes)?,
        "claude" | "codex" => {
            for (i, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
                if !state.json_line(line, i)? { break; }
            }
        }
        agent => return Err(format!("chat source {}: unknown provider {agent:?}", source.path.display())),
    }
    state.finish()
}

pub(super) struct State<'a> {
    source: &'a Source,
    session: Session,
    events: Vec<Event>,
    text: String,
    title_priority: u8,
    subagent: String,
}

impl<'a> State<'a> {
    pub(super) fn new(source: &'a Source) -> Self {
        Self {
            source,
            session: Session { key: String::new(), agent: source.agent.clone(), chat_id: String::new(), title: String::new(), cwd: String::new(), model: String::new(), effort: String::new(), updated_ms: source.modified_ms, path: source.path.clone(), harness_id: source.harness_id },
            events: Vec::new(), // alloc: the selected source's normalized events
            text: String::new(), // alloc: the full searchable conversation, owned by this one source parse
            title_priority: 0,
            subagent: String::new(),
        }
    }

    /// One complete line, or the final non-newline-terminated record at EOF. False means a torn final append.
    pub(super) fn json_line(&mut self, line: &[u8], n: usize) -> Result<bool, String> {
        if line.len() as u64 > MAX_RECORD {
            return Err(format!("chat source {} line {} exceeds the {MAX_RECORD}-byte record limit ({} bytes)", self.source.path.display(), n + 1, line.len()));
        }
        if line.iter().all(u8::is_ascii_whitespace) { return Ok(true); }
        match serde_json::from_slice::<Value>(line) {
            Ok(row) if row.is_object() => {
                if self.source.agent == "claude" { self.claude(&row, n)?; } else { self.codex(&row, n)?; }
                Ok(true)
            }
            Ok(_) => Err(format!("chat source {} line {}: record is not an object", self.source.path.display(), n + 1)),
            Err(e) if !line.ends_with(b"\n") && e.is_eof() => Ok(false),
            Err(e) => Err(format!("chat source {} line {}: {e}", self.source.path.display(), n + 1)),
        }
    }

    fn title(&mut self, title: &str, priority: u8) {
        if (priority > self.title_priority || (priority == self.title_priority && priority >= 3)) && !title.trim().is_empty() {
            self.session.title = title.split_whitespace().take(24).collect::<Vec<_>>().join(" ");
            if self.session.title.chars().count() > 200 { self.session.title = self.session.title.chars().take(200).collect(); }
            self.title_priority = priority;
        }
    }

    fn push(&mut self, ts: u64, body: Body) {
        let text = match &body {
            Body::User { text, .. } | Body::Block { text, .. } | Body::ToolResult { text, .. } | Body::Note { text } | Body::Error { text } => Some(text.clone()),
            Body::ToolCall { name, input, .. } => Some(format!("{name} {input}")),
            Body::TurnEnd { text, .. } => Some(text.clone()),
            _ => None,
        };
        if let Some(text) = text {
            self.text.push_str(&text);
            self.text.push('\n');
        }
        self.events.push(Event { ts, body });
    }

    fn claude(&mut self, row: &Value, n: usize) -> Result<(), String> {
        let ts = timestamp(row).unwrap_or(self.source.modified_ms);
        if let Some(id) = row["sessionId"].as_str() { self.session.chat_id = id.to_owned(); }
        if let Some(cwd) = row["cwd"].as_str() { self.session.cwd = cwd.to_owned(); }
        if row["isSidechain"] == true || self.source.path.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with("agent-")) {
            if let Some(id) = row["agentId"].as_str() { self.subagent = id.to_owned(); }
        }
        let ty = row["type"].as_str().unwrap_or("");
        match ty {
            "custom-title" => {
                if let Some(title) = row["customTitle"].as_str().or_else(|| row["title"].as_str()) { self.title(title, 4); }
            }
            "summary" => if let Some(title) = row["summary"].as_str() { self.title(title, 3); },
            "assistant" | "user" => {
                let message = &row["message"];
                if let Some(model) = message["model"].as_str() { self.session.model = model.to_owned(); }
                if let Some(effort) = row["perTurnEffort"].as_str().or_else(|| row["effort"].as_str()) { self.session.effort = effort.to_owned(); }
                let id = row["uuid"].as_str().map(str::to_owned).unwrap_or_else(|| format!("claude-line-{n}"));
                if let Some(text) = message["content"].as_str() {
                    if ty == "user" {
                        self.title(text, 1);
                        self.push(ts, Body::User { text: text.to_owned(), images: Vec::new() });
                    } else {
                        self.push(ts, Body::Block { block: id, kind: Kind::Text, text: text.to_owned() });
                    }
                } else if let Some(blocks) = message["content"].as_array() {
                    for (i, b) in blocks.iter().enumerate() {
                        let block = format!("{id}-{i}");
                        match b["type"].as_str().unwrap_or("") {
                            "text" => if let Some(text) = b["text"].as_str() {
                                if ty == "user" {
                                    self.title(text, 1);
                                    self.push(ts, Body::User { text: text.to_owned(), images: Vec::new() });
                                } else { self.push(ts, Body::Block { block, kind: Kind::Text, text: text.to_owned() }); }
                            },
                            "thinking" => if let Some(text) = b["thinking"].as_str() { self.push(ts, Body::Block { block, kind: Kind::Thinking, text: text.to_owned() }); },
                            "tool_use" => self.push(ts, Body::ToolCall { block: b["id"].as_str().unwrap_or(&block).to_owned(), name: b["name"].as_str().unwrap_or("tool").to_owned(), input: b["input"].clone() }),
                            "tool_result" => self.push(ts, Body::ToolResult { block: b["tool_use_id"].as_str().unwrap_or(&block).to_owned(), text: content_text(&b["content"]), error: b["is_error"].as_bool().unwrap_or(false) }),
                            _ => {}
                        }
                    }
                }
                if let Some(stop) = message["stop_reason"].as_str().filter(|s| matches!(*s, "end_turn" | "stop_sequence")) {
                    self.end(ts, true, stop, None, String::new());
                }
            }
            "system" if row["subtype"] == "turn_duration" => self.end(ts, true, "end_turn", row["durationMs"].as_u64(), String::new()),
            // Progress, API snapshots, filesystem snapshots and attachments are structural records, not conversation messages.
            _ => {}
        }
        Ok(())
    }

    fn codex(&mut self, row: &Value, n: usize) -> Result<(), String> {
        let ts = timestamp(row).unwrap_or(self.source.modified_ms);
        let p = &row["payload"];
        match row["type"].as_str().unwrap_or("") {
            "session_meta" => {
                if let Some(id) = p["id"].as_str().or_else(|| p["session_id"].as_str()) { self.session.chat_id = id.to_owned(); }
                if let Some(cwd) = p["cwd"].as_str() { self.session.cwd = cwd.to_owned(); }
                if let Some(title) = p["title"].as_str().or_else(|| p["name"].as_str()) { self.title(title, 4); }
            }
            "turn_context" => {
                if let Some(model) = p["model"].as_str() { self.session.model = model.to_owned(); }
                if let Some(effort) = p["effort"].as_str().or_else(|| p["model_reasoning_effort"].as_str()) { self.session.effort = effort.to_owned(); }
                if let Some(cwd) = p["cwd"].as_str() { self.session.cwd = cwd.to_owned(); }
            }
            "response_item" => {
                let block = p["id"].as_str().map(str::to_owned).unwrap_or_else(|| format!("codex-line-{n}"));
                match p["type"].as_str().unwrap_or("") {
                    "message" if p["role"] == "user" => {
                        let text = content_text(&p["content"]);
                        if !text.is_empty() { self.title(&text, 1); self.push(ts, Body::User { text, images: Vec::new() }); }
                    }
                    "message" if p["role"] == "assistant" => {
                        let text = content_text(&p["content"]);
                        if !text.is_empty() { self.push(ts, Body::Block { block, kind: Kind::Text, text }); }
                    }
                    "reasoning" => {
                        let text = content_text(&p["summary"]);
                        if !text.is_empty() { self.push(ts, Body::Block { block, kind: Kind::Thinking, text }); }
                    }
                    "function_call" | "custom_tool_call" | "mcp_tool_call" => {
                        let input = p.get("arguments").or_else(|| p.get("input")).cloned().unwrap_or(Value::Null);
                        let input = if let Some(s) = input.as_str() { serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_owned())) } else { input };
                        self.push(ts, Body::ToolCall { block: p["call_id"].as_str().unwrap_or(&block).to_owned(), name: p["name"].as_str().unwrap_or("tool").to_owned(), input });
                    }
                    "function_call_output" | "custom_tool_call_output" | "mcp_tool_call_output" => self.push(ts, Body::ToolResult { block: p["call_id"].as_str().unwrap_or(&block).to_owned(), text: content_text(&p["output"]), error: false }),
                    "agent_message" => {
                        let text = content_text(&p["content"]);
                        if !text.is_empty() { self.push(ts, Body::Note { text }); }
                    }
                    _ => {}
                }
            }
            "event_msg" => match p["type"].as_str().unwrap_or("") {
                "task_started" => self.push(ts, Body::Busy { busy: true }),
                "task_complete" => self.end(ts, true, "end_turn", p["duration_ms"].as_u64(), p["last_agent_message"].as_str().unwrap_or("").to_owned()),
                "turn_aborted" => self.end(ts, false, "interrupted", None, String::new()),
                "thread_name_updated" => if let Some(title) = p["name"].as_str().or_else(|| p["title"].as_str()) { self.title(title, 4); },
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, ts: u64, ok: bool, stop: &str, duration_ms: Option<u64>, text: String) {
        self.push(ts, Body::TurnEnd { ok, stop: stop.to_owned(), duration_ms, cost_usd: None, usage: Value::Null, text });
    }

    fn harness(&mut self, bytes: &[u8]) -> Result<(), String> {
        const MAGIC: &[u8] = b"harness-chat/1\n";
        if !bytes.starts_with(MAGIC) {
            return Err(format!("chat source {} has no harness-chat/1 header", self.source.path.display()));
        }
        let mut at = MAGIC.len();
        while at < bytes.len() {
            let frame = harness_proto::frame::split(&bytes[at..]).map_err(|e| format!("chat source {} byte {at}: {e}", self.source.path.display()))?;
            let Some((ty, payload, used)) = frame else { break };
            self.harness_event(ty, payload, at as u64)?;
            at += used;
        }
        Ok(())
    }

    pub(super) fn harness_event(&mut self, ty: u8, payload: &[u8], at: u64) -> Result<(), String> {
        if ty != harness_proto::frame::T_EVENT {
            return Err(format!("chat source {} byte {at}: expected an event frame, got type {ty}", self.source.path.display()));
        }
        let event: Event = harness_proto::cbor::decode(payload).map_err(|e| format!("chat source {} byte {at}: {e}", self.source.path.display()))?;
        match &event.body {
            Body::Session { agent, model, session_id, effort } => {
                self.session.agent = agent.clone();
                self.session.model = model.clone();
                if let Some(id) = session_id { self.session.chat_id = id.clone(); }
                if let Some(effort) = effort { self.session.effort = effort.clone(); }
            }
            Body::Choice { model, effort } => {
                if let Some(model) = model { self.session.model = model.clone(); }
                if let Some(effort) = effort { self.session.effort = effort.clone(); }
            }
            Body::User { text, .. } => self.title(text, 1),
            _ => {}
        }
        self.push(event.ts, event.body);
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<Parsed, String> {
        if self.session.chat_id.is_empty() {
            if let Some(id) = self.source.harness_id {
                self.session.chat_id = format!("s{id}");
            } else {
                let stem = self.source.path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let id = if self.source.agent == "codex" { stem.get(stem.len().saturating_sub(36)..).unwrap_or("") } else { stem };
                if id.is_empty() {
                    return Err(format!("chat source {} contains no conversation id", self.source.path.display()));
                }
                self.session.chat_id = id.to_owned();
            }
        }
        if !self.subagent.is_empty() { self.session.chat_id.push_str(&format!("/agent:{}", self.subagent)); }
        self.session.key = if let Some(id) = self.session.harness_id { format!("harness:s{id}") } else { format!("{}:{}", self.session.agent, self.session.chat_id) };
        if self.session.title.is_empty() { self.session.title = format!("{} chat {}", self.session.agent, self.session.chat_id); }
        if self.source.agent != "harness" {
            self.events.insert(0, Event { ts: self.events.first().map_or(self.source.modified_ms, |e| e.ts), body: Body::Session { agent: self.session.agent.clone(), model: self.session.model.clone(), session_id: Some(self.session.chat_id.clone()), effort: (!self.session.effort.is_empty()).then(|| self.session.effort.clone()) } });
        }
        Ok(Parsed { session: self.session, events: self.events, text: self.text })
    }
}

fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(content_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n"),
        Value::Object(o) => o.get("text").or_else(|| o.get("content")).map(content_text).unwrap_or_default(),
        _ => String::new(),
    }
}

fn timestamp(v: &Value) -> Option<u64> {
    if let Some(n) = v["timestamp"].as_u64() { return Some(n); }
    let s = v["timestamp"].as_str()?;
    let number = |a, b| s.get(a..b)?.parse::<i64>().ok();
    let (y, m, d, h, minute, sec) = (number(0, 4)?, number(5, 7)?, number(8, 10)?, number(11, 13)?, number(14, 16)?, number(17, 19)?);
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    let frac: String = s.get(20..).filter(|_| s.as_bytes().get(19) == Some(&b'.')).unwrap_or("").chars().take_while(char::is_ascii_digit).take(3).collect();
    let ms = if frac.is_empty() { 0 } else { format!("{frac:0<3}").parse().ok()? };
    u64::try_from(((days * 24 + h) * 60 + minute) * 60_000 + sec * 1000 + ms).ok()
}
