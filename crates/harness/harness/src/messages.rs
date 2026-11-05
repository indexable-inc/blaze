//! The `messages` stream of an agent object: its conversation, normalized from the agent's own
//! transcript into JSON lines {object, role, kind: text|thinking|tool_call|tool_result|turn_end, tool, text, ts_ms}.
//! `turn_end` (role system) marks the agent going idle after a turn: Claude's `turn_duration` entry,
//! Codex's `task_complete` event (text = its last agent message). `Obj.reply()` waits on it.
//!
//! Claude: started with `--session-id <uuid>` (meta.claude_session), so its transcript is
//! ~/.claude/projects/<cwd slug>/<uuid>.jsonl (the profile keeps HOME, so it's the user's ~/.claude).
//! Codex: CODEX_HOME is ~/.harness/codex-home, rollouts at sessions/YYYY/MM/DD/rollout-*.jsonl; a
//! session claims the first unclaimed rollout whose session_meta cwd matches, started after it.
//!
//! The daemon polls running agents every 2s and appends new turns; on its first pass after a start it
//! rebuilds each messages file from the whole transcript, so the stream is a pure function of it.
use crate::{db, paths};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use parking_lot::Mutex;

struct Cursor {
    src: PathBuf,
    offset: u64,
    tools: HashMap<String, String>, // tool call id -> tool name
}

static CURSORS: Mutex<Option<HashMap<String, Cursor>>> = Mutex::new(None);

fn home() -> PathBuf { PathBuf::from(std::env::var_os("HOME").unwrap_or_default()) }

/// "2026-11-04T00:23:15.838Z" -> unix ms.
fn iso_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    let mut ms = 0;
    if b.get(19) == Some(&b'.') {
        let frac: String = s[20..].chars().take_while(|c| c.is_ascii_digit()).collect();
        ms = format!("{frac:0<3}")[..3].parse().ok()?;
    }
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000 + ms)
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(|x| x["text"].as_str().map(str::to_string).unwrap_or_else(|| as_text(x))).collect::<Vec<_>>().join("\n"),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

fn claude_rows(o: &Value, tools: &mut HashMap<String, String>) -> Vec<Value> {
    let ty = o["type"].as_str().unwrap_or("");
    if ty == "system" && o["subtype"] == "turn_duration" {
        let ts = o["timestamp"].as_str().and_then(iso_ms);
        return vec![json!({ "role": "system", "kind": "turn_end", "tool": null, "text": "", "ts_ms": ts })];
    }
    if ty != "user" && ty != "assistant" {
        return vec![];
    }
    let ts = o["timestamp"].as_str().and_then(iso_ms);
    let role = o["message"]["role"].as_str().unwrap_or(ty);
    let row = |kind: &str, tool: Option<&str>, text: String| json!({ "role": role, "kind": kind, "tool": tool, "text": text, "ts_ms": ts });
    match &o["message"]["content"] {
        Value::String(s) => vec![row("text", None, s.clone())],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b["type"].as_str()? {
                "text" => Some(row("text", None, b["text"].as_str()?.to_string())),
                "thinking" => Some(row("thinking", None, b["thinking"].as_str().unwrap_or("").to_string())),
                "tool_use" => {
                    let name = b["name"].as_str().unwrap_or("");
                    if let Some(id) = b["id"].as_str() {
                        tools.insert(id.into(), name.into());
                    }
                    Some(row("tool_call", Some(name), b["input"].to_string()))
                }
                "tool_result" => {
                    let name = b["tool_use_id"].as_str().and_then(|i| tools.get(i)).cloned();
                    Some(row("tool_result", name.as_deref(), as_text(&b["content"])))
                }
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

fn codex_rows(o: &Value, tools: &mut HashMap<String, String>) -> Vec<Value> {
    if o["type"] == "event_msg" && o["payload"]["type"] == "task_complete" {
        let ts = o["timestamp"].as_str().and_then(iso_ms);
        let text = o["payload"]["last_agent_message"].as_str().unwrap_or("");
        return vec![json!({ "role": "system", "kind": "turn_end", "tool": null, "text": text, "ts_ms": ts })];
    }
    if o["type"] != "response_item" {
        return vec![];
    }
    let p = &o["payload"];
    let ts = o["timestamp"].as_str().and_then(iso_ms);
    let row = |role: &str, kind: &str, tool: Option<&str>, text: String| json!({ "role": role, "kind": kind, "tool": tool, "text": text, "ts_ms": ts });
    match p["type"].as_str().unwrap_or("") {
        "message" => vec![row(p["role"].as_str().unwrap_or(""), "text", None, as_text(&p["content"]))],
        "reasoning" => vec![row("assistant", "thinking", None, as_text(&p["summary"]))],
        "function_call" | "custom_tool_call" => {
            let name = p["name"].as_str().unwrap_or("");
            if let Some(id) = p["call_id"].as_str() {
                tools.insert(id.into(), name.into());
            }
            let args = p.get("arguments").or(p.get("input")).map(as_text).unwrap_or_default();
            vec![row("assistant", "tool_call", Some(name), args)]
        }
        "function_call_output" | "custom_tool_call_output" => {
            let name = p["call_id"].as_str().and_then(|i| tools.get(i)).cloned();
            vec![row("tool", "tool_result", name.as_deref(), as_text(&p["output"]))]
        }
        _ => vec![],
    }
}

fn claude_transcript(meta: &Value) -> Option<PathBuf> {
    let uuid = meta["claude_session"].as_str()?;
    let projects = home().join(".claude/projects");
    let cwd = meta["cwd"].as_str().unwrap_or("");
    let slug: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let direct = projects.join(slug).join(format!("{uuid}.jsonl"));
    if direct.exists() {
        return Some(direct);
    }
    std::fs::read_dir(&projects).ok()?.flatten().map(|e| e.path().join(format!("{uuid}.jsonl"))).find(|p| p.exists())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl")) {
            out.push(p);
        }
    }
}

fn codex_transcript(oid: &str, meta: &Value, claimed: &[PathBuf]) -> Option<PathBuf> {
    if let Some(p) = meta["transcript"].as_str() {
        return Some(PathBuf::from(p));
    }
    let canon = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
    let cwd = canon(meta["cwd"].as_str()?);
    let created = meta["created_ms"].as_i64()?;
    let mut files = vec![];
    walk(&paths::home().join("codex-home/sessions"), &mut files);
    files.sort();
    let found = files.into_iter().filter(|f| !claimed.contains(f)).find(|f| {
        let Ok(fh) = std::fs::File::open(f) else { return false };
        let mut first = String::new();
        let _ = std::io::BufReader::new(fh).read_line(&mut first);
        let Ok(v) = serde_json::from_str::<Value>(&first) else { return false };
        let p = &v["payload"];
        v["type"] == "session_meta"
            && p["cwd"].as_str().is_some_and(|c| canon(c) == cwd)
            && p["timestamp"].as_str().and_then(iso_ms).is_some_and(|t| t >= created - 2000)
    })?;
    db::set_meta(oid, "transcript", &json!(found));
    Some(found)
}

/// Bring every (or every running) agent's messages stream up to date with its transcript.
pub fn sync(only_running: bool) {
    let agents = db::agent_sessions(only_running);
    let claimed: Vec<PathBuf> =
        db::agent_sessions(false).iter().filter_map(|(_, _, m)| m["transcript"].as_str().map(PathBuf::from)).collect();
    for (oid, kind, meta) in agents {
        if let Err(e) = sync_one(&oid, &kind, &meta, &claimed) {
            eprintln!("messages {oid}: {e:#}");
        }
    }
}

/// Bring one agent's messages stream up to date now (instead of at the next 2s poll).
pub fn sync_object(oid: &str) -> anyhow::Result<()> {
    let Some((kind, meta)) = db::object_kind_meta(oid) else { anyhow::bail!("no object {oid}") };
    if kind != "claude" && kind != "codex" {
        return Ok(());
    }
    let claimed: Vec<PathBuf> =
        db::agent_sessions(false).iter().filter_map(|(_, _, m)| m["transcript"].as_str().map(PathBuf::from)).collect();
    sync_one(oid, &kind, &meta, &claimed)
}

fn sync_one(oid: &str, kind: &str, meta: &Value, claimed: &[PathBuf]) -> anyhow::Result<()> {
    // A chat session writes its own messages stream from its events (chat/mod.rs).
    if meta["mode"] == "chat" {
        return Ok(());
    }
    let mut guard = CURSORS.lock();
    let cursors = guard.get_or_insert_with(HashMap::new);
    let src = match cursors.get(oid) {
        Some(c) => c.src.clone(),
        None => {
            let Some(src) = (if kind == "claude" { claude_transcript(meta) } else { codex_transcript(oid, meta, claimed) }) else {
                return Ok(()); // not written yet
            };
            src
        }
    };
    let out = db::object_dir(oid).join("messages");
    let fresh = !cursors.contains_key(oid);
    let c = cursors.entry(oid.to_string()).or_insert_with(|| Cursor { src: src.clone(), offset: 0, tools: HashMap::new() });
    let len = std::fs::metadata(&src)?.len();
    if !fresh && len <= c.offset {
        return Ok(());
    }
    std::fs::create_dir_all(out.parent().unwrap())?;
    let mut f = std::fs::File::open(&src)?;
    f.seek(SeekFrom::Start(c.offset))?;
    let mut r = std::io::BufReader::new(f);
    let mut text = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            break; // a partial line waits for the next pass
        }
        c.offset += n as u64;
        let Ok(o) = serde_json::from_slice::<Value>(&line) else { continue };
        let rows = if kind == "claude" { claude_rows(&o, &mut c.tools) } else { codex_rows(&o, &mut c.tools) };
        for mut row in rows {
            row["object"] = json!(oid);
            text.extend(serde_json::to_vec(&row)?);
            text.push(b'\n');
        }
    }
    let mut w = std::fs::OpenOptions::new().create(true).append(!fresh).write(true).truncate(fresh).open(&out)?;
    w.write_all(&text)?;
    db::index_stream(oid, "messages", &out);
    Ok(())
}

/// Poll running agents' transcripts forever (daemon background thread).
pub fn poll() {
    sync(false);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        sync(true);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso() {
        assert_eq!(super::iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(super::iso_ms("2026-11-04T00:23:15.838Z"), Some(1793751795838));
    }
}
