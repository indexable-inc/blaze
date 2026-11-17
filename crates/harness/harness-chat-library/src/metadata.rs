//! Read the providers' metadata snapshots once per refresh, separately from their transcript bytes.
use crate::{Roots, Session};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct SessionMetadata {
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub effort: String,
    pub updated_ms: u64,
}

/// Direct Harness metadata wins over a provider's copy of the same conversation.
pub fn apply_metadata(session: &mut Session, all: &BTreeMap<String, SessionMetadata>) {
    let identity = format!("{}:{}", session.agent, session.chat_id);
    let Some(meta) = all.get(&session.key).or_else(|| all.get(&identity)) else { return };
    for (to, from) in [(&mut session.title, &meta.title), (&mut session.cwd, &meta.cwd), (&mut session.model, &meta.model), (&mut session.effort, &meta.effort)] {
        if !from.is_empty() { to.clone_from(from); }
    }
    session.updated_ms = session.updated_ms.max(meta.updated_ms);
}

/// Provider indexes contain names and choices; no credentials or settings files are read.
pub fn metadata(roots: &Roots) -> Result<BTreeMap<String, SessionMetadata>, String> {
    let mut all = BTreeMap::new(); // alloc: small metadata records owned by one refresh
    for root in [&roots.codex, &roots.harness.join("codex-home")] {
        if let Some(path) = codex_db(root)? { codex_sqlite(&path, &mut all)?; }
        codex_names(&root.join("session_index.jsonl"), &mut all)?;
    }
    harness_sqlite(&roots.harness.join("harness.db"), &mut all)?;
    claude_names(&roots.claude.join("projects"), &mut all)?;
    Ok(all)
}

fn optional_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("chat metadata {}: {e}", path.display())),
    };
    const LIMIT: u64 = 64 << 20;
    let mut bytes = Vec::new(); // alloc: one provider metadata snapshot, bounded by LIMIT
    file.take(LIMIT + 1).read_to_end(&mut bytes).map_err(|e| format!("chat metadata {}: {e}", path.display()))?;
    if bytes.len() as u64 > LIMIT { return Err(format!("chat metadata {} exceeds the {LIMIT}-byte limit", path.display())); }
    Ok(Some(bytes))
}

fn codex_db(root: &Path) -> Result<Option<PathBuf>, String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("chat metadata {}: {e}", root.display())),
    };
    let mut newest = None;
    for entry in entries {
        let entry = entry.map_err(|e| format!("chat metadata {}: {e}", root.display()))?;
        let name = entry.file_name();
        let Some(version) = name.to_str().and_then(|s| s.strip_prefix("state_")).and_then(|s| s.strip_suffix(".sqlite")).and_then(|s| s.parse::<u32>().ok()) else { continue };
        if newest.as_ref().is_none_or(|(v, _)| version > *v) { newest = Some((version, entry.path())); }
    }
    Ok(newest.map(|(_, path)| path))
}

fn readonly(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| format!("chat metadata {}: {e}", path.display()))
}

fn codex_sqlite(path: &Path, all: &mut BTreeMap<String, SessionMetadata>) -> Result<(), String> {
    let db = readonly(path)?;
    let mut columns = db.prepare("pragma table_info(threads)").map_err(|e| format!("chat metadata {}: {e}", path.display()))?;
    let names: Vec<String> = columns.query_map([], |r| r.get(1)).map_err(|e| format!("chat metadata {}: {e}", path.display()))?.collect::<Result<_, _>>().map_err(|e| format!("chat metadata {}: {e}", path.display()))?;
    if !names.iter().any(|s| s == "id") || !names.iter().any(|s| s == "title") {
        return Err(format!("chat metadata {}: threads has no id/title columns", path.display()));
    }
    // Schema is chosen before querying: newer Codex versions add these optional columns.
    let col = |name: &'static str| if names.iter().any(|s| s == name) { name } else { "null" };
    let sql = format!("select id, coalesce(nullif({}, ''), title, ''), coalesce({}, ''), coalesce({}, ''), coalesce({}, ''), coalesce({}, 0) from threads", col("name"), col("cwd"), col("model"), col("reasoning_effort"), col("updated_at_ms"));
    let mut stmt = db.prepare(&sql).map_err(|e| format!("chat metadata {}: {e}", path.display()))?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, SessionMetadata { title: r.get(1)?, cwd: r.get(2)?, model: r.get(3)?, effort: r.get(4)?, updated_ms: r.get::<_, i64>(5)?.max(0) as u64 })))
        .map_err(|e| format!("chat metadata {}: {e}", path.display()))?;
    for row in rows {
        let (id, meta) = row.map_err(|e| format!("chat metadata {}: {e}", path.display()))?;
        all.insert(format!("codex:{id}"), meta);
    }
    Ok(())
}

fn codex_names(path: &Path, all: &mut BTreeMap<String, SessionMetadata>) -> Result<(), String> {
    let Some(bytes) = optional_bytes(path)? else { return Ok(()) };
    let mut at = 0;
    for (i, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
        at += line.len();
        if line.iter().all(u8::is_ascii_whitespace) { continue; }
        let row: Value = match serde_json::from_slice(line) {
            Ok(row) => row,
            Err(e) if at == bytes.len() && !line.ends_with(b"\n") && e.is_eof() => break,
            Err(e) => return Err(format!("chat metadata {} line {}: {e}", path.display(), i + 1)),
        };
        if let (Some(id), Some(title)) = (row["id"].as_str(), row["thread_name"].as_str()) {
            all.entry(format!("codex:{id}")).or_default().title = title.to_owned();
        }
    }
    Ok(())
}

fn harness_sqlite(path: &Path, all: &mut BTreeMap<String, SessionMetadata>) -> Result<(), String> {
    if !path.try_exists().map_err(|e| format!("chat metadata {}: {e}", path.display()))? { return Ok(()) }
    let db = readonly(path)?;
    let mut stmt = db.prepare("select id, name, cwd, meta from objects where kind in ('claude', 'codex')").map_err(|e| format!("chat metadata {}: {e}", path.display()))?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?)))
        .map_err(|e| format!("chat metadata {}: {e}", path.display()))?;
    for row in rows {
        let (id, name, cwd, body) = row.map_err(|e| format!("chat metadata {}: {e}", path.display()))?;
        let v: Value = match body { Some(body) => serde_json::from_str(&body).map_err(|e| format!("chat metadata {} {id}: {e}", path.display()))?, None => Value::Null };
        let title = v["label"].as_str().filter(|s| !s.is_empty()).or_else(|| v["title"].as_str().filter(|s| !s.is_empty())).map(str::to_owned).or(name).unwrap_or_default();
        all.insert(format!("harness:{id}"), SessionMetadata { title, cwd: cwd.unwrap_or_default(), model: v["model"].as_str().unwrap_or("").to_owned(), effort: v["effort"].as_str().unwrap_or("").to_owned(), updated_ms: 0 });
    }
    Ok(())
}

fn claude_names(root: &Path, all: &mut BTreeMap<String, SessionMetadata>) -> Result<(), String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("chat metadata {}: {e}", root.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("chat metadata {}: {e}", root.display()))?;
        if !entry.file_type().map_err(|e| format!("chat metadata {}: {e}", entry.path().display()))?.is_dir() { continue; }
        let path = entry.path().join("sessions-index.json");
        let Some(bytes) = optional_bytes(&path)? else { continue };
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("chat metadata {}: {e}", path.display()))?;
        for entry in v["entries"].as_array().into_iter().flatten() {
            if let Some(id) = entry["sessionId"].as_str() {
                let title = entry["customTitle"].as_str().or_else(|| entry["summary"].as_str()).or_else(|| entry["firstPrompt"].as_str()).unwrap_or("").to_owned();
                all.insert(format!("claude:{id}"), SessionMetadata { title, cwd: entry["projectPath"].as_str().unwrap_or("").to_owned(), ..SessionMetadata::default() });
            }
        }
    }
    Ok(())
}
