//! Fast non-blocking SSH TCP reachability prober and bounded local agent transcript tail reader.
//!
//! Used by the interactive Agent Hub to show live SSH reachability with RTT badges without
//! blocking the Metal UI thread, and to preview recent user/assistant/tool messages from local
//! Claude Code and Codex CLI JSONL transcripts.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use blaze_data::Value;
use blaze_own::Alloc;

type JsonValue = Value<Alloc<'static>>;

// ============================================================================
// 1. Bounded non-blocking SSH TCP reachability prober
// ============================================================================

const PROBE_TTL: Duration = Duration::from_secs(30);
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_millis(650);
const WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_PROBE_WORKERS: usize = 10;
const MAX_PROBE_QUEUE: usize = 256;
const MAX_ADDRS_PER_HOST: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerReachability {
    Probing,
    Online { rtt_ms: u32 },
    Offline,
}

struct ProbeEntry {
    state: ServerReachability,
    updated: Instant,
    generation: u64,
}

struct ProbeJob {
    hostname: String,
    port: u16,
    generation: u64,
}

struct PoolState {
    queue: VecDeque<ProbeJob>,
    active_workers: usize,
    idle_workers: usize,
}

struct ProbePool {
    state: Mutex<PoolState>,
    cv: Condvar,
}

static PROBE_CACHE: OnceLock<Mutex<HashMap<String, ProbeEntry>>> = OnceLock::new();
static PROBE_POOL: OnceLock<ProbePool> = OnceLock::new();
static PROBE_REVISION: AtomicU64 = AtomicU64::new(0);
static PROBE_GENERATION: AtomicU64 = AtomicU64::new(1);

fn probe_cache() -> &'static Mutex<HashMap<String, ProbeEntry>> {
    PROBE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn probe_pool() -> &'static ProbePool {
    PROBE_POOL.get_or_init(|| ProbePool {
        state: Mutex::new(PoolState {
            queue: VecDeque::new(),
            active_workers: 0,
            idle_workers: 0,
        }),
        cv: Condvar::new(),
    })
}

fn normalize_hostname(hostname: &str) -> String {
    let trimmed = hostname.trim();
    let unwrapped = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    unwrapped.to_ascii_lowercase()
}

fn probe_cache_key(normalized_host: &str, port: u16) -> String {
    format!("{normalized_host}:{port}")
}

/// Monotonic revision bumped whenever any background TCP reachability probe completes.
pub fn probe_revision() -> u64 {
    PROBE_REVISION.load(Ordering::Acquire)
}

/// Non-blocking O(1) lookup of cached TCP reachability for `(hostname, port)`.
/// On cache miss or after TTL expiry (30s), queues a bounded background TCP probe
/// (650ms connect timeout, max 10 concurrent worker threads) and returns `Probing`.
pub fn lookup_or_probe_server(hostname: &str, port: u16) -> ServerReachability {
    let host_norm = normalize_hostname(hostname);
    if host_norm.is_empty() || port == 0 {
        return ServerReachability::Offline;
    }

    let key = probe_cache_key(&host_norm, port);
    let generation = PROBE_GENERATION.load(Ordering::Acquire);

    {
        let mut map = probe_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get(&key)
            && entry.generation == generation
            && entry.updated.elapsed() < PROBE_TTL
        {
            return entry.state;
        }
        map.insert(
            key,
            ProbeEntry {
                state: ServerReachability::Probing,
                updated: Instant::now(),
                generation,
            },
        );
    }

    enqueue_probe(host_norm, port, generation);
    ServerReachability::Probing
}

/// Clears all cached server probe entries so the next lookup immediately re-probes every host.
pub fn invalidate_probe_cache() {
    PROBE_GENERATION.fetch_add(1, Ordering::AcqRel);
    if let Some(cache) = PROBE_CACHE.get() {
        let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
        map.clear();
    }
    if let Some(pool) = PROBE_POOL.get() {
        let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
        state.queue.clear();
    }
}

fn enqueue_probe(hostname: String, port: u16, generation: u64) {
    let pool = probe_pool();
    let should_spawn = {
        let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .queue
            .iter()
            .any(|j| j.port == port && j.generation == generation && j.hostname == hostname)
        {
            return;
        }
        if state.queue.len() >= MAX_PROBE_QUEUE {
            state.queue.pop_front();
        }
        state.queue.push_back(ProbeJob {
            hostname: hostname.clone(),
            port,
            generation,
        });
        if state.queue.len() > state.idle_workers && state.active_workers < MAX_PROBE_WORKERS {
            state.active_workers += 1;
            true
        } else {
            pool.cv.notify_one();
            false
        }
    };

    if should_spawn {
        let spawned = std::thread::Builder::new()
            .name("blaze-ssh-probe".to_owned())
            .spawn(move || worker_loop(pool));
        if spawned.is_err() {
            let no_workers_left = {
                let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
                state.active_workers = state.active_workers.saturating_sub(1);
                if state.active_workers == 0 {
                    state.queue.clear();
                    true
                } else {
                    pool.cv.notify_one();
                    false
                }
            };
            if no_workers_left {
                record_probe_result(&hostname, port, generation, ServerReachability::Offline);
            }
        }
    }
}

fn worker_loop(pool: &'static ProbePool) {
    loop {
        let job = {
            let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = state.queue.pop_front() {
                    break Some(job);
                }
                state.idle_workers += 1;
                let (guard, timeout) = pool
                    .cv
                    .wait_timeout(state, WORKER_IDLE_TIMEOUT)
                    .unwrap_or_else(|e| e.into_inner());
                state = guard;
                state.idle_workers = state.idle_workers.saturating_sub(1);
                if let Some(job) = state.queue.pop_front() {
                    break Some(job);
                }
                if timeout.timed_out() {
                    state.active_workers = state.active_workers.saturating_sub(1);
                    break None;
                }
            }
        };

        let Some(job) = job else {
            return;
        };

        if PROBE_GENERATION.load(Ordering::Acquire) != job.generation {
            continue;
        }

        let result = probe_tcp_host(&job.hostname, job.port);
        record_probe_result(&job.hostname, job.port, job.generation, result);
    }
}

fn probe_tcp_host(hostname: &str, port: u16) -> ServerReachability {
    let start = Instant::now();
    let Ok(addrs) = (hostname, port).to_socket_addrs() else {
        return ServerReachability::Offline;
    };

    let mut v4: Vec<SocketAddr> = Vec::new();
    let mut v6: Vec<SocketAddr> = Vec::new();
    for addr in addrs {
        if addr.is_ipv4() {
            if v4.len() < MAX_ADDRS_PER_HOST {
                v4.push(addr);
            }
        } else if v6.len() < MAX_ADDRS_PER_HOST {
            v6.push(addr);
        }
    }

    for addr in v4.into_iter().chain(v6).take(MAX_ADDRS_PER_HOST) {
        if let Ok(stream) = TcpStream::connect_timeout(&addr, PROBE_CONNECT_TIMEOUT) {
            drop(stream);
            let rtt_ms = start.elapsed().as_millis().clamp(1, u32::MAX as u128) as u32;
            return ServerReachability::Online { rtt_ms };
        }
    }

    ServerReachability::Offline
}

fn record_probe_result(
    normalized_host: &str,
    port: u16,
    generation: u64,
    result: ServerReachability,
) {
    if PROBE_GENERATION.load(Ordering::Acquire) != generation {
        return;
    }

    let key = probe_cache_key(normalized_host, port);
    {
        let mut map = probe_cache().lock().unwrap_or_else(|e| e.into_inner());
        if PROBE_GENERATION.load(Ordering::Acquire) != generation {
            return;
        }
        if let Some(existing) = map.get(&key)
            && existing.generation > generation
        {
            return;
        }
        map.insert(
            key,
            ProbeEntry {
                state: result,
                updated: Instant::now(),
                generation,
            },
        );
    }

    PROBE_REVISION.fetch_add(1, Ordering::Release);
}

// ============================================================================
// 2. Bounded local agent transcript tail reader
// ============================================================================

const MAX_TAIL_BYTES: u64 = 24 * 1024;
const MAX_SNIPPET_CHARS: usize = 220;
const MAX_CODEX_SCAN_FILES: usize = 512;
const MAX_CODEX_CHECK_FILES: usize = 64;
const MAX_CODEX_META_LINE_BYTES: u64 = 128 * 1024;

const TRANSCRIPT_JSON_LIMITS: blaze_json::Limits = blaze_json::Limits {
    most_bytes: 256 * 1024,
    most_depth: 64,
    most_number_len: 100,
    duplicates: blaze_json::Duplicates::LastWins,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewRole {
    User,
    Assistant,
    Tool,
    Meta,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub role: PreviewRole,
    pub text: String,
}

/// Encodes a workspace `cwd` into Claude Code's `~/.claude/projects/<slug>` directory name.
///
/// Verified on disk: Claude Code replaces every non-ASCII-alphanumeric character
/// (`/`, `.`, `_`, whitespace, etc.) with `-` (e.g. `/Users/alice/.config/nix` ->
/// `-Users-alice--config-nix`).
pub fn encode_claude_project_dir(cwd: &str) -> String {
    let trimmed = if cwd.len() > 1 {
        cwd.trim_end_matches('/')
    } else {
        cwd
    };
    trimmed
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Reads up to `max_entries` recent user/assistant/tool messages from the most recently modified
/// local session transcript matching `(kind, cwd)` (bounded to the last 24 KiB of the file).
pub fn read_agent_transcript_tail(
    kind: &str,
    cwd: &str,
    home: &str,
    max_entries: usize,
) -> Vec<TranscriptEntry> {
    if max_entries == 0 || cwd.is_empty() || home.is_empty() {
        return Vec::new();
    }

    let kind_lower = kind.trim().to_ascii_lowercase();
    if kind_lower.contains("claude") {
        let Some(path) = find_latest_claude_transcript(cwd, home) else {
            return Vec::new();
        };
        let Some(tail) = read_file_tail(&path, MAX_TAIL_BYTES) else {
            return Vec::new();
        };
        parse_claude_jsonl_tail(&tail, max_entries)
    } else if kind_lower.contains("codex") {
        let Some(path) = find_latest_codex_transcript(cwd, home) else {
            return Vec::new();
        };
        let Some(tail) = read_file_tail(&path, MAX_TAIL_BYTES) else {
            return Vec::new();
        };
        parse_codex_jsonl_tail(&tail, max_entries)
    } else {
        Vec::new()
    }
}

/// Parses Claude Code JSONL transcript lines into clean, bounded preview entries.
pub fn parse_claude_jsonl_tail(text: &str, max_entries: usize) -> Vec<TranscriptEntry> {
    if max_entries == 0 {
        return Vec::new();
    }

    let mut entries: Vec<TranscriptEntry> = Vec::new();
    let alloc = Alloc::global();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(val) = blaze_json::read_in(line.as_bytes(), TRANSCRIPT_JSON_LIMITS, alloc) else {
            continue;
        };
        if val.get("isMeta").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(top_type) = val.get("type").and_then(Value::as_str) else {
            continue;
        };
        if top_type != "user" && top_type != "assistant" {
            continue;
        }
        let Some(message) = val.get("message") else {
            continue;
        };
        let role_str = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or(top_type);
        let base_role = match role_str {
            "user" => PreviewRole::User,
            "assistant" => PreviewRole::Assistant,
            _ => continue,
        };
        let Some(content) = message.get("content") else {
            continue;
        };

        match content {
            Value::Text(s) => {
                if !is_meta_noise(s.as_str())
                    && let Some(snippet) = clean_snippet(s.as_str(), MAX_SNIPPET_CHARS)
                {
                    push_entry(&mut entries, base_role, snippet);
                }
            }
            Value::List(blocks) => {
                for block in blocks.as_slice() {
                    let block_type = block.get("type").and_then(Value::as_str);
                    match block_type {
                        Some("text") | None => {
                            if let Some(t) = block.get("text").and_then(Value::as_str)
                                && !is_meta_noise(t)
                                && let Some(snippet) = clean_snippet(t, MAX_SNIPPET_CHARS)
                            {
                                push_entry(&mut entries, base_role, snippet);
                            }
                        }
                        Some("tool_use") => {
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let summary = format_tool_call(name, block.get("input"));
                            if let Some(snippet) = clean_snippet(&summary, MAX_SNIPPET_CHARS) {
                                push_entry(&mut entries, PreviewRole::Tool, snippet);
                            }
                        }
                        Some("tool_result") => {
                            if block.get("is_error").and_then(Value::as_bool) == Some(true)
                                && let Some(err_txt) = extract_content_text(block.get("content"))
                                && let Some(snippet) = clean_snippet(
                                    &format!("error: {err_txt}"),
                                    MAX_SNIPPET_CHARS,
                                )
                            {
                                push_entry(&mut entries, PreviewRole::Tool, snippet);
                            }
                        }
                        // Skip "thinking" and any other non-preview block types.
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    keep_last_n(&mut entries, max_entries);
    entries
}

/// Parses Codex CLI rollout JSONL lines into clean, bounded preview entries.
pub fn parse_codex_jsonl_tail(text: &str, max_entries: usize) -> Vec<TranscriptEntry> {
    if max_entries == 0 {
        return Vec::new();
    }

    let mut entries: Vec<TranscriptEntry> = Vec::new();
    let alloc = Alloc::global();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(val) = blaze_json::read_in(line.as_bytes(), TRANSCRIPT_JSON_LIMITS, alloc) else {
            continue;
        };
        if val.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = val.get("payload") else {
            continue;
        };
        let Some(ptype) = payload.get("type").and_then(Value::as_str) else {
            continue;
        };

        match ptype {
            "message" => {
                let role = match payload.get("role").and_then(Value::as_str) {
                    Some("user") => PreviewRole::User,
                    Some("assistant") => PreviewRole::Assistant,
                    _ => continue,
                };
                let Some(content) = payload.get("content") else {
                    continue;
                };
                match content {
                    Value::Text(s) => {
                        if !is_meta_noise(s.as_str())
                            && let Some(snippet) = clean_snippet(s.as_str(), MAX_SNIPPET_CHARS)
                        {
                            push_entry(&mut entries, role, snippet);
                        }
                    }
                    Value::List(items) => {
                        for item in items.as_slice() {
                            if let Some(t) = item.get("text").and_then(Value::as_str)
                                && !is_meta_noise(t)
                                && let Some(snippet) = clean_snippet(t, MAX_SNIPPET_CHARS)
                            {
                                push_entry(&mut entries, role, snippet);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "function_call" | "custom_tool_call" => {
                let name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let raw_args = payload.get("arguments").or_else(|| payload.get("input"));
                let summary = format_codex_tool_call(name, raw_args, alloc);
                if let Some(snippet) = clean_snippet(&summary, MAX_SNIPPET_CHARS) {
                    push_entry(&mut entries, PreviewRole::Tool, snippet);
                }
            }
            _ => {}
        }
    }

    keep_last_n(&mut entries, max_entries);
    entries
}

fn find_latest_claude_transcript(cwd: &str, home: &str) -> Option<PathBuf> {
    let projects_root = Path::new(home).join(".claude").join("projects");
    let mut candidate_dirs = Vec::with_capacity(2);
    candidate_dirs.push(projects_root.join(encode_claude_project_dir(cwd)));

    if let Ok(canon) = fs::canonicalize(cwd)
        && let Some(canon_str) = canon.to_str()
    {
        let canon_dir = projects_root.join(encode_claude_project_dir(canon_str));
        if canon_dir != candidate_dirs[0] {
            candidate_dirs.push(canon_dir);
        }
    }

    let mut best: Option<(SystemTime, PathBuf)> = None;
    for dir in candidate_dirs {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let is_jsonl = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("jsonl"));
            if !is_jsonl || !path.is_file() {
                continue;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let is_newer = match &best {
                None => true,
                Some((best_time, best_path)) => {
                    modified > *best_time || (modified == *best_time && path > *best_path)
                }
            };
            if is_newer {
                best = Some((modified, path));
            }
        }
        if best.is_some() {
            break;
        }
    }

    best.map(|(_, path)| path)
}

fn find_latest_codex_transcript(cwd: &str, home: &str) -> Option<PathBuf> {
    let roots = [
        Path::new(home).join(".codex").join("sessions"),
        Path::new(home)
            .join(".harness")
            .join("codex-home")
            .join("sessions"),
    ];

    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    for root in &roots {
        if root.is_dir() {
            collect_codex_rollouts(root, 0, &mut files);
        }
    }
    if files.is_empty() {
        return None;
    }

    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let target_norm = normalize_cwd_path(cwd);
    let target_canon = fs::canonicalize(cwd).ok();
    let alloc = Alloc::global();

    for (_, path) in files.into_iter().take(MAX_CODEX_CHECK_FILES) {
        if codex_rollout_matches_cwd(&path, &target_norm, target_canon.as_deref(), alloc) {
            return Some(path);
        }
    }
    None
}

fn collect_codex_rollouts(dir: &Path, depth: usize, out: &mut Vec<(SystemTime, PathBuf)>) {
    if depth > 4 || out.len() >= MAX_CODEX_SCAN_FILES {
        return;
    }
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = read_dir.flatten().map(|e| e.path()).collect();
    // Visit newer YYYY/MM/DD directories first so the bounded cap favors recent sessions.
    children.sort_by(|a, b| b.cmp(a));

    for path in children {
        if out.len() >= MAX_CODEX_SCAN_FILES {
            break;
        }
        if path.is_dir() {
            collect_codex_rollouts(&path, depth + 1, out);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with("rollout-")
            && name.ends_with(".jsonl")
        {
            let modified = fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((modified, path));
        }
    }
}

fn normalize_cwd_path(cwd: &str) -> String {
    if cwd.len() > 1 {
        cwd.trim_end_matches('/').to_owned()
    } else {
        cwd.to_owned()
    }
}

fn codex_rollout_matches_cwd(
    path: &Path,
    target_norm: &str,
    target_canon: Option<&Path>,
    alloc: Alloc<'static>,
) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file.take(MAX_CODEX_META_LINE_BYTES));
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).is_err() {
        return false;
    }
    let trimmed = first_line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Ok(val) = blaze_json::read_in(trimmed.as_bytes(), TRANSCRIPT_JSON_LIMITS, alloc) else {
        return false;
    };
    if val.get("type").and_then(Value::as_str) != Some("session_meta") {
        return false;
    }
    let Some(session_cwd) = val
        .get("payload")
        .and_then(|p| p.get("cwd"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    if normalize_cwd_path(session_cwd) == target_norm {
        return true;
    }
    if let (Some(tc), Ok(sc)) = (target_canon, fs::canonicalize(session_cwd)) {
        return sc == tc;
    }
    false
}

fn read_file_tail(path: &Path, max_bytes: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return Some(String::new());
    }

    let seeked = len > max_bytes;
    if seeked {
        file.seek(SeekFrom::End(-(max_bytes as i64))).ok()?;
    }

    let cap = len.min(max_bytes) as usize;
    let mut buf = Vec::with_capacity(cap);
    file.take(max_bytes).read_to_end(&mut buf).ok()?;

    let raw = String::from_utf8_lossy(&buf);
    if seeked {
        match raw.find('\n') {
            Some(idx) => Some(raw[idx + 1..].to_owned()),
            None => Some(String::new()),
        }
    } else {
        Some(raw.into_owned())
    }
}

fn format_tool_call(name: &str, input: Option<&JsonValue>) -> String {
    match input.and_then(extract_tool_detail) {
        Some(detail) if !detail.is_empty() => format!("{name}: {detail}"),
        _ => name.to_owned(),
    }
}

fn format_codex_tool_call(
    name: &str,
    raw_args: Option<&JsonValue>,
    alloc: Alloc<'static>,
) -> String {
    let Some(args) = raw_args else {
        return name.to_owned();
    };
    if let Some(s) = args.as_str() {
        let trimmed = s.trim();
        if trimmed.starts_with('{')
            && let Ok(parsed) =
                blaze_json::read_in(trimmed.as_bytes(), TRANSCRIPT_JSON_LIMITS, alloc)
            && let Some(detail) = extract_tool_detail(&parsed)
        {
            return format!("{name}: {detail}");
        }
        if !trimmed.is_empty() {
            return format!("{name}: {trimmed}");
        }
        return name.to_owned();
    }
    format_tool_call(name, Some(args))
}

fn extract_tool_detail(input: &JsonValue) -> Option<&str> {
    if let Some(s) = input.as_str() {
        let t = s.trim();
        return (!t.is_empty()).then_some(t);
    }
    let map = input.as_map()?;
    for key in [
        "command",
        "cmd",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "description",
        "code",
    ] {
        if let Some(val) = map.get(key).and_then(Value::as_str) {
            let t = val.trim();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    for (_, v) in map.iter() {
        if let Some(s) = v.as_str() {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

fn extract_content_text(content: Option<&JsonValue>) -> Option<String> {
    let content = content?;
    match content {
        Value::Text(s) => {
            let t = s.as_str().trim();
            (!t.is_empty()).then(|| t.to_owned())
        }
        Value::List(items) => {
            for item in items.as_slice() {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_owned());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn is_meta_noise(text: &str) -> bool {
    let t = text.trim_start();
    t.is_empty()
        || t.starts_with("<system-reminder>")
        || t.starts_with("<local-command-caveat>")
        || t.starts_with("<local-command-stdout>")
        || t.starts_with("<environment_context>")
        || t.starts_with("<permissions instructions>")
}

fn clean_snippet(raw: &str, max_chars: usize) -> Option<String> {
    if max_chars == 0 {
        return None;
    }

    let mut out = String::new();
    let mut chars_written = 0usize;
    let mut truncated = false;
    let mut prev_space = true;

    for ch in raw.trim().chars() {
        if ch.is_control() && !ch.is_whitespace() {
            continue;
        }
        if ch.is_whitespace() {
            if !prev_space {
                if chars_written >= max_chars {
                    truncated = true;
                    break;
                }
                out.push(' ');
                chars_written += 1;
                prev_space = true;
            }
        } else {
            if chars_written >= max_chars {
                truncated = true;
                break;
            }
            out.push(ch);
            chars_written += 1;
            prev_space = false;
        }
    }

    while out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        return None;
    }
    if truncated {
        if out.chars().count() >= max_chars {
            out.pop();
        }
        while out.ends_with(' ') {
            out.pop();
        }
        out.push('…');
    }
    Some(out)
}

fn push_entry(entries: &mut Vec<TranscriptEntry>, role: PreviewRole, text: String) {
    if entries
        .last()
        .is_some_and(|prev| prev.role == role && prev.text == text)
    {
        return;
    }
    entries.push(TranscriptEntry { role, text });
}

fn keep_last_n(entries: &mut Vec<TranscriptEntry>, max_entries: usize) {
    if entries.len() > max_entries {
        let drop_count = entries.len() - max_entries;
        entries.drain(..drop_count);
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::UNIX_EPOCH;

    use super::*;

    fn make_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "blaze-agent-probe-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn encode_claude_project_dir_matches_on_disk_convention() {
        assert_eq!(
            encode_claude_project_dir("/Volumes/Projects/blaze"),
            "-Volumes-Projects-blaze"
        );
        assert_eq!(
            encode_claude_project_dir("/Volumes/Projects/blaze/"),
            "-Volumes-Projects-blaze"
        );
        assert_eq!(
            encode_claude_project_dir("/Users/alice/.config/nix"),
            "-Users-alice--config-nix"
        );
        assert_eq!(
            encode_claude_project_dir("/Volumes/Projects/blaze/.claude/worktrees/agent_1"),
            "-Volumes-Projects-blaze--claude-worktrees-agent-1"
        );
        assert_eq!(encode_claude_project_dir("/"), "-");
        assert_eq!(encode_claude_project_dir(""), "");
    }

    #[test]
    fn parse_claude_jsonl_tail_handles_user_assistant_tools_and_malformed_lines() {
        let long_reply = "a".repeat(300);
        let sample = format!(
            concat!(
                "{{\"type\":\"session-owner\",\"sessionId\":\"abc\"}}\n",
                "{{\"type\":\"queue-operation\",\"operation\":\"enqueue\"}}\n",
                "not valid json at all {{ broken\n",
                "{{\"type\":\"user\",\"isMeta\":true,\"message\":{{\"role\":\"user\",\"content\":\"hidden meta\"}}}}\n",
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"<system-reminder>skip me</system-reminder>\"}}}}\n",
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"text\":\"Fix   the\\nbuild\\tnow\"}}]}}}}\n",
                "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"thinking\",\"thinking\":\"internal\"}},{{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{{\"command\":\"cargo test\\n--lib\"}}}}]}}}}\n",
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"content\":\"noisy stdout\"}}]}}}}\n",
                "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"t2\",\"name\":\"Read\",\"input\":{{\"file_path\":\"src/lib.rs\"}}}}]}}}}\n",
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t2\",\"is_error\":true,\"content\":\"not found\"}}]}}}}\n",
                "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{long_reply}\"}}]}}}}\n",
                "{{\"truncated_line_at_end\n"
            ),
            long_reply = long_reply
        );

        let all = parse_claude_jsonl_tail(&sample, 10);
        assert_eq!(all.len(), 5);
        assert_eq!(
            all[0],
            TranscriptEntry {
                role: PreviewRole::User,
                text: "Fix the build now".to_owned(),
            }
        );
        assert_eq!(
            all[1],
            TranscriptEntry {
                role: PreviewRole::Tool,
                text: "Bash: cargo test --lib".to_owned(),
            }
        );
        assert_eq!(
            all[2],
            TranscriptEntry {
                role: PreviewRole::Tool,
                text: "Read: src/lib.rs".to_owned(),
            }
        );
        assert_eq!(
            all[3],
            TranscriptEntry {
                role: PreviewRole::Tool,
                text: "error: not found".to_owned(),
            }
        );
        assert_eq!(all[4].role, PreviewRole::Assistant);
        assert!(all[4].text.ends_with('…'));
        assert!(all[4].text.chars().count() <= MAX_SNIPPET_CHARS);

        // max_entries returns the last N entries in chronological order.
        let last_two = parse_claude_jsonl_tail(&sample, 2);
        assert_eq!(last_two.len(), 2);
        assert_eq!(last_two[0], all[3]);
        assert_eq!(last_two[1], all[4]);
    }

    #[test]
    fn parse_codex_jsonl_tail_extracts_messages_and_tool_calls() {
        let sample = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/tmp/repo\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"developer\",\"content\":[{\"type\":\"input_text\",\"text\":\"<permissions instructions>hidden\"}]}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Update AGENTS.md\"}]}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"UserMessage\"}}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec_command\",\"arguments\":\"{\\\"cmd\\\":\\\"git push\\\",\\\"workdir\\\":\\\"/tmp/repo\\\"}\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"output\":\"ok\"}}\n",
            "malformed line {\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Updated AGENTS.md and pushed.\"}]}}\n",
        );

        let entries = parse_codex_jsonl_tail(sample, 6);
        assert_eq!(
            entries,
            vec![
                TranscriptEntry {
                    role: PreviewRole::User,
                    text: "Update AGENTS.md".to_owned(),
                },
                TranscriptEntry {
                    role: PreviewRole::Tool,
                    text: "exec_command: git push".to_owned(),
                },
                TranscriptEntry {
                    role: PreviewRole::Assistant,
                    text: "Updated AGENTS.md and pushed.".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn read_agent_transcript_tail_reads_bounded_claude_and_codex_files() {
        let root = make_temp_dir("tail");
        let home_str = root.to_str().expect("utf8 temp path");
        let cwd = "/Volumes/Projects/demo_app";

        // 1. Claude transcript larger than 24 KiB verifies bounded seek + partial-line skip.
        let claude_dir = root
            .join(".claude")
            .join("projects")
            .join(encode_claude_project_dir(cwd));
        fs::create_dir_all(&claude_dir).expect("create claude project dir");
        let claude_file = claude_dir.join("session-1.jsonl");
        {
            let mut f = File::create(&claude_file).expect("create claude jsonl");
            for i in 0..400 {
                writeln!(
                    f,
                    "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"padding line {i:04} xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}}}}"
                )
                .expect("write padding");
            }
            writeln!(
                f,
                "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Final Claude answer\"}}]}}}}"
            )
            .expect("write tail line");
        }
        let claude_entries = read_agent_transcript_tail("claude", cwd, home_str, 2);
        assert_eq!(claude_entries.len(), 2);
        assert_eq!(
            claude_entries[1],
            TranscriptEntry {
                role: PreviewRole::Assistant,
                text: "Final Claude answer".to_owned(),
            }
        );

        // 2. Codex transcript matched by session_meta cwd.
        let codex_dir = root.join(".codex").join("sessions").join("2026/03/20");
        fs::create_dir_all(&codex_dir).expect("create codex sessions dir");
        let codex_file = codex_dir.join("rollout-2026-03-20T15-42-09-test.jsonl");
        fs::write(
            &codex_file,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{cwd}\"}}}}\n",
                    "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"Run tests\"}}]}}}}\n",
                    "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"All green\"}}]}}}}\n"
                ),
                cwd = cwd
            ),
        )
        .expect("write codex jsonl");
        let codex_entries = read_agent_transcript_tail("codex", cwd, home_str, 4);
        assert_eq!(codex_entries.len(), 2);
        assert_eq!(codex_entries[0].text, "Run tests");
        assert_eq!(codex_entries[1].text, "All green");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_cache_probes_local_listener_and_invalidates_cleanly() {
        assert_eq!(lookup_or_probe_server("", 22), ServerReachability::Offline);
        assert_eq!(
            lookup_or_probe_server("127.0.0.1", 0),
            ServerReachability::Offline
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral tcp listener");
        let port = listener.local_addr().expect("local addr").port();

        invalidate_probe_cache();
        let rev_before = probe_revision();
        assert_eq!(
            lookup_or_probe_server("127.0.0.1", port),
            ServerReachability::Probing
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut final_state = ServerReachability::Probing;
        while Instant::now() < deadline {
            if probe_revision() > rev_before {
                let state = lookup_or_probe_server("127.0.0.1", port);
                if state != ServerReachability::Probing {
                    final_state = state;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        match final_state {
            ServerReachability::Online { rtt_ms } => assert!(rtt_ms >= 1),
            other => panic!("expected Online for local listener, got {other:?}"),
        }

        // Invalidation clears cache and bumps generation so stale writes are ignored.
        let old_gen = PROBE_GENERATION.load(Ordering::Acquire);
        invalidate_probe_cache();
        let rev_after_invalidate = probe_revision();
        record_probe_result("127.0.0.1", port, old_gen, ServerReachability::Offline);
        assert_eq!(probe_revision(), rev_after_invalidate);
        assert_eq!(
            lookup_or_probe_server("127.0.0.1", port),
            ServerReachability::Probing
        );

        drop(listener);
    }
}
