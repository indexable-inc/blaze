//! A chat's agent-neutral events: what the daemon records, a chat attach streams and a chat log holds (the table of
//! what each means is in crates/harness/harness/src/chat/mod.rs). Each is one [`crate::frame::T_EVENT`] frame, a map
//! with `ev` naming it and `ts` (ms since the epoch) beside its fields.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One event: when, and what.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Event {
    /// Ms since the epoch, stamped as the daemon records it.
    #[serde(default)]
    pub ts: u64,
    #[serde(flatten)]
    pub body: Body,
}

/// A text or thinking block.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Text,
    Thinking,
}

/// What happened. A `block` names one assistant block (text, thinking) or one tool call, unique in the session.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Body {
    /// The agent started (or resumed) with this model; `session_id` is its own id for the conversation, what a resume
    /// names. `effort` is the reasoning effort it runs with (claude's `--effort`, codex's `model_reasoning_effort`), when
    /// known.
    Session {
        agent: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// The models the agent offers, each with the efforts it takes, in its own order: its own list (claude's
    /// `list_models` control request, codex's `model/list`), asked for as it starts. What the composer's pickers list
    /// and what a [`Body::Choice`] is checked against.
    Models { models: Vec<ModelChoice> },
    /// The person chose the model (a [`ModelChoice::id`]) and/or the effort the turns from here on run with (written by
    /// the daemon as it asks the agent for it, before the agent's `session` event confirms it). A resume starts the agent
    /// with the latest of each.
    Choice {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// How much of the model's context window the conversation fills (tokens), as the agent last reported it: claude's
    /// last call of a turn (its input, cache and output tokens) against `modelUsage`'s `contextWindow`; codex's
    /// `thread/tokenUsage/updated` (`last.totalTokens` against `modelContextWindow`). `window` is 0 when the agent did not
    /// say.
    Context { used: u64, window: u64 },
    /// The person's message (written by the daemon as it sends it).
    User { text: String },
    /// A turn started (true) or ended (false).
    Busy { busy: bool },
    /// A block began streaming (partial).
    BlockStart { block: String, kind: Kind },
    /// More of a streaming block (partial).
    Delta { block: String, text: String },
    /// A block, whole (replaces its partials).
    Block { block: String, kind: Kind, text: String },
    /// A tool call began, its arguments streaming (partial).
    ToolStart { block: String, name: String },
    /// More of its arguments' JSON text (partial).
    ToolDelta { block: String, json: String },
    /// The call, whole (replaces its partials); `input` as the agent gave it.
    ToolCall { block: String, name: String, input: Value },
    /// What the call returned.
    ToolResult { block: String, text: String, error: bool },
    /// What a harness REPL cell the call ran did, whole each time.
    ToolDetail(Detail),
    /// Something said about the conversation that is not the assistant's words (an interrupt).
    Note { text: String },
    /// The agent reported an error, or the daemon did about this chat.
    Error { text: String },
    /// The turn is over: how (`stop`: end_turn | interrupted | error | ...), how long, what it cost (the turn's own;
    /// none when the agent says none), the usage the agent reported, and its last assistant words.
    TurnEnd {
        ok: bool,
        stop: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        #[serde(default)]
        usage: Value,
        #[serde(default)]
        text: String,
    },
}

/// One model an agent offers ([`Body::Models`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct ModelChoice {
    /// What a switch names (claude's alias, `opus`; codex's model id).
    pub id: String,
    /// The model it runs as (`claude-opus-5-5`), what the agent's `session` event names; empty when the agent did not say.
    #[serde(default)]
    pub model: String,
    /// Its name as the agent shows it (`Opus 5.5`).
    #[serde(default)]
    pub name: String,
    /// The agent's line about it.
    #[serde(default)]
    pub about: String,
    /// The reasoning efforts it takes, lowest first; none when it takes no effort.
    #[serde(default)]
    pub efforts: Vec<String>,
    /// The effort it runs with when none is chosen, when the agent says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl ModelChoice {
    /// Whether this is the model a `session` event (or a choice) names: by its id or by the model it runs as.
    #[must_use]
    pub fn is(&self, model: &str) -> bool {
        !model.is_empty() && (self.id == model || self.model == model)
    }
}

/// A REPL cell's state, as a `tool_detail` event carries it (mcp/src/effects.ts and shown.ts shape its parts).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Detail {
    pub block: String,
    /// `<repl>.<task>`.
    pub cell: String,
    /// running | done | error.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub effects: Vec<Effect>,
    /// Steps past the kernel's cap, not kept.
    #[serde(default)]
    pub effects_dropped: u64,
    /// The cell's value as the app shows it (shown.ts), or null.
    #[serde(default)]
    pub value: Value,
    /// What the cell draws (mcp/src/ui/), or null.
    #[serde(default)]
    pub ui: Value,
    #[serde(default)]
    pub logs: String,
}

/// One step a cell took outside the REPL (mcp/src/effects.ts).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Effect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    /// exec | read | write | delete | net | search | spawn | sql.
    #[serde(default)]
    pub kind: String,
    /// Its words while it runs and once it is done.
    #[serde(default)]
    pub doing: String,
    #[serde(default)]
    pub done: String,
    #[serde(default)]
    pub target: String,
    /// running | ok | error.
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    /// "exit 0", "5 results", "3.2 KB".
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub value: Value,
}

impl Event {
    /// This event as its [`crate::frame::T_EVENT`] frame's bytes: what a chat attach sends and its log holds.
    #[must_use]
    pub fn frame(&self) -> Vec<u8> {
        crate::frame::frame(crate::frame::T_EVENT, &crate::cbor::encode(self))
    }

    /// The event a JSON object is, read as the reducer before schema 1 read it: a field missing or of another type is
    /// its empty value (a string `""`, a flag false, a number none). None for an object that names no `ev`, or an `ev`
    /// this schema has no event for. The one place the NDJSON of before schema 1 is understood (the migration, and the
    /// kernel's REPL cell posts for [`Detail::of_task`]).
    #[must_use]
    pub fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
        let block = || s("block");
        let kind = || if v["kind"] == "thinking" { Kind::Thinking } else { Kind::Text };
        let body = match v["ev"].as_str()? {
            "session" => Body::Session {
                agent: s("agent"),
                model: s("model"),
                session_id: v["session_id"].as_str().filter(|i| !i.is_empty()).map(str::to_owned),
                effort: v["effort"].as_str().filter(|i| !i.is_empty()).map(str::to_owned),
            },
            "models" => Body::Models {
                models: v["models"].as_array().into_iter().flatten().map(|m| ModelChoice {
                    id: m["id"].as_str().unwrap_or("").to_owned(),
                    model: m["model"].as_str().unwrap_or("").to_owned(),
                    name: m["name"].as_str().unwrap_or("").to_owned(),
                    about: m["about"].as_str().unwrap_or("").to_owned(),
                    efforts: m["efforts"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
                    effort: m["effort"].as_str().map(str::to_owned),
                }).collect(),
            },
            "choice" => Body::Choice { model: v["model"].as_str().map(str::to_owned), effort: v["effort"].as_str().map(str::to_owned) },
            "context" => Body::Context { used: v["used"].as_u64().unwrap_or(0), window: v["window"].as_u64().unwrap_or(0) },
            "user" => Body::User { text: s("text") },
            "busy" => Body::Busy { busy: v["busy"].as_bool().unwrap_or(false) },
            "block_start" => Body::BlockStart { block: block(), kind: kind() },
            "delta" => Body::Delta { block: block(), text: s("text") },
            "block" => Body::Block { block: block(), kind: kind(), text: s("text") },
            "tool_start" => Body::ToolStart { block: block(), name: s("name") },
            "tool_delta" => Body::ToolDelta { block: block(), json: s("json") },
            "tool_call" => Body::ToolCall { block: block(), name: s("name"), input: v["input"].clone() },
            "tool_result" => {
                Body::ToolResult { block: block(), text: s("text"), error: v["error"].as_bool().unwrap_or(false) }
            }
            "tool_detail" => Body::ToolDetail(Detail {
                block: block(),
                cell: s("cell"),
                status: s("status"),
                ms: v["ms"].as_u64(),
                code: s("code"),
                effects: v["effects"].as_array().into_iter().flatten().map(Effect::from_json).collect(),
                effects_dropped: v["effects_dropped"].as_u64().unwrap_or(0),
                value: v["value"].clone(),
                ui: v["ui"].clone(),
                logs: s("logs"),
            }),
            "note" => Body::Note { text: s("text") },
            "error" => Body::Error { text: s("text") },
            "turn_end" => Body::TurnEnd {
                ok: v["ok"].as_bool().unwrap_or(false),
                stop: s("stop"),
                duration_ms: v["duration_ms"].as_u64(),
                cost_usd: v["cost_usd"].as_f64(),
                usage: v["usage"].clone(),
                text: s("text"),
            },
            _ => return None,
        };
        Some(Self { ts: v["ts"].as_u64().unwrap_or(0), body })
    }
}

impl Effect {
    /// A step as the kernel's JSON shapes it (effects.ts), each field its empty value when missing.
    #[must_use]
    pub fn from_json(e: &Value) -> Self {
        let s = |k: &str| e[k].as_str().unwrap_or("").to_owned();
        Self {
            n: e["n"].as_u64(),
            kind: s("kind"),
            doing: s("doing"),
            done: s("done"),
            target: s("target"),
            status: s("status"),
            started_ms: e["started_ms"].as_u64(),
            ms: e["ms"].as_u64(),
            detail: s("detail"),
            value: e["value"].clone(),
        }
    }
}

impl Detail {
    /// The `tool_detail` of tool call `block` from a REPL cell as the kernel posts it (`repl_task`, mcp/src/tasks.ts).
    #[must_use]
    pub fn of_task(block: &str, task: &Value) -> Self {
        let s = |k: &str| task[k].as_str().unwrap_or("").to_owned();
        Self {
            block: block.to_owned(),
            cell: format!("{}.{}", task["repl"].as_str().unwrap_or("repl"), task["id"].as_str().unwrap_or("t?")),
            status: s("status"),
            ms: match (task["started_ms"].as_u64(), task["ended_ms"].as_u64()) {
                (Some(a), Some(b)) => Some(b.saturating_sub(a)),
                _ => None,
            },
            code: s("code"),
            effects: task["effects"].as_array().into_iter().flatten().map(Effect::from_json).collect(),
            effects_dropped: task["effects_dropped"].as_u64().unwrap_or(0),
            value: task["value"].clone(),
            ui: task["ui"].clone(),
            logs: s("logs"),
        }
    }
}

impl Body {
    /// Its `ev` name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Session { .. } => "session",
            Self::Models { .. } => "models",
            Self::Choice { .. } => "choice",
            Self::Context { .. } => "context",
            Self::User { .. } => "user",
            Self::Busy { .. } => "busy",
            Self::BlockStart { .. } => "block_start",
            Self::Delta { .. } => "delta",
            Self::Block { .. } => "block",
            Self::ToolStart { .. } => "tool_start",
            Self::ToolDelta { .. } => "tool_delta",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::ToolDetail(_) => "tool_detail",
            Self::Note { .. } => "note",
            Self::Error { .. } => "error",
            Self::TurnEnd { .. } => "turn_end",
        }
    }

    /// The block (or tool call) it is about, if any.
    #[must_use]
    pub fn block(&self) -> Option<&str> {
        match self {
            Self::BlockStart { block, .. }
            | Self::Delta { block, .. }
            | Self::Block { block, .. }
            | Self::ToolStart { block, .. }
            | Self::ToolDelta { block, .. }
            | Self::ToolCall { block, .. }
            | Self::ToolResult { block, .. }
            | Self::ToolDetail(Detail { block, .. }) => Some(block),
            _ => None,
        }
    }

    /// A partial a whole block replaces.
    #[must_use]
    pub fn partial(&self) -> bool {
        matches!(self, Self::BlockStart { .. } | Self::Delta { .. } | Self::ToolStart { .. } | Self::ToolDelta { .. })
    }

    /// It starts a block's partials (the place in the conversation its whole form takes).
    #[must_use]
    pub fn starts(&self) -> bool {
        matches!(self, Self::BlockStart { .. } | Self::ToolStart { .. })
    }

    /// A block that came whole (it replaces its partials).
    #[must_use]
    pub fn whole(&self) -> bool {
        matches!(self, Self::Block { .. } | Self::ToolCall { .. })
    }
}
