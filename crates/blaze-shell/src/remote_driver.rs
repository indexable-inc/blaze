//! Thin async OS host driver over the `#![no_std]` sans-IO [`blaze_cli_spec::remote`] module:
//! - Queries live package/repository registries (GitHub repos, crates.io, npm, Homebrew) without
//!   ever blocking the UI frame loop, with 120ms debounce, bounded concurrency, and 5-minute TTL
//!   in-memory caching.
//! - Fetches and **disk-caches** circular Retina GitHub org/owner avatar images under
//!   [`crate::git_meta_driver::avatar_cache_dir`] (`org_<sanitized>.img`, 7-day mtime TTL) with
//!   atomic temp-file writes and PNG/JPEG/WebP magic-byte validation.
//! - Bumps [`remote_complete_revision`] whenever a remote query or org avatar download completes
//!   so the UI event loop repaints seamlessly.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use blaze_cli_spec::remote::{RemoteProvider, build_endpoint, parse_response};
use blaze_own::Alloc;

use crate::git_meta_driver::avatar_cache_dir;

static REVISION: AtomicU64 = AtomicU64::new(0);
static LATEST_QUERY_GEN: AtomicU64 = AtomicU64::new(0);
static ACTIVE_QUERY_WORKERS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_AVATAR_WORKERS: AtomicUsize = AtomicUsize::new(0);
static TMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

const MAX_CONCURRENT_QUERY_WORKERS: usize = 3;
const MAX_CONCURRENT_AVATAR_WORKERS: usize = 4;
const MAX_PREFETCH_AVATARS: usize = 8;
const MAX_QUERY_CACHE_ENTRIES: usize = 256;
const MAX_ORG_AVATAR_MEM_ENTRIES: usize = 512;

const DEBOUNCE_DELAY: Duration = Duration::from_millis(120);
const QUERY_CACHE_TTL: Duration = Duration::from_secs(300);
const QUERY_ERROR_TTL: Duration = Duration::from_secs(15);
const AVATAR_DISK_TTL: Duration = Duration::from_secs(7 * 86_400);
const AVATAR_MEM_RECHECK_TTL: Duration = Duration::from_secs(3600);
const AVATAR_FAIL_TTL: Duration = Duration::from_secs(300);
const GH_TOKEN_TIMEOUT: Duration = Duration::from_secs(2);

const MIN_IMAGE_BYTES: usize = 101;
const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SANITIZED_LOGIN_LEN: usize = 64;

/// Monotonic revision bumped whenever a remote completion query completes or an org/owner avatar
/// finishes downloading so the UI event loop repaints seamlessly.
pub fn remote_complete_revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

/// Owned completion candidate returned by [`query_remote_completions`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSuggestion {
    pub label: String,
    pub insert: String,
    pub detail: String,
    pub doc: String,
    pub owner_login: Option<String>,
    /// Local cached image path ready for `Scene::avatar_image(...)`, if already available.
    pub avatar_path: Option<String>,
    pub provider: RemoteProvider,
}

/// Non-blocking poll result for a remote registry completion query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemotePoll {
    /// Query is too short or does not map to a remote endpoint yet.
    Idle,
    /// Network fetch is debouncing or in flight.
    Loading,
    /// Cached or freshly fetched suggestions ready for display.
    Ready(Vec<RemoteSuggestion>),
}

#[derive(Clone, Debug)]
struct CachedCandidate {
    label: String,
    insert: String,
    detail: String,
    doc: String,
    owner_login: Option<String>,
    avatar_url: Option<String>,
    provider: RemoteProvider,
}

struct CachedQuery {
    fetched_at: Instant,
    ttl: Duration,
    candidates: Vec<CachedCandidate>,
}

struct QueryCache {
    entries: HashMap<String, CachedQuery>,
}

impl QueryCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn get_fresh(&self, key: &str, now: Instant) -> Option<&[CachedCandidate]> {
        let entry = self.entries.get(key)?;
        if is_cache_entry_fresh(entry.fetched_at, now, entry.ttl) {
            Some(&entry.candidates)
        } else {
            None
        }
    }

    fn insert(
        &mut self,
        key: String,
        candidates: Vec<CachedCandidate>,
        now: Instant,
        ttl: Duration,
    ) {
        if self.entries.len() >= MAX_QUERY_CACHE_ENTRIES && !self.entries.contains_key(&key) {
            self.entries
                .retain(|_, v| is_cache_entry_fresh(v.fetched_at, now, v.ttl));
            if self.entries.len() >= MAX_QUERY_CACHE_ENTRIES
                && let Some(oldest_key) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, v)| v.fetched_at)
                    .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest_key);
            }
        }
        self.entries.insert(
            key,
            CachedQuery {
                fetched_at: now,
                ttl,
                candidates,
            },
        );
    }
}

enum OrgAvatarState {
    Ready { path: String, checked_at: Instant },
    Failed { failed_at: Instant },
}

static LATEST_CACHE_KEY: OnceLock<Mutex<String>> = OnceLock::new();
static QUERY_CACHE: OnceLock<Mutex<QueryCache>> = OnceLock::new();
static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

static ORG_AVATAR_MEM: OnceLock<Mutex<HashMap<String, OrgAvatarState>>> = OnceLock::new();
static AVATAR_IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static GITHUB_TOKEN_CACHE: OnceLock<Option<String>> = OnceLock::new();

struct InFlightWorkerGuard {
    key: String,
    in_flight: &'static OnceLock<Mutex<HashSet<String>>>,
    active_workers: &'static AtomicUsize,
}

impl Drop for InFlightWorkerGuard {
    fn drop(&mut self) {
        if let Some(set) = self.in_flight.get()
            && let Ok(mut guard) = set.lock()
        {
            guard.remove(&self.key);
        }
        self.active_workers.fetch_sub(1, Ordering::AcqRel);
    }
}

fn try_acquire_worker_slot(counter: &AtomicUsize, max: usize) -> bool {
    let mut cur = counter.load(Ordering::Acquire);
    while cur < max {
        match counter.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(next) => cur = next,
        }
    }
    false
}

fn is_cache_entry_fresh(fetched_at: Instant, now: Instant, ttl: Duration) -> bool {
    now.checked_duration_since(fetched_at)
        .is_some_and(|age| age < ttl)
}

fn record_latest_query(cache_key: &str) -> u64 {
    let latest = LATEST_CACHE_KEY.get_or_init(|| Mutex::new(String::new()));
    if let Ok(mut guard) = latest.lock() {
        if *guard != cache_key {
            guard.clear();
            guard.push_str(cache_key);
            return LATEST_QUERY_GEN.fetch_add(1, Ordering::AcqRel) + 1;
        }
        return LATEST_QUERY_GEN.load(Ordering::Acquire);
    }
    LATEST_QUERY_GEN.fetch_add(1, Ordering::AcqRel) + 1
}

fn is_query_still_current(my_gen: u64, cache_key: &str) -> bool {
    if LATEST_QUERY_GEN.load(Ordering::Acquire) == my_gen {
        return true;
    }
    LATEST_CACHE_KEY
        .get()
        .and_then(|m| m.lock().ok())
        .is_some_and(|latest| *latest == cache_key)
}

/// Non-blocking remote completion lookup for `provider` and `query`.
///
/// - Returns [`RemotePoll::Idle`] when [`build_endpoint`] declines the query (e.g. too short).
/// - Returns [`RemotePoll::Ready`] immediately on a fresh in-memory cache hit, resolving
///   `avatar_path` fresh for every suggestion via [`lookup_or_fetch_org_avatar`].
/// - On cache miss, debounces for 120ms and spawns a bounded background fetch worker that updates
///   the cache, prefetches top-8 org/owner avatars, and bumps [`remote_complete_revision`].
pub fn query_remote_completions(provider: RemoteProvider, query: &str) -> RemotePoll {
    let Some(endpoint) = build_endpoint(provider.clone(), query, Alloc::global()) else {
        return RemotePoll::Idle;
    };
    let url = endpoint.url.as_str().to_owned();
    let cache_key = endpoint.cache_key.as_str().to_owned();
    drop(endpoint);

    let now = Instant::now();
    let cache = QUERY_CACHE.get_or_init(|| Mutex::new(QueryCache::new()));
    let cached_hit: Option<Vec<CachedCandidate>> = cache
        .lock()
        .ok()
        .and_then(|guard| guard.get_fresh(&cache_key, now).map(|s| s.to_vec()));

    if let Some(candidates) = cached_hit {
        let suggestions = candidates
            .into_iter()
            .map(|cand| {
                let avatar_path = cand.owner_login.as_deref().and_then(|login| {
                    lookup_or_fetch_org_avatar(login, cand.avatar_url.as_deref())
                });
                RemoteSuggestion {
                    label: cand.label,
                    insert: cand.insert,
                    detail: cand.detail,
                    doc: cand.doc,
                    owner_login: cand.owner_login,
                    avatar_path,
                    provider: cand.provider,
                }
            })
            .collect();
        return RemotePoll::Ready(suggestions);
    }

    let my_gen = record_latest_query(&cache_key);

    let inflight = IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()));
    let acquired = inflight
        .lock()
        .map(|mut set| {
            if set.contains(&cache_key) {
                false
            } else if try_acquire_worker_slot(&ACTIVE_QUERY_WORKERS, MAX_CONCURRENT_QUERY_WORKERS) {
                set.insert(cache_key.clone());
                true
            } else {
                false
            }
        })
        .unwrap_or(false);

    if acquired {
        let key_bg = cache_key.clone();
        let spawn_res = std::thread::Builder::new()
            .name("blaze-remote-query".to_owned())
            .spawn(move || {
                let guard = InFlightWorkerGuard {
                    key: key_bg.clone(),
                    in_flight: &IN_FLIGHT,
                    active_workers: &ACTIVE_QUERY_WORKERS,
                };

                std::thread::sleep(DEBOUNCE_DELAY);
                if !is_query_still_current(my_gen, &key_bg) {
                    // Superseded while debouncing: release slot and poke UI in case a newer
                    // query was waiting for a worker slot.
                    drop(guard);
                    REVISION.fetch_add(1, Ordering::Relaxed);
                    return;
                }

                let (candidates, ttl) = match fetch_remote_body(provider.clone(), &url) {
                    Some(body) => {
                        let parsed = parse_response(provider.clone(), &body, Alloc::global());
                        let mut out = Vec::with_capacity(parsed.len());
                        for cand in parsed {
                            out.push(CachedCandidate {
                                label: cand.label.as_str().to_owned(),
                                insert: cand.insert.as_str().to_owned(),
                                detail: cand.detail.as_str().to_owned(),
                                doc: cand.doc.as_str().to_owned(),
                                owner_login: cand
                                    .owner_login
                                    .as_ref()
                                    .map(|s| s.as_str().to_owned()),
                                avatar_url: cand
                                    .avatar_url
                                    .as_ref()
                                    .map(|s| s.as_str().to_owned()),
                                provider: provider.clone(),
                            });
                        }
                        (out, QUERY_CACHE_TTL)
                    }
                    None => (Vec::new(), QUERY_ERROR_TTL),
                };

                let prefetch_avatars: Vec<(String, Option<String>)> = candidates
                    .iter()
                    .take(MAX_PREFETCH_AVATARS)
                    .filter_map(|c| {
                        c.owner_login
                            .as_ref()
                            .map(|login| (login.clone(), c.avatar_url.clone()))
                    })
                    .collect();

                if let Some(cache) = QUERY_CACHE.get()
                    && let Ok(mut map) = cache.lock()
                {
                    map.insert(key_bg, candidates, Instant::now(), ttl);
                }

                drop(guard);

                for (login, avatar_url) in prefetch_avatars {
                    let _ = lookup_or_fetch_org_avatar(&login, avatar_url.as_deref());
                }

                REVISION.fetch_add(1, Ordering::Relaxed);
            });

        if spawn_res.is_err() {
            if let Ok(mut set) = inflight.lock() {
                set.remove(&cache_key);
            }
            ACTIVE_QUERY_WORKERS.fetch_sub(1, Ordering::AcqRel);
        }
    }

    RemotePoll::Loading
}

fn fetch_remote_body(provider: RemoteProvider, url: &str) -> Option<Vec<u8>> {
    if !url.starts_with("https://") {
        return None;
    }

    let mut cmd = Command::new("curl");
    cmd.args([
        "-sSfL",
        "--max-time",
        "4",
        "--connect-timeout",
        "3",
        "-H",
        "User-Agent: blaze-shell/0.1",
    ]);

    match provider {
        RemoteProvider::GitHubRepo { .. } => {
            cmd.args(["-H", "Accept: application/vnd.github+json"]);
            if let Some(token) = cached_github_token() {
                cmd.arg("-H").arg(format!("Authorization: Bearer {token}"));
            }
        }
        RemoteProvider::CratesIo | RemoteProvider::NpmRegistry | RemoteProvider::Homebrew => {
            cmd.args(["-H", "Accept: application/json"]);
        }
    }

    let out = cmd
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() || out.stdout.len() > MAX_RESPONSE_BYTES {
        return None;
    }
    Some(out.stdout)
}

fn cached_github_token() -> Option<&'static str> {
    GITHUB_TOKEN_CACHE
        .get_or_init(|| {
            for env_name in ["GITHUB_TOKEN", "GH_TOKEN"] {
                if let Ok(val) = std::env::var(env_name) {
                    let trimmed = val.trim();
                    if !trimmed.is_empty() && !trimmed.contains(['\r', '\n']) {
                        return Some(trimmed.to_owned());
                    }
                }
            }
            fetch_gh_auth_token(GH_TOKEN_TIMEOUT)
        })
        .as_deref()
}

fn fetch_gh_auth_token(timeout: Duration) -> Option<String> {
    let mut child = Command::new("gh")
        .args(["auth", "token"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let out = child.wait_with_output().ok()?;
                let raw = String::from_utf8_lossy(&out.stdout);
                let trimmed = raw.trim();
                if trimmed.is_empty() || trimmed.contains(['\r', '\n']) {
                    return None;
                }
                return Some(trimmed.to_owned());
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Sanitize a GitHub owner/org login into lowercase ASCII `[a-z0-9_-]` suitable for a flat
/// filename component under the avatar cache directory.
pub(crate) fn sanitize_login(login: &str) -> String {
    login
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .map(|c| c.to_ascii_lowercase())
        .take(MAX_SANITIZED_LOGIN_LEN)
        .collect()
}

/// Verify downloaded avatar bytes exceed 100 bytes (and stay under 2 MiB) and start with a valid
/// PNG, JPEG, or WebP magic header so HTML error/rate-limit pages never poison the disk cache.
pub(crate) fn is_valid_image_bytes(bytes: &[u8]) -> bool {
    if bytes.len() < MIN_IMAGE_BYTES || bytes.len() > MAX_IMAGE_BYTES {
        return false;
    }
    let is_png = bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let is_jpeg = bytes.starts_with(&[0xff, 0xd8, 0xff]);
    let is_webp = bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP";
    is_png || is_jpeg || is_webp
}

fn normalize_avatar_url(url: &str) -> String {
    if url.starts_with("https://avatars.githubusercontent.com/")
        && !url.contains("s=")
        && !url.contains("size=")
    {
        if url.contains('?') {
            format!("{url}&s=96")
        } else {
            format!("{url}?s=96")
        }
    } else {
        url.to_owned()
    }
}

fn is_fresh_avatar_file(path: &Path, now: SystemTime, max_age: Duration) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() < MIN_IMAGE_BYTES as u64 {
        return false;
    }
    let Ok(modified) = meta.modified() else {
        return false;
    };
    now.duration_since(modified).map_or(true, |age| age < max_age)
}

fn record_org_avatar_mem(sanitized: String, state: OrgAvatarState) {
    let mem = ORG_AVATAR_MEM.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = mem.lock() {
        if guard.len() >= MAX_ORG_AVATAR_MEM_ENTRIES && !guard.contains_key(&sanitized) {
            guard.clear();
        }
        guard.insert(sanitized, state);
    }
}

/// Non-blocking org/owner avatar lookup with persistent local disk cache
/// (`<avatar_cache_dir>/org_<sanitized>.img`, 7-day mtime TTL).
///
/// Returns an immediate cached image path when present (zero disk IO after first lookup);
/// otherwise, when `avatar_url` is `Some(https_url)`, kicks off a bounded background download and
/// bumps [`remote_complete_revision`] upon atomic cache write.
pub fn lookup_or_fetch_org_avatar(owner_login: &str, avatar_url: Option<&str>) -> Option<String> {
    let sanitized = sanitize_login(owner_login);
    if sanitized.is_empty() {
        return None;
    }

    let now_instant = Instant::now();
    let mem = ORG_AVATAR_MEM.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = mem.lock()
        && let Some(state) = guard.get(&sanitized)
    {
        match state {
            OrgAvatarState::Ready { path, checked_at } => {
                if is_cache_entry_fresh(*checked_at, now_instant, AVATAR_MEM_RECHECK_TTL) {
                    return Some(path.clone());
                }
            }
            OrgAvatarState::Failed { failed_at } => {
                if is_cache_entry_fresh(*failed_at, now_instant, AVATAR_FAIL_TTL) {
                    return None;
                }
            }
        }
    }

    let dir = avatar_cache_dir()?;
    let img_path = dir.join(format!("org_{sanitized}.img"));
    if is_fresh_avatar_file(&img_path, SystemTime::now(), AVATAR_DISK_TTL) {
        let path_str = img_path.to_string_lossy().into_owned();
        record_org_avatar_mem(
            sanitized,
            OrgAvatarState::Ready {
                path: path_str.clone(),
                checked_at: now_instant,
            },
        );
        return Some(path_str);
    }

    let url = avatar_url?.trim();
    if !url.starts_with("https://") {
        return None;
    }

    let inflight = AVATAR_IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()));
    let acquired = inflight
        .lock()
        .map(|mut set| {
            if set.contains(&sanitized) {
                false
            } else if try_acquire_worker_slot(&ACTIVE_AVATAR_WORKERS, MAX_CONCURRENT_AVATAR_WORKERS)
            {
                set.insert(sanitized.clone());
                true
            } else {
                false
            }
        })
        .unwrap_or(false);

    if acquired {
        let key_bg = sanitized.clone();
        let url_bg = normalize_avatar_url(url);
        let spawn_res = std::thread::Builder::new()
            .name("blaze-org-avatar".to_owned())
            .spawn(move || {
                let guard = InFlightWorkerGuard {
                    key: key_bg.clone(),
                    in_flight: &AVATAR_IN_FLIGHT,
                    active_workers: &ACTIVE_AVATAR_WORKERS,
                };
                let ok = fetch_org_avatar_bg(&key_bg, &url_bg, &dir);
                drop(guard);
                if ok {
                    REVISION.fetch_add(1, Ordering::Relaxed);
                }
            });

        if spawn_res.is_err() {
            if let Ok(mut set) = inflight.lock() {
                set.remove(&sanitized);
            }
            ACTIVE_AVATAR_WORKERS.fetch_sub(1, Ordering::AcqRel);
        }
    }

    None
}

fn fetch_org_avatar_bg(sanitized: &str, url: &str, dir: &Path) -> bool {
    let _ = fs::create_dir_all(dir);
    let seq = TMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tmp_path = dir.join(format!("org_{sanitized}.{pid}.{seq}.tmp"));
    let final_path = dir.join(format!("org_{sanitized}.img"));

    let ok = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "4",
            "--connect-timeout",
            "3",
            "-H",
            "User-Agent: blaze-shell/0.1",
            "-o",
        ])
        .arg(&tmp_path)
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    let valid = ok && fs::read(&tmp_path).is_ok_and(|bytes| is_valid_image_bytes(&bytes));
    if valid && fs::rename(&tmp_path, &final_path).is_ok() {
        record_org_avatar_mem(
            sanitized.to_owned(),
            OrgAvatarState::Ready {
                path: final_path.to_string_lossy().into_owned(),
                checked_at: Instant::now(),
            },
        );
        return true;
    }

    let _ = fs::remove_file(&tmp_path);
    record_org_avatar_mem(
        sanitized.to_owned(),
        OrgAvatarState::Failed {
            failed_at: Instant::now(),
        },
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_candidate(label: &str) -> CachedCandidate {
        CachedCandidate {
            label: label.to_owned(),
            insert: label.to_owned(),
            detail: "1.0.0".to_owned(),
            doc: "sample".to_owned(),
            owner_login: Some("rust-lang".to_owned()),
            avatar_url: Some("https://avatars.githubusercontent.com/u/5430905?v=4".to_owned()),
            provider: RemoteProvider::CratesIo,
        }
    }

    #[test]
    fn sanitize_login_normalizes_and_strips_unsafe_chars() {
        assert_eq!(sanitize_login("BurntSushi"), "burntsushi");
        assert_eq!(sanitize_login("rust-lang_org1"), "rust-lang_org1");
        assert_eq!(sanitize_login("../../etc/passwd?x=1"), "etcpasswdx1");
        assert_eq!(sanitize_login("  Tokio-RS!@# "), "tokio-rs");
        assert_eq!(sanitize_login(""), "");
        assert_eq!(sanitize_login("...///@@@"), "");

        let long = "A".repeat(100);
        let sanitized = sanitize_login(&long);
        assert_eq!(sanitized.len(), MAX_SANITIZED_LOGIN_LEN);
        assert!(sanitized.bytes().all(|b| b == b'a'));
    }

    #[test]
    fn is_valid_image_bytes_checks_magic_and_size() {
        // Valid PNG (> 100 bytes)
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.resize(128, 0);
        assert!(is_valid_image_bytes(&png));

        // Exact boundary: 100 bytes is rejected; 101 bytes is accepted.
        png.truncate(100);
        assert!(!is_valid_image_bytes(&png));
        png.push(0);
        assert_eq!(png.len(), 101);
        assert!(is_valid_image_bytes(&png));

        // Valid JPEG (> 100 bytes)
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe0];
        jpeg.resize(256, 0xab);
        assert!(is_valid_image_bytes(&jpeg));

        // Valid WebP (> 100 bytes)
        let mut webp = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        webp.resize(160, 0);
        assert!(is_valid_image_bytes(&webp));

        // RIFF non-WebP (e.g. WAV) must be rejected.
        let mut wav = b"RIFF\x00\x00\x00\x00WAVE".to_vec();
        wav.resize(160, 0);
        assert!(!is_valid_image_bytes(&wav));

        // HTML rate-limit / error page must be rejected even if > 100 bytes.
        let html = format!("<!DOCTYPE html><html><body>{}</body></html>", "x".repeat(200));
        assert!(!is_valid_image_bytes(html.as_bytes()));

        // JSON error body must be rejected.
        let json = format!(r#"{{"message":"API rate limit exceeded","pad":"{}"}}"#, "a".repeat(120));
        assert!(!is_valid_image_bytes(json.as_bytes()));

        // Empty and oversized payloads must be rejected.
        assert!(!is_valid_image_bytes(&[]));
        let mut oversized = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        oversized.resize(MAX_IMAGE_BYTES + 1, 0);
        assert!(!is_valid_image_bytes(&oversized));
    }

    #[test]
    fn query_cache_respects_ttl_and_eviction() {
        let mut cache = QueryCache::new();
        let t0 = Instant::now();

        cache.insert(
            "crates:ripgrep".to_owned(),
            vec![sample_candidate("ripgrep")],
            t0,
            QUERY_CACHE_TTL,
        );

        // Fresh inside 5-minute TTL window.
        assert_eq!(
            cache.get_fresh("crates:ripgrep", t0).map(<[_]>::len),
            Some(1)
        );
        assert_eq!(
            cache
                .get_fresh("crates:ripgrep", t0 + Duration::from_secs(299))
                .map(<[_]>::len),
            Some(1)
        );

        // Expired at and after 300s boundary.
        assert!(cache.get_fresh("crates:ripgrep", t0 + Duration::from_secs(300)).is_none());
        assert!(cache.get_fresh("crates:ripgrep", t0 + Duration::from_secs(600)).is_none());

        // Error entry expires on shorter error TTL.
        cache.insert(
            "gh:offline".to_owned(),
            Vec::new(),
            t0,
            QUERY_ERROR_TTL,
        );
        assert!(cache.get_fresh("gh:offline", t0 + Duration::from_secs(14)).is_some());
        assert!(cache.get_fresh("gh:offline", t0 + Duration::from_secs(15)).is_none());

        // Inserting past capacity evicts expired entries first and keeps newest entry.
        let t1 = t0 + Duration::from_secs(301);
        for i in 0..=MAX_QUERY_CACHE_ENTRIES {
            let ts = t1 + Duration::from_millis(i as u64);
            cache.insert(
                format!("crates:pkg{i}"),
                vec![sample_candidate(&format!("pkg{i}"))],
                ts,
                QUERY_CACHE_TTL,
            );
        }
        assert!(cache.entries.len() <= MAX_QUERY_CACHE_ENTRIES);
        let check_time = t1 + Duration::from_secs(1);
        assert!(cache.get_fresh("crates:ripgrep", check_time).is_none());
        assert!(cache.get_fresh("crates:pkg0", check_time).is_none());
        assert!(
            cache
                .get_fresh(&format!("crates:pkg{MAX_QUERY_CACHE_ENTRIES}"), check_time)
                .is_some()
        );
    }

    #[test]
    fn normalize_avatar_url_adds_retina_size_only_when_missing() {
        assert_eq!(
            normalize_avatar_url("https://avatars.githubusercontent.com/u/5430905?v=4"),
            "https://avatars.githubusercontent.com/u/5430905?v=4&s=96"
        );
        assert_eq!(
            normalize_avatar_url("https://avatars.githubusercontent.com/u/5430905"),
            "https://avatars.githubusercontent.com/u/5430905?s=96"
        );
        assert_eq!(
            normalize_avatar_url("https://avatars.githubusercontent.com/u/5430905?v=4&s=64"),
            "https://avatars.githubusercontent.com/u/5430905?v=4&s=64"
        );
        assert_eq!(
            normalize_avatar_url("https://example.com/avatar.png"),
            "https://example.com/avatar.png"
        );
    }
}
