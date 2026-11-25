//! Cross-host Zoxide-style frecency directory store backed by a conflict-free replicated
//! data type (state-based LWW/G-Counter semilattice keyed by `(host, path)`).
//!
//! Each machine only increments visit counts for its own `(host, path)` keys, so merging two
//! stores (`merge`) takes per-key `max(visits)` and `max(last_ns)` — strictly commutative,
//! associative, and idempotent across SSH syncs (`blaze-ssh` `BSH1`) with zero central server.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use blaze_own::Alloc;

/// One CRDT frecency record for `(host, path)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrecencyEntry {
    pub host: String,
    pub path: String,
    pub visits: u64,
    pub last_ns: i64,
}

impl FrecencyEntry {
    /// Zoxide-style recency-weighted frecency score at `now_ns`.
    pub fn frecency(&self, now_ns: i64) -> f64 {
        let age_ns = now_ns.saturating_sub(self.last_ns).max(0) as f64;
        let age_hours = age_ns / 3_600_000_000_000.0;
        let recency = if age_hours < 1.0 {
            4.0
        } else if age_hours < 24.0 {
            2.0
        } else if age_hours < 168.0 {
            0.5
        } else {
            0.25
        };
        (self.visits.max(1) as f64) * recency
    }
}

/// Ranked candidate returned by [`FrecencyStore::query`].
#[derive(Clone, Debug, PartialEq)]
pub struct ZoxideMatch {
    pub host: String,
    pub path: String,
    pub is_local: bool,
    pub score: f64,
}

/// Conflict-free replicated cross-host directory frecency table.
#[derive(Clone, Debug, Default)]
pub struct FrecencyStore {
    entries: BTreeMap<(String, String), FrecencyEntry>,
    store_file: Option<PathBuf>,
}

impl FrecencyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load persistent CRDT log from `<home>/.local/share/blaze-shell/zoxide.jsonl`, merging all
    /// rows through the semilattice join, and import any existing system `zoxide` v3 database
    /// (`~/Library/Application Support/zoxide/db.zo` / `~/.local/share/zoxide/db.zo`) directly
    /// with zero subprocesses!
    pub fn load(&mut self, home: &str) {
        if home.is_empty() {
            return;
        }
        let local_host = detect_local_hostname();
        self.import_system_zoxide_db(home, &local_host);

        let dir = PathBuf::from(home).join(".local/share/blaze-shell");
        let path = dir.join("zoxide.jsonl");
        let _ = fs::create_dir_all(&dir);
        if let Ok(raw) = fs::read_to_string(&path) {
            for line in raw.lines() {
                if let Some(entry) = parse_jsonl(line) {
                    self.join_entry(entry);
                }
            }
        }
        self.store_file = Some(path);
    }

    /// Import existing native `zoxide` v3 binary database (`db.zo`) into our CRDT semilattice.
    pub fn import_system_zoxide_db(&mut self, home: &str, local_host: &str) {
        let candidates = [
            PathBuf::from(home).join("Library/Application Support/zoxide/db.zo"),
            PathBuf::from(home).join(".local/share/zoxide/db.zo"),
        ];
        for path in candidates {
            if let Ok(bytes) = fs::read(&path) {
                for entry in parse_zoxide_db_v3(&bytes, local_host) {
                    self.join_entry(entry);
                }
            }
        }
    }

    /// Record a directory visit on `host` at `now_ns` and append to the persistent CRDT log.
    pub fn record_visit(&mut self, host: &str, path: &str, now_ns: i64) {
        let clean = normalize_dir(path);
        if clean.is_empty() || host.is_empty() {
            return;
        }
        let key = (host.to_owned(), clean.clone());
        let entry = self.entries.entry(key).or_insert_with(|| FrecencyEntry {
            host: host.to_owned(),
            path: clean,
            visits: 0,
            last_ns: now_ns,
        });
        entry.visits = entry.visits.saturating_add(1);
        entry.last_ns = entry.last_ns.max(now_ns);
        let snap = entry.clone();
        if let Some(file) = &self.store_file {
            append_jsonl(file, &snap);
        }
    }

    /// Semilattice join of one remote/replayed CRDT entry (commutative, associative, idempotent).
    pub fn join_entry(&mut self, incoming: FrecencyEntry) {
        if incoming.host.is_empty() || incoming.path.is_empty() {
            return;
        }
        let key = (incoming.host.clone(), incoming.path.clone());
        match self.entries.get_mut(&key) {
            Some(cur) => {
                cur.visits = cur.visits.max(incoming.visits);
                cur.last_ns = cur.last_ns.max(incoming.last_ns);
            }
            None => {
                self.entries.insert(key, incoming);
            }
        }
    }

    /// CRDT state merge: joins every `(host, path)` entry from `other` into `self`.
    pub fn merge(&mut self, other: &FrecencyStore) {
        for entry in other.entries.values() {
            self.join_entry(entry.clone());
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = &FrecencyEntry> {
        self.entries.values()
    }

    /// Query cross-host directories matching all whitespace-separated tokens in `query`.
    /// Local existing directories rank ahead of remote hosts on ties; remote hosts remain
    /// directly discoverable (e.g. `cd blaze-vm` or `cd app`).
    pub fn query(
        &self,
        query: &str,
        local_host: &str,
        now_ns: i64,
        limit: usize,
    ) -> Vec<ZoxideMatch> {
        let tokens: Vec<String> = query
            .split_whitespace()
            .filter(|t| !t.is_empty())
            .map(|t| t.to_ascii_lowercase())
            .collect();

        let mut scored: Vec<ZoxideMatch> = Vec::new();
        for entry in self.entries.values() {
            let is_local = entry.host == local_host;
            let hay_path = entry.path.to_ascii_lowercase();
            let hay_full = format!("{}:{}", entry.host.to_ascii_lowercase(), hay_path);
            let leaf = hay_path.rsplit('/').next().unwrap_or(&hay_path);

            let mut match_bonus = 1.0f64;
            if !tokens.is_empty() {
                let mut cursor = 0usize;
                let mut all_ok = true;
                for (ti, tok) in tokens.iter().enumerate() {
                    if let Some(pos) = hay_full[cursor..].find(tok.as_str()) {
                        cursor += pos + tok.len();
                        if ti + 1 == tokens.len() && leaf == tok.as_str() {
                            match_bonus += 6.0;
                        } else if ti + 1 == tokens.len() && leaf.contains(tok.as_str()) {
                            match_bonus += 3.0;
                        }
                    } else if let Some(pos) = hay_full.find(tok.as_str()) {
                        cursor = pos + tok.len();
                        match_bonus += 0.5;
                    } else {
                        all_ok = false;
                        break;
                    }
                }
                if !all_ok {
                    continue;
                }
            }
            if is_local && !Path::new(&entry.path).is_dir() {
                continue;
            }
            let local_boost = if is_local { 1.35 } else { 1.0 };
            let score = (entry.frecency(now_ns) + 1.0) * match_bonus * local_boost;
            scored.push(ZoxideMatch {
                host: entry.host.clone(),
                path: entry.path.clone(),
                is_local,
                score,
            });
        }
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(core::cmp::Ordering::Equal)
                .then_with(|| a.path.cmp(&b.path))
        });
        scored.truncate(limit);
        scored
    }
}

/// Decode zoxide's native bincode v3 database (`db.zo`) directly with zero subprocesses:
/// `[u32 version == 3][u64 count]([u64 path_len][utf8 path][f64 rank][i64 last_accessed_secs])*`.
pub fn parse_zoxide_db_v3(bytes: &[u8], host: &str) -> Vec<FrecencyEntry> {
    let mut out = Vec::new();
    if bytes.len() < 12 || host.is_empty() {
        return out;
    }
    let version = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if version != 3 {
        return out;
    }
    let count = u64::from_le_bytes([
        bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
    ])
    .min(20_000) as usize;
    let mut pos = 12usize;
    for _ in 0..count {
        if pos + 8 > bytes.len() {
            break;
        }
        let len = u64::from_le_bytes([
            bytes[pos],
            bytes[pos + 1],
            bytes[pos + 2],
            bytes[pos + 3],
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        pos += 8;
        if len > 4096 || pos + len + 16 > bytes.len() {
            break;
        }
        let path_bytes = &bytes[pos..pos + len];
        pos += len;
        let rank = f64::from_le_bytes([
            bytes[pos],
            bytes[pos + 1],
            bytes[pos + 2],
            bytes[pos + 3],
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]);
        pos += 8;
        let last_secs = i64::from_le_bytes([
            bytes[pos],
            bytes[pos + 1],
            bytes[pos + 2],
            bytes[pos + 3],
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]);
        pos += 8;
        if let Ok(path_str) = core::str::from_utf8(path_bytes) {
            let clean = normalize_dir(path_str);
            if !clean.is_empty() {
                let visits = if rank.is_finite() && rank > 0.0 {
                    rank.ceil().min(1_000_000.0) as u64
                } else {
                    1
                };
                out.push(FrecencyEntry {
                    host: host.to_owned(),
                    path: clean,
                    visits,
                    last_ns: last_secs.saturating_mul(1_000_000_000),
                });
            }
        }
    }
    out
}

/// Bounded shallow breadth-first directory lookup (depth ≤ 2, max 200 dirs) when `cd <query>`
/// matches neither a direct child nor any recorded zoxide entry yet.
pub fn shallow_find_dir(root: &Path, query: &str) -> Option<PathBuf> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return None;
    }
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((root.to_path_buf(), 0u8));
    let mut visited = 0usize;
    let mut partial: Option<PathBuf> = None;

    while let Some((dir, depth)) = queue.pop_front() {
        if depth >= 2 || visited >= 200 {
            continue;
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for ent in rd.flatten() {
            visited += 1;
            if visited >= 200 {
                break;
            }
            let Ok(ft) = ent.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || matches!(name.as_str(), "target" | "node_modules" | "vendor") {
                continue;
            }
            let lower = name.to_ascii_lowercase();
            let p = ent.path();
            if lower == needle {
                return Some(p);
            }
            if partial.is_none() && lower.contains(&needle) {
                partial = Some(p.clone());
            }
            if depth + 1 < 2 {
                queue.push_back((p, depth + 1));
            }
        }
    }
    partial
}

/// Detect a concise, human-readable local host name (e.g. `hydra`) without spawning a subprocess.
pub fn detect_local_hostname() -> String {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn gethostname(name: *mut u8, len: usize) -> i32;
        }
        let mut buf = [0u8; 128];
        // SAFETY: `buf` is writable for `buf.len()` bytes.
        let rc = unsafe { gethostname(buf.as_mut_ptr(), buf.len() - 1) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = core::str::from_utf8(&buf[..end]) {
                let short = s.split('.').next().unwrap_or(s).trim();
                if !short.is_empty() {
                    return short.to_owned();
                }
            }
        }
    }
    std::env::var("HOSTNAME")
        .ok()
        .and_then(|s| s.split('.').next().map(str::to_owned))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".to_owned())
}

fn normalize_dir(path: &str) -> String {
    let t = path.trim();
    if t == "/" {
        "/".to_owned()
    } else {
        t.trim_end_matches('/').to_owned()
    }
}

fn append_jsonl(path: &Path, e: &FrecencyEntry) {
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let esc = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    };
    let _ = writeln!(
        f,
        r#"{{"host":"{}","path":"{}","visits":{},"last_ns":{}}}"#,
        esc(&e.host),
        esc(&e.path),
        e.visits,
        e.last_ns,
    );
}

fn parse_jsonl(line: &str) -> Option<FrecencyEntry> {
    let alloc = Alloc::global();
    let val = blaze_json::read_in(line.as_bytes(), blaze_json::Limits::SHIPPED, alloc).ok()?;
    let blaze_data::Value::Map(ref map) = val else {
        return None;
    };
    let mut host = String::new();
    let mut path = String::new();
    let mut visits = 0u64;
    let mut last_ns = 0i64;
    for (k, v) in map.iter() {
        match (k, v) {
            ("host", blaze_data::Value::Text(s)) => host = s.as_str().to_owned(),
            ("path", blaze_data::Value::Text(s)) => path = s.as_str().to_owned(),
            ("visits", blaze_data::Value::Int(n)) => visits = n.as_i64().unwrap_or(0).max(0) as u64,
            ("last_ns", blaze_data::Value::Int(n)) => last_ns = n.as_i64().unwrap_or(0),
            _ => {}
        }
    }
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(FrecencyEntry { host, path, visits, last_ns })
}
