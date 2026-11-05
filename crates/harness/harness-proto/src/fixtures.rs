//! Golden fixtures shared with the kernel: each typed value's deterministic CBOR (`fixtures/<name>.cbor`) and the same
//! value as JSON (`fixtures/<name>.json`). This crate's test checks its encoder writes exactly the checked-in bytes;
//! `mcp/src/wire.test.ts` checks the kernel decodes each `.cbor` to its `.json` and encodes that back to the same bytes.
//! `HARNESS_PROTO_BLESS=1 cargo test -p harness-proto` rewrites them after a deliberate schema change.
use crate::event::{Body, Detail, Effect, Event, Kind};
use crate::msg::{Down, Reply, Request, RunReq, SessionInfo, Up};
use serde::Serialize;
use serde_json::{Value, json};

fn one<T: Serialize>(name: &'static str, v: &T) -> (&'static str, Vec<u8>, Value) {
    (name, crate::cbor::encode(v), serde_json::to_value(v).expect("a fixture is JSON"))
}

/// Every fixture: its name, its bytes, its JSON.
#[must_use]
pub fn all() -> Vec<(&'static str, Vec<u8>, Value)> {
    let info = SessionInfo {
        id: 7,
        object: "s7".into(),
        kind: "claude".into(),
        name: None,
        parent: Some(3),
        title: "count the files".into(),
        cwd: Some("/tmp".into()),
        argv: vec!["claude".into(), "-p".into()],
        created_ms: Some(1_762_300_000_000),
        ended_ms: None,
        exit: None,
        status: Some("running".into()),
        attached: 1,
        cols: Some(120),
        rows: Some(40),
        busy: true,
        mode: "chat".into(),
        model: Some("opus".into()),
        past: false,
    };
    vec![
        one("up-hello", &Up::Hello { schema: crate::SCHEMA }),
        one("up-list", &Up::Req(Request::List { all: false, chats: true })),
        one("up-attach", &Up::Req(Request::Attach { id: 7, cols: Some(80), rows: Some(24), events: true })),
        one(
            "up-run-read",
            &Up::Req(Request::Runs(RunReq::RunRead {
                id: "r12".into(),
                stage: Some(0),
                stream: "ordered".into(),
                offset: 4096,
                max_bytes: None,
                wait_ms: 1000,
            })),
        ),
        one(
            "up-repl-task",
            &Up::Req(Request::ReplTask {
                task: json!({"id": "t3", "repl": "r", "status": "done", "started_ms": 1_762_300_000_000u64,
                    "ended_ms": 1_762_300_000_125u64, "value": {"t": "scalar", "type": "number", "v": 0.5},
                    "effects": [], "neg": -40000, "big": 4_294_967_296u64}),
            }),
        ),
        one("up-resize", &Up::Resize { cols: 300, rows: 70 }),
        one("up-detach", &Up::Detach),
        one("down-hello", &Down::Hello { schema: crate::SCHEMA, pid: 4242 }),
        one("down-sessions", &Down::Reply(Reply::Sessions { sessions: vec![info.clone()] })),
        one("down-attached", &Down::Reply(Reply::Attached { info })),
        one("down-ok", &Down::Reply(Reply::Ok)),
        one("down-record", &Down::Reply(Reply::Record(json!({"id": "r12", "exit_code": 0, "signal": null})))),
        one("down-error", &Down::Error { error: "no session 99".into() }),
        one("down-changed", &Down::Changed { generation: 70_000 }),
        one("down-exit", &Down::Exit { code: -1 }),
        one(
            "event-session",
            &Event { ts: 1, body: Body::Session { agent: "claude".into(), model: "opus".into(), session_id: Some("9f2c".into()) } },
        ),
        one("event-block", &Event { ts: 5, body: Body::Block { block: "m:0".into(), kind: Kind::Thinking, text: "list them\n\u{2026}".into() } }),
        one(
            "event-tool-call",
            &Event { ts: 6, body: Body::ToolCall { block: "t1".into(), name: "ts".into(), input: json!({"code": "1+1", "timeout_ms": 2.0}) } },
        ),
        one(
            "event-tool-detail",
            &Event {
                ts: 7,
                body: Body::ToolDetail(Detail {
                    block: "t1".into(),
                    cell: "r.t1".into(),
                    status: "done".into(),
                    ms: Some(4),
                    code: "await glob('src/*')".into(),
                    effects: vec![Effect { kind: "search".into(), done: "Found 3".into(), status: "ok".into(), ..Effect::default() }],
                    effects_dropped: 0,
                    value: json!({"t": "table", "cols": [{"name": "path", "type": "string"}], "rows": [["a"], ["b"]], "total": 2}),
                    ui: Value::Null,
                    logs: String::new(),
                }),
            },
        ),
        one(
            "event-turn-end",
            &Event {
                ts: 9,
                body: Body::TurnEnd {
                    ok: true,
                    stop: "end_turn".into(),
                    duration_ms: Some(900),
                    cost_usd: Some(0.02),
                    usage: json!({"input_tokens": 1200, "output_tokens": 30}),
                    text: "3 files.".into(),
                },
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The checked-in fixtures are exactly what this crate writes (and decode back to the value).
    #[test]
    fn fixtures_are_current() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let bless = std::env::var_os("HARNESS_PROTO_BLESS").is_some();
        let mut stale = Vec::new();
        for (name, bytes, json) in all() {
            let (cb, js) = (dir.join(format!("{name}.cbor")), dir.join(format!("{name}.json")));
            let text = serde_json::to_string_pretty(&json).unwrap() + "\n";
            if bless {
                std::fs::write(&cb, &bytes).unwrap();
                std::fs::write(&js, &text).unwrap();
            }
            if std::fs::read(&cb).ok().as_deref() != Some(&bytes[..]) || std::fs::read_to_string(&js).ok().as_deref() != Some(&text) {
                stale.push(name);
            }
            let back: Value = crate::cbor::decode(&bytes).unwrap();
            assert_eq!(crate::cbor::encode(&back), bytes, "{name}: decoded and encoded again, the same bytes");
        }
        assert!(stale.is_empty(), "stale fixtures {stale:?}: rerun with HARNESS_PROTO_BLESS=1 and commit them");
    }

    /// Each typed message decodes back to itself.
    #[test]
    fn typed_round_trip() {
        let up = Up::Req(Request::Runs(RunReq::Runs { session: None, limit: Some(100), before: None, q: Some("x".into()) }));
        assert_eq!(crate::cbor::decode::<Up>(&crate::cbor::encode(&up)).unwrap(), up);
        let task = Up::Req(Request::ReplTask { task: json!({"id": "t1", "status": "running"}) });
        assert_eq!(crate::cbor::decode::<Up>(&crate::cbor::encode(&task)).unwrap(), task);
        for (name, bytes, _) in all() {
            if name.starts_with("event-") {
                let e: Event = crate::cbor::decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(crate::cbor::encode(&e), bytes, "{name}");
            } else if name.starts_with("down-") {
                let d: Down = crate::cbor::decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(crate::cbor::encode(&d), bytes, "{name}");
            } else {
                let u: Up = crate::cbor::decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(crate::cbor::encode(&u), bytes, "{name}");
            }
        }
    }
}
