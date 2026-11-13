use super::*;
use harness_proto::event::Body;
use serde_json::Value;
use serde_json::json;

fn source(agent: &str) -> Source {
    Source { path: PathBuf::from(format!("/{agent}/11111111-2222-3333-4444-555555555555.jsonl")), agent: agent.to_owned(), modified_ms: 123, len: 0, harness_id: None }
}

fn jsonl(rows: &[Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for row in rows {
        bytes.extend(serde_json::to_vec(row).unwrap());
        bytes.push(b'\n');
    }
    bytes
}

/// Real Claude record shapes produce one canonical conversation, not copies of its API snapshots.
#[test]
fn claude_records_normalize_without_api_snapshot_duplicates() {
    let rows = [
        json!({"type":"user","uuid":"u1","sessionId":"claude-1","cwd":"/project","timestamp":"2026-11-13T10:00:00.123Z","message":{"role":"user","content":"Find the allocator bug"}}),
        json!({"type":"api-request-blob","sessionId":"claude-1","message":{"role":"user","content":"Find the allocator bug"}}),
        json!({"type":"assistant","uuid":"a1","sessionId":"claude-1","perTurnEffort":"high","message":{"model":"claude-opus-5-5","role":"assistant","content":[{"type":"thinking","thinking":"Inspect allocation ownership"},{"type":"tool_use","id":"tool-1","name":"Read","input":{"file":"alloc.rs"}}]}}),
        json!({"type":"user","sessionId":"claude-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":[{"type":"text","text":"owner drops here"}]}]}}),
        json!({"type":"summary","sessionId":"claude-1","summary":"Allocator ownership"}),
        json!({"type":"custom-title","sessionId":"claude-1","customTitle":"Fix task allocator"}),
    ];
    let parsed = parse(&source("claude"), &jsonl(&rows)).unwrap();
    assert_eq!((parsed.session.key.as_str(), parsed.session.title.as_str(), parsed.session.cwd.as_str(), parsed.session.effort.as_str()), ("claude:claude-1", "Fix task allocator", "/project", "high"));
    assert_eq!(parsed.events.iter().filter(|e| matches!(e.body, Body::User { .. })).count(), 1);
    assert!(parsed.events.iter().any(|e| matches!(&e.body, Body::ToolCall { block, name, .. } if block == "tool-1" && name == "Read")));
    assert!(parsed.events.iter().any(|e| matches!(&e.body, Body::ToolResult { block, text, .. } if block == "tool-1" && text == "owner drops here")));
    assert!(parsed.text.contains("Inspect allocation ownership") && parsed.text.contains("alloc.rs"));
}

/// A Claude subagent shares its parent's sessionId but must remain a separate selectable conversation.
#[test]
fn claude_subagents_do_not_collapse_into_the_parent() {
    let rows = jsonl(&[json!({"type":"user","sessionId":"parent","agentId":"worker-a","isSidechain":true,"message":{"content":"Review this file"}})]);
    let parsed = parse(&source("claude"), &rows).unwrap();
    assert_eq!(parsed.session.key, "claude:parent/agent:worker-a");
}

/// Each prefix of an active append preserves the complete earlier messages; a complete malformed record is refused.
#[test]
fn active_jsonl_appends_keep_only_complete_records() {
    let first = jsonl(&[json!({"type":"session_meta","payload":{"id":"codex-1","cwd":"/project"}})]);
    let next = serde_json::to_vec(&json!({"type":"response_item","payload":{"type":"message","id":"m1","role":"assistant","content":[{"type":"output_text","text":"Found it"}]}})).unwrap();
    for cut in 1..next.len() {
        let mut bytes = first.clone();
        bytes.extend(&next[..cut]);
        let parsed = parse(&source("codex"), &bytes).unwrap();
        assert!(!parsed.events.iter().any(|e| matches!(e.body, Body::Block { .. })), "partial append at {cut}");
        assert_eq!(parsed.session.chat_id, "codex-1");
    }
    let mut bytes = first.clone();
    bytes.extend(&next);
    assert!(parse(&source("codex"), &bytes).unwrap().events.iter().any(|e| matches!(e.body, Body::Block { .. })));
    bytes.extend_from_slice(b"\n{bad}\n");
    let error = parse(&source("codex"), &bytes).err().unwrap();
    assert!(error.contains("line 3") && error.contains("codex"));
}

/// Codex's rollout metadata, reasoning and calls are searchable without indexing injected developer instructions.
#[test]
fn codex_rollouts_keep_conversation_and_tool_identity() {
    let rows = jsonl(&[
        json!({"type":"session_meta","payload":{"id":"thread-1","cwd":"/project","base_instructions":"Never index this injected prompt"}}),
        json!({"type":"turn_context","payload":{"model":"gpt-6.1-sol","effort":"high"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"Hidden instructions"}]}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Debug page admission"}]}}),
        json!({"type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"exec","arguments":"{\"cmd\":\"ls\"}"}}),
        json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"page.rs"}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Admission fixed","duration_ms":120}}),
    ]);
    let parsed = parse(&source("codex"), &rows).unwrap();
    assert_eq!((parsed.session.key.as_str(), parsed.session.title.as_str(), parsed.session.model.as_str(), parsed.session.effort.as_str()), ("codex:thread-1", "Debug page admission", "gpt-6.1-sol", "high"));
    assert!(parsed.text.contains("page.rs") && parsed.text.contains("Admission fixed"));
    assert!(!parsed.text.contains("Hidden instructions") && !parsed.text.contains("Never index"));
    assert!(parsed.events.iter().any(|e| matches!(&e.body, Body::ToolCall { block, input, .. } if block == "call-1" && input["cmd"] == "ls")));
}

/// The provider's conversation id survives Harness's own session number, allowing the index to deduplicate it.
#[test]
fn harness_cbor_keeps_provider_identity_and_tolerates_a_torn_tail() {
    let first = Event { ts: 10, body: Body::Session { agent: "claude".into(), model: "claude-fable-5-2".into(), session_id: Some("provider-1".into()), effort: Some("medium".into()) } };
    let second = Event { ts: 11, body: Body::User { text: "Fix the store".into(), images: Vec::new() } };
    let mut bytes = b"harness-chat/1\n".to_vec();
    bytes.extend(first.frame());
    let boundary = bytes.len();
    let frame = second.frame();
    let mut source = source("harness");
    source.harness_id = Some(42);
    for cut in 0..frame.len() {
        let mut partial = bytes.clone();
        partial.extend(&frame[..cut]);
        assert_eq!(parse(&source, &partial).unwrap().events, vec![first.clone()]);
    }
    bytes.extend(frame);
    let parsed = parse(&source, &bytes).unwrap();
    assert_eq!(parsed.events, vec![first, second]);
    assert_eq!((parsed.session.key.as_str(), parsed.session.agent.as_str(), parsed.session.chat_id.as_str(), parsed.session.harness_id), ("harness:s42", "claude", "provider-1", Some(42)));
    bytes[boundary] = harness_proto::frame::T_DATA;
    assert!(parse(&source, &bytes).err().unwrap().contains("expected an event frame"));
}

/// The real streaming driver feeds the same pure reducer as snapshot parsing, including an interrupted final append.
#[test]
fn streaming_records_equal_snapshots_across_buffer_boundaries_and_partial_appends() {
    use std::io::{BufReader, Cursor};
    let rows = jsonl(&[
        json!({"type":"user","uuid":"u1","sessionId":"chat-1","message":{"content":"Find λ ownership"}}),
        json!({"type":"assistant","uuid":"a1","message":{"content":[{"type":"text","text":"A borrowed allocation"}]}}),
        json!({"type":"custom-title","sessionId":"chat-1","customTitle":"Ownership"}),
        json!({"type":"custom-title","sessionId":"chat-1","customTitle":"Borrowed ownership"}),
        json!({"type":"user","uuid":"u2","sessionId":"chat-1","message":{"content":"A later request keeps the explicit chat title"}}),
    ]);
    let source = source("claude");
    for capacity in [1, 3, 17, 1024] {
        for suffix in [b"".as_slice(), b"{\"type\":\"assistant\",\"message\":{"] {
            let mut bytes = rows.clone();
            bytes.extend_from_slice(suffix);
            let snapshot = parse(&source, &bytes).unwrap();
            let streamed = read_jsonl(&source, &mut BufReader::with_capacity(capacity, Cursor::new(&bytes))).unwrap();
            assert_eq!(streamed.session, snapshot.session, "buffer capacity {capacity}");
            assert_eq!(streamed.events, snapshot.events, "buffer capacity {capacity}");
            assert_eq!(streamed.text, snapshot.text, "buffer capacity {capacity}");
            assert_eq!(streamed.session.title, "Borrowed ownership");
        }
    }
    let mut bad = rows;
    bad.extend_from_slice(b"{broken}\n");
    assert!(read_jsonl(&source, &mut Cursor::new(bad)).err().unwrap().contains("line 6"));

    let mut source = source;
    source.agent = "harness".into();
    source.harness_id = Some(8);
    let event = Event { ts: 1, body: Body::User { text: "Streaming records".into(), images: Vec::new() } };
    let mut bytes = b"harness-chat/1\n".to_vec();
    bytes.extend(event.frame());
    bytes.extend_from_slice(&[harness_proto::frame::T_EVENT, 0, 0]);
    let snapshot = parse(&source, &bytes).unwrap();
    let streamed = read_harness(&source, &mut BufReader::with_capacity(3, Cursor::new(bytes))).unwrap();
    assert_eq!(streamed.session, snapshot.session);
    assert_eq!(streamed.events, snapshot.events);
    assert_eq!(streamed.text, snapshot.text);
}
