//! Thin native macOS telemetry & `.app` Retina icon driver over [`blaze_sysmon`].
//!
//! Zero external subprocesses (`ps`, `top`, `ioreg`, `sips` are never spawned):
//! - Per-core CPU ticks, Activity Monitor-exact memory (`internal - purgeable`, wired,
//!   compressed, cached), swap, boottime uptime, load average, Mach timebase, process inventory,
//!   physical footprint, CPU nanoseconds, disk I/O, threads, CWD, command lines, and POSIX signals
//!   routed through [`blaze_sys::macos_proc`]
//! - Apple Silicon GPU model, core count, device/renderer/tiler utilization, VRAM, and per-process
//!   GPU time + active command queues via `IOKit.framework` (`IOAccelerator` & child
//!   `AGXDeviceUserClient` `AppUsage` entries) + `CoreFoundation.framework`
//! - Persistent 48×48 Retina `.app` icon PNG disk cache (`~/Library/Caches/blaze-shell/app_icons/`)
//!   extracted lazily via `NSWorkspace::sharedWorkspace().iconForFile(...)` + `CoreGraphics` +
//!   `ImageIO`, bumping [`app_icon_revision`] on completion so the UI repaints cleanly.

use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicUsize;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blaze_own::Alloc;
use blaze_sysmon::{RawSnapshot, ShellPanePid, SysMonEngine};

static MONOTONIC_EPOCH: OnceLock<Instant> = OnceLock::new();
static APP_ICON_REVISION: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "macos")]
static ACTIVE_ICON_WORKERS: AtomicUsize = AtomicUsize::new(0);
static APP_ICON_CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
#[cfg(target_os = "macos")]
static APP_ICON_IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

#[cfg(target_os = "macos")]
const MAX_CONCURRENT_ICON_WORKERS: usize = 4;
const MAX_CACHED_APP_ICONS: usize = 512;

/// Monotonic nanoseconds elapsed since this process's telemetry epoch (never jumps backwards).
fn monotonic_now_ns() -> u64 {
    let epoch = MONOTONIC_EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

/// Monotonic revision bumped whenever an `.app` Retina icon finishes caching to disk.
pub fn app_icon_revision() -> u64 {
    APP_ICON_REVISION.load(Ordering::Acquire)
}

/// Persistent disk cache directory for extracted `.app` bundle icons
/// (`~/Library/Caches/blaze-shell/app_icons` on macOS, or `$XDG_CACHE_HOME/blaze-shell/app_icons`),
/// matching [`crate::git_meta_driver::avatar_cache_dir`].
pub fn app_icon_cache_dir() -> Option<PathBuf> {
    let avatars_dir = crate::git_meta_driver::avatar_cache_dir()?;
    Some(avatars_dir.parent()?.join("app_icons"))
}

/// Deterministic 64-bit FNV-1a hash formatted as 16 lowercase hex chars for cache filenames.
fn fnv1a_64_hex(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in input.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Parse a PID from an IOKit `IOUserClientCreator` string such as `"pid 465, WindowServer"`.
pub fn parse_creator_pid(creator: &str) -> Option<u32> {
    let idx = creator.find("pid ")?;
    let rest = &creator[idx + 4..];
    let digits_len = rest
        .bytes()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digits_len == 0 {
        return None;
    }
    rest[..digits_len].parse::<u32>().ok()
}

/// Parse a macOS `sysctl([CTL_KERN, KERN_PROCARGS2, pid])` byte buffer into a concise
/// displayable command line (at most 120 chars, using the basename of `argv[0]`).
pub fn parse_kern_procargs2(buf: &[u8]) -> String {
    const MAX_ARGS: usize = 64;
    const MAX_CMDLINE_CHARS: usize = 120;

    if buf.len() < 4 {
        return String::new();
    }
    let raw_argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if raw_argc <= 0 {
        return String::new();
    }
    let argc = (raw_argc as usize).clamp(1, MAX_ARGS);

    // Skip the NUL-terminated `exec_path` immediately following `argc`.
    let mut pos = 4usize;
    let Some(exec_nul) = buf[pos..].iter().position(|&b| b == 0) else {
        return String::new();
    };
    pos += exec_nul;
    // Skip the `exec_path` NUL terminator plus any trailing NUL alignment padding bytes.
    while pos < buf.len() && buf[pos] == 0 {
        pos += 1;
    }

    let mut args: Vec<String> = Vec::with_capacity(argc.min(16));
    for arg_idx in 0..argc {
        if pos >= buf.len() {
            break;
        }
        let end = match buf[pos..].iter().position(|&b| b == 0) {
            Some(rel) => pos + rel,
            None => buf.len(),
        };
        let raw = String::from_utf8_lossy(&buf[pos..end]);
        if !raw.is_empty() {
            if arg_idx == 0 {
                let display = Path::new(raw.as_ref())
                    .file_name()
                    .and_then(|s| s.to_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(raw.as_ref());
                args.push(display.to_owned());
            } else {
                args.push(raw.into_owned());
            }
        }
        pos = end.saturating_add(1);
    }

    let joined = args.join(" ");
    if joined.chars().count() <= MAX_CMDLINE_CHARS {
        return joined;
    }
    let mut truncated: String = joined.chars().take(MAX_CMDLINE_CHARS - 1).collect();
    while truncated.ends_with(' ') {
        truncated.pop();
    }
    truncated.push('…');
    truncated
}

/// Read the displayable command line (`argv[0]` basename + arguments, <= 120 chars) for `pid`.
///
/// On macOS this queries `sysctl([CTL_KERN, KERN_PROCARGS2, pid])` directly with zero subprocesses.
/// Intended for small result sets (such as the top 16 folder processes).
pub fn read_proc_cmdline(pid: u32) -> String {
    #[cfg(target_os = "macos")]
    {
        macos::read_proc_cmdline(pid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        String::new()
    }
}

/// Capture a complete system telemetry snapshot (CPU cores, memory, swap, GPU, per-process metrics,
/// load average, and uptime) in one native FFI pass with zero subprocesses.
pub fn sample_snapshot(alloc: Alloc<'static>) -> RawSnapshot<'static> {
    let timestamp_ns = monotonic_now_ns();
    let wall_time_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    #[cfg(target_os = "macos")]
    {
        let cores = macos::sample_cores(alloc);
        let (mem, load_avg, uptime_secs) = macos::sample_memory_and_host();
        let (gpu, gpu_by_pid) = macos::sample_macos_gpu(alloc);
        let processes = macos::sample_processes(alloc, &gpu_by_pid);
        RawSnapshot {
            timestamp_ns,
            wall_time_secs,
            cores,
            mem,
            gpu,
            processes,
            load_avg,
            uptime_secs,
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        use blaze_own::{AString, AVec};
        use blaze_sysmon::{RawGpuSample, RawMemSample};

        RawSnapshot {
            timestamp_ns,
            wall_time_secs,
            cores: AVec::new_in(alloc),
            mem: RawMemSample {
                total_bytes: 0,
                app_bytes: 0,
                wired_bytes: 0,
                compressed_bytes: 0,
                cached_bytes: 0,
                swap_used_bytes: 0,
                swap_total_bytes: 0,
            },
            gpu: RawGpuSample {
                model: AString::new_in(alloc),
                core_count: 0,
                device_util_pct: 0.0,
                renderer_util_pct: 0.0,
                tiler_util_pct: 0.0,
                vram_in_use_bytes: 0,
                vram_alloc_bytes: 0,
            },
            processes: AVec::new_in(alloc),
            load_avg: [0.0; 3],
            uptime_secs: 0,
        }
    }
}

/// Look up a cached 48×48 Retina PNG icon for a macOS `.app` bundle path, or schedule bounded
/// background extraction via `NSWorkspace` + `CoreGraphics` + `ImageIO`.
///
/// - O(1) in-memory map hit with zero disk IO once resolved.
/// - Immediate disk-cache hit when `~/Library/Caches/blaze-shell/app_icons/<fnv1a>.png` already
///   exists and is non-empty.
/// - On cache miss, spawns at most `MAX_CONCURRENT_ICON_WORKERS` background extractor threads and
///   bumps [`app_icon_revision`] once the PNG is atomically persisted to disk.
pub fn lookup_or_cache_app_icon(app_bundle_path: &str) -> Option<String> {
    if app_bundle_path.is_empty() {
        return None;
    }

    let cache = APP_ICON_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(entry) = guard.get(app_bundle_path)
    {
        return entry.clone();
    }

    let cache_dir = app_icon_cache_dir()?;
    let hash_hex = fnv1a_64_hex(app_bundle_path);
    let target_path = cache_dir.join(format!("{hash_hex}.png"));

    if let Ok(meta) = fs::metadata(&target_path)
        && meta.is_file()
        && meta.len() > 0
    {
        let path_str = target_path.to_string_lossy().into_owned();
        if let Ok(mut guard) = cache.lock() {
            if guard.len() >= MAX_CACHED_APP_ICONS {
                guard.clear();
            }
            guard.insert(app_bundle_path.to_owned(), Some(path_str.clone()));
        }
        return Some(path_str);
    }

    #[cfg(target_os = "macos")]
    {
        let in_flight = APP_ICON_IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()));
        let inserted = match in_flight.lock() {
            Ok(mut set) => set.insert(app_bundle_path.to_owned()),
            Err(_) => false,
        };
        if !inserted {
            return None;
        }

        // Acquire a bounded background worker slot without exceeding MAX_CONCURRENT_ICON_WORKERS.
        let mut acquired = false;
        let _ = ACTIVE_ICON_WORKERS.try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
            if cur < MAX_CONCURRENT_ICON_WORKERS {
                acquired = true;
                Some(cur + 1)
            } else {
                None
            }
        });
        if !acquired {
            if let Ok(mut set) = in_flight.lock() {
                set.remove(app_bundle_path);
            }
            return None;
        }

        let bundle_owned = app_bundle_path.to_owned();
        let spawned = std::thread::Builder::new()
            .name("blaze-app-icon".to_owned())
            .spawn(move || {
                struct WorkerSlotGuard<'a> {
                    bundle: &'a str,
                }
                impl Drop for WorkerSlotGuard<'_> {
                    fn drop(&mut self) {
                        if let Some(set) = APP_ICON_IN_FLIGHT.get()
                            && let Ok(mut guard) = set.lock()
                        {
                            guard.remove(self.bundle);
                        }
                        ACTIVE_ICON_WORKERS.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                let _guard = WorkerSlotGuard {
                    bundle: &bundle_owned,
                };

                let result = macos::extract_and_cache_app_icon_png(&bundle_owned, &cache_dir, &target_path, &hash_hex);
                let cache = APP_ICON_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
                if let Ok(mut guard) = cache.lock() {
                    if guard.len() >= MAX_CACHED_APP_ICONS {
                        guard.clear();
                    }
                    guard.insert(bundle_owned.clone(), result.clone());
                }
                if result.is_some() {
                    APP_ICON_REVISION.fetch_add(1, Ordering::Release);
                }
            });

        if spawned.is_err() {
            if let Ok(mut set) = in_flight.lock() {
                set.remove(app_bundle_path);
            }
            ACTIVE_ICON_WORKERS.fetch_sub(1, Ordering::AcqRel);
        }
    }

    None
}

/// Send `SIGTERM` (`sigkill = false`) or `SIGKILL` (`sigkill = true`) to a target process ID.
///
/// Refuses `pid == 0` or values exceeding `i32::MAX` so POSIX `kill(0, sig)` / `kill(-1, sig)`
/// can never broadcast to a process group.
pub fn send_process_signal(pid: u32, sigkill: bool) -> Result<(), String> {
    if pid == 0 {
        return Err("refusing to signal pid 0 (process group / kernel_task)".to_owned());
    }
    let raw_pid = i32::try_from(pid)
        .map_err(|_| format!("pid {pid} exceeds POSIX pid_t range (i32::MAX)"))?;

    const SIGKILL: i32 = 9;
    const SIGTERM: i32 = 15;
    let sig = if sigkill { SIGKILL } else { SIGTERM };
    let sig_name = if sigkill { "SIGKILL" } else { "SIGTERM" };

    #[cfg(target_os = "macos")]
    {
        blaze_sys::macos_proc::kill_pid(raw_pid, sig)
            .map_err(|err| format!("failed to send {sig_name} to pid {pid}: {err}"))
    }

    #[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        blaze_sys::posix::signal::kill(raw_pid, sig)
            .map_err(|err| format!("failed to send {sig_name} to pid {pid}: {err}"))
    }

    #[cfg(not(any(
        target_os = "macos",
        all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64"))
    )))]
    {
        let _ = (raw_pid, sig, sig_name);
        Err(format!("send_process_signal(pid={pid}) is unsupported on this platform"))
    }
}

// -----------------------------------------------------------------------------
// Background non-blocking folder process watcher (zero-block UI polling)
// -----------------------------------------------------------------------------

/// Owned UI-ready description of one process associated with a watched folder.
#[derive(Clone, Debug, PartialEq)]
pub struct FolderProcItem {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub cwd: String,
    pub rel_dir: String,
    pub exe_path: String,
    pub cmdline: String,
    pub parent_name: String,
    pub app_icon_path: Option<String>,
    pub origin_app_name: String,
    pub origin_app_icon_path: Option<String>,
    pub origin_chain: String,
    pub cpu_pct: f32,
    pub gpu_pct: f32,
    pub mem_bytes: u64,
    pub threads: u32,
    pub uptime_secs: u64,
    pub is_hot: bool,
    pub via_descendant: bool,
    pub shell_pane: Option<ShellPanePid>,
}

/// Owned UI snapshot of all active processes rooted under one watched folder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FolderProcSnapshot {
    pub folder: String,
    pub items: Vec<FolderProcItem>,
    pub total_cpu_pct: f32,
    pub total_gpu_pct: f32,
    pub total_mem_bytes: u64,
    pub is_hot: bool,
    pub primary_name: String,
}

const MAX_WATCHED_FOLDERS: usize = 8;
const FOLDER_POLL_ACTIVE_TTL: Duration = Duration::from_secs(10);
const FOLDER_SAMPLE_INTERVAL: Duration = Duration::from_millis(1400);
const FOLDER_IDLE_WAIT: Duration = Duration::from_secs(5);

struct WatchedFolder {
    pane_roots: Vec<ShellPanePid>,
    last_polled: Instant,
}

struct WatchState {
    folders: HashMap<String, WatchedFolder>,
    /// Incremented whenever a brand-new folder is registered or polling resumes after idle.
    wake_seq: u64,
}

struct FolderProcWatcher {
    cache: Mutex<HashMap<String, FolderProcSnapshot>>,
    watch: Mutex<WatchState>,
    wake: Condvar,
}

impl FolderProcWatcher {
    fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            watch: Mutex::new(WatchState {
                folders: HashMap::new(),
                wake_seq: 0,
            }),
            wake: Condvar::new(),
        }
    }
}

static FOLDER_PROC_REVISION: AtomicU64 = AtomicU64::new(0);
static FOLDER_PROC_WATCHER: OnceLock<FolderProcWatcher> = OnceLock::new();
static FOLDER_PROC_WORKER_STARTED: OnceLock<()> = OnceLock::new();

/// Monotonic revision bumped whenever the background folder process sampler refreshes its cache.
pub fn folder_proc_revision() -> u64 {
    FOLDER_PROC_REVISION.load(Ordering::Acquire)
}

/// Immediately wake the background folder-process sampler (e.g. after killing a PID from the popover)
/// and optimistically strip `killed_pid` from all cached folder snapshots so UI updates in 0ms.
pub fn notify_folder_proc_killed(killed_pid: u32) {
    if let Some(watcher) = FOLDER_PROC_WATCHER.get() {
        if let Ok(mut cache) = watcher.cache.lock() {
            for snap in cache.values_mut() {
                snap.items.retain(|it| it.pid != killed_pid);
                snap.total_cpu_pct = snap.items.iter().map(|it| it.cpu_pct).sum();
                snap.total_gpu_pct = snap.items.iter().map(|it| it.gpu_pct).sum();
                snap.total_mem_bytes = snap
                    .items
                    .iter()
                    .fold(0u64, |acc, it| acc.saturating_add(it.mem_bytes));
                snap.is_hot = snap.total_cpu_pct >= 25.0
                    || snap.total_gpu_pct >= 15.0
                    || snap.items.iter().any(|it| it.is_hot);
                snap.primary_name = snap
                    .items
                    .first()
                    .map(|it| it.name.clone())
                    .unwrap_or_default();
            }
        }
        if let Ok(mut state) = watcher.watch.lock() {
            state.wake_seq = state.wake_seq.wrapping_add(1);
            watcher.wake.notify_all();
        }
        FOLDER_PROC_REVISION.fetch_add(1, Ordering::Release);
    }
}

/// Resolve version-only executable basenames like `~/.local/share/claude/versions/2.1.332`
/// to their parent tool name (`"claude"`) so Starship pills, popovers, and `top` stay readable.
fn friendly_versioned_exe_name<'a>(chosen_name: &'a str, exe_path: &'a str) -> &'a str {
    let looks_like_version = !chosen_name.is_empty()
        && chosen_name.bytes().any(|b| b.is_ascii_digit())
        && chosen_name
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_'));
    if looks_like_version
        && let Some((prefix, _)) = exe_path.rsplit_once("/versions/")
        && let Some(tool) = prefix.rsplit('/').next()
        && !tool.is_empty()
    {
        return tool;
    }
    chosen_name
}

/// Zero-block O(1) lookup of active processes running inside `folder`.
///
/// Registers `(folder, pane_roots)` into a bounded watch table (at most 8 active folders) and
/// lazily starts a single background sampler thread (`blaze-folder-procs`) that refreshes every
/// 1.4s while polled and parks cleanly when idle for more than 10s. Registering a brand-new folder
/// wakes the sampler immediately so directory switches update promptly.
pub fn poll_folder_procs(folder: &str, pane_roots: &[ShellPanePid]) -> FolderProcSnapshot {
    if folder.is_empty() {
        return FolderProcSnapshot::default();
    }

    let watcher = FOLDER_PROC_WATCHER.get_or_init(FolderProcWatcher::new);
    let now = Instant::now();
    let mut should_wake = false;

    if let Ok(mut state) = watcher.watch.lock() {
        if let Some(entry) = state.folders.get_mut(folder) {
            if now.duration_since(entry.last_polled) > FOLDER_POLL_ACTIVE_TTL {
                should_wake = true;
            }
            entry.last_polled = now;
            if entry.pane_roots.as_slice() != pane_roots {
                entry.pane_roots.clear();
                entry.pane_roots.extend_from_slice(pane_roots);
            }
            if should_wake {
                state.wake_seq = state.wake_seq.wrapping_add(1);
            }
        } else {
            // Evict the least-recently-polled folder when the watch table reaches its cap.
            if state.folders.len() >= MAX_WATCHED_FOLDERS
                && let Some(oldest_key) = state
                    .folders
                    .iter()
                    .min_by_key(|(_, v)| v.last_polled)
                    .map(|(k, _)| k.clone())
            {
                state.folders.remove(&oldest_key);
            }
            state.folders.insert(
                folder.to_owned(),
                WatchedFolder {
                    pane_roots: pane_roots.to_vec(),
                    last_polled: now,
                },
            );
            state.wake_seq = state.wake_seq.wrapping_add(1);
            should_wake = true;
        }
    }

    FOLDER_PROC_WORKER_STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("blaze-folder-procs".to_owned())
            .spawn(folder_proc_worker_loop);
    });

    if should_wake {
        watcher.wake.notify_one();
    }

    if let Ok(cache) = watcher.cache.lock()
        && let Some(snap) = cache.get(folder)
    {
        return snap.clone();
    }

    FolderProcSnapshot {
        folder: folder.to_owned(),
        ..FolderProcSnapshot::default()
    }
}

fn folder_proc_worker_loop() {
    let alloc = Alloc::global();
    let mut engine = SysMonEngine::new_in(alloc);
    let own_pid = std::process::id();

    loop {
        let Some(watcher) = FOLDER_PROC_WATCHER.get() else {
            return;
        };

        // Snapshot recently-polled folders and park cleanly when nothing has been polled for 10s.
        let (active, observed_wake_seq): (Vec<(String, Vec<ShellPanePid>)>, u64) = {
            let Ok(mut state) = watcher.watch.lock() else {
                return;
            };
            let now = Instant::now();
            state
                .folders
                .retain(|_, w| now.duration_since(w.last_polled) <= FOLDER_POLL_ACTIVE_TTL);

            if state.folders.is_empty() {
                let seq = state.wake_seq;
                let Ok((mut state, _)) = watcher.wake.wait_timeout_while(
                    state,
                    FOLDER_IDLE_WAIT,
                    |s| s.wake_seq == seq && s.folders.is_empty(),
                ) else {
                    return;
                };
                let now = Instant::now();
                state
                    .folders
                    .retain(|_, w| now.duration_since(w.last_polled) <= FOLDER_POLL_ACTIVE_TTL);
                if state.folders.is_empty() {
                    continue;
                }
                let list = state
                    .folders
                    .iter()
                    .take(MAX_WATCHED_FOLDERS)
                    .map(|(k, v)| (k.clone(), v.pane_roots.clone()))
                    .collect();
                (list, state.wake_seq)
            } else {
                let list = state
                    .folders
                    .iter()
                    .take(MAX_WATCHED_FOLDERS)
                    .map(|(k, v)| (k.clone(), v.pane_roots.clone()))
                    .collect();
                (list, state.wake_seq)
            }
        };

        let raw = sample_snapshot(alloc);
        engine.ingest(raw);

        let mut updated: Vec<(String, FolderProcSnapshot)> = Vec::with_capacity(active.len());
        for (folder, roots) in &active {
            let summary = engine.folder_processes_in(folder, own_pid, roots, alloc);
            let mut items = Vec::with_capacity(summary.entries.len());
            for entry in summary.entries.iter() {
                let app_icon_path = entry
                    .app_bundle_path
                    .as_ref()
                    .and_then(|p| lookup_or_cache_app_icon(p.as_str()));
                let origin_app_icon_path = entry
                    .origin_app_bundle_path
                    .as_ref()
                    .and_then(|p| lookup_or_cache_app_icon(p.as_str()));
                let cmdline = read_proc_cmdline(entry.pid);
                items.push(FolderProcItem {
                    pid: entry.pid,
                    ppid: entry.ppid,
                    name: entry.name.as_str().to_owned(),
                    cwd: entry.cwd.as_str().to_owned(),
                    rel_dir: entry.rel_dir.as_str().to_owned(),
                    exe_path: entry.exe_path.as_str().to_owned(),
                    cmdline,
                    parent_name: entry.parent_name.as_str().to_owned(),
                    app_icon_path,
                    origin_app_name: entry.origin_app_name.as_str().to_owned(),
                    origin_app_icon_path,
                    origin_chain: entry.origin_chain.as_str().to_owned(),
                    cpu_pct: entry.cpu_pct,
                    gpu_pct: entry.gpu_pct,
                    mem_bytes: entry.mem_bytes,
                    threads: entry.threads,
                    uptime_secs: entry.uptime_secs,
                    is_hot: entry.is_hot,
                    via_descendant: entry.via_descendant,
                    shell_pane: entry.shell_pane.clone(),
                });
            }
            updated.push((
                folder.clone(),
                FolderProcSnapshot {
                    folder: summary.folder.as_str().to_owned(),
                    items,
                    total_cpu_pct: summary.total_cpu_pct,
                    total_gpu_pct: summary.total_gpu_pct,
                    total_mem_bytes: summary.total_mem_bytes,
                    is_hot: summary.is_hot,
                    primary_name: summary.primary_name.as_str().to_owned(),
                },
            ));
        }

        if let Ok(mut cache) = watcher.cache.lock() {
            cache.retain(|k, _| active.iter().any(|(af, _)| af == k));
            for (k, v) in updated {
                cache.insert(k, v);
            }
        }
        FOLDER_PROC_REVISION.fetch_add(1, Ordering::Release);

        // Sleep 1.4s between ticks, waking immediately if a brand-new folder is registered.
        if let Ok(state) = watcher.watch.lock() {
            let _ = watcher.wake.wait_timeout_while(
                state,
                FOLDER_SAMPLE_INTERVAL,
                |s| s.wake_seq == observed_wake_seq,
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(clashing_extern_declarations)]
mod macos {
    use std::collections::HashMap;
    use std::ffi::{c_char, c_void};
    use std::fs;
    use std::mem::size_of;
    use std::path::Path;
    use std::ptr::{null, null_mut};
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    use blaze_own::{AString, AVec, Alloc};
    use blaze_sys::macos_proc;
    use blaze_sysmon::{
        CoreTicks, RawGpuSample, RawMemSample, RawProcess, extract_app_bundle_path,
    };
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::NSWorkspace;
    use objc2_core_graphics::CGImage;
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    use super::{parse_creator_pid, parse_kern_procargs2};

    // -------------------------------------------------------------------------
    // Buffer caps and Apple Framework UI/GPU constants
    // -------------------------------------------------------------------------

    const MAX_CPU_CORES: usize = 512;
    const PROCARGS2_STACK_LEN: usize = 8192;
    const PROCARGS2_MAX_LEN: usize = 256 * 1024;
    const MAXPATHLEN: usize = 1024;
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;

    const KERN_SUCCESS: i32 = 0;
    const K_IO_MAIN_PORT_DEFAULT: u32 = 0;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_CF_NUMBER_SINT64_TYPE: isize = 4;
    const K_CF_NUMBER_FLOAT64_TYPE: isize = 6;

    /// `kCGImageAlphaPremultipliedLast (1) | kCGBitmapByteOrder32Big (0)` = RGBA8 premultiplied.
    const K_CG_BITMAP_RGBA8_PREMULTIPLIED: u32 = 1;
    /// `kCGInterpolationHigh` in `CoreGraphics/CGContext.h`.
    const K_CG_INTERPOLATION_HIGH: i32 = 3;
    /// Target Retina @2x icon canvas (48×48 device pixels for 24×24 pt slots).
    const ICON_RETINA_PX: usize = 48;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRectRaw {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    // -------------------------------------------------------------------------
    // Apple Framework UI / GPU FFI declarations (IOKit / CF / CG / ImageIO)
    // -------------------------------------------------------------------------

    // Public IOKit.framework symbols for Apple Silicon `IOAccelerator` & `AGXDeviceUserClient`.
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        /// Consumes one reference to `matching` even on failure.
        fn IOServiceGetMatchingServices(
            main_port: u32,
            matching: *mut c_void,
            existing: *mut u32,
        ) -> i32;
        fn IOIteratorNext(iterator: u32) -> u32;
        fn IORegistryEntryCreateCFProperties(
            entry: u32,
            properties: *mut *mut c_void,
            allocator: *const c_void,
            options: u32,
        ) -> i32;
        fn IORegistryEntryGetChildIterator(
            entry: u32,
            plane: *const c_char,
            iterator: *mut u32,
        ) -> i32;
        fn IOObjectRelease(object: u32) -> i32;
    }

    // Public CoreFoundation.framework symbols for type-checked CF dictionary/array/string/number
    // inspection and mutable CFData buffers for PNG encoding.
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
        fn CFGetTypeID(cf: *const c_void) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFDictionaryGetTypeID() -> usize;
        fn CFArrayGetTypeID() -> usize;
        fn CFStringCreateWithBytes(
            alloc: *const c_void,
            bytes: *const u8,
            num_bytes: isize,
            encoding: u32,
            is_external: u8,
        ) -> *const c_void;
        fn CFStringGetLength(the_string: *const c_void) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetCString(
            the_string: *const c_void,
            buffer: *mut c_char,
            buffer_size: isize,
            encoding: u32,
        ) -> u8;
        fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
        fn CFNumberGetValue(
            number: *const c_void,
            the_type: isize,
            value_ptr: *mut c_void,
        ) -> u8;
        fn CFArrayGetCount(the_array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(the_array: *const c_void, idx: isize) -> *const c_void;
        fn CFDataCreateMutable(allocator: *const c_void, capacity: isize) -> *mut c_void;
        fn CFDataGetLength(the_data: *const c_void) -> isize;
        fn CFDataGetBytePtr(the_data: *const c_void) -> *const u8;
    }

    // Public CoreGraphics.framework symbols for 48×48 Retina icon resampling.
    #[allow(clashing_extern_declarations)]
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGColorSpaceRelease(space: *mut c_void);
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: *const c_void,
            bitmap_info: u32,
        ) -> *mut c_void;
        fn CGContextSetInterpolationQuality(ctx: *mut c_void, quality: i32);
        fn CGContextDrawImage(ctx: *mut c_void, rect: CGRectRaw, image: *const c_void);
        fn CGBitmapContextCreateImage(ctx: *mut c_void) -> *mut c_void;
        fn CGContextRelease(ctx: *mut c_void);
        fn CGImageRelease(image: *mut c_void);
    }

    // Public ImageIO.framework symbols for in-memory PNG encoding (`public.png`).
    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        fn CGImageDestinationCreateWithData(
            data: *mut c_void,
            uti_type: *const c_void,
            count: usize,
            options: *const c_void,
        ) -> *mut c_void;
        fn CGImageDestinationAddImage(
            idst: *mut c_void,
            image: *const c_void,
            properties: *const c_void,
        );
        fn CGImageDestinationFinalize(idst: *mut c_void) -> bool;
    }

    // -------------------------------------------------------------------------
    // RAII handle guards — guarantee zero IOKit / CF / CG leaks
    // -------------------------------------------------------------------------

    struct IoObjectGuard(u32);
    impl Drop for IoObjectGuard {
        fn drop(&mut self) {
            if self.0 != 0 {
                // SAFETY: `self.0` is an owned `io_object_t` handle from IOKit.
                unsafe {
                    let _ = IOObjectRelease(self.0);
                }
            }
        }
    }

    struct CfGuard(*const c_void);
    impl Drop for CfGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is an owned CoreFoundation object following the Create/Copy rule.
                unsafe {
                    CFRelease(self.0);
                }
            }
        }
    }

    struct CgColorSpaceGuard(*mut c_void);
    impl Drop for CgColorSpaceGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is an owned `CGColorSpaceRef`.
                unsafe {
                    CGColorSpaceRelease(self.0);
                }
            }
        }
    }

    struct CgContextGuard(*mut c_void);
    impl Drop for CgContextGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is an owned `CGContextRef`.
                unsafe {
                    CGContextRelease(self.0);
                }
            }
        }
    }

    struct CgImageGuard(*mut c_void);
    impl Drop for CgImageGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is an owned `CGImageRef` from `CGBitmapContextCreateImage`.
                unsafe {
                    CGImageRelease(self.0);
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // CoreFoundation typed helpers
    // -------------------------------------------------------------------------

    fn cf_string_from_str(s: &str) -> Option<CfGuard> {
        let len = isize::try_from(s.len()).ok()?;
        // SAFETY: `s.as_ptr()` is valid UTF-8 for `len` bytes; returned CFString is owned.
        let ptr = unsafe {
            CFStringCreateWithBytes(null(), s.as_ptr(), len, K_CF_STRING_ENCODING_UTF8, 0)
        };
        if ptr.is_null() {
            None
        } else {
            Some(CfGuard(ptr))
        }
    }

    /// Borrow a value from a `CFDictionary` by UTF-8 key name (unowned per Get Rule).
    unsafe fn cf_dict_get(dict: *const c_void, key: &str) -> *const c_void {
        if dict.is_null() {
            return null();
        }
        // SAFETY: caller guarantees `dict` is a valid CFTypeRef; verify dictionary type first.
        let is_dict = unsafe { CFGetTypeID(dict) == CFDictionaryGetTypeID() };
        if !is_dict {
            return null();
        }
        let Some(cf_key) = cf_string_from_str(key) else {
            return null();
        };
        // SAFETY: `dict` is a verified `CFDictionaryRef` and `cf_key.0` is a valid `CFStringRef`.
        unsafe { CFDictionaryGetValue(dict, cf_key.0) }
    }

    /// Convert a `CFStringRef` (type-checked) into an owned Rust `String`.
    unsafe fn cf_to_string(cf: *const c_void) -> Option<String> {
        if cf.is_null() {
            return None;
        }
        // SAFETY: caller passes a valid CFTypeRef; verify string type before CFString calls.
        if unsafe { CFGetTypeID(cf) != CFStringGetTypeID() } {
            return None;
        }
        // SAFETY: `cf` is a verified `CFStringRef`.
        let char_len = unsafe { CFStringGetLength(cf) };
        if char_len <= 0 {
            return Some(String::new());
        }
        // SAFETY: `cf` is a verified `CFStringRef`.
        let max_bytes = unsafe {
            CFStringGetMaximumSizeForEncoding(char_len, K_CF_STRING_ENCODING_UTF8)
        };
        let buf_len = usize::try_from(max_bytes.saturating_add(1)).ok()?;
        if buf_len == 0 || buf_len > 64 * 1024 {
            return None;
        }
        let mut buf = vec![0u8; buf_len];
        // SAFETY: `buf` has `buf_len` writable bytes for the NUL-terminated UTF-8 output.
        let ok = unsafe {
            CFStringGetCString(
                cf,
                buf.as_mut_ptr().cast::<c_char>(),
                buf_len as isize,
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        if ok == 0 {
            return None;
        }
        let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Some(String::from_utf8_lossy(&buf[..nul]).into_owned())
    }

    /// Extract a signed 64-bit integer from a type-checked `CFNumberRef`.
    unsafe fn cf_to_i64(cf: *const c_void) -> Option<i64> {
        if cf.is_null() {
            return None;
        }
        // SAFETY: verify `cf` is a `CFNumberRef` before reading its numeric value.
        if unsafe { CFGetTypeID(cf) != CFNumberGetTypeID() } {
            return None;
        }
        let mut val: i64 = 0;
        // SAFETY: `val` is a valid aligned `i64` matching `kCFNumberSInt64Type`.
        let ok = unsafe {
            CFNumberGetValue(
                cf,
                K_CF_NUMBER_SINT64_TYPE,
                (&raw mut val).cast::<c_void>(),
            )
        };
        if ok != 0 { Some(val) } else { None }
    }

    /// Extract a `f64` from a type-checked `CFNumberRef`.
    unsafe fn cf_to_f64(cf: *const c_void) -> Option<f64> {
        if cf.is_null() {
            return None;
        }
        // SAFETY: verify `cf` is a `CFNumberRef` before reading its numeric value.
        if unsafe { CFGetTypeID(cf) != CFNumberGetTypeID() } {
            return None;
        }
        let mut val: f64 = 0.0;
        // SAFETY: `val` is a valid aligned `f64` matching `kCFNumberFloat64Type`.
        let ok = unsafe {
            CFNumberGetValue(
                cf,
                K_CF_NUMBER_FLOAT64_TYPE,
                (&raw mut val).cast::<c_void>(),
            )
        };
        if ok != 0 { Some(val) } else { None }
    }

    // -------------------------------------------------------------------------
    // Mach timebase (cached once per process)
    // -------------------------------------------------------------------------

    fn cached_mach_timebase() -> (u32, u32) {
        static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
        *TIMEBASE.get_or_init(|| match macos_proc::mach_timebase() {
            Ok(tb) if tb.numer > 0 && tb.denom > 0 => (tb.numer, tb.denom),
            _ => (1, 1),
        })
    }

    // -------------------------------------------------------------------------
    // 1. Per-core CPU ticks (via `blaze_sys::macos_proc`)
    // -------------------------------------------------------------------------

    pub(super) fn sample_cores(alloc: Alloc<'static>) -> AVec<'static, CoreTicks> {
        let mut raw = [const {
            macos_proc::CpuCoreTicks {
                user: 0,
                system: 0,
                idle: 0,
                nice: 0,
            }
        }; MAX_CPU_CORES];
        let Ok(count) = macos_proc::cpu_core_ticks(&mut raw) else {
            return AVec::new_in(alloc);
        };
        let n = count.min(raw.len());
        let mut vec = AVec::with_capacity_in(n, alloc);
        for core in &raw[..n] {
            vec.push(CoreTicks {
                user: u64::from(core.user),
                system: u64::from(core.system),
                idle: u64::from(core.idle),
                nice: u64::from(core.nice),
            });
        }
        vec
    }

    // -------------------------------------------------------------------------
    // 2. Memory, Swap, Uptime, Load Average (via `blaze_sys::macos_proc`)
    // -------------------------------------------------------------------------

    pub(super) fn sample_memory_and_host() -> (RawMemSample, [f32; 3], u64) {
        let mem = match macos_proc::vm_mem_info() {
            Ok(vm) => RawMemSample {
                total_bytes: vm.total_bytes,
                app_bytes: vm.app_bytes,
                wired_bytes: vm.wired_bytes,
                compressed_bytes: vm.compressed_bytes,
                cached_bytes: vm.cached_bytes,
                swap_used_bytes: vm.swap_used_bytes,
                swap_total_bytes: vm.swap_total_bytes,
            },
            Err(_) => RawMemSample {
                total_bytes: 0,
                app_bytes: 0,
                wired_bytes: 0,
                compressed_bytes: 0,
                cached_bytes: 0,
                swap_used_bytes: 0,
                swap_total_bytes: 0,
            },
        };

        let uptime_secs = macos_proc::boot_time_secs()
            .ok()
            .and_then(|boot_secs| {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
                Some(now.saturating_sub(boot_secs))
            })
            .unwrap_or(0);

        let load_avg = match macos_proc::load_average() {
            Ok(raw) => [raw[0] as f32, raw[1] as f32, raw[2] as f32],
            Err(_) => [0.0; 3],
        };

        (mem, load_avg, uptime_secs)
    }

    // -------------------------------------------------------------------------
    // 3. Apple Silicon GPU Telemetry (System + Per-Process via IOKit)
    // -------------------------------------------------------------------------

    pub(super) fn sample_macos_gpu(
        alloc: Alloc<'static>,
    ) -> (RawGpuSample<'static>, HashMap<u32, (u64, u32)>) {
        let mut gpu = RawGpuSample {
            model: AString::new_in(alloc),
            core_count: 0,
            device_util_pct: 0.0,
            renderer_util_pct: 0.0,
            tiler_util_pct: 0.0,
            vram_in_use_bytes: 0,
            vram_alloc_bytes: 0,
        };
        let mut gpu_by_pid: HashMap<u32, (u64, u32)> = HashMap::new();

        // SAFETY: `c"IOAccelerator"` is a valid NUL-terminated C string.
        let matching = unsafe { IOServiceMatching(c"IOAccelerator".as_ptr()) };
        if matching.is_null() {
            return (gpu, gpu_by_pid);
        }

        let mut raw_iter: u32 = 0;
        // SAFETY: `IOServiceGetMatchingServices` always consumes one reference to `matching`.
        let kr = unsafe {
            IOServiceGetMatchingServices(K_IO_MAIN_PORT_DEFAULT, matching, &raw mut raw_iter)
        };
        if kr != KERN_SUCCESS || raw_iter == 0 {
            return (gpu, gpu_by_pid);
        }
        let iter = IoObjectGuard(raw_iter);
        let mut primary_filled = false;

        loop {
            // SAFETY: `iter.0` is an active `io_iterator_t`.
            let raw_service = unsafe { IOIteratorNext(iter.0) };
            if raw_service == 0 {
                break;
            }
            let service = IoObjectGuard(raw_service);

            // Read accelerator-level properties (model, gpu-core-count, PerformanceStatistics).
            let mut raw_props: *mut c_void = null_mut();
            // SAFETY: `service.0` is a valid `io_registry_entry_t`; returned dict is released by `CfGuard`.
            let pkr = unsafe {
                IORegistryEntryCreateCFProperties(service.0, &raw mut raw_props, null(), 0)
            };
            if pkr == KERN_SUCCESS && !raw_props.is_null() {
                let props = CfGuard(raw_props);
                if !primary_filled {
                    // SAFETY: `props.0` is a valid retained `CFMutableDictionaryRef`.
                    unsafe {
                        if let Some(model) = cf_to_string(cf_dict_get(props.0, "model")) {
                            gpu.model = AString::from_str_in(&model, alloc);
                        }
                        if let Some(cores) = cf_to_i64(cf_dict_get(props.0, "gpu-core-count"))
                            && cores > 0
                        {
                            gpu.core_count = cores.min(i64::from(u32::MAX)) as u32;
                        }
                        let perf = cf_dict_get(props.0, "PerformanceStatistics");
                        if !perf.is_null() {
                            if let Some(v) = cf_to_f64(cf_dict_get(perf, "Device Utilization %")) {
                                gpu.device_util_pct = v as f32;
                            }
                            if let Some(v) = cf_to_f64(cf_dict_get(perf, "Renderer Utilization %"))
                            {
                                gpu.renderer_util_pct = v as f32;
                            }
                            if let Some(v) = cf_to_f64(cf_dict_get(perf, "Tiler Utilization %")) {
                                gpu.tiler_util_pct = v as f32;
                            }
                            if let Some(v) = cf_to_i64(cf_dict_get(perf, "In use system memory"))
                                && v >= 0
                            {
                                gpu.vram_in_use_bytes = v as u64;
                            }
                            if let Some(v) = cf_to_i64(cf_dict_get(perf, "Alloc system memory"))
                                && v >= 0
                            {
                                gpu.vram_alloc_bytes = v as u64;
                            }
                        }
                    }
                    primary_filled = !gpu.model.as_str().is_empty() || gpu.core_count > 0;
                }
            }

            // Walk `AGXDeviceUserClient` children to accumulate per-PID GPU time & command queues.
            let mut raw_child_iter: u32 = 0;
            // SAFETY: `service.0` is a valid registry entry and `c"IOService"` is NUL-terminated.
            let ckr = unsafe {
                IORegistryEntryGetChildIterator(
                    service.0,
                    c"IOService".as_ptr(),
                    &raw mut raw_child_iter,
                )
            };
            if ckr == KERN_SUCCESS && raw_child_iter != 0 {
                let child_iter = IoObjectGuard(raw_child_iter);
                loop {
                    // SAFETY: `child_iter.0` is an active `io_iterator_t`.
                    let raw_child = unsafe { IOIteratorNext(child_iter.0) };
                    if raw_child == 0 {
                        break;
                    }
                    let child = IoObjectGuard(raw_child);

                    let mut raw_cprops: *mut c_void = null_mut();
                    // SAFETY: `child.0` is a valid registry entry; `cprops` is freed by `CfGuard`.
                    let cpkr = unsafe {
                        IORegistryEntryCreateCFProperties(child.0, &raw mut raw_cprops, null(), 0)
                    };
                    if cpkr != KERN_SUCCESS || raw_cprops.is_null() {
                        continue;
                    }
                    let cprops = CfGuard(raw_cprops);

                    // SAFETY: `cprops.0` is a valid retained `CFMutableDictionaryRef`.
                    unsafe {
                        let Some(creator) =
                            cf_to_string(cf_dict_get(cprops.0, "IOUserClientCreator"))
                        else {
                            continue;
                        };
                        let Some(pid) = parse_creator_pid(&creator) else {
                            continue;
                        };

                        let queues = cf_to_i64(cf_dict_get(cprops.0, "CommandQueueCount"))
                            .filter(|&v| v > 0)
                            .map_or(0u32, |v| v.min(i64::from(u32::MAX)) as u32);

                        let mut client_gpu_ns: u64 = 0;
                        let app_usage = cf_dict_get(cprops.0, "AppUsage");
                        if !app_usage.is_null() && CFGetTypeID(app_usage) == CFArrayGetTypeID() {
                            let n = CFArrayGetCount(app_usage);
                            for idx in 0..n {
                                let elem = CFArrayGetValueAtIndex(app_usage, idx);
                                if let Some(ns) =
                                    cf_to_i64(cf_dict_get(elem, "accumulatedGPUTime"))
                                    && ns > 0
                                {
                                    client_gpu_ns = client_gpu_ns.saturating_add(ns as u64);
                                }
                            }
                        }

                        let entry = gpu_by_pid.entry(pid).or_insert((0, 0));
                        entry.0 = entry.0.saturating_add(client_gpu_ns);
                        entry.1 = entry.1.saturating_add(queues);
                    }
                }
            }
        }

        (gpu, gpu_by_pid)
    }

    // -------------------------------------------------------------------------
    // 4. Processes via `blaze_sys::macos_proc`
    // -------------------------------------------------------------------------

    fn c_bytes_to_str(bytes: &[u8]) -> &str {
        let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        std::str::from_utf8(&bytes[..nul]).unwrap_or("")
    }

    pub(super) fn sample_processes(
        alloc: Alloc<'static>,
        gpu_by_pid: &HashMap<u32, (u64, u32)>,
    ) -> AVec<'static, RawProcess<'static>> {
        let (tb_numer, tb_denom) = cached_mach_timebase();
        let to_ns = |ticks: u64| {
            macos_proc::ticks_to_ns(
                ticks,
                macos_proc::MachTimebase {
                    numer: tb_numer,
                    denom: tb_denom,
                },
            )
        };

        let Ok(estimated) = macos_proc::count_all_pids() else {
            return AVec::new_in(alloc);
        };
        if estimated == 0 {
            return AVec::new_in(alloc);
        }

        let cap = estimated.saturating_add(128).min(65_536);
        let mut pids = vec![0i32; cap];
        let Ok(actual_count) = macos_proc::list_all_pids(&mut pids) else {
            return AVec::new_in(alloc);
        };
        if actual_count == 0 {
            return AVec::new_in(alloc);
        }
        let pid_slice = &pids[..actual_count.min(pids.len())];

        let mut out = AVec::with_capacity_in(pid_slice.len(), alloc);
        let mut path_buf = [0u8; PROC_PIDPATHINFO_MAXSIZE];
        let mut cwd_buf = [0u8; MAXPATHLEN];

        for &raw_pid in pid_slice {
            // Skip PID 0 (`kernel_task`) and PID 1 (`launchd`) so top-level apps sit cleanly at
            // depth 0 in `top` while root bridge processes like `/usr/bin/login` stay in the tree.
            if raw_pid <= 1 {
                continue;
            }
            let pid = raw_pid as u32;

            let (bsd_info, kinfo_basic) = match macos_proc::proc_bsd_info(raw_pid) {
                Ok(bsd) => (Some(bsd), None),
                Err(_) => match macos_proc::proc_kinfo_basic(raw_pid) {
                    Ok(ki) => (None, Some(ki)),
                    Err(_) => continue,
                },
            };

            let (proc_pid, ppid, uid, start_time_secs, bsd_name, bsd_comm, trunc_cap) =
                match (&bsd_info, &kinfo_basic) {
                    (Some(bsd), _) => (
                        bsd.pid,
                        bsd.ppid,
                        bsd.uid,
                        bsd.start_time_secs,
                        std::str::from_utf8(bsd.name_bytes()).unwrap_or(""),
                        c_bytes_to_str(&bsd.comm),
                        31usize,
                    ),
                    (_, Some(ki)) => (
                        ki.pid,
                        ki.ppid,
                        0,
                        ki.start_time_secs,
                        std::str::from_utf8(ki.name_bytes()).unwrap_or(""),
                        "",
                        16usize,
                    ),
                    (None, None) => unreachable!(),
                };

            // Executable path & `.app` bundle path.
            let exe_path_str = match macos_proc::proc_exe_path(raw_pid, &mut path_buf) {
                Ok(len) if len > 0 => {
                    let used = len.min(path_buf.len());
                    String::from_utf8_lossy(&path_buf[..used]).into_owned()
                }
                _ => String::new(),
            };
            let app_bundle_path =
                extract_app_bundle_path(&exe_path_str).map(|s| AString::from_str_in(s, alloc));

            // Prefer `pbi_name` (32 chars) over `pbi_comm` (16 chars); if the kernel name hit its
            // truncation cap and the executable basename extends it, use the full basename.
            let exe_basename = Path::new(&exe_path_str)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let chosen_name = if !bsd_name.is_empty() {
                if bsd_name.len() >= trunc_cap && exe_basename.starts_with(bsd_name) {
                    exe_basename
                } else {
                    bsd_name
                }
            } else if !bsd_comm.is_empty() {
                bsd_comm
            } else {
                exe_basename
            };
            let chosen_name = super::friendly_versioned_exe_name(chosen_name, &exe_path_str);

            // Task info (thread count, plus fallback RSS & CPU ticks when `proc_pid_rusage` gets EPERM).
            let task_info = macos_proc::proc_task_info(raw_pid).ok();
            let threads = task_info.as_ref().map_or(0, |t| t.threads);

            // Preferred Activity Monitor metrics via `proc_rusage_v4`.
            let (cpu_time_ns, mem_bytes, disk_read_bytes, disk_write_bytes) =
                if let Ok(ru) = macos_proc::proc_rusage_v4(raw_pid) {
                    let ticks = ru.user_time.saturating_add(ru.system_time);
                    (
                        to_ns(ticks),
                        ru.phys_footprint,
                        ru.disk_read_bytes,
                        ru.disk_write_bytes,
                    )
                } else if let Some(ref task) = task_info {
                    // Unprivileged fallback for root/other-UID processes where `proc_pid_rusage`
                    // returns EPERM but `PROC_PIDTASKINFO` still succeeded.
                    let ticks = task.total_user.saturating_add(task.total_system);
                    (to_ns(ticks), task.resident_size, 0, 0)
                } else {
                    (0, 0, 0, 0)
                };

            let (gpu_time_ns, gpu_queues) = gpu_by_pid.get(&pid).copied().unwrap_or((0, 0));

            // Process current working directory. Kernel / other-UID processes may return EPERM,
            // leaving `cwd` empty.
            let cwd = match macos_proc::proc_cwd(raw_pid, &mut cwd_buf) {
                Ok(len) if len > 0 => {
                    let used = len.min(cwd_buf.len());
                    AString::from_str_in(c_bytes_to_str(&cwd_buf[..used]), alloc)
                }
                _ => AString::new_in(alloc),
            };

            out.push(RawProcess {
                pid: proc_pid,
                ppid,
                uid,
                name: AString::from_str_in(chosen_name, alloc),
                exe_path: AString::from_str_in(&exe_path_str, alloc),
                cwd,
                app_bundle_path,
                cpu_time_ns,
                gpu_time_ns,
                gpu_queues,
                mem_bytes,
                disk_read_bytes,
                disk_write_bytes,
                threads,
                start_time_secs,
            });
        }

        out
    }

    /// Read a process's command line via `blaze_sys::macos_proc::proc_args2`.
    ///
    /// Uses an 8 KiB stack buffer for typical processes (0 heap allocations) and probes the exact
    /// kernel buffer size first so processes with large environments (> 8 KiB) still parse cleanly
    /// (XNU returns zeroed tail bytes if `oldlen < p_argslen`).
    pub(super) fn read_proc_cmdline(pid: u32) -> String {
        if pid == 0 {
            return String::new();
        }
        let Ok(raw_pid) = i32::try_from(pid) else {
            return String::new();
        };
        let Ok(needed) = macos_proc::proc_args2_len(raw_pid) else {
            return String::new();
        };
        if needed < size_of::<i32>() {
            return String::new();
        }

        if needed <= PROCARGS2_STACK_LEN {
            let mut stack_buf = [0u8; PROCARGS2_STACK_LEN];
            let Ok(size) = macos_proc::proc_args2(raw_pid, &mut stack_buf) else {
                return String::new();
            };
            if size == 0 {
                return String::new();
            }
            let used = size.min(stack_buf.len());
            parse_kern_procargs2(&stack_buf[..used])
        } else {
            let cap = needed.min(PROCARGS2_MAX_LEN);
            let mut heap_buf = vec![0u8; cap];
            let Ok(size) = macos_proc::proc_args2(raw_pid, &mut heap_buf) else {
                return String::new();
            };
            if size == 0 {
                return String::new();
            }
            let used = size.min(heap_buf.len());
            parse_kern_procargs2(&heap_buf[..used])
        }
    }

    // -------------------------------------------------------------------------
    // 5. macOS `.app` Retina Icon Extraction (NSWorkspace -> 48×48 PNG cache)
    // -------------------------------------------------------------------------

    pub(super) fn extract_and_cache_app_icon_png(
        app_bundle_path: &str,
        cache_dir: &Path,
        target_path: &Path,
        hash_hex: &str,
    ) -> Option<String> {
        let png_bytes = render_app_icon_png_bytes(app_bundle_path)?;
        if png_bytes.is_empty() {
            return None;
        }
        fs::create_dir_all(cache_dir).ok()?;
        let tmp_name = format!("{hash_hex}.{}.png.tmp", std::process::id());
        let tmp_path = cache_dir.join(tmp_name);
        fs::write(&tmp_path, &png_bytes).ok()?;
        fs::rename(&tmp_path, target_path).ok()?;
        Some(target_path.to_string_lossy().into_owned())
    }

    fn render_app_icon_png_bytes(app_bundle_path: &str) -> Option<Vec<u8>> {
        autoreleasepool(|_| {
            let ns_path = NSString::from_str(app_bundle_path);
            let workspace = NSWorkspace::sharedWorkspace();
            let ns_img = workspace.iconForFile(&ns_path);

            let mut proposed = NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(ICON_RETINA_PX as f64, ICON_RETINA_PX as f64),
            );
            // SAFETY: `proposed` is a valid mutable `NSRect` pointer; `None` context & hints are
            // documented defaults. Returned `CGImage` is retained/released by `objc2`.
            let cg_img = unsafe {
                ns_img.CGImageForProposedRect_context_hints(&raw mut proposed, None, None)
            }?;
            let src_cg_ptr = std::ptr::from_ref::<CGImage>(&*cg_img).cast::<c_void>();

            // Resample into a crisp 48×48 RGBA8 Retina canvas.
            // SAFETY: CoreGraphics calls operate on owned handles released by RAII guards.
            let cs = CgColorSpaceGuard(unsafe { CGColorSpaceCreateDeviceRGB() });
            if cs.0.is_null() {
                return None;
            }
            // SAFETY: passing null `data` lets CoreGraphics allocate and own the backing buffer.
            let ctx = CgContextGuard(unsafe {
                CGBitmapContextCreate(
                    null_mut(),
                    ICON_RETINA_PX,
                    ICON_RETINA_PX,
                    8,
                    ICON_RETINA_PX * 4,
                    cs.0,
                    K_CG_BITMAP_RGBA8_PREMULTIPLIED,
                )
            });
            if ctx.0.is_null() {
                return None;
            }
            // SAFETY: `ctx.0` is a valid bitmap `CGContextRef` and `src_cg_ptr` is alive for this scope.
            let scaled = CgImageGuard(unsafe {
                CGContextSetInterpolationQuality(ctx.0, K_CG_INTERPOLATION_HIGH);
                CGContextDrawImage(
                    ctx.0,
                    CGRectRaw {
                        x: 0.0,
                        y: 0.0,
                        w: ICON_RETINA_PX as f64,
                        h: ICON_RETINA_PX as f64,
                    },
                    src_cg_ptr,
                );
                CGBitmapContextCreateImage(ctx.0)
            });
            if scaled.0.is_null() {
                return None;
            }

            // Encode the 48×48 CGImage to PNG bytes in memory via ImageIO.
            // SAFETY: `CFDataCreateMutable` returns an owned `CFMutableDataRef` freed by `CfGuard`.
            let out_data = CfGuard(unsafe { CFDataCreateMutable(null(), 0) }.cast_const());
            if out_data.0.is_null() {
                return None;
            }
            let png_uti = cf_string_from_str("public.png")?;
            // SAFETY: `out_data.0` is a valid `CFMutableDataRef` and `png_uti.0` is `"public.png"`.
            let dest = CfGuard(
                unsafe {
                    CGImageDestinationCreateWithData(
                        out_data.0.cast_mut(),
                        png_uti.0,
                        1,
                        null(),
                    )
                }
                .cast_const(),
            );
            if dest.0.is_null() {
                return None;
            }
            // SAFETY: `dest.0` and `scaled.0` are valid ImageIO/CoreGraphics objects.
            let finalized = unsafe {
                CGImageDestinationAddImage(dest.0.cast_mut(), scaled.0, null());
                CGImageDestinationFinalize(dest.0.cast_mut())
            };
            if !finalized {
                return None;
            }

            // SAFETY: `out_data.0` is a valid `CFDataRef` whose byte buffer stays valid until drop.
            let len = unsafe { CFDataGetLength(out_data.0) };
            if len <= 0 {
                return None;
            }
            // SAFETY: `CFDataGetBytePtr` returns a pointer valid for `len` bytes until `out_data` drops.
            let ptr = unsafe { CFDataGetBytePtr(out_data.0) };
            if ptr.is_null() {
                return None;
            }
            // SAFETY: `ptr` is non-null and valid for `len as usize` contiguous bytes in `out_data`.
            let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
            Some(bytes.to_vec())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_creator_pid_extracts_valid_pids_and_rejects_malformed() {
        assert_eq!(parse_creator_pid("pid 465, WindowServer"), Some(465));
        assert_eq!(parse_creator_pid("pid 1"), Some(1));
        assert_eq!(
            parse_creator_pid("  pid 4201, Google Chrome Helper (GPU)"),
            Some(4201)
        );
        assert_eq!(parse_creator_pid("pid 0, kernel_task"), Some(0));
        assert_eq!(parse_creator_pid("WindowServer"), None);
        assert_eq!(parse_creator_pid("pid abc, WindowServer"), None);
        assert_eq!(parse_creator_pid("pid "), None);
        assert_eq!(parse_creator_pid(""), None);
        assert_eq!(parse_creator_pid("pid 99999999999999999999, overflow"), None);
    }

    #[test]
    fn friendly_versioned_exe_name_maps_claude_versions_to_claude() {
        assert_eq!(
            friendly_versioned_exe_name(
                "2.1.332",
                "/Users/me/.local/share/claude/versions/2.1.332"
            ),
            "claude"
        );
        assert_eq!(
            friendly_versioned_exe_name("bun", "/opt/homebrew/bin/bun"),
            "bun"
        );
        assert_eq!(
            friendly_versioned_exe_name("python3.13", "/usr/bin/python3.13"),
            "python3.13"
        );
    }

    #[test]
    fn fnv1a_64_hex_is_deterministic_and_16_chars() {
        let a = fnv1a_64_hex("/System/Applications/Calculator.app");
        let b = fnv1a_64_hex("/System/Applications/Calculator.app");
        let c = fnv1a_64_hex("/Applications/Safari.app");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mach_ticks_to_ns_handles_large_values_without_overflow() {
        use blaze_sys::macos_proc::{MachTimebase, ticks_to_ns};

        // Apple Silicon 24 MHz counter: numer=125, denom=3.
        assert_eq!(ticks_to_ns(3, MachTimebase { numer: 125, denom: 3 }), 125);
        assert_eq!(
            ticks_to_ns(24_000_000, MachTimebase { numer: 125, denom: 3 }),
            1_000_000_000
        );
        // Large tick count that would overflow u64 * 125 without u128 intermediate math.
        let huge_ticks = u64::MAX / 10;
        let expected = ((u128::from(huge_ticks) * 125) / 3).min(u64::MAX as u128) as u64;
        assert_eq!(
            ticks_to_ns(huge_ticks, MachTimebase { numer: 125, denom: 3 }),
            expected
        );
    }

    #[test]
    fn send_process_signal_refuses_pid_zero() {
        assert!(send_process_signal(0, false).is_err());
        assert!(send_process_signal(0, true).is_err());
    }

    #[test]
    fn parse_kern_procargs2_extracts_argv_basenames_and_truncates() {
        fn build_procargs2(
            argc: i32,
            exec_path: &str,
            pad_nuls: usize,
            argv: &[&str],
            envp: &[&str],
        ) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&argc.to_ne_bytes());
            out.extend_from_slice(exec_path.as_bytes());
            out.extend(std::iter::repeat_n(0u8, pad_nuls.max(1)));
            for arg in argv {
                out.extend_from_slice(arg.as_bytes());
                out.push(0);
            }
            for env in envp {
                out.extend_from_slice(env.as_bytes());
                out.push(0);
            }
            out
        }

        // Standard macOS layout: argv[0] path compacted to basename, NUL padding skipped, envp ignored.
        let buf = build_procargs2(
            2,
            "/opt/homebrew/bin/nu",
            5,
            &["/opt/homebrew/bin/nu", "--login"],
            &["SECRET_TOKEN=must-not-leak"],
        );
        assert_eq!(parse_kern_procargs2(&buf), "nu --login");

        // Empty args are skipped while still counting toward `argc` so `envp` is never read.
        let buf_empty_arg = build_procargs2(
            3,
            "/usr/bin/cargo",
            3,
            &["/usr/bin/cargo", "", "test"],
            &["HOME=/Users/dev"],
        );
        assert_eq!(parse_kern_procargs2(&buf_empty_arg), "cargo test");

        // Plain argv[0] without slashes stays intact.
        let buf_plain = build_procargs2(2, "/bin/zsh", 1, &["-zsh", "-i"], &[]);
        assert_eq!(parse_kern_procargs2(&buf_plain), "-zsh -i");

        // Long command line (including multi-byte UTF-8) truncates cleanly to <= 120 chars.
        let long_arg = "α".repeat(150);
        let buf_long = build_procargs2(2, "/usr/bin/python3", 2, &["/usr/bin/python3", &long_arg], &[]);
        let parsed_long = parse_kern_procargs2(&buf_long);
        assert!(parsed_long.chars().count() <= 120);
        assert!(parsed_long.starts_with("python3 "));
        assert!(parsed_long.ends_with('…'));

        // `argc` above 64 is clamped cleanly without panicking.
        let many_args: Vec<&str> = std::iter::once("/usr/bin/tool")
            .chain(std::iter::repeat_n("x", 80))
            .collect();
        let buf_many = build_procargs2(100, "/usr/bin/tool", 2, &many_args, &["ENV=1"]);
        let parsed_many = parse_kern_procargs2(&buf_many);
        assert!(parsed_many.chars().count() <= 120);
        assert!(parsed_many.starts_with("tool x x"));

        // Malformed / short / non-positive-argc inputs return empty string.
        assert_eq!(parse_kern_procargs2(&[]), "");
        assert_eq!(parse_kern_procargs2(&[1, 0, 0]), "");
        assert_eq!(
            parse_kern_procargs2(&build_procargs2(0, "/bin/sh", 2, &["/bin/sh"], &["A=1"])),
            ""
        );
        assert_eq!(
            parse_kern_procargs2(&build_procargs2(-3, "/bin/sh", 2, &["/bin/sh"], &[])),
            ""
        );
        assert_eq!(parse_kern_procargs2(&[1, 0, 0, 0, b'/', b'a']), "");
        assert_eq!(parse_kern_procargs2(&[1, 0, 0, 0, 0, 0, 0, 0]), "");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn read_proc_cmdline_returns_current_process_cmdline() {
        let cmdline = read_proc_cmdline(std::process::id());
        assert!(
            !cmdline.is_empty(),
            "expected KERN_PROCARGS2 to return non-empty cmdline for current process"
        );
        assert!(cmdline.chars().count() <= 120);
        assert!(read_proc_cmdline(0).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn live_sample_snapshot_populates_real_host_telemetry() {
        let alloc = Alloc::global();
        let s1 = sample_snapshot(alloc);
        let s2 = sample_snapshot(alloc);

        assert!(s2.timestamp_ns >= s1.timestamp_ns, "timestamp must be monotonic");
        assert!(s1.wall_time_secs > 0, "expected non-zero Unix wall time");
        assert!(!s1.cores.is_empty(), "expected at least one CPU core");
        assert!(s1.mem.total_bytes > 0, "expected non-zero total RAM");
        assert!(s1.uptime_secs > 0, "expected non-zero host uptime");

        let my_pid = std::process::id();
        let me = s1
            .processes
            .iter()
            .find(|p| p.pid == my_pid)
            .expect("expected current test process in process list");
        assert!(
            me.start_time_secs > 0 && me.start_time_secs <= s1.wall_time_secs.saturating_add(1),
            "expected valid pbi_start_tvsec for current process"
        );
        assert!(
            !me.cwd.as_str().is_empty(),
            "expected PROC_PIDVNODEPATHINFO to populate current process cwd"
        );
        if let Ok(expected) = std::env::current_dir().and_then(fs::canonicalize)
            && let Ok(actual) = fs::canonicalize(me.cwd.as_str())
        {
            assert_eq!(
                actual, expected,
                "current process cwd from PROC_PIDVNODEPATHINFO should match current_dir"
            );
        }
    }
}
