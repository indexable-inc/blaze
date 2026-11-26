//! Thin native macOS telemetry & `.app` Retina icon driver over [`blaze_sysmon`].
//!
//! Zero external subprocesses (`ps`, `top`, `ioreg`, `sips` are never spawned):
//! - Per-core CPU ticks via Mach `host_processor_info(PROCESSOR_CPU_LOAD_INFO)`
//! - Activity Monitor-exact memory (`internal - purgeable`, wired, compressed, cached), swap,
//!   boottime uptime, and load average via Mach `host_statistics64(HOST_VM_INFO64)` + `sysctlbyname`
//!   + `getloadavg`
//! - Apple Silicon GPU model, core count, device/renderer/tiler utilization, VRAM, and per-process
//!   GPU time + active command queues via `IOKit.framework` (`IOAccelerator` & child
//!   `AGXDeviceUserClient` `AppUsage` entries) + `CoreFoundation.framework`
//! - Process inventory, Activity Monitor physical footprint, CPU nanoseconds, disk I/O, threads,
//!   and `.app` bundle path via `libproc` (`proc_listallpids`, `proc_pidinfo`, `proc_pid_rusage`,
//!   `proc_pidpath`) + `mach_timebase_info`
//! - Persistent 48×48 Retina `.app` icon PNG disk cache (`~/Library/Caches/blaze-shell/app_icons/`)
//!   extracted lazily via `NSWorkspace::sharedWorkspace().iconForFile(...)` + `CoreGraphics` +
//!   `ImageIO`, bumping [`app_icon_revision`] on completion so the UI repaints cleanly.

use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicUsize;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

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

/// Convert Mach absolute-time ticks into nanoseconds using `u128` intermediate math to avoid overflow.
#[cfg(any(target_os = "macos", test))]
#[inline]
fn mach_ticks_to_ns(ticks: u64, numer: u64, denom: u64) -> u64 {
    let d = u128::from(denom.max(1));
    let ns = (u128::from(ticks) * u128::from(numer)) / d;
    ns.min(u64::MAX as u128) as u64
}

/// Capture a complete system telemetry snapshot (CPU cores, memory, swap, GPU, per-process metrics,
/// load average, and uptime) in one native FFI pass with zero subprocesses.
pub fn sample_snapshot(alloc: Alloc<'static>) -> RawSnapshot<'static> {
    let timestamp_ns = monotonic_now_ns();

    #[cfg(target_os = "macos")]
    {
        let cores = macos::sample_cores(alloc);
        let (mem, load_avg, uptime_secs) = macos::sample_memory_and_host();
        let (gpu, gpu_by_pid) = macos::sample_macos_gpu(alloc);
        let processes = macos::sample_processes(alloc, &gpu_by_pid);
        RawSnapshot {
            timestamp_ns,
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

    #[cfg(unix)]
    {
        const SIGKILL: i32 = 9;
        const SIGTERM: i32 = 15;

        // Public POSIX `kill(2)` symbol from libSystem / libc.
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }

        let sig = if sigkill { SIGKILL } else { SIGTERM };
        let sig_name = if sigkill { "SIGKILL" } else { "SIGTERM" };
        // SAFETY: `raw_pid > 0` and `sig` is a valid POSIX signal number.
        let rc = unsafe { kill(raw_pid, sig) };
        if rc == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            Err(format!("failed to send {sig_name} to pid {pid}: {err}"))
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (raw_pid, sigkill);
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
    pub app_icon_path: Option<String>,
    pub cpu_pct: f32,
    pub gpu_pct: f32,
    pub mem_bytes: u64,
    pub threads: u32,
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
                items.push(FolderProcItem {
                    pid: entry.pid,
                    ppid: entry.ppid,
                    name: entry.name.as_str().to_owned(),
                    cwd: entry.cwd.as_str().to_owned(),
                    rel_dir: entry.rel_dir.as_str().to_owned(),
                    exe_path: entry.exe_path.as_str().to_owned(),
                    app_icon_path,
                    cpu_pct: entry.cpu_pct,
                    gpu_pct: entry.gpu_pct,
                    mem_bytes: entry.mem_bytes,
                    threads: entry.threads,
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
    use std::mem::{MaybeUninit, size_of};
    use std::path::Path;
    use std::ptr::{null, null_mut};
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    use blaze_own::{AString, AVec, Alloc};
    use blaze_sysmon::{
        CoreTicks, RawGpuSample, RawMemSample, RawProcess, extract_app_bundle_path,
    };
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::NSWorkspace;
    use objc2_core_graphics::CGImage;
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    use super::{mach_ticks_to_ns, parse_creator_pid};

    // -------------------------------------------------------------------------
    // Mach / XNU / BSD / libproc constants (verified against MacOSX.sdk headers)
    // -------------------------------------------------------------------------

    const KERN_SUCCESS: i32 = 0;
    const PROCESSOR_CPU_LOAD_INFO: i32 = 2;
    const CPU_STATE_USER: usize = 0;
    const CPU_STATE_SYSTEM: usize = 1;
    const CPU_STATE_IDLE: usize = 2;
    const CPU_STATE_NICE: usize = 3;
    const CPU_STATE_MAX: usize = 4;

    const HOST_VM_INFO64: i32 = 4;
    /// `HOST_VM_INFO64_REV1_COUNT` (38 `natural_t` words = 152 bytes): supported on all macOS
    /// versions 10.9 through 26+ and includes `internal_page_count`, `purgeable_count`,
    /// `wire_count`, `compressor_page_count`, and `external_page_count`.
    const HOST_VM_INFO64_REV1_COUNT: u32 = 38;

    const PROC_PIDTBSDINFO: i32 = 3;
    const PROC_PIDTASKINFO: i32 = 4;
    const PROC_PIDVNODEPATHINFO: i32 = 9;
    const RUSAGE_INFO_V4: i32 = 4;
    const MAXPATHLEN: usize = 1024;
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;

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

    // -------------------------------------------------------------------------
    // Byte-exact XNU / BSD C structs (verified against MacOSX.sdk)
    // -------------------------------------------------------------------------

    /// `<mach/mach_time.h>` `struct mach_timebase_info` (8 bytes).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct MachTimebaseInfo {
        pub numer: u32,
        pub denom: u32,
    }

    /// `<mach/vm_statistics.h>` `struct vm_statistics64` rev1 prefix (152 bytes = 38 × `u32`).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct VmStatistics64Rev1 {
        pub free_count: u32,
        pub active_count: u32,
        pub inactive_count: u32,
        pub wire_count: u32,
        pub zero_fill_count: u64,
        pub reactivations: u64,
        pub pageins: u64,
        pub pageouts: u64,
        pub faults: u64,
        pub cow_faults: u64,
        pub lookups: u64,
        pub hits: u64,
        pub purges: u64,
        pub purgeable_count: u32,
        pub speculative_count: u32,
        pub decompressions: u64,
        pub compressions: u64,
        pub swapins: u64,
        pub swapouts: u64,
        pub compressor_page_count: u32,
        pub throttled_count: u32,
        pub external_page_count: u32,
        pub internal_page_count: u32,
        pub total_uncompressed_pages_in_compressor: u64,
    }

    /// `<sys/sysctl.h>` `struct xsw_usage` (32 bytes).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct XswUsage {
        pub xsu_total: u64,
        pub xsu_avail: u64,
        pub xsu_used: u64,
        pub xsu_pagesize: u32,
        pub xsu_encrypted: u32,
    }

    /// `<sys/_types/_timeval.h>` `struct timeval` on 64-bit macOS (16 bytes).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct Timeval {
        pub tv_sec: i64,
        pub tv_usec: i32,
        pub _pad: i32,
    }

    /// `<sys/proc_info.h>` `struct proc_bsdinfo` (136 bytes).
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct ProcBsdInfo {
        pub pbi_flags: u32,
        pub pbi_status: u32,
        pub pbi_xstatus: u32,
        pub pbi_pid: u32,
        pub pbi_ppid: u32,
        pub pbi_uid: u32,
        pub pbi_gid: u32,
        pub pbi_ruid: u32,
        pub pbi_rgid: u32,
        pub pbi_svuid: u32,
        pub pbi_svgid: u32,
        pub rfu_1: u32,
        pub pbi_comm: [u8; 16],
        pub pbi_name: [u8; 32],
        pub pbi_nfiles: u32,
        pub pbi_pgid: u32,
        pub pbi_pjobc: u32,
        pub e_tdev: u32,
        pub e_tpgid: u32,
        pub pbi_nice: i32,
        pub pbi_start_tvsec: u64,
        pub pbi_start_tvusec: u64,
    }

    /// `<sys/proc_info.h>` `struct proc_taskinfo` (96 bytes).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct ProcTaskInfo {
        pub pti_virtual_size: u64,
        pub pti_resident_size: u64,
        pub pti_total_user: u64,
        pub pti_total_system: u64,
        pub pti_threads_user: u64,
        pub pti_threads_system: u64,
        pub pti_policy: i32,
        pub pti_faults: i32,
        pub pti_pageins: i32,
        pub pti_cow_faults: i32,
        pub pti_messages_sent: i32,
        pub pti_messages_received: i32,
        pub pti_syscalls_mach: i32,
        pub pti_syscalls_unix: i32,
        pub pti_csw: i32,
        pub pti_threadnum: i32,
        pub pti_numrunning: i32,
        pub pti_priority: i32,
    }

    /// `<sys/resource.h>` `struct rusage_info_v4` (296 bytes).
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct RusageInfoV4 {
        pub ri_uuid: [u8; 16],
        pub ri_user_time: u64,
        pub ri_system_time: u64,
        pub ri_pkg_idle_wkups: u64,
        pub ri_interrupt_wkups: u64,
        pub ri_pageins: u64,
        pub ri_wired_size: u64,
        pub ri_resident_size: u64,
        pub ri_phys_footprint: u64,
        pub ri_proc_start_abstime: u64,
        pub ri_proc_exit_abstime: u64,
        pub ri_child_user_time: u64,
        pub ri_child_system_time: u64,
        pub ri_child_pkg_idle_wkups: u64,
        pub ri_child_interrupt_wkups: u64,
        pub ri_child_pageins: u64,
        pub ri_child_elapsed_abstime: u64,
        pub ri_diskio_bytesread: u64,
        pub ri_diskio_byteswritten: u64,
        pub ri_cpu_time_qos_default: u64,
        pub ri_cpu_time_qos_maintenance: u64,
        pub ri_cpu_time_qos_background: u64,
        pub ri_cpu_time_qos_utility: u64,
        pub ri_cpu_time_qos_legacy: u64,
        pub ri_cpu_time_qos_user_initiated: u64,
        pub ri_cpu_time_qos_user_interactive: u64,
        pub ri_billed_system_time: u64,
        pub ri_serviced_system_time: u64,
        pub ri_logical_writes: u64,
        pub ri_lifetime_max_phys_footprint: u64,
        pub ri_instructions: u64,
        pub ri_cycles: u64,
        pub ri_billed_energy: u64,
        pub ri_serviced_energy: u64,
        pub ri_interval_max_phys_footprint: u64,
        pub ri_runnable_time: u64,
    }

    /// `<sys/proc_info.h>` `struct vnode_info_path` (1176 bytes, 8-byte aligned).
    ///
    /// `vip_vi` (`struct vnode_info`) is 152 bytes (`19 × u64`), followed immediately at offset
    /// 152 by `vip_path[MAXPATHLEN]` (1024 bytes).
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct VnodeInfoPath {
        pub vip_vi: [u64; 19],
        pub vip_path: [u8; MAXPATHLEN],
    }

    /// `<sys/proc_info.h>` `struct proc_vnodepathinfo` (2352 bytes, 8-byte aligned).
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct ProcVnodePathInfo {
        pub pvi_cdir: VnodeInfoPath,
        pub pvi_rdir: VnodeInfoPath,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRectRaw {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    // -------------------------------------------------------------------------
    // Direct C FFI declarations (documented per Rule 45)
    // -------------------------------------------------------------------------

    // Public Mach, BSD sysctl, and libproc symbols in libSystem.B.dylib.
    unsafe extern "C" {
        /// Global `mach_port_t` for the current task (libSystem).
        static mach_task_self_: u32;
        /// Allocates a send right for the Mach host port; release with `mach_port_deallocate`.
        fn mach_host_self() -> u32;
        fn mach_port_deallocate(task: u32, name: u32) -> i32;
        fn host_page_size(host: u32, out_page_size: *mut usize) -> i32;
        fn host_processor_info(
            host: u32,
            flavor: i32,
            out_processor_count: *mut u32,
            out_processor_info: *mut *mut i32,
            out_processor_info_cnt: *mut u32,
        ) -> i32;
        fn host_statistics64(
            host: u32,
            flavor: i32,
            host_info_out: *mut i32,
            host_info_out_cnt: *mut u32,
        ) -> i32;
        fn vm_deallocate(target_task: u32, address: usize, size: usize) -> i32;
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;

        fn sysctlbyname(
            name: *const c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *const c_void,
            newlen: usize,
        ) -> i32;
        fn getloadavg(loadavg: *mut f64, nelem: i32) -> i32;

        // Public libproc (`<libproc.h>`) process inspection APIs.
        fn proc_listallpids(buffer: *mut c_void, buffersize: i32) -> i32;
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut c_void,
            buffersize: i32,
        ) -> i32;
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut c_void) -> i32;
        fn proc_pidpath(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
    }

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
    // RAII handle guards — guarantee zero Mach / IOKit / CF / CG leaks
    // -------------------------------------------------------------------------

    struct MachPortGuard(u32);
    impl Drop for MachPortGuard {
        fn drop(&mut self) {
            if self.0 != 0 {
                // SAFETY: `self.0` is an owned Mach port send right acquired via `mach_host_self`.
                unsafe {
                    let _ = mach_port_deallocate(mach_task_self_, self.0);
                }
            }
        }
    }

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

    fn mach_timebase() -> (u64, u64) {
        static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
        *TIMEBASE.get_or_init(|| {
            let mut info = MachTimebaseInfo::default();
            // SAFETY: `info` is an initialized 8-byte `mach_timebase_info_data_t`.
            let kr = unsafe { mach_timebase_info(&raw mut info) };
            if kr == KERN_SUCCESS && info.numer > 0 && info.denom > 0 {
                (u64::from(info.numer), u64::from(info.denom))
            } else {
                (1, 1)
            }
        })
    }

    // -------------------------------------------------------------------------
    // 1. Per-core CPU ticks
    // -------------------------------------------------------------------------

    pub(super) fn sample_cores(alloc: Alloc<'static>) -> AVec<'static, CoreTicks> {
        // SAFETY: `mach_host_self()` returns a send right released by `MachPortGuard`.
        let host = MachPortGuard(unsafe { mach_host_self() });
        let mut num_cpus: u32 = 0;
        let mut info_ptr: *mut i32 = null_mut();
        let mut info_cnt: u32 = 0;

        // SAFETY: out-pointers are valid stack locals; on success `info_ptr` is vm-allocated by
        // the kernel and deallocated below via `vm_deallocate`.
        let kr = unsafe {
            host_processor_info(
                host.0,
                PROCESSOR_CPU_LOAD_INFO,
                &raw mut num_cpus,
                &raw mut info_ptr,
                &raw mut info_cnt,
            )
        };

        let out = if kr == KERN_SUCCESS && !info_ptr.is_null() {
            let total_words = info_cnt as usize;
            let cpu_count = (num_cpus as usize).min(total_words / CPU_STATE_MAX);
            let mut vec = AVec::with_capacity_in(cpu_count, alloc);
            // SAFETY: `info_ptr` points to `total_words` contiguous `i32`s allocated by Mach.
            let slice = unsafe { std::slice::from_raw_parts(info_ptr, total_words) };
            for i in 0..cpu_count {
                let base = i * CPU_STATE_MAX;
                vec.push(CoreTicks {
                    user: u64::from(slice[base + CPU_STATE_USER] as u32),
                    system: u64::from(slice[base + CPU_STATE_SYSTEM] as u32),
                    idle: u64::from(slice[base + CPU_STATE_IDLE] as u32),
                    nice: u64::from(slice[base + CPU_STATE_NICE] as u32),
                });
            }
            // SAFETY: `info_ptr` was allocated by `host_processor_info` with `total_words * 4` bytes.
            unsafe {
                let _ = vm_deallocate(
                    mach_task_self_,
                    info_ptr as usize,
                    total_words * size_of::<i32>(),
                );
            }
            vec
        } else {
            AVec::new_in(alloc)
        };
        out
    }

    // -------------------------------------------------------------------------
    // 2. Memory, Swap, Uptime, Load Average
    // -------------------------------------------------------------------------

    fn sysctl_pod<T: Copy + Default>(name: &std::ffi::CStr) -> Option<T> {
        let mut val = T::default();
        let mut size = size_of::<T>();
        // SAFETY: `name` is NUL-terminated; `val` has `size` writable bytes and `newp` is null.
        let rc = unsafe {
            sysctlbyname(
                name.as_ptr(),
                (&raw mut val).cast::<c_void>(),
                &raw mut size,
                null(),
                0,
            )
        };
        if rc == 0 && size == size_of::<T>() {
            Some(val)
        } else {
            None
        }
    }

    pub(super) fn sample_memory_and_host() -> (RawMemSample, [f32; 3], u64) {
        let total_bytes = sysctl_pod::<u64>(c"hw.memsize").unwrap_or(0);

        // SAFETY: `mach_host_self()` returns a send right released by `MachPortGuard`.
        let host = MachPortGuard(unsafe { mach_host_self() });
        let mut page_size_raw: usize = 0;
        // SAFETY: `host.0` is a valid host port and `page_size_raw` is writable.
        let page_size = if unsafe { host_page_size(host.0, &raw mut page_size_raw) } == KERN_SUCCESS
            && page_size_raw > 0
        {
            page_size_raw as u64
        } else {
            16384
        };

        let mut vm = VmStatistics64Rev1::default();
        let mut count = HOST_VM_INFO64_REV1_COUNT;
        // SAFETY: `VmStatistics64Rev1` is 152 bytes = 38 `u32` words matching `HOST_VM_INFO64_REV1_COUNT`.
        let vm_kr = unsafe {
            host_statistics64(
                host.0,
                HOST_VM_INFO64,
                (&raw mut vm).cast::<i32>(),
                &raw mut count,
            )
        };

        let (app_bytes, wired_bytes, compressed_bytes, cached_bytes) = if vm_kr == KERN_SUCCESS {
            let internal = u64::from(vm.internal_page_count);
            let purgeable = u64::from(vm.purgeable_count);
            let wired = u64::from(vm.wire_count);
            let compressed = u64::from(vm.compressor_page_count);
            let external = u64::from(vm.external_page_count);
            (
                internal.saturating_sub(purgeable).saturating_mul(page_size),
                wired.saturating_mul(page_size),
                compressed.saturating_mul(page_size),
                external.saturating_add(purgeable).saturating_mul(page_size),
            )
        } else {
            (0, 0, 0, 0)
        };

        let swap = sysctl_pod::<XswUsage>(c"vm.swapusage").unwrap_or_default();

        let uptime_secs = sysctl_pod::<Timeval>(c"kern.boottime")
            .and_then(|boot| {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
                let boot_secs = u64::try_from(boot.tv_sec).ok()?;
                Some(now.saturating_sub(boot_secs))
            })
            .unwrap_or(0);

        let mut raw_load = [0.0f64; 3];
        // SAFETY: `raw_load` holds 3 `f64` slots matching `nelem = 3`.
        let n_load = unsafe { getloadavg(raw_load.as_mut_ptr(), 3) };
        let load_avg = if n_load >= 3 {
            [raw_load[0] as f32, raw_load[1] as f32, raw_load[2] as f32]
        } else if n_load > 0 {
            let mut out = [0.0f32; 3];
            for i in 0..(n_load as usize).min(3) {
                out[i] = raw_load[i] as f32;
            }
            out
        } else {
            [0.0; 3]
        };

        let mem = RawMemSample {
            total_bytes,
            app_bytes,
            wired_bytes,
            compressed_bytes,
            cached_bytes,
            swap_used_bytes: swap.xsu_used,
            swap_total_bytes: swap.xsu_total,
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
    // 4. Processes via libproc + Mach timebase
    // -------------------------------------------------------------------------

    fn c_bytes_to_str(bytes: &[u8]) -> &str {
        let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        std::str::from_utf8(&bytes[..nul]).unwrap_or("")
    }

    pub(super) fn sample_processes(
        alloc: Alloc<'static>,
        gpu_by_pid: &HashMap<u32, (u64, u32)>,
    ) -> AVec<'static, RawProcess<'static>> {
        let (tb_numer, tb_denom) = mach_timebase();

        // SAFETY: `proc_listallpids(NULL, 0)` queries the current process count without writing.
        let estimated = unsafe { proc_listallpids(null_mut(), 0) };
        if estimated <= 0 {
            return AVec::new_in(alloc);
        }

        let cap = (estimated as usize).saturating_add(128).min(65_536);
        let mut pids = vec![0i32; cap];
        let buf_bytes = i32::try_from(cap * size_of::<i32>()).unwrap_or(i32::MAX);
        // SAFETY: `pids` holds `cap` `i32` slots (`buf_bytes` bytes).
        let actual_count = unsafe { proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), buf_bytes) };
        if actual_count <= 0 {
            return AVec::new_in(alloc);
        }
        let pid_slice = &pids[..(actual_count as usize).min(pids.len())];

        let mut out = AVec::with_capacity_in(pid_slice.len(), alloc);
        let mut path_buf = [0u8; PROC_PIDPATHINFO_MAXSIZE];

        for &raw_pid in pid_slice {
            if raw_pid < 0 {
                continue;
            }
            let pid = raw_pid as u32;

            let mut bsd = MaybeUninit::<ProcBsdInfo>::zeroed();
            // SAFETY: `bsd` points to 136 writable bytes matching `PROC_PIDTBSDINFO`.
            let bsd_len = unsafe {
                proc_pidinfo(
                    raw_pid,
                    PROC_PIDTBSDINFO,
                    0,
                    bsd.as_mut_ptr().cast::<c_void>(),
                    size_of::<ProcBsdInfo>() as i32,
                )
            };
            if bsd_len != size_of::<ProcBsdInfo>() as i32 {
                continue;
            }
            // SAFETY: `proc_pidinfo` wrote the full `ProcBsdInfo` struct.
            let bsd = unsafe { bsd.assume_init() };

            // Executable path & `.app` bundle path.
            // SAFETY: `path_buf` has `PROC_PIDPATHINFO_MAXSIZE` (4096) writable bytes.
            let path_len = unsafe {
                proc_pidpath(
                    raw_pid,
                    path_buf.as_mut_ptr().cast::<c_void>(),
                    PROC_PIDPATHINFO_MAXSIZE as u32,
                )
            };
            let exe_path_str = if path_len > 0 {
                let len = (path_len as usize).min(path_buf.len());
                String::from_utf8_lossy(&path_buf[..len]).into_owned()
            } else {
                String::new()
            };
            let app_bundle_path =
                extract_app_bundle_path(&exe_path_str).map(|s| AString::from_str_in(s, alloc));

            // Prefer `pbi_name` (32 chars) over `pbi_comm` (16 chars); if `pbi_name` hit the
            // 31-byte kernel truncation cap and the executable basename extends it, use the full
            // basename.
            let bsd_name = c_bytes_to_str(&bsd.pbi_name);
            let bsd_comm = c_bytes_to_str(&bsd.pbi_comm);
            let exe_basename = Path::new(&exe_path_str)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let chosen_name = if !bsd_name.is_empty() {
                if bsd_name.len() >= 31 && exe_basename.starts_with(bsd_name) {
                    exe_basename
                } else {
                    bsd_name
                }
            } else if !bsd_comm.is_empty() {
                bsd_comm
            } else {
                exe_basename
            };

            // Task info (thread count, plus fallback RSS & CPU ticks when `proc_pid_rusage` gets EPERM).
            let mut task_info = ProcTaskInfo::default();
            // SAFETY: `task_info` is 96 writable bytes matching `PROC_PIDTASKINFO`.
            let task_len = unsafe {
                proc_pidinfo(
                    raw_pid,
                    PROC_PIDTASKINFO,
                    0,
                    (&raw mut task_info).cast::<c_void>(),
                    size_of::<ProcTaskInfo>() as i32,
                )
            };
            let has_task = task_len == size_of::<ProcTaskInfo>() as i32;
            let threads = if has_task && task_info.pti_threadnum > 0 {
                task_info.pti_threadnum as u32
            } else {
                0
            };

            // Preferred Activity Monitor metrics via `proc_pid_rusage(RUSAGE_INFO_V4)`.
            let mut rusage = MaybeUninit::<RusageInfoV4>::zeroed();
            // SAFETY: `rusage` is 296 writable bytes matching `RUSAGE_INFO_V4`.
            let ru_rc = unsafe {
                proc_pid_rusage(raw_pid, RUSAGE_INFO_V4, rusage.as_mut_ptr().cast::<c_void>())
            };
            let (cpu_time_ns, mem_bytes, disk_read_bytes, disk_write_bytes) = if ru_rc == 0 {
                // SAFETY: `proc_pid_rusage` returned 0 and populated `RusageInfoV4`.
                let ru = unsafe { rusage.assume_init() };
                let ticks = ru.ri_user_time.saturating_add(ru.ri_system_time);
                (
                    mach_ticks_to_ns(ticks, tb_numer, tb_denom),
                    ru.ri_phys_footprint,
                    ru.ri_diskio_bytesread,
                    ru.ri_diskio_byteswritten,
                )
            } else if has_task {
                // Unprivileged fallback for root/other-UID processes where `proc_pid_rusage`
                // returns EPERM but `PROC_PIDTASKINFO` still succeeded.
                let ticks = task_info
                    .pti_total_user
                    .saturating_add(task_info.pti_total_system);
                (
                    mach_ticks_to_ns(ticks, tb_numer, tb_denom),
                    task_info.pti_resident_size,
                    0,
                    0,
                )
            } else {
                (0, 0, 0, 0)
            };

            let (gpu_time_ns, gpu_queues) = gpu_by_pid.get(&pid).copied().unwrap_or((0, 0));

            // Process current working directory via `PROC_PIDVNODEPATHINFO`. Kernel / other-UID
            // processes may return EPERM (`vnode_len <= 0`), leaving `cwd` empty.
            let mut vnode = MaybeUninit::<ProcVnodePathInfo>::zeroed();
            // SAFETY: `vnode` points to 2352 writable bytes matching `PROC_PIDVNODEPATHINFO`.
            let vnode_len = unsafe {
                proc_pidinfo(
                    raw_pid,
                    PROC_PIDVNODEPATHINFO,
                    0,
                    vnode.as_mut_ptr().cast::<c_void>(),
                    size_of::<ProcVnodePathInfo>() as i32,
                )
            };
            let cwd = if vnode_len == size_of::<ProcVnodePathInfo>() as i32 {
                // SAFETY: `proc_pidinfo` wrote the full `ProcVnodePathInfo` struct.
                let vnode = unsafe { vnode.assume_init() };
                AString::from_str_in(c_bytes_to_str(&vnode.pvi_cdir.vip_path), alloc)
            } else {
                AString::new_in(alloc)
            };

            out.push(RawProcess {
                pid: bsd.pbi_pid,
                ppid: bsd.pbi_ppid,
                uid: bsd.pbi_uid,
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
            });
        }

        out
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
    fn fnv1a_64_hex_is_deterministic_and_16_chars() {
        let a = fnv1a_64_hex("/System/Applications/Calculator.app");
        let b = fnv1a_64_hex("/System/Applications/Calculator.app");
        let c = fnv1a_64_hex("/Applications/Safari.app");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn mach_ticks_to_ns_handles_large_values_without_overflow() {
        // Apple Silicon 24 MHz counter: numer=125, denom=3.
        assert_eq!(mach_ticks_to_ns(3, 125, 3), 125);
        assert_eq!(mach_ticks_to_ns(24_000_000, 125, 3), 1_000_000_000);
        // Large tick count that would overflow u64 * 125 without u128 intermediate math.
        let huge_ticks = u64::MAX / 10;
        let expected = ((u128::from(huge_ticks) * 125) / 3).min(u64::MAX as u128) as u64;
        assert_eq!(mach_ticks_to_ns(huge_ticks, 125, 3), expected);
    }

    #[test]
    fn send_process_signal_refuses_pid_zero() {
        assert!(send_process_signal(0, false).is_err());
        assert!(send_process_signal(0, true).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn xnu_struct_layouts_match_macos_sdk_headers() {
        use std::mem::{offset_of, size_of};

        use macos::{
            MachTimebaseInfo, ProcBsdInfo, ProcTaskInfo, ProcVnodePathInfo, RusageInfoV4, Timeval,
            VmStatistics64Rev1, VnodeInfoPath, XswUsage,
        };

        assert_eq!(size_of::<ProcBsdInfo>(), 136);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_flags), 0);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_pid), 12);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_ppid), 16);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_uid), 20);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_comm), 48);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_name), 64);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_nfiles), 96);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_nice), 116);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_start_tvsec), 120);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_start_tvusec), 128);

        assert_eq!(size_of::<ProcTaskInfo>(), 96);
        assert_eq!(offset_of!(ProcTaskInfo, pti_virtual_size), 0);
        assert_eq!(offset_of!(ProcTaskInfo, pti_resident_size), 8);
        assert_eq!(offset_of!(ProcTaskInfo, pti_total_user), 16);
        assert_eq!(offset_of!(ProcTaskInfo, pti_total_system), 24);
        assert_eq!(offset_of!(ProcTaskInfo, pti_threadnum), 84);

        assert_eq!(size_of::<RusageInfoV4>(), 296);
        assert_eq!(offset_of!(RusageInfoV4, ri_user_time), 16);
        assert_eq!(offset_of!(RusageInfoV4, ri_system_time), 24);
        assert_eq!(offset_of!(RusageInfoV4, ri_phys_footprint), 72);
        assert_eq!(offset_of!(RusageInfoV4, ri_diskio_bytesread), 144);
        assert_eq!(offset_of!(RusageInfoV4, ri_diskio_byteswritten), 152);

        assert_eq!(size_of::<VmStatistics64Rev1>(), 152);
        assert_eq!(size_of::<VmStatistics64Rev1>() / size_of::<u32>(), 38);
        assert_eq!(offset_of!(VmStatistics64Rev1, free_count), 0);
        assert_eq!(offset_of!(VmStatistics64Rev1, wire_count), 12);
        assert_eq!(offset_of!(VmStatistics64Rev1, purgeable_count), 88);
        assert_eq!(offset_of!(VmStatistics64Rev1, compressor_page_count), 128);
        assert_eq!(offset_of!(VmStatistics64Rev1, external_page_count), 136);
        assert_eq!(offset_of!(VmStatistics64Rev1, internal_page_count), 140);

        assert_eq!(size_of::<XswUsage>(), 32);
        assert_eq!(offset_of!(XswUsage, xsu_total), 0);
        assert_eq!(offset_of!(XswUsage, xsu_avail), 8);
        assert_eq!(offset_of!(XswUsage, xsu_used), 16);
        assert_eq!(offset_of!(XswUsage, xsu_pagesize), 24);
        assert_eq!(offset_of!(XswUsage, xsu_encrypted), 28);

        assert_eq!(size_of::<Timeval>(), 16);
        assert_eq!(offset_of!(Timeval, tv_sec), 0);
        assert_eq!(offset_of!(Timeval, tv_usec), 8);

        assert_eq!(size_of::<MachTimebaseInfo>(), 8);

        assert_eq!(size_of::<VnodeInfoPath>(), 1176);
        assert_eq!(offset_of!(VnodeInfoPath, vip_vi), 0);
        assert_eq!(offset_of!(VnodeInfoPath, vip_path), 152);

        assert_eq!(size_of::<ProcVnodePathInfo>(), 2352);
        assert_eq!(offset_of!(ProcVnodePathInfo, pvi_cdir), 0);
        assert_eq!(offset_of!(ProcVnodePathInfo, pvi_cdir.vip_path), 152);
        assert_eq!(offset_of!(ProcVnodePathInfo, pvi_rdir), 1176);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn live_sample_snapshot_populates_real_host_telemetry() {
        let alloc = Alloc::global();
        let s1 = sample_snapshot(alloc);
        let s2 = sample_snapshot(alloc);

        assert!(s2.timestamp_ns >= s1.timestamp_ns, "timestamp must be monotonic");
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
