//! Local transcript discovery and the thin filesystem driver around the pure record parser.
use harness_proto::event::Event;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Search metadata, not an invitation to resume the external agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub key: String,
    pub agent: String,
    pub chat_id: String,
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub effort: String,
    pub updated_ms: u64,
    pub path: PathBuf,
    pub harness_id: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Source {
    pub path: PathBuf,
    pub agent: String,
    pub modified_ms: u64,
    pub len: u64,
    pub harness_id: Option<u64>,
}

pub struct Parsed {
    pub session: Session,
    pub events: Vec<Event>,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Roots {
    /// Claude's config directory (contains `projects`).
    pub claude: PathBuf,
    /// Codex's home (contains `sessions` and `archived_sessions`).
    pub codex: PathBuf,
    pub harness: PathBuf,
}

impl Roots {
    pub fn user() -> Result<Self, String> {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("chat library: HOME is not set")?;
        Ok(Self {
            claude: std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude")),
            codex: std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex")),
            harness: std::env::var_os("HARNESS_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".harness")),
        })
    }
}

const MAX_SOURCE: u64 = 4 << 30;
const MAX_RECORD: u64 = 64 << 20;
const MAX_SNAPSHOT: u64 = 512 << 20;
const MAX_FILES: usize = 100_000;
const MAX_DEPTH: usize = 16;

fn modified(m: &std::fs::Metadata, path: &Path) -> Result<u64, String> {
    let t = m.modified().map_err(|e| format!("chat source {}: modification time: {e}", path.display()))?;
    Ok(t.duration_since(UNIX_EPOCH).map_err(|e| format!("chat source {}: modification time before epoch: {e}", path.display()))?.as_millis().min(u128::from(u64::MAX)) as u64)
}

/// Discover only transcript files. Symlinks are not traversed; credentials are never opened.
pub fn discover(roots: &Roots) -> Result<Vec<Source>, String> {
    let mut out = Vec::new(); // alloc: the worker's source catalogue, bounded by MAX_FILES
    for (root, agent) in [
        (roots.claude.join("projects"), "claude"),
        (roots.codex.join("sessions"), "codex"),
        (roots.codex.join("archived_sessions"), "codex"),
        (roots.harness.join("codex-home/sessions"), "codex"),
        (roots.harness.join("objects"), "harness"),
    ] {
        walk(&root, agent, 0, &mut out)?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    Ok(out)
}

fn walk(root: &Path, agent: &str, depth: usize, out: &mut Vec<Source>) -> Result<(), String> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && depth == 0 => return Ok(()),
        Err(e) => return Err(format!("chat discovery {}: {e}", root.display())),
    };
    if depth > MAX_DEPTH {
        return Err(format!("chat discovery {} exceeds {MAX_DEPTH} directory levels", root.display()));
    }
    for entry in entries {
        let entry = entry.map_err(|e| format!("chat discovery {}: {e}", root.display()))?;
        let path = entry.path();
        let ty = entry.file_type().map_err(|e| format!("chat discovery {}: {e}", path.display()))?;
        if ty.is_dir() {
            if agent != "harness" || (depth == 0 && path.file_name().and_then(|s| s.to_str()).is_some_and(|s| s.strip_prefix('s').is_some_and(|n| n.parse::<u64>().is_ok()))) {
                walk(&path, agent, depth + 1, out)?;
            }
        } else if ty.is_file() {
            let is_log = if agent == "harness" { path.file_name().is_some_and(|n| n == "chat") } else { path.extension().is_some_and(|e| e == "jsonl") };
            if !is_log {
                continue;
            }
            if out.len() >= MAX_FILES {
                return Err(format!("chat discovery {} exceeds {MAX_FILES} transcript files", path.display()));
            }
            let meta = entry.metadata().map_err(|e| format!("chat discovery {}: {e}", path.display()))?;
            let harness_id = (agent == "harness").then(|| path.parent()?.file_name()?.to_str()?.strip_prefix('s')?.parse().ok()).flatten();
            out.push(Source { path: path.clone(), agent: agent.to_owned(), modified_ms: modified(&meta, &path)?, len: meta.len(), harness_id });
        }
    }
    Ok(())
}

/// Read a selected source without changing or locking the provider's log.
pub fn read(source: &Source) -> Result<Parsed, String> {
    use std::io::{BufReader, Read};
    let file = std::fs::File::open(&source.path).map_err(|e| format!("chat source {}: {e}", source.path.display()))?;
    let len = file.metadata().map_err(|e| format!("chat source {}: {e}", source.path.display()))?.len();
    if len > MAX_SOURCE {
        return Err(format!("chat source {} is {len} bytes, exceeding the {MAX_SOURCE}-byte source limit", source.path.display()));
    }
    // Capture this byte extent once: an external writer cannot keep the reader chasing new appends forever.
    let mut reader = BufReader::new(file.take(len));
    match source.agent.as_str() {
        "harness" => read_harness(source, &mut reader),
        "claude" | "codex" => read_jsonl(source, &mut reader),
        agent => Err(format!("chat source {}: unknown provider {agent:?}", source.path.display())),
    }
}

fn read_jsonl(source: &Source, reader: &mut impl std::io::BufRead) -> Result<Parsed, String> {
    use std::io::{BufRead, Read};
    let mut state = parser::State::new(source);
    let mut line = Vec::new(); // alloc: one reusable record buffer, bounded by MAX_RECORD
    let mut total = 0u64;
    let mut n = 0usize;
    loop {
        line.clear();
        let read = Read::take(&mut *reader, MAX_RECORD + 1).read_until(b'\n', &mut line)
            .map_err(|e| format!("chat source {} line {}: {e}", source.path.display(), n + 1))?;
        if read == 0 { break; }
        total += read as u64;
        if total > MAX_SOURCE {
            return Err(format!("chat source {} exceeds the {MAX_SOURCE}-byte source limit while reading", source.path.display()));
        }
        if read as u64 > MAX_RECORD {
            return Err(format!("chat source {} line {} exceeds the {MAX_RECORD}-byte record limit (read at least {read} bytes)", source.path.display(), n + 1));
        }
        if !state.json_line(&line, n)? { break; }
        n += 1;
    }
    state.finish()
}

fn read_harness(source: &Source, reader: &mut impl std::io::Read) -> Result<Parsed, String> {
    use std::io::ErrorKind;
    const MAGIC: &[u8] = b"harness-chat/1\n";
    let mut magic = [0u8; MAGIC.len()];
    reader.read_exact(&mut magic).map_err(|e| format!("chat source {}: header: {e}", source.path.display()))?;
    if magic.as_slice() != MAGIC {
        return Err(format!("chat source {} has no harness-chat/1 header", source.path.display()));
    }
    let mut state = parser::State::new(source);
    let mut at = MAGIC.len() as u64;
    let mut payload = Vec::new(); // alloc: one CBOR frame, reused and bounded by frame::MAX_FRAME
    loop {
        let mut head = [0u8; harness_proto::frame::HEAD];
        match reader.read_exact(&mut head) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("chat source {} byte {at}: {e}", source.path.display())),
        }
        let (ty, len) = harness_proto::frame::check(&head).map_err(|e| format!("chat source {} byte {at}: {e}", source.path.display()))?;
        if at + head.len() as u64 + len as u64 > MAX_SOURCE {
            return Err(format!("chat source {} exceeds the {MAX_SOURCE}-byte source limit while reading frame at byte {at}", source.path.display()));
        }
        payload.resize(len, 0);
        match reader.read_exact(&mut payload) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("chat source {} byte {at}: {e}", source.path.display())),
        }
        state.harness_event(ty, &payload, at)?;
        at += head.len() as u64 + len as u64;
    }
    state.finish()
}

pub fn read_session(session: &Session) -> Result<Parsed, String> {
    let m = std::fs::metadata(&session.path).map_err(|e| format!("chat source {}: {e}", session.path.display()))?;
    let mut parsed = read(&Source {
        path: session.path.clone(),
        agent: if session.harness_id.is_some() { "harness".to_owned() } else { session.agent.clone() },
        modified_ms: modified(&m, &session.path)?,
        len: m.len(),
        harness_id: session.harness_id,
    })?;
    let mut roots = Roots::user()?;
    if session.harness_id.is_some() {
        if let Some(home) = session.path.parent().and_then(Path::parent).and_then(Path::parent) { roots.harness = home.to_path_buf(); }
    } else if session.agent == "claude" {
        if let Some(projects) = session.path.ancestors().find(|p| p.file_name().is_some_and(|n| n == "projects")) {
            if let Some(home) = projects.parent() { roots.claude = home.to_path_buf(); }
        }
    } else if session.agent == "codex" {
        if let Some(sessions) = session.path.ancestors().find(|p| p.file_name().is_some_and(|n| n == "sessions" || n == "archived_sessions")) {
            if let Some(home) = sessions.parent() { roots.codex = home.to_path_buf(); }
        }
    }
    crate::apply_metadata(&mut parsed.session, &crate::metadata(&roots)?);
    Ok(parsed)
}


mod parser;
pub use parser::parse;

#[cfg(test)]
mod tests;
