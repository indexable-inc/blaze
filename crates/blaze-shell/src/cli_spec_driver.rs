//! Thin OS host driver over the `#![no_std]` sans-IO [`blaze_cli_spec`] crate:
//! reads system fish completions, persists parsed `--help`/`man` specs under
//! `~/.cache/blaze-shell/cli-specs/`, and runs bounded background probes without ever blocking UI.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant, UNIX_EPOCH};

use blaze_cli_spec::codec::{decode_cache_in, encode_cache_in};
use blaze_cli_spec::help_parser::parse_help_output_in;
use blaze_cli_spec::man_parser::{
    parse_fish_completions_in, parse_man_output_in, parse_man_summary_in,
};
use blaze_cli_spec::manual::manual_spec_in;
use blaze_cli_spec::{CliSpec, SpecSource, is_unsafe_to_exec_help};
use blaze_own::Alloc;

static REVISION: AtomicU64 = AtomicU64::new(0);

/// Monotonic revision bumped whenever a background `--help`/`man` probe finishes.
pub fn spec_revision() -> u64 {
    REVISION.load(Ordering::Acquire)
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    cmd: String,
    subcmd: Option<String>,
}

/// Cross-thread encoded cache entry (pure `Arc<str>` so `Alloc<'static>` never crosses threads).
struct EncodedRecord {
    bin_mtime: u128,
    bin_len: u64,
    rev: u64,
    encoded: Arc<str>,
}

struct LocalDecoded {
    bin_mtime: u128,
    bin_len: u64,
    rev: u64,
    spec: Rc<CliSpec<'static>>,
}

static SHARED_CACHE: OnceLock<Mutex<HashMap<CacheKey, EncodedRecord>>> = OnceLock::new();
static IN_FLIGHT: OnceLock<Mutex<HashSet<CacheKey>>> = OnceLock::new();

type ManSummaryKey = (String, Option<String>);
static MAN_SUMMARY_CACHE: OnceLock<Mutex<HashMap<ManSummaryKey, Option<String>>>> = OnceLock::new();
static MAN_SUMMARY_IN_FLIGHT: OnceLock<Mutex<HashSet<ManSummaryKey>>> = OnceLock::new();

const MAX_CONCURRENT_MAN_SUMMARY_PROBES: usize = 3;
const MAX_CACHED_MAN_SUMMARIES: usize = 512;
const MAN_SUMMARY_TIMEOUT: Duration = Duration::from_secs(2);
/// Read at most 64 KiB from `man` stdout so large pages (e.g. `git-log` ~156 KiB) never fill the
/// OS pipe buffer and deadlock while still capturing far past the leading `NAME` section.
const MAN_SUMMARY_MAX_BYTES: usize = 64 * 1024;

/// Non-blocking lookup of flags and subcommands for `(cmd, subcmd)`.
///
/// Immediately merges curated manual specs, disk cache, and system fish completions. If the
/// installed binary has not been dynamically probed yet for `(mtime, len)`, spawns a single
/// bounded background probe thread that updates the cache and bumps [`spec_revision`].
pub fn lookup_nonblocking(
    cmd: &str,
    subcmd: Option<&str>,
    path_env: &str,
    home: Option<&str>,
) -> Option<Rc<CliSpec<'static>>> {
    thread_local! {
        static LOCAL_CACHE: RefCell<HashMap<CacheKey, LocalDecoded>> = RefCell::new(HashMap::new());
    }

    let base_cmd = Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd);
    if base_cmd.is_empty()
        || base_cmd.contains('/')
        || base_cmd.starts_with('.')
        || base_cmd.len() > 64
    {
        return None;
    }
    let clean_sub = subcmd.filter(|s| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.len() <= 32
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    });

    let key = CacheKey {
        cmd: base_cmd.to_owned(),
        subcmd: clean_sub.map(str::to_owned),
    };

    let resolved = resolve_executable(base_cmd, path_env);
    let (bin_mtime, bin_len) = resolved.as_deref().and_then(file_stamp).unwrap_or((0, 0));

    let shared = SHARED_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let shared_hit = shared.lock().ok().and_then(|g| {
        g.get(&key)
            .filter(|e| e.bin_mtime == bin_mtime && e.bin_len == bin_len)
            .map(|e| (e.rev, Arc::clone(&e.encoded)))
    });

    if let Some((rev, encoded)) = shared_hit {
        if let Some(hit) = LOCAL_CACHE.with(|c| {
            c.borrow()
                .get(&key)
                .filter(|e| e.bin_mtime == bin_mtime && e.bin_len == bin_len && e.rev == rev)
                .map(|e| Rc::clone(&e.spec))
        }) {
            return if hit.is_empty() { None } else { Some(hit) };
        }
        let alloc = Alloc::global();
        let decoded = decode_cache_in(base_cmd, &encoded, bin_mtime, bin_len, alloc)
            .unwrap_or_else(|| CliSpec::empty_in(base_cmd, SpecSource::Help, alloc));
        let rc = Rc::new(decoded);
        LOCAL_CACHE.with(|c| {
            let mut m = c.borrow_mut();
            if m.len() > 128 {
                m.clear();
            }
            m.insert(
                key,
                LocalDecoded {
                    bin_mtime,
                    bin_len,
                    rev,
                    spec: Rc::clone(&rc),
                },
            );
        });
        return if rc.is_empty() { None } else { Some(rc) };
    }

    // 1. Assemble instant zero-subprocess base spec: manual + disk cache + fish completion.
    let alloc = Alloc::global();
    let mut base = manual_spec_in(base_cmd, clean_sub, alloc)
        .unwrap_or_else(|| CliSpec::empty_in(base_cmd, SpecSource::Manual, alloc));
    let mut already_probed = false;

    if let Some(disk_spec) = load_disk_cache(home, base_cmd, clean_sub, bin_mtime, bin_len, alloc) {
        base.merge_from(&disk_spec, alloc);
        already_probed = true;
    } else if clean_sub.is_none()
        && let Some(fish) = load_fish_file(base_cmd, home, alloc)
    {
        base.merge_from(&fish, alloc);
    }

    let encoded: Arc<str> = encode_cache_in(&base, bin_mtime, bin_len, alloc).as_str().into();
    let cur_rev = REVISION.load(Ordering::Relaxed);
    if let Ok(mut guard) = shared.lock() {
        if guard.len() > 256 {
            guard.clear();
        }
        guard.insert(
            key.clone(),
            EncodedRecord {
                bin_mtime,
                bin_len,
                rev: cur_rev,
                encoded,
            },
        );
    }
    let rc = Rc::new(base);
    LOCAL_CACHE.with(|c| {
        let mut m = c.borrow_mut();
        if m.len() > 128 {
            m.clear();
        }
        m.insert(
            key.clone(),
            LocalDecoded {
                bin_mtime,
                bin_len,
                rev: cur_rev,
                spec: Rc::clone(&rc),
            },
        );
    });

    // 2. If not yet dynamically probed and binary exists on `$PATH`, spawn bounded background probe.
    if !already_probed && let Some(bin_path) = resolved {
        let inflight = IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()));
        let should_spawn = inflight
            .lock()
            .map(|mut s| {
                if s.len() < 16 && !s.contains(&key) {
                    s.insert(key.clone());
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);

        if should_spawn {
            let cmd_owned = base_cmd.to_owned();
            let sub_owned = clean_sub.map(str::to_owned);
            let home_owned = home.map(str::to_owned);
            let key_bg = key;
            let _ = std::thread::Builder::new()
                .name(format!("cli-spec-{cmd_owned}"))
                .spawn(move || {
                    let alloc = Alloc::global();
                    let probed =
                        probe_binary_spec(&cmd_owned, &bin_path, sub_owned.as_deref(), alloc);
                    if let Some(ref pspec) = probed {
                        save_disk_cache(
                            home_owned.as_deref(),
                            &cmd_owned,
                            sub_owned.as_deref(),
                            bin_mtime,
                            bin_len,
                            pspec,
                            alloc,
                        );
                    }
                    let mut merged = manual_spec_in(&cmd_owned, sub_owned.as_deref(), alloc)
                        .unwrap_or_else(|| CliSpec::empty_in(&cmd_owned, SpecSource::Help, alloc));
                    if sub_owned.is_none()
                        && let Some(fish) =
                            load_fish_file(&cmd_owned, home_owned.as_deref(), alloc)
                    {
                        merged.merge_from(&fish, alloc);
                    }
                    if let Some(pspec) = probed {
                        merged.merge_from(&pspec, alloc);
                    }
                    let encoded: Arc<str> =
                        encode_cache_in(&merged, bin_mtime, bin_len, alloc).as_str().into();
                    let next_rev = REVISION.fetch_add(1, Ordering::Release) + 1;
                    if let Some(c) = SHARED_CACHE.get()
                        && let Ok(mut g) = c.lock()
                    {
                        g.insert(
                            key_bg.clone(),
                            EncodedRecord {
                                bin_mtime,
                                bin_len,
                                rev: next_rev,
                                encoded,
                            },
                        );
                    }
                    if let Some(f) = IN_FLIGHT.get()
                        && let Ok(mut s) = f.lock()
                    {
                        s.remove(&key_bg);
                    }
                });
        }
    }

    if rc.is_empty() { None } else { Some(rc) }
}

/// Non-blocking lookup of the one-line man-page `NAME` summary for `(cmd, subcmd)`.
///
/// - On cache hit in [`MAN_SUMMARY_CACHE`], returns immediately with zero IO.
/// - On cache miss, spawns a bounded background probe thread (at most 3 concurrent probes) that
///   runs `man` (trying `<cmd>-<subcmd>` first when `subcmd` is `Some`, then `<cmd>`) with
///   `MANPAGER=cat`, `PAGER=cat`, `MANWIDTH=100`, `Stdio::null()` stdin, and a 2-second timeout,
///   parses stdout via [`parse_man_summary_in`], caches the result, and bumps [`spec_revision`].
pub fn lookup_man_summary_nonblocking(cmd: &str, subcmd: Option<&str>) -> Option<String> {
    let base_cmd = Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd);
    if base_cmd.is_empty()
        || base_cmd.contains('/')
        || base_cmd.starts_with('.')
        || base_cmd.starts_with('-')
        || base_cmd.len() > 64
        || !base_cmd
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '+')
    {
        return None;
    }
    let clean_sub = subcmd.filter(|s| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.len() <= 32
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    });

    let key: ManSummaryKey = (base_cmd.to_owned(), clean_sub.map(str::to_owned));

    let cache = MAN_SUMMARY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(cached) = guard.get(&key)
    {
        return cached.clone();
    }

    let in_flight = MAN_SUMMARY_IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()));
    let should_spawn = in_flight
        .lock()
        .map(|mut set| {
            if set.len() < MAX_CONCURRENT_MAN_SUMMARY_PROBES && !set.contains(&key) {
                set.insert(key.clone());
                true
            } else {
                false
            }
        })
        .unwrap_or(false);

    if should_spawn {
        let key_bg = key;
        let thread_name = format!("man-summary-{}", key_bg.0);
        let spawned = std::thread::Builder::new().name(thread_name).spawn(move || {
            struct InFlightGuard {
                key: ManSummaryKey,
            }
            impl Drop for InFlightGuard {
                fn drop(&mut self) {
                    if let Some(set) = MAN_SUMMARY_IN_FLIGHT.get()
                        && let Ok(mut guard) = set.lock()
                    {
                        guard.remove(&self.key);
                    }
                }
            }
            let guard = InFlightGuard { key: key_bg };

            let (summary, base_summary) =
                probe_man_summary(&guard.key.0, guard.key.1.as_deref(), MAN_SUMMARY_TIMEOUT);

            let cache = MAN_SUMMARY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
            if let Ok(mut map) = cache.lock() {
                if map.len() >= MAX_CACHED_MAN_SUMMARIES {
                    map.clear();
                }
                if let Some(base) = base_summary {
                    let base_key = (guard.key.0.clone(), None);
                    map.entry(base_key).or_insert(base);
                }
                map.insert(guard.key.clone(), summary);
            }
            REVISION.fetch_add(1, Ordering::Release);
        });

        if spawned.is_err()
            && let Some(set) = MAN_SUMMARY_IN_FLIGHT.get()
            && let Ok(mut guard) = set.lock()
        {
            guard.remove(&(base_cmd.to_owned(), clean_sub.map(str::to_owned)));
        }
    }

    None
}

/// Runs `man` for `(cmd, subcmd)` within `timeout` and extracts its `NAME` summary.
///
/// Returns `(resolved_summary, optional_base_cmd_summary)` so a fallback probe of `cmd` can also
/// populate `(cmd, None)` in [`MAN_SUMMARY_CACHE`].
fn probe_man_summary(
    cmd: &str,
    subcmd: Option<&str>,
    timeout: Duration,
) -> (Option<String>, Option<Option<String>>) {
    let alloc = Alloc::global();
    let deadline = Instant::now() + timeout;

    if let Some(sub) = subcmd {
        let sub_topic = format!("{cmd}-{sub}");
        let remaining = deadline.saturating_duration_since(Instant::now());
        if let Some(text) = run_man_stdout(&sub_topic, remaining)
            && let Some(parsed) = parse_man_summary_in(&text, alloc)
        {
            let trimmed = parsed.as_str().trim();
            if !trimmed.is_empty() {
                return (Some(trimmed.to_owned()), None);
            }
        }

        // Reuse cached base-command summary if already available before spawning another `man`.
        if let Some(cache) = MAN_SUMMARY_CACHE.get()
            && let Ok(map) = cache.lock()
            && let Some(cached_base) = map.get(&(cmd.to_owned(), None))
        {
            return (cached_base.clone(), None);
        }
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    let base_summary = run_man_stdout(cmd, remaining).and_then(|text| {
        let parsed = parse_man_summary_in(&text, alloc)?;
        let trimmed = parsed.as_str().trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    });

    if subcmd.is_some() {
        (base_summary.clone(), Some(base_summary))
    } else {
        (base_summary, None)
    }
}

/// Spawns `/usr/bin/man <topic>` non-interactively with a bounded timeout and capped stdout read,
/// returning `None` on timeout, missing page, or empty output.
fn run_man_stdout(topic: &str, timeout: Duration) -> Option<String> {
    if topic.is_empty() || timeout.is_zero() {
        return None;
    }

    let mut child = Command::new("/usr/bin/man")
        .arg(topic)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("MANPAGER", "cat")
        .env("PAGER", "cat")
        .env("MANWIDTH", "100")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("TERM", "dumb")
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
    let reader_spawned = std::thread::Builder::new()
        .name("man-stdout-read".to_owned())
        .spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.take(MAN_SUMMARY_MAX_BYTES as u64).read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    if reader_spawned.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }

    match rx.recv_timeout(timeout) {
        Ok(buf) => {
            if buf.len() >= MAN_SUMMARY_MAX_BYTES {
                // Early cap reached on a large man page: terminate and reap cleanly.
                let _ = child.kill();
                let _ = child.wait();
            } else {
                // Stdout hit EOF; reap with a short bounded grace period so we never hang.
                let reap_start = Instant::now();
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if reap_start.elapsed() < Duration::from_millis(50) => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        _ => {
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                    }
                }
            }
            if buf.is_empty() {
                return None;
            }
            let text = String::from_utf8_lossy(&buf).into_owned();
            if text.trim().is_empty() { None } else { Some(text) }
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

fn load_fish_file<'a>(cmd: &str, home: Option<&str>, alloc: Alloc<'a>) -> Option<CliSpec<'a>> {
    let mut dirs: Vec<PathBuf> = vec![
        PathBuf::from("/opt/homebrew/share/fish/vendor_completions.d"),
        PathBuf::from("/opt/homebrew/share/fish/completions"),
        PathBuf::from("/usr/local/share/fish/vendor_completions.d"),
        PathBuf::from("/usr/local/share/fish/completions"),
        PathBuf::from("/usr/share/fish/vendor_completions.d"),
        PathBuf::from("/usr/share/fish/completions"),
    ];
    if let Some(h) = home {
        dirs.insert(0, PathBuf::from(h).join(".config/fish/completions"));
    }
    let filename = format!("{cmd}.fish");
    for dir in dirs {
        if let Ok(src) = fs::read_to_string(dir.join(&filename)) {
            let spec = parse_fish_completions_in(cmd, &src, alloc);
            if !spec.is_empty() {
                return Some(spec);
            }
        }
    }
    None
}

fn probe_binary_spec<'a>(
    cmd: &str,
    bin_path: &Path,
    subcmd: Option<&str>,
    alloc: Alloc<'a>,
) -> Option<CliSpec<'a>> {
    let label = match subcmd {
        Some(s) => format!("{cmd} {s}"),
        None => cmd.to_owned(),
    };
    if !is_unsafe_to_exec_help(cmd) {
        for help_flag in ["--help", "-h"] {
            let mut args: Vec<&str> = Vec::new();
            if let Some(s) = subcmd {
                args.push(s);
            }
            args.push(help_flag);
            if let Some(out) = run_bounded(bin_path, &args, Duration::from_millis(1500)) {
                let spec = parse_help_output_in(&label, &out, alloc);
                if !spec.is_empty() {
                    return Some(spec);
                }
            }
        }
    }
    let man_topic = match subcmd {
        Some(s) => format!("{cmd}-{s}"),
        None => cmd.to_owned(),
    };
    if let Some(out) =
        run_bounded(Path::new("/usr/bin/man"), &[&man_topic], Duration::from_millis(1200))
    {
        let spec = parse_man_output_in(&label, &out, alloc);
        if !spec.is_empty() {
            return Some(spec);
        }
    }
    None
}

fn run_bounded(program: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("PAGER", "cat")
        .env("MANPAGER", "cat")
        .env("TERM", "dumb")
        .env("COLUMNS", "120")
        .env("CLAUDE_CODE_SIMPLE", "1")
        .spawn()
        .ok()?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let out = child.wait_with_output().ok()?;
                let mut combined = String::new();
                if !out.stdout.is_empty() {
                    combined.push_str(&String::from_utf8_lossy(&out.stdout));
                }
                if !out.stderr.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(&String::from_utf8_lossy(&out.stderr));
                }
                if combined.len() > 256 * 1024 {
                    combined.truncate(256 * 1024);
                }
                return if combined.trim().is_empty() { None } else { Some(combined) };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

fn resolve_executable(cmd: &str, path_env: &str) -> Option<PathBuf> {
    for dir in path_env.split(':') {
        if dir.is_empty() {
            continue;
        }
        let cand = Path::new(dir).join(cmd);
        if let Ok(meta) = fs::metadata(&cand)
            && meta.is_file()
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 != 0 {
                    return Some(cand);
                }
            }
            #[cfg(not(unix))]
            return Some(cand);
        }
    }
    None
}

fn file_stamp(p: &Path) -> Option<(u128, u64)> {
    let m = fs::metadata(p).ok()?;
    let mt = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Some((mt, m.len()))
}

fn cache_file_path(home: Option<&str>, cmd: &str, subcmd: Option<&str>) -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.trim().is_empty()
    {
        PathBuf::from(xdg).join("blaze-shell/cli-specs")
    } else {
        PathBuf::from(home?).join(".cache/blaze-shell/cli-specs")
    };
    let name = match subcmd {
        Some(s) => format!("{cmd}__{s}.spec"),
        None => format!("{cmd}.spec"),
    };
    Some(base.join(name))
}

fn load_disk_cache<'a>(
    home: Option<&str>,
    cmd: &str,
    subcmd: Option<&str>,
    bin_mtime: u128,
    bin_len: u64,
    alloc: Alloc<'a>,
) -> Option<CliSpec<'a>> {
    let path = cache_file_path(home, cmd, subcmd)?;
    let raw = fs::read_to_string(&path).ok()?;
    decode_cache_in(cmd, &raw, bin_mtime, bin_len, alloc)
}

fn save_disk_cache(
    home: Option<&str>,
    cmd: &str,
    subcmd: Option<&str>,
    bin_mtime: u128,
    bin_len: u64,
    spec: &CliSpec<'_>,
    alloc: Alloc<'_>,
) {
    let Some(path) = cache_file_path(home, cmd, subcmd) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let encoded = encode_cache_in(spec, bin_mtime, bin_len, alloc);
    let _ = fs::write(&path, encoded.as_str());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_man_summary_rejects_invalid_command_names() {
        assert_eq!(lookup_man_summary_nonblocking("", None), None);
        assert_eq!(lookup_man_summary_nonblocking("-rf", None), None);
        assert_eq!(lookup_man_summary_nonblocking(".hidden", None), None);
        assert_eq!(lookup_man_summary_nonblocking("bad cmd", None), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn lookup_man_summary_populates_cache_and_bumps_revision() {
        let rev_before = spec_revision();
        let initial = lookup_man_summary_nonblocking("ls", None);
        if initial.is_some() {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut resolved = None;
        while Instant::now() < deadline {
            if let Some(summary) = lookup_man_summary_nonblocking("ls", None) {
                resolved = Some(summary);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            spec_revision() > rev_before,
            "expected spec_revision() to advance after background man probe"
        );
        assert!(
            resolved.as_deref().is_some_and(|s| !s.is_empty()),
            "expected non-empty man NAME summary for `ls`"
        );
    }
}
