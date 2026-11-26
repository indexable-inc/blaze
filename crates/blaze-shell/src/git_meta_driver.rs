//! Thin OS host driver over the `#![no_std]` sans-IO [`blaze_git_meta`] crate:
//! - Lazily queries the latest git commit touching each immediate child of a directory in ONE
//!   bounded `git log` pass per directory (zero latency on `ls -l` / `ll` execution!).
//! - Lazily fetches and **disk-caches** GitHub user avatar images (`~/Library/Caches/blaze-shell/avatars/`
//!   on macOS or `$XDG_CACHE_HOME/blaze-shell/avatars/`) keyed by deterministic FNV-1a email hash
//!   with 7-day hit TTL & 24h negative-lookup TTL.
//! - Bumps [`git_meta_revision`] whenever new commit metadata or a cached avatar arrives so the
//!   Metal UI redraws smoothly without blocking a single frame.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blaze_git_meta::{
    AvatarCacheStatus, AvatarIndexRow, AvatarSource, avatar_cache_key_in, format_avatar_index_in,
    parse_avatar_index_in, parse_dir_commits_in, parse_github_remote, resolve_avatar_source,
};
use blaze_own::{AString, AVec, Alloc};

static REVISION: AtomicU64 = AtomicU64::new(0);
static ACTIVE_LOG_WORKERS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_AVATAR_WORKERS: AtomicUsize = AtomicUsize::new(0);

const MAX_CONCURRENT_LOG_WORKERS: usize = 2;
const MAX_CONCURRENT_AVATAR_WORKERS: usize = 4;
const DIR_COMMIT_TTL: Duration = Duration::from_secs(30);
const AVATAR_OK_TTL_SECS: i64 = 7 * 86_400;
const AVATAR_MISS_TTL_SECS: i64 = 86_400;

/// Monotonic revision bumped whenever any directory commit scan or avatar download finishes.
pub fn git_meta_revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

/// Owned per-entry last-commit metadata ready for the UI layout thread.
#[derive(Clone, Debug)]
pub struct CommitMeta {
    pub full_sha: String,
    pub short_sha: String,
    pub author_name: String,
    pub author_email: String,
    pub author_time_secs: i64,
    pub subject: String,
}

/// Snapshot of last-commit metadata for every tracked entry in a directory.
#[derive(Clone, Debug)]
pub struct DirCommitSnapshot {
    pub github_repo: Option<(String, String)>,
    pub entries: HashMap<String, CommitMeta>,
    pub done: bool,
}

struct CachedDirCommits {
    updated_at: Instant,
    snapshot: Option<Arc<DirCommitSnapshot>>,
}

/// Resolved avatar state for a git author email.
#[derive(Clone, Debug)]
pub struct CachedAvatar {
    /// Local cached PNG/JPEG path ready for `Scene::avatar_image(...)`, if available.
    pub image_path: Option<String>,
    /// Resolved GitHub `@login`, if known.
    pub github_login: Option<String>,
}

struct AvatarIndexEntry {
    fetched_secs: i64,
    github_login: String,
    status: AvatarCacheStatus,
}

static DIR_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedDirCommits>>> = OnceLock::new();
static DIR_IN_FLIGHT: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

static AVATAR_INDEX: OnceLock<Mutex<HashMap<String, AvatarIndexEntry>>> = OnceLock::new();
static AVATAR_IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static REMOTE_CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<(String, String)>>>> = OnceLock::new();

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().min(i64::MAX as u64) as i64)
}

/// Local persistent disk cache directory for GitHub avatars (`~/Library/Caches/blaze-shell/avatars`).
pub fn avatar_cache_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.trim().is_empty()
    {
        return Some(PathBuf::from(xdg).join("blaze-shell").join("avatars"));
    }
    let home = std::env::var("HOME").ok()?;
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from(home).join("Library/Caches/blaze-shell/avatars"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(PathBuf::from(home).join(".cache/blaze-shell/avatars"))
    }
}

fn ensure_avatar_index() -> &'static Mutex<HashMap<String, AvatarIndexEntry>> {
    AVATAR_INDEX.get_or_init(|| {
        let mut map = HashMap::new();
        if let Some(dir) = avatar_cache_dir()
            && let Ok(raw) = fs::read_to_string(dir.join("index.tsv"))
        {
            let alloc = Alloc::global();
            for row in parse_avatar_index_in(&raw, alloc) {
                map.insert(
                    row.email_key.as_str().to_owned(),
                    AvatarIndexEntry {
                        fetched_secs: row.fetched_secs,
                        github_login: row.github_login.as_str().to_owned(),
                        status: row.status,
                    },
                );
            }
        }
        Mutex::new(map)
    })
}

fn persist_avatar_index(index: &HashMap<String, AvatarIndexEntry>) {
    let Some(dir) = avatar_cache_dir() else {
        return;
    };
    let _ = fs::create_dir_all(&dir);
    let alloc = Alloc::global();
    let mut rows: AVec<'_, AvatarIndexRow<'_>> = blaze_own::Vec::new_in(alloc);
    let mut keys: Vec<&String> = index.keys().collect();
    keys.sort();
    for k in keys {
        if let Some(v) = index.get(k) {
            rows.push(AvatarIndexRow {
                email_key: AString::from_str_in(k, alloc),
                fetched_secs: v.fetched_secs,
                github_login: AString::from_str_in(&v.github_login, alloc),
                status: v.status,
            });
        }
    }
    let encoded = format_avatar_index_in(rows.as_slice(), alloc);
    let tmp = dir.join("index.tsv.tmp");
    if fs::write(&tmp, encoded.as_str()).is_ok() {
        let _ = fs::rename(&tmp, dir.join("index.tsv"));
    }
}

/// Query cached GitHub `owner/repo` for any directory inside a git repository (sync fast cache
/// populated by `git config --get remote.origin.url`).
pub fn github_repo_for_dir(dir: &Path) -> Option<(String, String)> {
    let cache = REMOTE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(hit) = guard.get(dir)
    {
        return hit.clone();
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["config", "--get", "remote.origin.url"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let parsed = out.and_then(|url| {
        let alloc = Alloc::global();
        parse_github_remote(&url, alloc)
            .map(|r| (r.owner.as_str().to_owned(), r.repo.as_str().to_owned()))
    });
    if let Ok(mut guard) = cache.lock() {
        if guard.len() > 128 {
            guard.clear();
        }
        guard.insert(dir.to_path_buf(), parsed.clone());
    }
    parsed
}

/// Non-blocking progressive query for the latest git commit touching each immediate child of
/// `dir_path`. Returns `None` outside git repos (or before the first background scan finishes).
pub fn request_dir_commits(dir_path: &Path) -> Option<Arc<DirCommitSnapshot>> {
    if dir_path.as_os_str().is_empty() || dir_path == Path::new("/") {
        return None;
    }
    let cache = DIR_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(hit) = guard.get(dir_path)
        && hit.updated_at.elapsed() < DIR_COMMIT_TTL
    {
        return hit.snapshot.clone();
    }

    let inflight = DIR_IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()));
    let should_spawn = inflight
        .lock()
        .map(|mut set| {
            if set.contains(dir_path) {
                false
            } else if ACTIVE_LOG_WORKERS.load(Ordering::Relaxed) >= MAX_CONCURRENT_LOG_WORKERS {
                false
            } else {
                set.insert(dir_path.to_path_buf());
                true
            }
        })
        .unwrap_or(false);

    if should_spawn {
        ACTIVE_LOG_WORKERS.fetch_add(1, Ordering::Relaxed);
        let root = dir_path.to_path_buf();
        let spawn_res = std::thread::Builder::new()
            .name("blaze-git-meta".to_owned())
            .spawn(move || {
                run_dir_commit_scan(&root);
                if let Some(f) = DIR_IN_FLIGHT.get()
                    && let Ok(mut s) = f.lock()
                {
                    s.remove(&root);
                }
                ACTIVE_LOG_WORKERS.fetch_sub(1, Ordering::Relaxed);
            });
        if spawn_res.is_err() {
            if let Some(f) = DIR_IN_FLIGHT.get()
                && let Ok(mut s) = f.lock()
            {
                s.remove(dir_path);
            }
            ACTIVE_LOG_WORKERS.fetch_sub(1, Ordering::Relaxed);
        }
    }

    cache
        .lock()
        .ok()
        .and_then(|g| g.get(dir_path).and_then(|e| e.snapshot.clone()))
}

fn run_dir_commit_scan(dir: &Path) {
    let prefix_out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-prefix"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(prefix_out) = prefix_out else {
        publish_dir_commits(dir, None);
        return;
    };
    if !prefix_out.status.success() {
        publish_dir_commits(dir, None);
        return;
    }
    let rel_dir = String::from_utf8_lossy(&prefix_out.stdout).trim().to_owned();
    let github_repo = github_repo_for_dir(dir);

    let log_out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "log",
            "-n",
            "512",
            "--format=%x1e%H%x1f%h%x1f%an%x1f%ae%x1f%at%x1f%s%x1f",
            "--name-only",
            "--",
            ".",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();

    let Ok(log_out) = log_out else {
        publish_dir_commits(dir, None);
        return;
    };
    if !log_out.status.success() {
        publish_dir_commits(dir, None);
        return;
    }

    let raw = String::from_utf8_lossy(&log_out.stdout);
    let alloc = Alloc::global();
    let parsed = parse_dir_commits_in(&raw, &rel_dir, 256, alloc);

    let mut entries = HashMap::with_capacity(parsed.len());
    let mut unique_authors: Vec<(String, String)> = Vec::new();
    let mut seen_emails: HashSet<String> = HashSet::new();

    for c in parsed {
        let email = c.author_email.as_str().to_owned();
        let full_sha = c.full_sha.as_str().to_owned();
        if !email.is_empty() && unique_authors.len() < 12 && seen_emails.insert(email.clone()) {
            unique_authors.push((email.clone(), full_sha.clone()));
        }
        entries.insert(
            c.entry_name.as_str().to_owned(),
            CommitMeta {
                full_sha,
                short_sha: c.short_sha.as_str().to_owned(),
                author_name: c.author_name.as_str().to_owned(),
                author_email: email,
                author_time_secs: c.author_time_secs,
                subject: c.subject.as_str().to_owned(),
            },
        );
    }

    let snap = Arc::new(DirCommitSnapshot {
        github_repo: github_repo.clone(),
        entries,
        done: true,
    });
    publish_dir_commits(dir, Some(snap));

    // Pre-warm GitHub avatar cache in background for the unique authors seen in this directory.
    for (email, sha) in unique_authors {
        let _ = request_avatar(&email, Some(&sha), github_repo.as_ref());
    }
}

fn publish_dir_commits(dir: &Path, snapshot: Option<Arc<DirCommitSnapshot>>) {
    let cache = DIR_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        if guard.len() > 128 {
            guard.clear();
        }
        guard.insert(
            dir.to_path_buf(),
            CachedDirCommits {
                updated_at: Instant::now(),
                snapshot,
            },
        );
    }
    REVISION.fetch_add(1, Ordering::Relaxed);
}

/// Non-blocking GitHub avatar lookup with persistent local disk cache.
///
/// Returns an immediate disk-cached image path when present; otherwise kicks off a bounded
/// background fetch (from GitHub noreply email OR `gh api` / GitHub commit API) and bumps
/// [`git_meta_revision`] on completion.
pub fn request_avatar(
    author_email: &str,
    sample_sha: Option<&str>,
    github_repo: Option<&(String, String)>,
) -> CachedAvatar {
    let trimmed = author_email.trim();
    if trimmed.is_empty() {
        return CachedAvatar { image_path: None, github_login: None };
    }
    let Some(dir) = avatar_cache_dir() else {
        return CachedAvatar { image_path: None, github_login: None };
    };
    let alloc = Alloc::global();
    let key = avatar_cache_key_in(trimmed, alloc).as_str().to_owned();
    let img_path = dir.join(format!("{key}.img"));
    let now = now_unix_secs();

    let index = ensure_avatar_index();
    let mut needs_fetch = true;
    let mut cached_login: Option<String> = None;
    let mut ready_path: Option<String> = None;

    if let Ok(guard) = index.lock()
        && let Some(ent) = guard.get(&key)
    {
        let age = now.saturating_sub(ent.fetched_secs);
        if !ent.github_login.is_empty() {
            cached_login = Some(ent.github_login.clone());
        }
        match ent.status {
            AvatarCacheStatus::Ok => {
                if img_path.is_file() {
                    ready_path = Some(img_path.to_string_lossy().into_owned());
                    if age < AVATAR_OK_TTL_SECS {
                        needs_fetch = false;
                    }
                }
            }
            AvatarCacheStatus::Miss => {
                if age < AVATAR_MISS_TTL_SECS {
                    needs_fetch = false;
                }
            }
        }
    }

    if needs_fetch {
        let inflight = AVATAR_IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()));
        let should_spawn = inflight
            .lock()
            .map(|mut set| {
                if set.contains(&key) {
                    false
                } else if ACTIVE_AVATAR_WORKERS.load(Ordering::Relaxed)
                    >= MAX_CONCURRENT_AVATAR_WORKERS
                {
                    false
                } else {
                    set.insert(key.clone());
                    true
                }
            })
            .unwrap_or(false);

        if should_spawn {
            ACTIVE_AVATAR_WORKERS.fetch_add(1, Ordering::Relaxed);
            let email_owned = trimmed.to_owned();
            let key_owned = key.clone();
            let sha_owned = sample_sha.map(str::to_owned);
            let repo_owned = github_repo.cloned();
            let spawn_res = std::thread::Builder::new()
                .name("blaze-avatar".to_owned())
                .spawn(move || {
                    fetch_avatar_bg(
                        &email_owned,
                        &key_owned,
                        sha_owned.as_deref(),
                        repo_owned.as_ref(),
                        &dir,
                    );
                    if let Some(f) = AVATAR_IN_FLIGHT.get()
                        && let Ok(mut s) = f.lock()
                    {
                        s.remove(&key_owned);
                    }
                    ACTIVE_AVATAR_WORKERS.fetch_sub(1, Ordering::Relaxed);
                });
            if spawn_res.is_err() {
                if let Some(f) = AVATAR_IN_FLIGHT.get()
                    && let Ok(mut s) = f.lock()
                {
                    s.remove(&key);
                }
                ACTIVE_AVATAR_WORKERS.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    CachedAvatar {
        image_path: ready_path,
        github_login: cached_login,
    }
}

fn fetch_avatar_bg(
    email: &str,
    key: &str,
    sample_sha: Option<&str>,
    github_repo: Option<&(String, String)>,
    cache_dir: &Path,
) {
    let alloc = Alloc::global();
    let (mut login, mut avatar_url): (String, Option<String>) =
        match resolve_avatar_source(email, alloc) {
            AvatarSource::GitHubUserId { id, login } => (
                login.as_str().to_owned(),
                Some(format!("https://avatars.githubusercontent.com/u/{id}?s=64&v=4")),
            ),
            AvatarSource::GitHubLogin(login) => {
                let l = login.as_str().to_owned();
                let url = format!("https://github.com/{l}.png?size=64");
                (l, Some(url))
            }
            AvatarSource::NeedsCommitLookup => (String::new(), None),
        };

    // For regular non-noreply emails inside a GitHub repo, resolve via `gh api` (works on private
    // repos too!) with a fallback to the public GitHub commits API.
    if avatar_url.is_none()
        && let (Some((owner, repo)), Some(sha)) = (github_repo, sample_sha)
        && sha.len() >= 7
    {
        if let Some((l, u)) = lookup_github_commit_author(owner, repo, sha) {
            login = l;
            avatar_url = Some(u);
        }
    }

    let mut status = AvatarCacheStatus::Miss;
    if let Some(url) = avatar_url {
        let sized_url = if url.contains('?') {
            if url.contains("s=") || url.contains("size=") {
                url
            } else {
                format!("{url}&s=64")
            }
        } else {
            format!("{url}?s=64")
        };
        let _ = fs::create_dir_all(cache_dir);
        let tmp_path = cache_dir.join(format!("{key}.img.tmp"));
        let final_path = cache_dir.join(format!("{key}.img"));
        let ok = Command::new("curl")
            .args([
                "-fsSL",
                "--max-time",
                "5",
                "--connect-timeout",
                "3",
                "-A",
                "blaze-shell",
                "-o",
            ])
            .arg(&tmp_path)
            .arg(&sized_url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok && verify_image_magic(&tmp_path) && fs::rename(&tmp_path, &final_path).is_ok() {
            status = AvatarCacheStatus::Ok;
        } else {
            let _ = fs::remove_file(&tmp_path);
        }
    }

    let index = ensure_avatar_index();
    if let Ok(mut guard) = index.lock() {
        guard.insert(
            key.to_owned(),
            AvatarIndexEntry {
                fetched_secs: now_unix_secs(),
                github_login: login,
                status,
            },
        );
        persist_avatar_index(&guard);
    }
    REVISION.fetch_add(1, Ordering::Relaxed);
}

fn lookup_github_commit_author(owner: &str, repo: &str, sha: &str) -> Option<(String, String)> {
    let endpoint = format!("repos/{owner}/{repo}/commits/{sha}");
    // 1. Authenticated `gh api` (works seamlessly on private repos & avoids 60/hr anon cap).
    if let Ok(out) = Command::new("gh")
        .args([
            "api",
            &endpoint,
            "--jq",
            r#"(.author.login // "") + "\t" + (.author.avatar_url // "")"#,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        && out.status.success()
    {
        let line = String::from_utf8_lossy(&out.stdout);
        let trimmed = line.trim();
        if let Some((login, url)) = trimmed.split_once('\t')
            && url.starts_with("https://")
        {
            return Some((login.trim().to_owned(), url.trim().to_owned()));
        }
    }
    // 2. Public GitHub REST API fallback (for public repos when `gh` isn't signed in).
    let api_url = format!("https://api.github.com/{endpoint}");
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "4",
            "--connect-timeout",
            "2",
            "-H",
            "Accept: application/vnd.github+json",
            "-A",
            "blaze-shell",
            &api_url,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let author_pos = body.find("\"author\"")?;
    let slice = &body[author_pos..];
    let avatar_url = extract_json_str_field(slice, "avatar_url")?;
    if !avatar_url.starts_with("https://") {
        return None;
    }
    let login = extract_json_str_field(slice, "login").unwrap_or_default();
    Some((login, avatar_url))
}

fn extract_json_str_field(haystack: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let idx = haystack.find(&needle)? + needle.len();
    let rest = haystack[idx..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// Verify downloaded bytes start with a valid raster image header (PNG, JPEG, GIF, WebP) so an
/// HTML rate-limit / 404 page never poisons the local disk cache.
fn verify_image_magic(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    if bytes.len() < 64 || bytes.len() > 2 * 1024 * 1024 {
        return false;
    }
    let is_png = bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let is_jpeg = bytes.starts_with(&[0xff, 0xd8, 0xff]);
    let is_gif = bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a");
    let is_webp = bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP";
    is_png || is_jpeg || is_gif || is_webp
}
