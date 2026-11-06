//! What a chat's context holds, part by part: the `parts` and `last` of a `context` event (harness-proto's
//! `Body::Context`), the chat footer meter's popover. Each part says whether it is the agent's own count or an estimate
//! of harness's, and which:
//!
//! - claude answers a `get_context_usage` control request (its `/context`) with its categories (system prompt, system
//!   and MCP tools, MCP server instructions, memory files, skills, messages) and a split of the messages (user,
//!   assistant, tool calls, tool results), each counted with Anthropic's `count_tokens` endpoint; those are claude's
//!   counts ([`claude_parts`]). Counted one part at a time, they add up to a little more than the call's exact input.
//! - codex says only its exact total and its split (input, cached, output, reasoning). Harness counts what it can see
//!   itself with the o200k_base tokenizer ([`Ledger`]): its system note's topics, and the messages that went by. The rest
//!   (codex's own prompt and tool definitions, a resumed thread's history, a compaction's summary) is the exact total
//!   less those, named as such.
//! - The harness note's topics (`profiles::SYSTEM_NOTE_PARTS`, one file each under `system_note/`) are always counted by
//!   harness with o200k_base ([`note_parts`]): neither agent splits its system prompt. o200k_base is OpenAI's tokenizer
//!   (GPT-5's); Claude's is not public, so for claude its counts are labelled estimates.
//!
//! Pure functions of what the adapters read; no IO.
use harness_proto::event::{Body, ContextCall, ContextPart, Kind};
use serde_json::Value;

const O200K: &str = ContextPart::O200K;
const REST: &str = ContextPart::REST;

/// `text`'s tokens under o200k_base.
#[must_use]
pub fn count(text: &str) -> u64 {
    tiktoken_rs::o200k_base_singleton().encode_ordinary(text).len() as u64
}

/// The harness note's topics, each a split of the part `of` (claude's `system_prompt`, codex's `note`).
#[must_use]
pub fn note_parts(of: &str) -> Vec<ContextPart> {
    crate::profiles::SYSTEM_NOTE_PARTS
        .iter()
        .map(|(topic, text)| ContextPart {
            id: format!("note.{topic}"),
            name: format!("{topic}.md"),
            tokens: count(text.trim()),
            of: of.to_owned(),
            estimate: O200K.to_owned(),
        })
        .collect() // alloc: five rows once a turn
}

fn part(id: &str, name: &str, tokens: u64, of: &str, estimate: &str) -> ContextPart {
    ContextPart { id: id.to_owned(), name: name.to_owned(), tokens, of: of.to_owned(), estimate: estimate.to_owned() }
}

/// The parts of claude's `get_context_usage` answer (`detail: "full"`): its used categories in its order, the harness
/// note's topics under its system prompt, and its split of the messages under them. Claude leaves out the messages
/// category when the split is all it has (after a turn, as 2.1.319 does): then the split's sum stands for it.
///
/// Its messages category is exact (2.1.319: the call's total less the other categories), its split is not: it counts
/// every entry of its transcript, and its `attachmentTokens` sums every attachment there, the memory files, skill and
/// deferred-tool listings that the categories already count among them, so it can pass the messages it is said to be a
/// part of (a popover read "Reminders 9.2k" under "Messages 1.1k" of a 7.3k context). It is never shown. What the
/// messages hold past the split's user, assistant, tool call and tool result counts (the reminders a turn carries: the
/// date, a listing that changed) is the messages less those, `messages.other`, so the rows under it add up to it.
#[must_use]
pub fn claude_parts(a: &Value) -> Vec<ContextPart> {
    let mut out = Vec::new(); // alloc: the popover's rows once a turn
    let mut messages = None;
    for c in a["categories"].as_array().into_iter().flatten().filter(|c| c["kind"] == "used") {
        let (Some(id), Some(tokens)) = (c["id"].as_str(), c["tokens"].as_u64()) else { continue };
        out.push(part(id, c["name"].as_str().unwrap_or(id), tokens, "", ""));
        if id == "system_prompt" {
            out.extend(note_parts("system_prompt"));
        }
        if id == "messages" {
            messages = Some(tokens);
        }
    }
    let b = &a["messageBreakdown"];
    let n = |k: &str| b[k].as_u64().unwrap_or(0);
    let split = [
        ("messages.user", "User", n("userMessageTokens")),
        ("messages.assistant", "Assistant", n("assistantMessageTokens")),
        ("messages.tool_calls", "Tool calls", n("toolCallTokens")),
        ("messages.tool_results", "Tool results", n("toolResultTokens")),
    ];
    let said: u64 = split.iter().map(|s| s.2).sum();
    if messages.is_none() && said > 0 {
        out.push(part("messages", "Messages", said, "", ""));
    }
    if messages.is_some() || said > 0 {
        out.extend(split.iter().filter(|s| s.2 > 0).map(|&(id, name, t)| part(id, name, t, "messages", "")));
    }
    if let Some(other) = messages.map(|m| m.saturating_sub(said)).filter(|o| *o > 0) {
        out.push(part("messages.other", "Reminders and other", other, "messages", ""));
    }
    out
}

/// Claude's last call's split, from its usage (`input_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`,
/// `output_tokens`), with the turn's thinking (`thinking`, from the turn's `result`).
#[must_use]
pub fn claude_call(u: &Value, thinking: Option<u64>) -> ContextCall {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    ContextCall {
        input: n("input_tokens"),
        cache_read: n("cache_read_input_tokens"),
        cache_write: n("cache_creation_input_tokens"),
        output: n("output_tokens"),
        thinking,
    }
}

/// Codex's last call's split, from `thread/tokenUsage/updated`'s `last` (`inputTokens` counts the cached ones too).
#[must_use]
pub fn codex_call(last: &Value) -> ContextCall {
    let n = |k: &str| last[k].as_u64().unwrap_or(0);
    ContextCall {
        input: n("inputTokens").saturating_sub(n("cachedInputTokens")),
        cache_read: n("cachedInputTokens"),
        cache_write: 0,
        output: n("outputTokens"),
        thinking: last["reasoningOutputTokens"].as_u64(),
    }
}

/// The messages of a conversation as they went by, counted with o200k_base: what harness can say of an agent that
/// splits nothing (codex). Only whole forms count (a block, a call, a result, a user message), never partials.
#[derive(Default)]
pub struct Ledger {
    user: u64,
    assistant: u64,
    /// The thinking harness saw: codex's summaries (its reasoning itself is encrypted, and not counted here).
    thinking: u64,
    calls: u64,
    results: u64,
}

impl Ledger {
    /// The person's message.
    pub fn user(&mut self, text: &str) {
        self.user += count(text);
    }

    /// An event the agent's adapter made.
    pub fn see(&mut self, b: &Body) {
        match b {
            Body::Block { kind: Kind::Text, text, .. } => self.assistant += count(text),
            Body::Block { kind: Kind::Thinking, text, .. } => self.thinking += count(text),
            Body::ToolCall { name, input, .. } => self.calls += count(name) + count(&input.to_string()),
            Body::ToolResult { text, .. } => self.results += count(text),
            _ => {}
        }
    }

    /// The context was compacted: what it held of the messages is a summary now, part of the rest.
    pub fn compacted(&mut self) {
        *self = Self::default();
    }

    /// The parts of a context of `used` tokens (the agent's exact total): the harness note and its topics, the messages
    /// and their split, then the rest, `rest` naming what it is.
    #[must_use]
    pub fn parts(&self, used: u64, rest: &str) -> Vec<ContextPart> {
        let topics = note_parts("note");
        let note: u64 = topics.iter().map(|p| p.tokens).sum();
        let split = [
            ("messages.user", "User", self.user),
            ("messages.assistant", "Assistant", self.assistant),
            ("messages.thinking", "Thinking summaries", self.thinking),
            ("messages.tool_calls", "Tool calls", self.calls),
            ("messages.tool_results", "Tool results", self.results),
        ];
        let messages: u64 = split.iter().map(|s| s.2).sum();
        let mut out = Vec::with_capacity(topics.len() + split.len() + 3); // alloc: the popover's rows once a turn
        out.push(part("note", "Harness note", note, "", O200K));
        out.extend(topics);
        if messages > 0 {
            out.push(part("messages", "Messages", messages, "", O200K));
            out.extend(split.iter().filter(|s| s.2 > 0).map(|&(id, name, t)| part(id, name, t, "messages", O200K)));
        }
        out.push(part("rest", rest, used.saturating_sub(note + messages), "", REST));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Claude's answer as 2.1.319 gives it after a turn: no messages category, the split stands for it, the note's topics
    /// under the system prompt, nothing of claude's labelled an estimate.
    #[test]
    fn claude_answer_after_a_turn() {
        let a = json!({
            "categories": [
                {"id": "system_prompt", "name": "System prompt", "tokens": 3796, "kind": "used"},
                {"id": "mcp_tools", "name": "MCP tools", "tokens": 6201, "kind": "used"},
                {"id": "autocompact_buffer", "name": "Autocompact buffer", "tokens": 33000, "kind": "buffer"},
                {"id": "free_space", "name": "Free space", "tokens": 234830, "kind": "free"},
            ],
            "messageBreakdown": {"toolCallTokens": 0, "toolResultTokens": 0, "attachmentTokens": 33240,
                "assistantMessageTokens": 11, "userMessageTokens": 11},
        });
        let p = claude_parts(&a);
        let ids: Vec<&str> = p.iter().map(|p| p.id.as_str()).collect();
        let topics = crate::profiles::SYSTEM_NOTE_PARTS.len();
        assert_eq!(ids[0], "system_prompt");
        assert!(ids[1..=topics].iter().all(|i| i.starts_with("note.")));
        assert_eq!(&ids[topics + 1..], ["mcp_tools", "messages", "messages.user", "messages.assistant"]);
        assert_eq!(p.iter().find(|p| p.id == "messages").map(|p| p.tokens), Some(22));
        assert!(p.iter().filter(|p| !p.id.starts_with("note.")).all(|p| p.estimate.is_empty()));
        assert!(p.iter().filter(|p| p.id.starts_with("note.")).all(|p| p.estimate == O200K && p.of == "system_prompt"));
    }

    /// The answer that read "Reminders 9.2k" under "Messages 1.1k" of a 7.3k context: claude's transcript-wide attachment
    /// count is not shown; the messages' rows are claude's split and what the messages hold past it, and add up to them.
    #[test]
    fn claude_messages_split_adds_up() {
        let a = json!({
            "categories": [
                {"id": "system_prompt", "name": "System prompt", "tokens": 3796, "kind": "used"},
                {"id": "mcp_tools", "name": "MCP tools", "tokens": 2400, "kind": "used"},
                {"id": "messages", "name": "Messages", "tokens": 1100, "kind": "used"},
            ],
            "messageBreakdown": {"toolCallTokens": 120, "toolResultTokens": 80, "attachmentTokens": 9200,
                "assistantMessageTokens": 300, "userMessageTokens": 400},
        });
        let p = claude_parts(&a);
        let under: Vec<(&str, u64)> = p.iter().filter(|p| p.of == "messages").map(|p| (p.id.as_str(), p.tokens)).collect();
        assert_eq!(
            under,
            [
                ("messages.user", 400),
                ("messages.assistant", 300),
                ("messages.tool_calls", 120),
                ("messages.tool_results", 80),
                ("messages.other", 200)
            ]
        );
        assert_eq!(under.iter().map(|u| u.1).sum::<u64>(), 1100);
        assert!(p.iter().all(|p| p.tokens != 9200));
    }

    /// Codex's parts add up to its exact total: the rest is what harness's counts leave.
    #[test]
    fn codex_parts_add_up() {
        let mut l = Ledger::default();
        l.user("count the files in src");
        l.see(&Body::ToolCall { block: "c".into(), name: "ts".into(), input: json!({"code": "await glob('src/*')"}) });
        l.see(&Body::ToolResult { block: "c".into(), text: "a.rs\nb.rs\nc.rs".into(), error: false });
        l.see(&Body::Block { block: "m".into(), kind: Kind::Text, text: "3 files.".into() });
        let used = 20_000;
        let p = l.parts(used, "Codex's prompt and tools");
        let whole: u64 = p.iter().filter(|p| p.of.is_empty()).map(|p| p.tokens).sum();
        assert_eq!(whole, used);
        assert_eq!(p.last().map(|p| p.estimate.as_str()), Some(REST));
        l.compacted();
        assert!(l.parts(used, "rest").iter().all(|p| !p.id.starts_with("messages")));
    }
}
