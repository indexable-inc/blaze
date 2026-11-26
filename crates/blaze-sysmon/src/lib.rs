//! Pure `#![no_std]`, zero-unsafe, sans-IO system & process monitor state machine built on
//! `blaze-own` explicit allocators (`Alloc<'a>`, `AVec`, `AString`).
//!
//! A thin host driver samples OS counters (CPU ticks, memory, GPU, process list) into a
//! [`RawSnapshot`] and feeds it into [`SysMonEngine::ingest`]. All delta math, history rings,
//! tree aggregation, filtering, sorting, and box-drawing tree prefixes live here with zero IO.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use core::cmp::Ordering;

use blaze_own::{ABTreeMap, ABTreeSet, AString, AVec, Alloc, BTreeMap, BTreeSet, String, Vec};

/// Per-core cumulative CPU tick counters from the OS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreTicks {
    pub user: u64,
    pub system: u64,
    pub idle: u64,
    pub nice: u64,
}

/// Raw GPU utilization and memory counters for one sample.
#[derive(Debug, PartialEq)]
pub struct RawGpuSample<'a> {
    pub model: AString<'a>,
    pub core_count: u32,
    pub device_util_pct: f32,
    pub renderer_util_pct: f32,
    pub tiler_util_pct: f32,
    pub vram_in_use_bytes: u64,
    pub vram_alloc_bytes: u64,
}

impl<'a> RawGpuSample<'a> {
    /// An empty zeroed GPU sample in `alloc`.
    pub fn empty_in(alloc: Alloc<'a>) -> Self {
        Self {
            model: String::new_in(alloc),
            core_count: 0,
            device_util_pct: 0.0,
            renderer_util_pct: 0.0,
            tiler_util_pct: 0.0,
            vram_in_use_bytes: 0,
            vram_alloc_bytes: 0,
        }
    }

    /// Clones this GPU sample into `alloc`.
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> RawGpuSample<'b> {
        RawGpuSample {
            model: String::from_str_in(self.model.as_str(), alloc),
            core_count: self.core_count,
            device_util_pct: self.device_util_pct,
            renderer_util_pct: self.renderer_util_pct,
            tiler_util_pct: self.tiler_util_pct,
            vram_in_use_bytes: self.vram_in_use_bytes,
            vram_alloc_bytes: self.vram_alloc_bytes,
        }
    }
}

/// Raw system memory and swap breakdown for one sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawMemSample {
    pub total_bytes: u64,
    pub app_bytes: u64,
    pub wired_bytes: u64,
    pub compressed_bytes: u64,
    pub cached_bytes: u64,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
}

impl RawMemSample {
    /// Active memory used (`app + wired + compressed`), saturating on overflow.
    #[inline]
    pub fn used_bytes(self) -> u64 {
        self.app_bytes
            .saturating_add(self.wired_bytes)
            .saturating_add(self.compressed_bytes)
    }

    /// Percentage of total physical memory currently in use (`0.0..=100.0`).
    #[inline]
    pub fn used_pct(self) -> f32 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        ((self.used_bytes() as f64 / self.total_bytes as f64) * 100.0).clamp(0.0, 100.0) as f32
    }
}

/// Raw per-process counters collected by the host driver for one snapshot.
#[derive(Debug)]
pub struct RawProcess<'a> {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub name: AString<'a>,
    pub cwd: AString<'a>,
    pub exe_path: AString<'a>,
    pub app_bundle_path: Option<AString<'a>>,
    pub cpu_time_ns: u64,
    pub gpu_time_ns: u64,
    pub gpu_queues: u32,
    pub mem_bytes: u64,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub threads: u32,
    pub start_time_secs: u64,
}

/// One complete raw system snapshot passed into [`SysMonEngine::ingest`].
#[derive(Debug)]
pub struct RawSnapshot<'a> {
    pub timestamp_ns: u64,
    pub wall_time_secs: u64,
    pub cores: AVec<'a, CoreTicks>,
    pub mem: RawMemSample,
    pub gpu: RawGpuSample<'a>,
    pub processes: AVec<'a, RawProcess<'a>>,
    pub load_avg: [f32; 3],
    pub uptime_secs: u64,
}

/// Active sort column for process rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortColumn {
    #[default]
    Cpu,
    Gpu,
    Mem,
    Pid,
    Name,
}

impl SortColumn {
    /// Decodes a compact integer column index (`0 => Cpu, 1 => Gpu, 2 => Mem, 3 => Pid, _ => Name`).
    #[inline]
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Cpu,
            1 => Self::Gpu,
            2 => Self::Mem,
            3 => Self::Pid,
            _ => Self::Name,
        }
    }

    /// Encodes the column as a compact integer index.
    #[inline]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Cpu => 0,
            Self::Gpu => 1,
            Self::Mem => 2,
            Self::Pid => 3,
            Self::Name => 4,
        }
    }
}

/// Fixed-capacity ring buffer of percentage samples clamped to `0.0..=100.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HistoryRing<const N: usize> {
    buf: [f32; N],
    len: usize,
    head: usize,
}

// Sound: `new()` fills `0.0` and `push()` maps `NaN` to `0.0` and clamps to `0.0..=100.0`,
// so no `NaN` can ever be stored in `buf`.
impl<const N: usize> Eq for HistoryRing<N> {}

impl<const N: usize> Default for HistoryRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> HistoryRing<N> {
    /// Creates an empty history ring.
    pub const fn new() -> Self {
        Self {
            buf: [0.0; N],
            len: 0,
            head: 0,
        }
    }

    /// Appends a sample clamped to `0.0..=100.0`, overwriting the oldest entry when full.
    pub fn push(&mut self, v: f32) {
        if N == 0 {
            return;
        }
        let clamped = if v.is_nan() {
            0.0
        } else {
            let c = v.clamp(0.0, 100.0);
            if c == 0.0 { 0.0 } else { c }
        };
        self.buf[self.head] = clamped;
        self.head = (self.head + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }

    /// Number of valid samples currently stored (`0..=N`).
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if no samples have been pushed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the sample at `chronological_idx` (`0` = oldest, `len - 1` = newest), or `0.0` if out of bounds.
    #[inline]
    pub fn get(&self, chronological_idx: usize) -> f32 {
        if N == 0 || chronological_idx >= self.len {
            return 0.0;
        }
        let start = if self.len < N { 0 } else { self.head };
        self.buf[(start + chronological_idx) % N]
    }

    /// Returns the most recently pushed sample, or `0.0` when empty.
    #[inline]
    pub fn latest(&self) -> f32 {
        if N == 0 || self.len == 0 {
            return 0.0;
        }
        self.buf[(self.head + N - 1) % N]
    }
}

/// One rendered row in [`SysMonEngine::rows`], either in tree or flat mode.
#[derive(Debug)]
pub struct ProcessRow<'a> {
    pub pid: u32,
    pub ppid: u32,
    pub depth: u16,
    pub has_children: bool,
    pub collapsed: bool,
    pub is_last_sibling: bool,
    pub ancestor_mask: u64,
    pub child_count: u32,
    pub tree_prefix: AString<'a>,
    pub name: AString<'a>,
    pub cwd: AString<'a>,
    pub exe_path: AString<'a>,
    pub app_bundle_path: Option<AString<'a>>,
    pub cpu_pct: f32,
    pub subtree_cpu_pct: f32,
    pub gpu_pct: f32,
    pub subtree_gpu_pct: f32,
    pub gpu_queues: u32,
    pub mem_bytes: u64,
    pub subtree_mem_bytes: u64,
    pub disk_bps: u64,
    pub subtree_disk_bps: u64,
    pub threads: u32,
    pub subtree_threads: u32,
    pub start_time_secs: u64,
}

/// Finds the outermost `.app` bundle directory containing `exe_path`, so nested helper apps
/// (such as `Google Chrome Helper (GPU).app`) inherit their parent app's icon.
pub fn extract_app_bundle_path(exe_path: &str) -> Option<&str> {
    let bytes = exe_path.as_bytes();
    let mut seg_start = 0usize;
    while seg_start < bytes.len() {
        let seg_end = match bytes[seg_start..].iter().position(|&b| b == b'/') {
            Some(rel) => seg_start + rel,
            None => bytes.len(),
        };
        let seg_len = seg_end - seg_start;
        if seg_len > 4 && bytes[seg_end - 4..seg_end].eq_ignore_ascii_case(b".app") {
            return Some(&exe_path[..seg_end]);
        }
        seg_start = seg_end.saturating_add(1);
    }
    None
}

/// Extracts the display name of a macOS `.app` bundle path
/// (e.g. `"/Applications/Ghostty.app"` -> `"Ghostty"`).
pub fn app_display_name(bundle_path: &str) -> &str {
    let trimmed = bundle_path.trim_end_matches('/');
    let last = match trimmed.rsplit_once('/') {
        Some((_, name)) => name,
        None => trimmed,
    };
    if last.len() > 4 && last.as_bytes()[last.len() - 4..].eq_ignore_ascii_case(b".app") {
        &last[..last.len() - 4]
    } else {
        last
    }
}

#[inline]
fn trim_trailing_slashes(s: &str) -> &str {
    if s.is_empty() {
        return "";
    }
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() { "/" } else { trimmed }
}

/// Returns `true` if `path` is equal to `folder` or strictly inside `folder`.
///
/// Trailing `/` characters are trimmed on both paths (except root `"/"`).
/// Returns `false` if either `path` or `folder` is empty.
pub fn path_is_within(path: &str, folder: &str) -> bool {
    if path.is_empty() || folder.is_empty() {
        return false;
    }
    let path = trim_trailing_slashes(path);
    let folder = trim_trailing_slashes(folder);
    if folder == "/" {
        return path.starts_with('/');
    }
    path == folder
        || (path.len() > folder.len()
            && path.starts_with(folder)
            && path.as_bytes()[folder.len()] == b'/')
}

/// Returns `"."` if `path` equals `folder`, the relative subpath (e.g. `"crates/blaze-shell"`)
/// if `path` is strictly inside `folder`, or `path` unchanged if outside `folder`.
pub fn relative_subpath<'a>(path: &'a str, folder: &str) -> &'a str {
    if path.is_empty() || folder.is_empty() {
        return path;
    }
    let trimmed_path = trim_trailing_slashes(path);
    let trimmed_folder = trim_trailing_slashes(folder);
    if trimmed_path == trimmed_folder {
        return ".";
    }
    if trimmed_folder == "/" {
        if trimmed_path.starts_with('/') {
            let rest = trimmed_path.trim_start_matches('/');
            return if rest.is_empty() { "." } else { rest };
        }
        return path;
    }
    if trimmed_path.len() > trimmed_folder.len()
        && trimmed_path.starts_with(trimmed_folder)
        && trimmed_path.as_bytes()[trimmed_folder.len()] == b'/'
    {
        let rest = trimmed_path[trimmed_folder.len() + 1..].trim_start_matches('/');
        return if rest.is_empty() { "." } else { rest };
    }
    path
}

/// CPU percentage threshold at or above which a process or folder is considered hot.
const HOT_CPU_PCT: f32 = 25.0;
/// GPU percentage threshold at or above which a process or folder is considered hot.
const HOT_GPU_PCT: f32 = 15.0;
/// Maximum ancestor depth walked when attributing folder descendants and shell panes.
const MAX_ANCESTOR_DEPTH: usize = 32;

/// Identifies a child PTY or running job PID owned by a `blaze-shell` pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellPanePid {
    pub pid: u32,
    pub pane_id: u64,
    pub block_id: u64,
    pub pane_number: u32,
    pub is_alt_screen: bool,
}

/// One live running process attributed to a queried folder.
#[derive(Debug, PartialEq)]
pub struct FolderProcEntry<'a> {
    pub pid: u32,
    pub ppid: u32,
    pub name: AString<'a>,
    pub parent_name: AString<'a>,
    pub cwd: AString<'a>,
    pub rel_dir: AString<'a>,
    pub exe_path: AString<'a>,
    pub app_bundle_path: Option<AString<'a>>,
    pub cpu_pct: f32,
    pub gpu_pct: f32,
    pub mem_bytes: u64,
    pub threads: u32,
    pub uptime_secs: u64,
    pub is_hot: bool,
    pub via_descendant: bool,
    pub shell_pane: Option<ShellPanePid>,
    pub origin_app_name: AString<'a>,
    pub origin_app_bundle_path: Option<AString<'a>>,
    pub origin_chain: AString<'a>,
}

/// Summary of all live running processes scoped to a folder (including subfolders & descendants).
#[derive(Debug, PartialEq)]
pub struct FolderProcSummary<'a> {
    pub folder: AString<'a>,
    pub entries: AVec<'a, FolderProcEntry<'a>>,
    pub total_cpu_pct: f32,
    pub total_gpu_pct: f32,
    pub total_mem_bytes: u64,
    pub is_hot: bool,
    pub primary_name: AString<'a>,
}

#[derive(Clone, Copy, Debug)]
struct PrevProcCounters {
    cpu_time_ns: u64,
    gpu_time_ns: u64,
    disk_bytes: u64,
}

#[derive(Debug)]
struct ComputedProc<'a> {
    pid: u32,
    ppid: u32,
    name: AString<'a>,
    cwd: AString<'a>,
    exe_path: AString<'a>,
    app_bundle_path: Option<AString<'a>>,
    cpu_pct: f32,
    gpu_pct: f32,
    gpu_queues: u32,
    mem_bytes: u64,
    disk_bps: u64,
    threads: u32,
    start_time_secs: u64,
}

/// Pure sans-IO system & process monitor state machine.
pub struct SysMonEngine<'a> {
    alloc: Alloc<'a>,
    tree_mode: bool,
    sort: SortColumn,
    ascending: bool,
    paused: bool,
    filter: AString<'a>,
    collapsed: ABTreeSet<'a, u32>,
    sample_count: u64,
    prev_timestamp_ns: Option<u64>,
    wall_time_secs: u64,
    prev_cores: AVec<'a, CoreTicks>,
    prev_procs: ABTreeMap<'a, u32, PrevProcCounters>,
    core_utils: AVec<'a, f32>,
    total_cpu_pct: f32,
    gpu: RawGpuSample<'a>,
    mem: RawMemSample,
    load_avg: [f32; 3],
    uptime_secs: u64,
    process_count: usize,
    thread_count: u32,
    cpu_history: HistoryRing<64>,
    gpu_history: HistoryRing<64>,
    gpu_renderer_history: HistoryRing<64>,
    gpu_tiler_history: HistoryRing<64>,
    mem_history: HistoryRing<64>,
    computed_procs: AVec<'a, ComputedProc<'a>>,
    rows: AVec<'a, ProcessRow<'a>>,
}

impl<'a> SysMonEngine<'a> {
    /// Creates a fresh engine backed by `alloc`.
    ///
    /// Defaults: `tree_mode: true`, `sort: SortColumn::Cpu`, `ascending: false`, `paused: false`.
    pub fn new_in(alloc: Alloc<'a>) -> Self {
        Self {
            alloc,
            tree_mode: true,
            sort: SortColumn::Cpu,
            ascending: false,
            paused: false,
            filter: String::new_in(alloc),
            collapsed: BTreeSet::new_in(alloc),
            sample_count: 0,
            prev_timestamp_ns: None,
            wall_time_secs: 0,
            prev_cores: Vec::new_in(alloc),
            prev_procs: BTreeMap::new_in(alloc),
            core_utils: Vec::new_in(alloc),
            total_cpu_pct: 0.0,
            gpu: RawGpuSample::empty_in(alloc),
            mem: RawMemSample::default(),
            load_avg: [0.0; 3],
            uptime_secs: 0,
            process_count: 0,
            thread_count: 0,
            cpu_history: HistoryRing::new(),
            gpu_history: HistoryRing::new(),
            gpu_renderer_history: HistoryRing::new(),
            gpu_tiler_history: HistoryRing::new(),
            mem_history: HistoryRing::new(),
            computed_procs: Vec::new_in(alloc),
            rows: Vec::new_in(alloc),
        }
    }

    /// Ingests a new raw system snapshot, updating deltas, history rings, and rendered rows.
    ///
    /// When `paused()` is `true`, snapshots are ignored except for the very first snapshot so the
    /// initial view is never blank.
    pub fn ingest(&mut self, snap: RawSnapshot<'a>) {
        if self.paused && self.sample_count > 0 {
            return;
        }

        // 1. Per-core and aggregate CPU delta utilization.
        self.core_utils.clear();
        if self.prev_cores.len() == snap.cores.len() && !snap.cores.is_empty() {
            let mut sum_active: u128 = 0;
            let mut sum_total: u128 = 0;
            for (cur, prev) in snap.cores.iter().zip(self.prev_cores.iter()) {
                let d_user = cur.user.saturating_sub(prev.user);
                let d_sys = cur.system.saturating_sub(prev.system);
                let d_nice = cur.nice.saturating_sub(prev.nice);
                let d_idle = cur.idle.saturating_sub(prev.idle);
                let d_active = d_user.saturating_add(d_sys).saturating_add(d_nice);
                let d_total = d_active.saturating_add(d_idle);
                let pct = if d_total == 0 {
                    0.0
                } else {
                    ((d_active as f64 / d_total as f64) * 100.0).clamp(0.0, 100.0) as f32
                };
                self.core_utils.push(pct);
                sum_active += u128::from(d_active);
                sum_total += u128::from(d_total);
            }
            self.total_cpu_pct = if sum_total == 0 {
                0.0
            } else {
                ((sum_active as f64 / sum_total as f64) * 100.0).clamp(0.0, 100.0) as f32
            };
        } else {
            for _ in 0..snap.cores.len() {
                self.core_utils.push(0.0);
            }
            self.total_cpu_pct = 0.0;
        }
        self.prev_cores = snap.cores;

        // 2. Per-process CPU %, GPU %, and disk bytes/sec deltas.
        let dt_ns = self
            .prev_timestamp_ns
            .map(|prev| snap.timestamp_ns.saturating_sub(prev))
            .unwrap_or(0);
        let mut next_prev_procs: ABTreeMap<'a, u32, PrevProcCounters> = BTreeMap::new_in(self.alloc);
        self.computed_procs.clear();

        let mut total_threads: u32 = 0;
        for proc in snap.processes {
            total_threads = total_threads.saturating_add(proc.threads);
            let disk_total = proc.disk_read_bytes.saturating_add(proc.disk_write_bytes);
            let (cpu_pct, gpu_pct, disk_bps) = match self.prev_procs.get(&proc.pid) {
                Some(prev) if dt_ns > 0 => {
                    let d_cpu = proc.cpu_time_ns.saturating_sub(prev.cpu_time_ns);
                    let d_gpu = proc.gpu_time_ns.saturating_sub(prev.gpu_time_ns);
                    let d_disk = disk_total.saturating_sub(prev.disk_bytes);
                    let cpu = ((d_cpu as f64 / dt_ns as f64) * 100.0) as f32;
                    let gpu = ((d_gpu as f64 / dt_ns as f64) * 100.0) as f32;
                    let bps = ((u128::from(d_disk) * 1_000_000_000u128) / u128::from(dt_ns))
                        .min(u128::from(u64::MAX)) as u64;
                    (cpu, gpu, bps)
                }
                _ => (0.0, 0.0, 0),
            };
            next_prev_procs.insert(
                proc.pid,
                PrevProcCounters {
                    cpu_time_ns: proc.cpu_time_ns,
                    gpu_time_ns: proc.gpu_time_ns,
                    disk_bytes: disk_total,
                },
            );
            let app_bundle_path = match proc.app_bundle_path {
                Some(path) => Some(path),
                None => extract_app_bundle_path(proc.exe_path.as_str())
                    .map(|bundle| String::from_str_in(bundle, self.alloc)),
            };
            self.computed_procs.push(ComputedProc {
                pid: proc.pid,
                ppid: proc.ppid,
                name: proc.name,
                cwd: proc.cwd,
                exe_path: proc.exe_path,
                app_bundle_path,
                cpu_pct,
                gpu_pct,
                gpu_queues: proc.gpu_queues,
                mem_bytes: proc.mem_bytes,
                disk_bps,
                threads: proc.threads,
                start_time_secs: proc.start_time_secs,
            });
        }

        self.prev_procs = next_prev_procs;
        self.prev_timestamp_ns = Some(snap.timestamp_ns);
        self.wall_time_secs = snap.wall_time_secs;
        self.process_count = self.computed_procs.len();
        self.thread_count = total_threads;
        self.mem = snap.mem;
        self.gpu = snap.gpu;
        self.load_avg = snap.load_avg;
        self.uptime_secs = snap.uptime_secs;
        self.sample_count = self.sample_count.saturating_add(1);

        // 3. Push history rings.
        self.cpu_history.push(self.total_cpu_pct);
        self.gpu_history.push(self.gpu.device_util_pct);
        self.gpu_renderer_history.push(self.gpu.renderer_util_pct);
        self.gpu_tiler_history.push(self.gpu.tiler_util_pct);
        self.mem_history.push(self.mem.used_pct());

        // 4. Rebuild display rows.
        self.rebuild_rows();
    }

    /// Sets the active sort column. Clicking the same column toggles ascending/descending;
    /// switching to a new column defaults to ascending for `Pid`/`Name` and descending for
    /// `Cpu`/`Gpu`/`Mem`.
    pub fn set_sort(&mut self, col: SortColumn) {
        if self.sort == col {
            self.ascending = !self.ascending;
        } else {
            self.sort = col;
            self.ascending = matches!(col, SortColumn::Pid | SortColumn::Name);
        }
        self.rebuild_rows();
    }

    /// Toggles between process tree mode and flat list mode, rebuilding rows immediately.
    pub fn toggle_tree_mode(&mut self) {
        self.tree_mode = !self.tree_mode;
        self.rebuild_rows();
    }

    /// Toggles pause state for future snapshots.
    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
    }

    /// Toggles collapse state for `pid` in tree mode, rebuilding rows immediately.
    pub fn toggle_collapse(&mut self, pid: u32) {
        if !self.collapsed.remove(&pid) {
            self.collapsed.insert(pid);
        }
        self.rebuild_rows();
    }

    /// Updates the case-insensitive filter query and rebuilds rows immediately.
    pub fn set_filter(&mut self, query: &str) {
        self.filter.clear();
        self.filter.push_str(query);
        self.rebuild_rows();
    }

    #[inline]
    pub fn filter(&self) -> &str {
        self.filter.as_str()
    }

    #[inline]
    pub fn tree_mode(&self) -> bool {
        self.tree_mode
    }

    #[inline]
    pub fn paused(&self) -> bool {
        self.paused
    }

    #[inline]
    pub fn sort(&self) -> SortColumn {
        self.sort
    }

    #[inline]
    pub fn ascending(&self) -> bool {
        self.ascending
    }

    #[inline]
    pub fn rows(&self) -> &[ProcessRow<'a>] {
        self.rows.as_slice()
    }

    #[inline]
    pub fn core_utils(&self) -> &[f32] {
        self.core_utils.as_slice()
    }

    #[inline]
    pub fn total_cpu_pct(&self) -> f32 {
        self.total_cpu_pct
    }

    #[inline]
    pub fn gpu(&self) -> &RawGpuSample<'a> {
        &self.gpu
    }

    #[inline]
    pub fn mem(&self) -> RawMemSample {
        self.mem
    }

    #[inline]
    pub fn cpu_history(&self) -> &HistoryRing<64> {
        &self.cpu_history
    }

    #[inline]
    pub fn gpu_history(&self) -> &HistoryRing<64> {
        &self.gpu_history
    }

    #[inline]
    pub fn gpu_renderer_history(&self) -> &HistoryRing<64> {
        &self.gpu_renderer_history
    }

    #[inline]
    pub fn gpu_tiler_history(&self) -> &HistoryRing<64> {
        &self.gpu_tiler_history
    }

    #[inline]
    pub fn mem_history(&self) -> &HistoryRing<64> {
        &self.mem_history
    }

    #[inline]
    pub fn process_count(&self) -> usize {
        self.process_count
    }

    #[inline]
    pub fn thread_count(&self) -> u32 {
        self.thread_count
    }

    #[inline]
    pub fn load_avg(&self) -> [f32; 3] {
        self.load_avg
    }

    #[inline]
    pub fn uptime_secs(&self) -> u64 {
        self.uptime_secs
    }

    #[inline]
    pub fn wall_time_secs(&self) -> u64 {
        self.wall_time_secs
    }

    #[inline]
    pub fn sample_count(&self) -> u64 {
        self.sample_count
    }

    /// Computes all live running processes scoped to `folder` (including subfolders,
    /// descendants up to 32 levels deep, hot CPU/GPU indicators, and `blaze-shell` pane
    /// attribution), sorted hottest-first.
    pub fn folder_processes_in<'b>(
        &self,
        folder: &str,
        own_pid: u32,
        pane_roots: &[ShellPanePid],
        alloc: Alloc<'b>,
    ) -> FolderProcSummary<'b> {
        let mut entries: AVec<'b, FolderProcEntry<'b>> = Vec::new_in(alloc);
        if folder.is_empty() || self.computed_procs.is_empty() {
            return FolderProcSummary {
                folder: String::from_str_in(folder, alloc),
                entries,
                total_cpu_pct: 0.0,
                total_gpu_pct: 0.0,
                total_mem_bytes: 0,
                is_hot: false,
                primary_name: String::new_in(alloc),
            };
        }

        // Index PID -> computed_procs index (first occurrence wins).
        let mut pid_to_idx: ABTreeMap<'b, u32, usize> = BTreeMap::new_in(alloc);
        for (i, proc) in self.computed_procs.iter().enumerate() {
            if !pid_to_idx.contains_key(&proc.pid) {
                pid_to_idx.insert(proc.pid, i);
            }
        }

        // Index pane root PID -> ShellPanePid and identify panes whose root cwd is in `folder`.
        let mut pane_by_pid: ABTreeMap<'b, u32, ShellPanePid> = BTreeMap::new_in(alloc);
        let mut panes_in_folder: ABTreeSet<'b, u64> = BTreeSet::new_in(alloc);
        for &root in pane_roots {
            if !pane_by_pid.contains_key(&root.pid) {
                pane_by_pid.insert(root.pid, root);
            }
            if let Some(&idx) = pid_to_idx.get(&root.pid)
                && path_is_within(self.computed_procs[idx].cwd.as_str(), folder)
            {
                panes_in_folder.insert(root.pane_id);
            }
        }

        for proc in self.computed_procs.iter() {
            if proc.pid == own_pid {
                continue;
            }

            let direct_match = path_is_within(proc.cwd.as_str(), folder)
                || (proc.cwd.is_empty()
                    && pane_by_pid
                        .get(&proc.pid)
                        .is_some_and(|sp| panes_in_folder.contains(&sp.pane_id)));

            let via_descendant = if direct_match {
                false
            } else {
                let mut matched_ancestor = false;
                let mut visited = [0u32; MAX_ANCESTOR_DEPTH + 1];
                visited[0] = proc.pid;
                let mut visited_len = 1usize;
                let mut cur = proc.ppid;

                for _ in 0..MAX_ANCESTOR_DEPTH {
                    if cur == 0 || cur == 1 || cur == own_pid {
                        break;
                    }
                    if visited[..visited_len].contains(&cur) {
                        break;
                    }
                    visited[visited_len] = cur;
                    visited_len += 1;

                    match pid_to_idx.get(&cur) {
                        Some(&anc_idx) => {
                            let anc = &self.computed_procs[anc_idx];
                            if path_is_within(anc.cwd.as_str(), folder) {
                                matched_ancestor = true;
                                break;
                            }
                            if anc.cwd.is_empty()
                                && pane_by_pid
                                    .get(&anc.pid)
                                    .is_some_and(|sp| panes_in_folder.contains(&sp.pane_id))
                            {
                                matched_ancestor = true;
                                break;
                            }
                            cur = anc.ppid;
                        }
                        None => {
                            if pane_by_pid
                                .get(&cur)
                                .is_some_and(|sp| panes_in_folder.contains(&sp.pane_id))
                            {
                                matched_ancestor = true;
                            }
                            break;
                        }
                    }
                }
                if !matched_ancestor {
                    continue;
                }
                true
            };

            // Attribute nearest matching ShellPanePid across self -> ancestors (up to depth 32).
            let shell_pane = if pane_by_pid.is_empty() {
                None
            } else {
                let mut found = None;
                let mut visited = [0u32; MAX_ANCESTOR_DEPTH + 1];
                let mut visited_len = 0usize;
                let mut cur = proc.pid;
                for _ in 0..=MAX_ANCESTOR_DEPTH {
                    if cur == 0 || cur == 1 || cur == own_pid {
                        break;
                    }
                    if visited[..visited_len].contains(&cur) {
                        break;
                    }
                    visited[visited_len] = cur;
                    visited_len += 1;

                    if let Some(&sp) = pane_by_pid.get(&cur) {
                        found = Some(sp);
                        break;
                    }
                    match pid_to_idx.get(&cur) {
                        Some(&idx) => cur = self.computed_procs[idx].ppid,
                        None => break,
                    }
                }
                found
            };

            let is_hot = proc.cpu_pct >= HOT_CPU_PCT || proc.gpu_pct >= HOT_GPU_PCT;
            let uptime_secs = if self.wall_time_secs > 0 && proc.start_time_secs > 0 {
                self.wall_time_secs.saturating_sub(proc.start_time_secs)
            } else {
                0
            };
            let parent_name = match pid_to_idx.get(&proc.ppid) {
                Some(&parent_idx) => {
                    String::from_str_in(self.computed_procs[parent_idx].name.as_str(), alloc)
                }
                None => String::new_in(alloc),
            };

            // Walk ancestors `[parent, grandparent, ...]` (up to MAX_ANCESTOR_DEPTH, cycle-guarded,
            // stopping at 0, 1, or missing PID) to discover host `.app` origin & clean ancestor chain.
            let mut ancestor_indices = [0usize; MAX_ANCESTOR_DEPTH];
            let mut ancestor_len = 0usize;
            {
                let mut visited = [0u32; MAX_ANCESTOR_DEPTH + 1];
                visited[0] = proc.pid;
                let mut visited_len = 1usize;
                let mut cur = proc.ppid;
                for _ in 0..MAX_ANCESTOR_DEPTH {
                    if cur == 0 || cur == 1 {
                        break;
                    }
                    if visited[..visited_len].contains(&cur) {
                        break;
                    }
                    visited[visited_len] = cur;
                    visited_len += 1;
                    let Some(&anc_idx) = pid_to_idx.get(&cur) else {
                        break;
                    };
                    ancestor_indices[ancestor_len] = anc_idx;
                    ancestor_len += 1;
                    cur = self.computed_procs[anc_idx].ppid;
                }
            }
            let ancestors = &ancestor_indices[..ancestor_len];

            let mut origin_app_pos: Option<usize> = None;
            let mut origin_app_bundle_path: Option<AString<'b>> = None;
            let mut origin_app_name = String::new_in(alloc);

            if shell_pane.is_some() {
                origin_app_name.push_str("blaze-shell");
                if let Some(&own_idx) = pid_to_idx.get(&own_pid) {
                    origin_app_bundle_path = self.computed_procs[own_idx]
                        .app_bundle_path
                        .as_ref()
                        .map(|p| String::from_str_in(p.as_str(), alloc));
                }
                origin_app_pos = ancestors
                    .iter()
                    .position(|&idx| self.computed_procs[idx].pid == own_pid);
            } else {
                let mut found_app_pos = None;
                for (pos, &anc_idx) in ancestors.iter().enumerate() {
                    let anc = &self.computed_procs[anc_idx];
                    if anc.pid == own_pid {
                        continue;
                    }
                    if anc.app_bundle_path.is_some() {
                        found_app_pos = Some(pos);
                        break;
                    }
                }
                if let Some(mut app_pos) = found_app_pos {
                    let bundle = self.computed_procs[ancestors[app_pos]]
                        .app_bundle_path
                        .as_ref()
                        .map(|p| p.as_str())
                        .unwrap_or("");
                    // Walk up through contiguous ancestors belonging to the same `.app` bundle
                    // so helper binaries inside the bundle (e.g. `Antigravity.app -> language_server`)
                    // stay visible in `origin_chain` underneath the host app.
                    while app_pos + 1 < ancestors.len() {
                        let next = &self.computed_procs[ancestors[app_pos + 1]];
                        if next.pid == own_pid {
                            break;
                        }
                        if next.app_bundle_path.as_ref().map(|p| p.as_str()) == Some(bundle) {
                            app_pos += 1;
                        } else {
                            break;
                        }
                    }
                    origin_app_pos = Some(app_pos);
                    origin_app_bundle_path = Some(String::from_str_in(bundle, alloc));
                    origin_app_name.push_str(app_display_name(bundle));
                } else if let Some(bundle) = proc.app_bundle_path.as_ref() {
                    origin_app_bundle_path = Some(String::from_str_in(bundle.as_str(), alloc));
                    origin_app_name.push_str(app_display_name(bundle.as_str()));
                } else {
                    for &anc_idx in ancestors.iter() {
                        let anc = &self.computed_procs[anc_idx];
                        if anc.pid == own_pid {
                            continue;
                        }
                        let name = anc.name.as_str();
                        if !is_os_wrapper_for_origin(name) {
                            origin_app_name.push_str(name);
                            break;
                        }
                    }
                }
            }

            let mut hop_buf = [""; MAX_ANCESTOR_DEPTH];
            let mut hop_len = 0usize;
            if !ancestors.is_empty() {
                let start_pos = origin_app_pos.unwrap_or(ancestors.len() - 1);
                for pos in (0..=start_pos).rev() {
                    let is_origin_hop = origin_app_pos == Some(pos);
                    let hop_name = if is_origin_hop {
                        origin_app_name.as_str()
                    } else {
                        self.computed_procs[ancestors[pos]].name.as_str()
                    };
                    if hop_name.is_empty() || (!is_origin_hop && is_noisy_chain_wrapper(hop_name)) {
                        continue;
                    }
                    if hop_len > 0 && hop_buf[hop_len - 1].eq_ignore_ascii_case(hop_name) {
                        continue;
                    }
                    hop_buf[hop_len] = hop_name;
                    hop_len += 1;
                }
            }

            const MAX_CHAIN_HOPS: usize = 4;
            let mut origin_chain = String::new_in(alloc);
            if hop_len <= MAX_CHAIN_HOPS {
                for (i, &hop) in hop_buf[..hop_len].iter().enumerate() {
                    if i > 0 {
                        origin_chain.push_str(" › ");
                    }
                    origin_chain.push_str(hop);
                }
            } else if origin_app_pos.is_some() {
                origin_chain.push_str(hop_buf[0]);
                let mut prev = hop_buf[0];
                for &hop in &hop_buf[hop_len - (MAX_CHAIN_HOPS - 1)..hop_len] {
                    if !prev.eq_ignore_ascii_case(hop) {
                        origin_chain.push_str(" › ");
                        origin_chain.push_str(hop);
                        prev = hop;
                    }
                }
            } else {
                for (i, &hop) in hop_buf[hop_len - MAX_CHAIN_HOPS..hop_len].iter().enumerate() {
                    if i > 0 {
                        origin_chain.push_str(" › ");
                    }
                    origin_chain.push_str(hop);
                }
            }

            entries.push(FolderProcEntry {
                pid: proc.pid,
                ppid: proc.ppid,
                name: String::from_str_in(proc.name.as_str(), alloc),
                parent_name,
                cwd: String::from_str_in(proc.cwd.as_str(), alloc),
                rel_dir: String::from_str_in(relative_subpath(proc.cwd.as_str(), folder), alloc),
                exe_path: String::from_str_in(proc.exe_path.as_str(), alloc),
                app_bundle_path: proc
                    .app_bundle_path
                    .as_ref()
                    .map(|p| String::from_str_in(p.as_str(), alloc)),
                cpu_pct: proc.cpu_pct,
                gpu_pct: proc.gpu_pct,
                mem_bytes: proc.mem_bytes,
                threads: proc.threads,
                uptime_secs,
                is_hot,
                via_descendant,
                shell_pane,
                origin_app_name,
                origin_app_bundle_path,
                origin_chain,
            });
        }

        entries.as_mut_slice().sort_unstable_by(|a, b| {
            b.is_hot
                .cmp(&a.is_hot)
                .then_with(|| (b.cpu_pct + b.gpu_pct).total_cmp(&(a.cpu_pct + a.gpu_pct)))
                .then_with(|| b.mem_bytes.cmp(&a.mem_bytes))
                .then_with(|| a.pid.cmp(&b.pid))
        });

        let mut total_cpu_pct = 0.0f32;
        let mut total_gpu_pct = 0.0f32;
        let mut total_mem_bytes = 0u64;
        let mut any_entry_hot = false;
        for entry in entries.iter() {
            total_cpu_pct += entry.cpu_pct;
            total_gpu_pct += entry.gpu_pct;
            total_mem_bytes = total_mem_bytes.saturating_add(entry.mem_bytes);
            any_entry_hot |= entry.is_hot;
        }
        let is_hot = total_cpu_pct >= HOT_CPU_PCT || total_gpu_pct >= HOT_GPU_PCT || any_entry_hot;
        let primary_name = match entries.first() {
            Some(first) => String::from_str_in(first.name.as_str(), alloc),
            None => String::new_in(alloc),
        };

        FolderProcSummary {
            folder: String::from_str_in(folder, alloc),
            entries,
            total_cpu_pct,
            total_gpu_pct,
            total_mem_bytes,
            is_hot,
            primary_name,
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let n = self.computed_procs.len();
        if n == 0 {
            return;
        }
        if self.tree_mode {
            self.rebuild_tree_rows();
        } else {
            self.rebuild_flat_rows();
        }
    }

    fn rebuild_flat_rows(&mut self) {
        let query = self.filter.as_str();
        let mut indices: AVec<'a, usize> = Vec::new_in(self.alloc);
        for (i, proc) in self.computed_procs.iter().enumerate() {
            if matches_filter(proc, query) {
                indices.push(i);
            }
        }

        let sort = self.sort;
        let ascending = self.ascending;
        let procs = self.computed_procs.as_slice();
        indices.as_mut_slice().sort_unstable_by(|&a, &b| {
            let pa = &procs[a];
            let pb = &procs[b];
            compare_rows(
                sort,
                ascending,
                SortKey {
                    cpu: pa.cpu_pct,
                    gpu: pa.gpu_pct,
                    mem: pa.mem_bytes,
                    pid: pa.pid,
                    name: pa.name.as_str(),
                },
                SortKey {
                    cpu: pb.cpu_pct,
                    gpu: pb.gpu_pct,
                    mem: pb.mem_bytes,
                    pid: pb.pid,
                    name: pb.name.as_str(),
                },
            )
        });

        let total = indices.len();
        for (pos, idx) in indices.iter().copied().enumerate() {
            let proc = &self.computed_procs[idx];
            self.rows.push(ProcessRow {
                pid: proc.pid,
                ppid: proc.ppid,
                depth: 0,
                has_children: false,
                collapsed: false,
                is_last_sibling: pos + 1 == total,
                ancestor_mask: 0,
                child_count: 0,
                tree_prefix: String::new_in(self.alloc),
                name: String::from_str_in(proc.name.as_str(), self.alloc),
                cwd: String::from_str_in(proc.cwd.as_str(), self.alloc),
                exe_path: String::from_str_in(proc.exe_path.as_str(), self.alloc),
                app_bundle_path: proc
                    .app_bundle_path
                    .as_ref()
                    .map(|p| String::from_str_in(p.as_str(), self.alloc)),
                cpu_pct: proc.cpu_pct,
                subtree_cpu_pct: proc.cpu_pct,
                gpu_pct: proc.gpu_pct,
                subtree_gpu_pct: proc.gpu_pct,
                gpu_queues: proc.gpu_queues,
                mem_bytes: proc.mem_bytes,
                subtree_mem_bytes: proc.mem_bytes,
                disk_bps: proc.disk_bps,
                subtree_disk_bps: proc.disk_bps,
                threads: proc.threads,
                subtree_threads: proc.threads,
                start_time_secs: proc.start_time_secs,
            });
        }
    }

    fn rebuild_tree_rows(&mut self) {
        let alloc = self.alloc;
        let n = self.computed_procs.len();
        let query = self.filter.as_str();
        let filtering = !query.is_empty();

        // 1. Map PID -> index (first occurrence wins if duplicates exist).
        let mut pid_to_idx: ABTreeMap<'a, u32, usize> = BTreeMap::new_in(alloc);
        for (i, proc) in self.computed_procs.iter().enumerate() {
            if !pid_to_idx.contains_key(&proc.pid) {
                pid_to_idx.insert(proc.pid, i);
            }
        }

        // 2. Resolve parent index per node, treating ppid == 0 or self-parent as root.
        let mut parent_idx: AVec<'a, Option<usize>> = Vec::with_capacity_in(n, alloc);
        for (i, proc) in self.computed_procs.iter().enumerate() {
            let parent = if proc.ppid == 0 || proc.ppid == proc.pid {
                None
            } else {
                match pid_to_idx.get(&proc.ppid).copied() {
                    Some(pidx) if pidx != i => Some(pidx),
                    _ => None,
                }
            };
            parent_idx.push(parent);
        }

        // 3. Break any multi-node parent cycles defensively so the graph is a strict forest.
        // state: 0 = unvisited, 1 = on current path, 2 = verified acyclic.
        let mut state: AVec<'a, u8> = Vec::with_capacity_in(n, alloc);
        for _ in 0..n {
            state.push(0);
        }
        let mut path: AVec<'a, usize> = Vec::new_in(alloc);
        for start in 0..n {
            if state[start] != 0 {
                continue;
            }
            path.clear();
            let mut cur = start;
            loop {
                state[cur] = 1;
                path.push(cur);
                match parent_idx[cur] {
                    Some(p) if state[p] == 0 => cur = p,
                    Some(p) if state[p] == 1 => {
                        // Cycle detected: detach `cur` as a root.
                        parent_idx[cur] = None;
                        break;
                    }
                    _ => break,
                }
            }
            for &node in path.iter() {
                state[node] = 2;
            }
        }

        // 4. Build parent -> children adjacency lists and roots list.
        let mut children: AVec<'a, AVec<'a, usize>> = Vec::with_capacity_in(n, alloc);
        for _ in 0..n {
            children.push(Vec::new_in(alloc));
        }
        let mut roots: AVec<'a, usize> = Vec::new_in(alloc);
        for i in 0..n {
            if let Some(p) = parent_idx[i] {
                children[p].push(i);
            } else {
                roots.push(i);
            }
        }

        // 5. Compute top-down order iteratively so its reverse is a valid bottom-up post-order.
        let mut order: AVec<'a, usize> = Vec::with_capacity_in(n, alloc);
        let mut bfs_stack: AVec<'a, usize> = Vec::with_capacity_in(n, alloc);
        for &r in roots.iter() {
            bfs_stack.push(r);
        }
        while let Some(node) = bfs_stack.pop() {
            order.push(node);
            for &child in children[node].iter() {
                bfs_stack.push(child);
            }
        }

        // 6. Bottom-up rollup of subtree CPU, GPU, memory, disk, threads, descendant counts, and filter visibility.
        let mut subtree_cpu: AVec<'a, f32> = Vec::with_capacity_in(n, alloc);
        let mut subtree_gpu: AVec<'a, f32> = Vec::with_capacity_in(n, alloc);
        let mut subtree_mem: AVec<'a, u64> = Vec::with_capacity_in(n, alloc);
        let mut subtree_disk: AVec<'a, u64> = Vec::with_capacity_in(n, alloc);
        let mut subtree_threads: AVec<'a, u32> = Vec::with_capacity_in(n, alloc);
        let mut child_count: AVec<'a, u32> = Vec::with_capacity_in(n, alloc);
        let mut visible: AVec<'a, bool> = Vec::with_capacity_in(n, alloc);
        for proc in self.computed_procs.iter() {
            subtree_cpu.push(proc.cpu_pct);
            subtree_gpu.push(proc.gpu_pct);
            subtree_mem.push(proc.mem_bytes);
            subtree_disk.push(proc.disk_bps);
            subtree_threads.push(proc.threads);
            child_count.push(0);
            visible.push(!filtering || matches_filter(proc, query));
        }

        for &idx in order.iter().rev() {
            if let Some(p) = parent_idx[idx] {
                subtree_cpu[p] += subtree_cpu[idx];
                subtree_gpu[p] += subtree_gpu[idx];
                subtree_mem[p] = subtree_mem[p].saturating_add(subtree_mem[idx]);
                subtree_disk[p] = subtree_disk[p].saturating_add(subtree_disk[idx]);
                subtree_threads[p] = subtree_threads[p].saturating_add(subtree_threads[idx]);
                child_count[p] = child_count[p]
                    .saturating_add(1)
                    .saturating_add(child_count[idx]);
                if visible[idx] {
                    visible[p] = true;
                }
            }
        }

        // 7. Filter roots and child lists to visible nodes, and sort each sibling list by subtree values.
        let sort = self.sort;
        let ascending = self.ascending;
        let procs = self.computed_procs.as_slice();
        let scpu = subtree_cpu.as_slice();
        let sgpu = subtree_gpu.as_slice();
        let smem = subtree_mem.as_slice();
        let vis = visible.as_slice();

        let cmp_nodes = |&a: &usize, &b: &usize| -> Ordering {
            compare_rows(
                sort,
                ascending,
                SortKey {
                    cpu: scpu[a],
                    gpu: sgpu[a],
                    mem: smem[a],
                    pid: procs[a].pid,
                    name: procs[a].name.as_str(),
                },
                SortKey {
                    cpu: scpu[b],
                    gpu: sgpu[b],
                    mem: smem[b],
                    pid: procs[b].pid,
                    name: procs[b].name.as_str(),
                },
            )
        };

        if filtering {
            roots.retain(|&idx| vis[idx]);
        }
        roots.as_mut_slice().sort_unstable_by(cmp_nodes);

        for list in children.as_mut_slice().iter_mut() {
            if filtering {
                list.retain(|&idx| vis[idx]);
            }
            list.as_mut_slice().sort_unstable_by(cmp_nodes);
        }

        // 8. Iterative DFS emission with clean box-drawing prefixes.
        struct DfsFrame {
            node: usize,
            depth: usize,
            is_last: bool,
        }

        let mut stack: AVec<'a, DfsFrame> = Vec::with_capacity_in(n, alloc);
        let root_count = roots.len();
        for (pos, &r) in roots.iter().enumerate().rev() {
            stack.push(DfsFrame {
                node: r,
                depth: 0,
                is_last: pos + 1 == root_count,
            });
        }

        let mut ancestor_has_more: AVec<'a, bool> = Vec::new_in(alloc);

        while let Some(frame) = stack.pop() {
            if frame.depth == 0 {
                ancestor_has_more.clear();
            } else {
                ancestor_has_more.truncate(frame.depth - 1);
            }

            let mut ancestor_mask = 0u64;
            for (lvl, &has_more) in ancestor_has_more.iter().enumerate() {
                if has_more && lvl < 64 {
                    ancestor_mask |= 1u64 << lvl;
                }
            }

            let mut tree_prefix = String::new_in(alloc);
            if frame.depth > 0 {
                for &has_more in ancestor_has_more.iter() {
                    if has_more {
                        tree_prefix.push_str("│  ");
                    } else {
                        tree_prefix.push_str("   ");
                    }
                }
                if frame.is_last {
                    tree_prefix.push_str("└─ ");
                } else {
                    tree_prefix.push_str("├─ ");
                }
            }

            let proc = &self.computed_procs[frame.node];
            let node_children = children[frame.node].as_slice();
            let has_children = !node_children.is_empty();
            // Auto-expand filtered branches so matching descendants stay visible.
            let collapsed = has_children && !filtering && self.collapsed.contains(&proc.pid);

            self.rows.push(ProcessRow {
                pid: proc.pid,
                ppid: proc.ppid,
                depth: frame.depth.min(usize::from(u16::MAX)) as u16,
                has_children,
                collapsed,
                is_last_sibling: frame.is_last,
                ancestor_mask,
                child_count: child_count[frame.node],
                tree_prefix,
                name: String::from_str_in(proc.name.as_str(), alloc),
                cwd: String::from_str_in(proc.cwd.as_str(), alloc),
                exe_path: String::from_str_in(proc.exe_path.as_str(), alloc),
                app_bundle_path: proc
                    .app_bundle_path
                    .as_ref()
                    .map(|p| String::from_str_in(p.as_str(), alloc)),
                cpu_pct: proc.cpu_pct,
                subtree_cpu_pct: subtree_cpu[frame.node],
                gpu_pct: proc.gpu_pct,
                subtree_gpu_pct: subtree_gpu[frame.node],
                gpu_queues: proc.gpu_queues,
                mem_bytes: proc.mem_bytes,
                subtree_mem_bytes: subtree_mem[frame.node],
                disk_bps: proc.disk_bps,
                subtree_disk_bps: subtree_disk[frame.node],
                threads: proc.threads,
                subtree_threads: subtree_threads[frame.node],
                start_time_secs: proc.start_time_secs,
            });

            if has_children && !collapsed {
                if frame.depth > 0 {
                    ancestor_has_more.push(!frame.is_last);
                }
                let count = node_children.len();
                for (pos, &child) in node_children.iter().enumerate().rev() {
                    stack.push(DfsFrame {
                        node: child,
                        depth: frame.depth + 1,
                        is_last: pos + 1 == count,
                    });
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct SortKey<'a> {
    cpu: f32,
    gpu: f32,
    mem: u64,
    pid: u32,
    name: &'a str,
}

fn compare_rows(sort: SortColumn, ascending: bool, a: SortKey<'_>, b: SortKey<'_>) -> Ordering {
    let primary = match sort {
        SortColumn::Cpu => a.cpu.total_cmp(&b.cpu).then_with(|| a.mem.cmp(&b.mem)),
        SortColumn::Gpu => a.gpu.total_cmp(&b.gpu).then_with(|| a.mem.cmp(&b.mem)),
        SortColumn::Mem => a.mem.cmp(&b.mem),
        SortColumn::Pid => a.pid.cmp(&b.pid),
        SortColumn::Name => cmp_ignore_ascii_case(a.name, b.name).then_with(|| a.name.cmp(b.name)),
    };
    let ordered = if ascending { primary } else { primary.reverse() };
    ordered.then_with(|| a.pid.cmp(&b.pid))
}

fn cmp_ignore_ascii_case(a: &str, b: &str) -> Ordering {
    let mut ai = a.bytes().map(|c| c.to_ascii_lowercase());
    let mut bi = b.bytes().map(|c| c.to_ascii_lowercase());
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) => {
                let ord = x.cmp(&y);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

fn format_u32_decimal(mut v: u32, buf: &mut [u8; 10]) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    match core::str::from_utf8(&buf[i..]) {
        Ok(s) => s,
        Err(_) => "",
    }
}

fn matches_filter(proc: &ComputedProc<'_>, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if contains_ignore_ascii_case(proc.name.as_str(), query) {
        return true;
    }
    if contains_ignore_ascii_case(proc.exe_path.as_str(), query) {
        return true;
    }
    let mut pid_buf = [0u8; 10];
    let pid_str = format_u32_decimal(proc.pid, &mut pid_buf);
    contains_ignore_ascii_case(pid_str, query)
}

fn is_os_wrapper_for_origin(name: &str) -> bool {
    name.is_empty()
        || name.eq_ignore_ascii_case("login")
        || name.eq_ignore_ascii_case("launchd")
        || name.eq_ignore_ascii_case("kernel_task")
        || name.eq_ignore_ascii_case("init")
        || name.eq_ignore_ascii_case("systemd")
}

fn is_noisy_chain_wrapper(name: &str) -> bool {
    is_os_wrapper_for_origin(name)
        || name.eq_ignore_ascii_case("sudo")
        || name.eq_ignore_ascii_case("su")
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_own::{Erased, Inline};

    fn make_proc<'a>(
        alloc: Alloc<'a>,
        pid: u32,
        ppid: u32,
        name: &str,
        exe_path: &str,
        cpu_time_ns: u64,
        gpu_time_ns: u64,
        mem_bytes: u64,
        disk_read_bytes: u64,
        disk_write_bytes: u64,
        threads: u32,
    ) -> RawProcess<'a> {
        RawProcess {
            pid,
            ppid,
            uid: 501,
            name: String::from_str_in(name, alloc),
            cwd: AString::new_in(alloc),
            exe_path: String::from_str_in(exe_path, alloc),
            app_bundle_path: None,
            cpu_time_ns,
            gpu_time_ns,
            gpu_queues: if gpu_time_ns > 0 { 2 } else { 0 },
            mem_bytes,
            disk_read_bytes,
            disk_write_bytes,
            threads,
            start_time_secs: 0,
        }
    }

    fn make_gpu<'a>(
        alloc: Alloc<'a>,
        model: &str,
        dev: f32,
        ren: f32,
        tiler: f32,
    ) -> RawGpuSample<'a> {
        RawGpuSample {
            model: String::from_str_in(model, alloc),
            core_count: 16,
            device_util_pct: dev,
            renderer_util_pct: ren,
            tiler_util_pct: tiler,
            vram_in_use_bytes: 512 * 1024 * 1024,
            vram_alloc_bytes: 1024 * 1024 * 1024,
        }
    }

    fn make_mem(total: u64, app: u64, wired: u64, compressed: u64) -> RawMemSample {
        RawMemSample {
            total_bytes: total,
            app_bytes: app,
            wired_bytes: wired,
            compressed_bytes: compressed,
            cached_bytes: 128,
            swap_used_bytes: 0,
            swap_total_bytes: 0,
        }
    }

    #[test]
    fn cpu_gpu_disk_delta_math_and_pause_semantics() {
        let arena = Inline::<131_072>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();

        let mut engine = SysMonEngine::new_in(alloc);
        assert!(engine.tree_mode());
        assert_eq!(engine.sort(), SortColumn::Cpu);
        assert!(!engine.ascending());
        assert!(!engine.paused());
        assert_eq!(engine.sample_count(), 0);

        // Pausing before the very first snapshot must still accept sample #1 so the UI is not blank.
        engine.toggle_pause();
        assert!(engine.paused());

        let mut cores1 = Vec::new_in(alloc);
        cores1.push(CoreTicks { user: 100, system: 50, idle: 850, nice: 0 });
        cores1.push(CoreTicks { user: 200, system: 100, idle: 700, nice: 0 });
        let mut procs1 = Vec::new_in(alloc);
        procs1.push(make_proc(
            alloc,
            10,
            0,
            "worker",
            "/usr/local/bin/worker",
            1_000_000_000,
            500_000_000,
            4096,
            10_000,
            5_000,
            4,
        ));
        engine.ingest(RawSnapshot {
            timestamp_ns: 1_000_000_000,
            wall_time_secs: 1_700_000_000,
            cores: cores1,
            mem: make_mem(1000, 300, 150, 50),
            gpu: make_gpu(alloc, "Apple M4 Max", 25.0, 20.0, 15.0),
            processes: procs1,
            load_avg: [1.5, 1.2, 0.9],
            uptime_secs: 3600,
        });
        assert_eq!(engine.sample_count(), 1);
        assert_eq!(engine.wall_time_secs(), 1_700_000_000);
        assert_eq!(engine.core_utils(), &[0.0, 0.0]);
        assert_eq!(engine.total_cpu_pct(), 0.0);
        assert_eq!(engine.mem().used_bytes(), 500);
        assert!((engine.mem().used_pct() - 50.0).abs() < 1e-4);
        assert_eq!(engine.gpu().model.as_str(), "Apple M4 Max");
        assert_eq!(engine.gpu().clone_in(alloc), *engine.gpu());
        assert_eq!(engine.process_count(), 1);
        assert_eq!(engine.thread_count(), 4);
        assert_eq!(engine.load_avg(), [1.5, 1.2, 0.9]);
        assert_eq!(engine.uptime_secs(), 3600);

        // While paused and sample_count > 0, subsequent snapshots are ignored.
        engine.ingest(RawSnapshot {
            timestamp_ns: 2_000_000_000,
            wall_time_secs: 1_700_009_999,
            cores: Vec::new_in(alloc),
            mem: make_mem(1000, 900, 0, 0),
            gpu: RawGpuSample::empty_in(alloc),
            processes: Vec::new_in(alloc),
            load_avg: [9.0, 9.0, 9.0],
            uptime_secs: 9999,
        });
        assert_eq!(engine.sample_count(), 1);
        assert_eq!(engine.uptime_secs(), 3600);
        assert_eq!(engine.wall_time_secs(), 1_700_000_000);

        // Unpause and feed sample #2 after dt = 500ms (0.5s).
        engine.toggle_pause();
        let mut cores2 = Vec::new_in(alloc);
        // Core 0: d_user=50, d_sys=25, d_nice=5, d_idle=20 => 80 / 100 = 80%
        cores2.push(CoreTicks { user: 150, system: 75, idle: 870, nice: 5 });
        // Core 1: d_user=10, d_sys=10, d_nice=0, d_idle=80 => 20 / 100 = 20%
        cores2.push(CoreTicks { user: 210, system: 110, idle: 780, nice: 0 });
        // Aggregate: (80 + 20) / (100 + 100) = 50%

        let mut procs2 = Vec::new_in(alloc);
        // dt = 500_000_000 ns (0.5s):
        // d_cpu = 750_000_000 ns => 150.0% (multi-core process!)
        // d_gpu = 125_000_000 ns => 25.0%
        // d_disk = (15_000 + 10_000) - (10_000 + 5_000) = 10_000 bytes over 0.5s => 20_000 B/s
        let mut worker2 = make_proc(
            alloc,
            10,
            0,
            "worker",
            "/usr/local/bin/worker",
            1_750_000_000,
            625_000_000,
            8192,
            15_000,
            10_000,
            6,
        );
        worker2.start_time_secs = 1_699_999_900;
        procs2.push(worker2);
        engine.ingest(RawSnapshot {
            timestamp_ns: 1_500_000_000,
            wall_time_secs: 1_700_000_001,
            cores: cores2,
            mem: make_mem(1000, 400, 200, 100),
            gpu: make_gpu(alloc, "Apple M4 Max", 42.0, 35.0, 18.0),
            processes: procs2,
            load_avg: [2.0, 1.5, 1.0],
            uptime_secs: 3601,
        });

        assert_eq!(engine.sample_count(), 2);
        assert_eq!(engine.wall_time_secs(), 1_700_000_001);
        assert!((engine.core_utils()[0] - 80.0).abs() < 1e-4);
        assert!((engine.core_utils()[1] - 20.0).abs() < 1e-4);
        assert!((engine.total_cpu_pct() - 50.0).abs() < 1e-4);
        assert_eq!(engine.rows().len(), 1);
        let r = &engine.rows()[0];
        assert!((r.cpu_pct - 150.0).abs() < 1e-3);
        assert!((r.gpu_pct - 25.0).abs() < 1e-3);
        assert_eq!(r.disk_bps, 20_000);
        assert_eq!(r.threads, 6);
        assert_eq!(r.gpu_queues, 2);
        assert_eq!(r.start_time_secs, 1_699_999_900);

        // History rings should hold 2 points.
        assert_eq!(engine.cpu_history().len(), 2);
        assert!((engine.cpu_history().latest() - 50.0).abs() < 1e-4);
        assert!((engine.gpu_history().latest() - 42.0).abs() < 1e-4);
        assert!((engine.gpu_renderer_history().latest() - 35.0).abs() < 1e-4);
        assert!((engine.gpu_tiler_history().latest() - 18.0).abs() < 1e-4);
        assert!((engine.mem_history().latest() - 70.0).abs() < 1e-4);
    }

    #[test]
    fn history_ring_clamping_and_wraparound() {
        let mut ring = HistoryRing::<4>::new();
        assert!(ring.is_empty());
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.latest(), 0.0);
        assert_eq!(ring.get(0), 0.0);

        ring.push(-15.0); // clamps to 0.0
        ring.push(25.0);
        ring.push(150.0); // clamps to 100.0
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.get(0), 0.0);
        assert_eq!(ring.get(1), 25.0);
        assert_eq!(ring.get(2), 100.0);
        assert_eq!(ring.get(3), 0.0); // out of bounds
        assert_eq!(ring.latest(), 100.0);

        ring.push(f32::NAN); // sanitized to 0.0
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.get(0), 0.0);
        assert_eq!(ring.get(3), 0.0);

        // Wrap around past capacity 4.
        ring.push(40.0);
        ring.push(60.0);
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.get(0), 100.0);
        assert_eq!(ring.get(1), 0.0);
        assert_eq!(ring.get(2), 40.0);
        assert_eq!(ring.get(3), 60.0);
        assert_eq!(ring.latest(), 60.0);

        // Eq holds because NaN can never enter the ring.
        assert_eq!(ring, ring);

        // Zero-capacity ring is safe.
        let mut zero = HistoryRing::<0>::new();
        zero.push(50.0);
        assert!(zero.is_empty());
        assert_eq!(zero.latest(), 0.0);
    }

    #[test]
    fn extract_app_bundle_path_handles_nested_helpers_and_edge_cases() {
        assert_eq!(
            extract_app_bundle_path(
                "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Versions/130/Helpers/Google Chrome Helper (GPU).app/Contents/MacOS/Google Chrome Helper (GPU)"
            ),
            Some("/Applications/Google Chrome.app")
        );
        assert_eq!(
            extract_app_bundle_path("/Applications/Safari.app/Contents/MacOS/Safari"),
            Some("/Applications/Safari.app")
        );
        assert_eq!(
            extract_app_bundle_path("/Applications/MyTool.app"),
            Some("/Applications/MyTool.app")
        );
        assert_eq!(
            extract_app_bundle_path("/Applications/MyTool.app/"),
            Some("/Applications/MyTool.app")
        );
        assert_eq!(extract_app_bundle_path("/usr/bin/zsh"), None);
        assert_eq!(extract_app_bundle_path("/usr/local/foo.app.bak/bin/run"), None);
        assert_eq!(extract_app_bundle_path("/Applications/.app/Contents/MacOS/x"), None);
        assert_eq!(extract_app_bundle_path(""), None);
    }

    #[test]
    fn tree_rollup_prefixes_collapse_and_filter_auto_expand() {
        let arena = Inline::<262_144>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();

        let mut engine = SysMonEngine::new_in(alloc);

        // Build a two-snapshot sequence so CPU/GPU deltas are non-zero:
        // Root 1 ("launchd") -> 10 ("Chrome", own 10% CPU, 100 mem)
        //                         -> 11 ("Chrome GPU", own 40% CPU, 50% GPU, 200 mem)
        //                            -> 13 ("Shader", own 5% CPU, 50 mem)
        //                         -> 12 ("Chrome Tab", own 20% CPU, 150 mem)
        //                    -> 20 ("Terminal", own 3% CPU, 80 mem)
        //                         -> 21 ("zsh", own 1% CPU, 20 mem)
        let make_tree_snap = |ts: u64, mult: u64| {
            let mut procs = Vec::new_in(alloc);
            procs.push(make_proc(alloc, 1, 0, "launchd", "/sbin/launchd", 10_000_000 * mult, 0, 10, 0, 0, 2));
            procs.push(make_proc(
                alloc,
                10,
                1,
                "Google Chrome",
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                100_000_000 * mult,
                0,
                100,
                0,
                0,
                10,
            ));
            procs.push(make_proc(
                alloc,
                11,
                10,
                "Google Chrome Helper (GPU)",
                "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Versions/130/Helpers/Google Chrome Helper (GPU).app/Contents/MacOS/Google Chrome Helper (GPU)",
                400_000_000 * mult,
                500_000_000 * mult,
                200,
                0,
                0,
                8,
            ));
            procs.push(make_proc(alloc, 13, 11, "ShaderWorker", "/tmp/shader", 50_000_000 * mult, 0, 50, 0, 0, 1));
            procs.push(make_proc(alloc, 12, 10, "Chrome Tab", "/Applications/Google Chrome.app/Contents/MacOS/Helper", 200_000_000 * mult, 0, 150, 0, 0, 4));
            procs.push(make_proc(alloc, 20, 1, "Terminal", "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal", 30_000_000 * mult, 0, 80, 0, 0, 3));
            procs.push(make_proc(alloc, 21, 20, "zsh", "/bin/zsh", 10_000_000 * mult, 0, 20, 0, 0, 1));
            RawSnapshot {
                timestamp_ns: ts,
                wall_time_secs: 0,
                cores: Vec::new_in(alloc),
                mem: make_mem(10_000, 500, 100, 0),
                gpu: RawGpuSample::empty_in(alloc),
                processes: procs,
                load_avg: [0.5, 0.4, 0.3],
                uptime_secs: 100,
            }
        };

        engine.ingest(make_tree_snap(1_000_000_000, 1));
        engine.ingest(make_tree_snap(2_000_000_000, 2));

        // Expected tree order (Cpu descending by subtree):
        // 1  launchd          ""
        // 10 Google Chrome    "├─ "          (subtree CPU = 10+40+5+20 = 75%, subtree mem = 500, child_count = 3)
        // 11 Chrome GPU       "│  ├─ "       (subtree CPU = 40+5 = 45%, subtree GPU = 50%, child_count = 1)
        // 13 ShaderWorker     "│  │  └─ "
        // 12 Chrome Tab       "│  └─ "
        // 20 Terminal         "└─ "          (subtree CPU = 3+1 = 4%, child_count = 1)
        // 21 zsh              "   └─ "
        let rows = engine.rows();
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[0].pid, 1);
        assert_eq!(rows[0].tree_prefix.as_str(), "");
        assert_eq!(rows[0].child_count, 6);
        assert!((rows[0].subtree_cpu_pct - 80.0).abs() < 1e-3);

        assert_eq!(rows[1].pid, 10);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].tree_prefix.as_str(), "├─ ");
        assert!((rows[1].subtree_cpu_pct - 75.0).abs() < 1e-3);
        assert!((rows[1].subtree_gpu_pct - 50.0).abs() < 1e-3);
        assert_eq!(rows[1].subtree_mem_bytes, 500);
        assert_eq!(rows[1].child_count, 3);
        assert!(rows[1].has_children);
        assert!(!rows[1].collapsed);

        assert_eq!(rows[2].pid, 11);
        assert_eq!(rows[2].depth, 2);
        assert_eq!(rows[2].tree_prefix.as_str(), "│  ├─ ");
        // Helper inherits outermost Google Chrome.app bundle!
        assert_eq!(
            rows[2].app_bundle_path.as_ref().map(|s| s.as_str()),
            Some("/Applications/Google Chrome.app")
        );

        assert_eq!(rows[3].pid, 13);
        assert_eq!(rows[3].depth, 3);
        assert_eq!(rows[3].tree_prefix.as_str(), "│  │  └─ ");

        assert_eq!(rows[4].pid, 12);
        assert_eq!(rows[4].depth, 2);
        assert_eq!(rows[4].tree_prefix.as_str(), "│  └─ ");

        assert_eq!(rows[5].pid, 20);
        assert_eq!(rows[5].depth, 1);
        assert_eq!(rows[5].tree_prefix.as_str(), "└─ ");

        assert_eq!(rows[6].pid, 21);
        assert_eq!(rows[6].depth, 2);
        assert_eq!(rows[6].tree_prefix.as_str(), "   └─ ");

        // Collapse PID 10 ("Google Chrome"): its 3 descendants disappear immediately.
        engine.toggle_collapse(10);
        let rows = engine.rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].pid, 10);
        assert!(rows[1].collapsed);
        assert!(rows[1].has_children);
        assert_eq!(rows[1].child_count, 3);
        assert_eq!(rows[2].pid, 20);
        assert_eq!(rows[3].pid, 21);

        // Filter by "shader" (case-insensitive): keeps ancestors 1 -> 10 -> 11 -> 13 and AUTO-EXPANDS 10!
        engine.set_filter("SHADER");
        assert_eq!(engine.filter(), "SHADER");
        let rows = engine.rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].pid, 1);
        assert_eq!(rows[1].pid, 10);
        assert!(!rows[1].collapsed);
        assert_eq!(rows[2].pid, 11);
        assert_eq!(rows[3].pid, 13);

        // Filter by decimal PID ("21"): keeps launchd -> Terminal -> zsh.
        engine.set_filter("21");
        let rows = engine.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].pid, 1);
        assert_eq!(rows[1].pid, 20);
        assert_eq!(rows[2].pid, 21);

        // Filter by exe_path ("/bin/zsh"): matches zsh and keeps ancestors.
        engine.set_filter("/BIN/ZSH");
        assert_eq!(engine.rows().len(), 3);
        assert_eq!(engine.rows()[2].pid, 21);

        // Clearing filter restores PID 10's collapsed state.
        engine.set_filter("");
        assert_eq!(engine.rows().len(), 4);
        assert!(engine.rows()[1].collapsed);
    }

    #[test]
    fn tree_vs_flat_sorting_and_sort_toggle() {
        let arena = Inline::<262_144>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();

        let mut engine = SysMonEngine::new_in(alloc);

        // Root A (pid 10): own CPU 5%, child A1 (pid 11) own CPU 50% => subtree CPU 55%.
        // Root B (pid 20): own CPU 30%, no children => subtree CPU 30%.
        let make_snap = |ts: u64, mult: u64| {
            let mut procs = Vec::new_in(alloc);
            procs.push(make_proc(alloc, 10, 0, "alpha", "/a", 50_000_000 * mult, 0, 100, 0, 0, 1));
            procs.push(make_proc(alloc, 11, 10, "worker", "/w", 500_000_000 * mult, 0, 500, 0, 0, 1));
            procs.push(make_proc(alloc, 20, 0, "Beta", "/b", 300_000_000 * mult, 0, 300, 0, 0, 1));
            RawSnapshot {
                timestamp_ns: ts,
                wall_time_secs: 0,
                cores: Vec::new_in(alloc),
                mem: RawMemSample::default(),
                gpu: RawGpuSample::empty_in(alloc),
                processes: procs,
                load_avg: [0.0; 3],
                uptime_secs: 1,
            }
        };

        engine.ingest(make_snap(1_000_000_000, 1));
        engine.ingest(make_snap(2_000_000_000, 2));

        // Tree mode sorts roots by subtree CPU descending: A (55%) before B (30%).
        assert_eq!(engine.rows()[0].pid, 10);
        assert_eq!(engine.rows()[1].pid, 11);
        assert_eq!(engine.rows()[2].pid, 20);

        // Switch to flat mode: sorts by individual CPU descending: A1 (50%), B (30%), A (5%).
        engine.toggle_tree_mode();
        assert!(!engine.tree_mode());
        assert_eq!(engine.rows()[0].pid, 11);
        assert_eq!(engine.rows()[1].pid, 20);
        assert_eq!(engine.rows()[2].pid, 10);
        assert_eq!(engine.rows()[0].tree_prefix.as_str(), "");

        // Clicking Cpu again toggles ascending: A (5%), B (30%), A1 (50%).
        engine.set_sort(SortColumn::Cpu);
        assert!(engine.ascending());
        assert_eq!(engine.rows()[0].pid, 10);
        assert_eq!(engine.rows()[1].pid, 20);
        assert_eq!(engine.rows()[2].pid, 11);

        // Switching to Mem defaults descending: 11 (500), 20 (300), 10 (100).
        engine.set_sort(SortColumn::Mem);
        assert!(!engine.ascending());
        assert_eq!(engine.rows()[0].pid, 11);
        assert_eq!(engine.rows()[1].pid, 20);
        assert_eq!(engine.rows()[2].pid, 10);

        // Switching to Pid defaults ascending: 10, 11, 20.
        engine.set_sort(SortColumn::Pid);
        assert!(engine.ascending());
        assert_eq!(engine.rows()[0].pid, 10);
        assert_eq!(engine.rows()[1].pid, 11);
        assert_eq!(engine.rows()[2].pid, 20);

        // Switching to Name defaults ascending (case-insensitive): alpha, Beta, worker.
        engine.set_sort(SortColumn::Name);
        assert!(engine.ascending());
        assert_eq!(engine.rows()[0].pid, 10);
        assert_eq!(engine.rows()[1].pid, 20);
        assert_eq!(engine.rows()[2].pid, 11);

        // Verify SortColumn u8 conversions.
        for (i, col) in [
            SortColumn::Cpu,
            SortColumn::Gpu,
            SortColumn::Mem,
            SortColumn::Pid,
            SortColumn::Name,
        ]
        .iter()
        .copied()
        .enumerate()
        {
            assert_eq!(col.as_u8(), i as u8);
            assert_eq!(SortColumn::from_u8(i as u8), col);
        }
        assert_eq!(SortColumn::from_u8(99), SortColumn::Name);
    }

    #[test]
    fn cyclic_ppid_graphs_are_broken_without_hanging_or_dropping_nodes() {
        let arena = Inline::<131_072>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();

        let mut engine = SysMonEngine::new_in(alloc);
        let mut procs = Vec::new_in(alloc);
        // Self-cycle (pid == ppid) and 2-node cycle (2 -> 3 -> 2).
        procs.push(make_proc(alloc, 1, 1, "self-loop", "/self", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 2, 3, "cycle-a", "/a", 0, 0, 20, 0, 0, 1));
        procs.push(make_proc(alloc, 3, 2, "cycle-b", "/b", 0, 0, 30, 0, 0, 1));

        engine.ingest(RawSnapshot {
            timestamp_ns: 1_000_000_000,
            wall_time_secs: 0,
            cores: Vec::new_in(alloc),
            mem: RawMemSample::default(),
            gpu: RawGpuSample::empty_in(alloc),
            processes: procs,
            load_avg: [0.0; 3],
            uptime_secs: 0,
        });

        assert_eq!(engine.rows().len(), 3);
    }

    fn make_proc_with_cwd<'a>(
        alloc: Alloc<'a>,
        pid: u32,
        ppid: u32,
        name: &str,
        cwd: &str,
        exe_path: &str,
        cpu_time_ns: u64,
        gpu_time_ns: u64,
        mem_bytes: u64,
        threads: u32,
    ) -> RawProcess<'a> {
        let mut p = make_proc(
            alloc,
            pid,
            ppid,
            name,
            exe_path,
            cpu_time_ns,
            gpu_time_ns,
            mem_bytes,
            0,
            0,
            threads,
        );
        p.cwd = String::from_str_in(cwd, alloc);
        p
    }

    #[test]
    fn path_is_within_and_relative_subpath_edge_cases() {
        // Exact match and trailing slashes.
        assert!(path_is_within("/Volumes/Projects/blaze", "/Volumes/Projects/blaze"));
        assert!(path_is_within("/Volumes/Projects/blaze/", "/Volumes/Projects/blaze"));
        assert!(path_is_within("/Volumes/Projects/blaze", "/Volumes/Projects/blaze/"));
        assert!(path_is_within("/Volumes/Projects/blaze///", "/Volumes/Projects/blaze//"));

        // Subfolders match.
        assert!(path_is_within("/Volumes/Projects/blaze/crates", "/Volumes/Projects/blaze"));
        assert!(path_is_within(
            "/Volumes/Projects/blaze/crates/blaze-shell/",
            "/Volumes/Projects/blaze/"
        ));

        // Prefix-collision siblings MUST NOT match!
        assert!(!path_is_within("/Volumes/Projects/blaze-2", "/Volumes/Projects/blaze"));
        assert!(!path_is_within(
            "/Volumes/Projects/blaze-2/crates",
            "/Volumes/Projects/blaze"
        ));
        assert!(!path_is_within(
            "/Volumes/Projects/blaze_extra",
            "/Volumes/Projects/blaze"
        ));

        // Empty inputs.
        assert!(!path_is_within("", "/Volumes/Projects/blaze"));
        assert!(!path_is_within("/Volumes/Projects/blaze", ""));
        assert!(!path_is_within("", ""));

        // Root folder "/".
        assert!(path_is_within("/", "/"));
        assert!(path_is_within("/Volumes/Projects/blaze", "/"));
        assert!(path_is_within("/tmp", "///"));
        assert!(!path_is_within("relative/path", "/"));

        // relative_subpath cases.
        assert_eq!(
            relative_subpath("/Volumes/Projects/blaze", "/Volumes/Projects/blaze"),
            "."
        );
        assert_eq!(
            relative_subpath("/Volumes/Projects/blaze/", "/Volumes/Projects/blaze"),
            "."
        );
        assert_eq!(
            relative_subpath(
                "/Volumes/Projects/blaze/crates/blaze-shell",
                "/Volumes/Projects/blaze"
            ),
            "crates/blaze-shell"
        );
        assert_eq!(
            relative_subpath(
                "/Volumes/Projects/blaze/crates/blaze-shell/",
                "/Volumes/Projects/blaze/"
            ),
            "crates/blaze-shell"
        );
        assert_eq!(relative_subpath("/tmp", "/Volumes/Projects/blaze"), "/tmp");
        assert_eq!(
            relative_subpath("/Volumes/Projects/blaze-2", "/Volumes/Projects/blaze"),
            "/Volumes/Projects/blaze-2"
        );
        assert_eq!(relative_subpath("/", "/"), ".");
        assert_eq!(relative_subpath("/tmp/work", "/"), "tmp/work");
        assert_eq!(relative_subpath("", "/Volumes/Projects/blaze"), "");
    }

    #[test]
    fn folder_processes_direct_subfolder_and_deep_descendant_inheritance() {
        let arena = Inline::<262_144>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();

        let mut engine = SysMonEngine::new_in(alloc);
        let folder = "/Volumes/Projects/blaze";

        // Hierarchy:
        // 1 (launchd, cwd="/")
        //  ├─ 100 (zsh, cwd="/Volumes/Projects/blaze") -> direct match, rel_dir="."
        //  │   └─ 101 (cargo, cwd="/Volumes/Projects/blaze/crates/blaze-shell") -> direct match, rel_dir="crates/blaze-shell"
        //  │       └─ 102 (rustc-wrapper, cwd="/var/folders/tmp") -> grandchild via_descendant=true, rel_dir="/var/folders/tmp"
        //  │           └─ 103 (rustc, cwd="/tmp") -> great-grandchild via_descendant=true, rel_dir="/tmp"
        //  ├─ 200 (other-shell, cwd="/Volumes/Projects/blaze-2") -> MUST NOT match!
        //  │   └─ 201 (other-child, cwd="/tmp") -> MUST NOT match!
        let make_snap = |ts: u64, mult: u64| {
            let mut procs = Vec::new_in(alloc);
            let mut p_launchd = make_proc_with_cwd(alloc, 1, 0, "launchd", "/", "/sbin/launchd", 0, 0, 100, 1);
            p_launchd.start_time_secs = 1_700_000_000;
            procs.push(p_launchd);
            let mut p_zsh = make_proc_with_cwd(
                alloc,
                100,
                1,
                "zsh",
                "/Volumes/Projects/blaze",
                "/bin/zsh",
                10_000_000 * mult,
                0,
                200,
                1,
            );
            p_zsh.start_time_secs = 1_700_000_100;
            procs.push(p_zsh);
            let mut p_cargo = make_proc_with_cwd(
                alloc,
                101,
                100,
                "cargo",
                "/Volumes/Projects/blaze/crates/blaze-shell",
                "/usr/bin/cargo",
                50_000_000 * mult,
                0,
                400,
                4,
            );
            p_cargo.start_time_secs = 1_700_000_350;
            procs.push(p_cargo);
            let mut p_wrapper = make_proc_with_cwd(
                alloc,
                102,
                101,
                "rustc-wrapper",
                "/var/folders/tmp",
                "/usr/bin/rustc-wrapper",
                20_000_000 * mult,
                0,
                300,
                2,
            );
            p_wrapper.start_time_secs = 1_700_000_400;
            procs.push(p_wrapper);
            let mut p_rustc = make_proc_with_cwd(
                alloc,
                103,
                102,
                "rustc",
                "/tmp",
                "/usr/bin/rustc",
                100_000_000 * mult,
                0,
                1000,
                8,
            );
            p_rustc.start_time_secs = 1_700_000_480;
            procs.push(p_rustc);
            procs.push(make_proc_with_cwd(
                alloc,
                200,
                1,
                "other-zsh",
                "/Volumes/Projects/blaze-2",
                "/bin/zsh",
                300_000_000 * mult,
                0,
                5000,
                1,
            ));
            procs.push(make_proc_with_cwd(
                alloc,
                201,
                200,
                "other-worker",
                "/tmp",
                "/usr/bin/worker",
                300_000_000 * mult,
                0,
                5000,
                1,
            ));
            RawSnapshot {
                timestamp_ns: ts,
                wall_time_secs: 1_700_000_500,
                cores: Vec::new_in(alloc),
                mem: RawMemSample::default(),
                gpu: RawGpuSample::empty_in(alloc),
                processes: procs,
                load_avg: [0.0; 3],
                uptime_secs: 10,
            }
        };

        engine.ingest(make_snap(1_000_000_000, 1));
        engine.ingest(make_snap(2_000_000_000, 2));

        // ProcessRow carries cwd and start_time_secs in both tree and flat modes.
        let zsh_row = engine.rows().iter().find(|r| r.pid == 100).unwrap();
        assert_eq!(zsh_row.cwd.as_str(), "/Volumes/Projects/blaze");
        assert_eq!(zsh_row.start_time_secs, 1_700_000_100);
        engine.toggle_tree_mode();
        let cargo_row = engine.rows().iter().find(|r| r.pid == 101).unwrap();
        assert_eq!(cargo_row.cwd.as_str(), "/Volumes/Projects/blaze/crates/blaze-shell");
        assert_eq!(cargo_row.start_time_secs, 1_700_000_350);

        let summary = engine.folder_processes_in(folder, 9999, &[], alloc);
        assert_eq!(summary.folder.as_str(), folder);
        assert_eq!(summary.entries.len(), 4);

        // Ordered hottest-first: 103 (10%), 101 (5%), 102 (2%), 100 (1%).
        assert_eq!(summary.entries[0].pid, 103);
        assert!(summary.entries[0].via_descendant);
        assert_eq!(summary.entries[0].rel_dir.as_str(), "/tmp");
        assert_eq!(summary.entries[0].cwd.as_str(), "/tmp");
        assert_eq!(summary.entries[0].uptime_secs, 20);
        assert_eq!(summary.entries[0].parent_name.as_str(), "rustc-wrapper");

        assert_eq!(summary.entries[1].pid, 101);
        assert!(!summary.entries[1].via_descendant);
        assert_eq!(summary.entries[1].rel_dir.as_str(), "crates/blaze-shell");
        assert_eq!(summary.entries[1].uptime_secs, 150);
        assert_eq!(summary.entries[1].parent_name.as_str(), "zsh");

        assert_eq!(summary.entries[2].pid, 102);
        assert!(summary.entries[2].via_descendant);
        assert_eq!(summary.entries[2].rel_dir.as_str(), "/var/folders/tmp");
        assert_eq!(summary.entries[2].uptime_secs, 100);
        assert_eq!(summary.entries[2].parent_name.as_str(), "cargo");

        assert_eq!(summary.entries[3].pid, 100);
        assert!(!summary.entries[3].via_descendant);
        assert_eq!(summary.entries[3].rel_dir.as_str(), ".");
        assert_eq!(summary.entries[3].uptime_secs, 400);
        assert_eq!(summary.entries[3].parent_name.as_str(), "launchd");

        assert_eq!(summary.primary_name.as_str(), "rustc");
        assert_eq!(summary.total_mem_bytes, 200 + 400 + 300 + 1000);
        assert!((summary.total_cpu_pct - 18.0).abs() < 1e-3);
        assert!(!summary.is_hot);
    }

    #[test]
    fn folder_processes_shell_pane_attribution_and_alt_screen_propagation() {
        let arena = Inline::<262_144>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();

        let mut engine = SysMonEngine::new_in(alloc);
        let folder = "/Volumes/Projects/blaze";
        let own_pid = 50;

        let pane1 = ShellPanePid {
            pid: 501,
            pane_id: 1001,
            block_id: 2001,
            pane_number: 1,
            is_alt_screen: true,
        };
        let pane2 = ShellPanePid {
            pid: 502,
            pane_id: 1002,
            block_id: 2002,
            pane_number: 2,
            is_alt_screen: false,
        };
        // Pane 3 belongs to a DIFFERENT folder and must not bleed into `folder`.
        let pane3 = ShellPanePid {
            pid: 503,
            pane_id: 1003,
            block_id: 2003,
            pane_number: 3,
            is_alt_screen: true,
        };

        let make_snap = |ts: u64, mult: u64| {
            let mut procs = Vec::new_in(alloc);
            // Host blaze-shell process itself (must be excluded by own_pid and stop ancestor walk).
            procs.push(make_proc_with_cwd(
                alloc,
                own_pid,
                1,
                "blaze-shell",
                folder,
                "/Applications/BlazeShell.app/Contents/MacOS/blaze-shell",
                100_000_000 * mult,
                0,
                9000,
                12,
            ));
            // Pane 1 (alt screen, e.g. claude) in folder -> child -> grandchild.
            procs.push(make_proc_with_cwd(
                alloc,
                501,
                own_pid,
                "zsh",
                folder,
                "/bin/zsh",
                10_000_000 * mult,
                0,
                100,
                1,
            ));
            procs.push(make_proc_with_cwd(
                alloc,
                511,
                501,
                "claude",
                folder,
                "/usr/local/bin/claude",
                60_000_000 * mult,
                0,
                800,
                4,
            ));
            procs.push(make_proc_with_cwd(
                alloc,
                521,
                511,
                "node-worker",
                "/tmp",
                "/usr/local/bin/node",
                40_000_000 * mult,
                0,
                500,
                2,
            ));
            // Pane 2 (normal screen) -> cargo -> rustc-wrapper -> rustc (great-grandchild).
            procs.push(make_proc_with_cwd(
                alloc,
                502,
                own_pid,
                "zsh",
                "/Volumes/Projects/blaze/crates",
                "/bin/zsh",
                10_000_000 * mult,
                0,
                100,
                1,
            ));
            procs.push(make_proc_with_cwd(
                alloc,
                512,
                502,
                "cargo",
                "/Volumes/Projects/blaze/crates",
                "/usr/bin/cargo",
                30_000_000 * mult,
                0,
                400,
                4,
            ));
            procs.push(make_proc_with_cwd(
                alloc,
                522,
                512,
                "rustc-wrapper",
                "/tmp",
                "/usr/bin/rustc-wrapper",
                20_000_000 * mult,
                0,
                300,
                2,
            ));
            procs.push(make_proc_with_cwd(
                alloc,
                532,
                522,
                "rustc",
                "/tmp",
                "/usr/bin/rustc",
                300_000_000 * mult,
                0,
                2000,
                8,
            ));
            // Pane 3 in a different folder, child of own_pid (must NOT inherit own_pid's cwd!).
            procs.push(make_proc_with_cwd(
                alloc,
                503,
                own_pid,
                "zsh",
                "/Volumes/Projects/other",
                "/bin/zsh",
                500_000_000 * mult,
                0,
                1000,
                1,
            ));
            procs.push(make_proc_with_cwd(
                alloc,
                513,
                503,
                "vim",
                "/tmp",
                "/usr/bin/vim",
                500_000_000 * mult,
                0,
                1000,
                1,
            ));
            RawSnapshot {
                timestamp_ns: ts,
                wall_time_secs: 0,
                cores: Vec::new_in(alloc),
                mem: RawMemSample::default(),
                gpu: RawGpuSample::empty_in(alloc),
                processes: procs,
                load_avg: [0.0; 3],
                uptime_secs: 10,
            }
        };

        engine.ingest(make_snap(1_000_000_000, 1));
        engine.ingest(make_snap(2_000_000_000, 2));

        let summary = engine.folder_processes_in(folder, own_pid, &[pane1, pane2, pane3], alloc);

        // own_pid (50) and Pane 3 (503, 513) must be excluded.
        assert!(summary.entries.iter().all(|e| e.pid != own_pid));
        assert!(summary.entries.iter().all(|e| e.pid != 503 && e.pid != 513));
        assert_eq!(summary.entries.len(), 7);

        // Hottest entry is great-grandchild `rustc` (30% CPU, is_hot=true, Pane 2).
        assert_eq!(summary.entries[0].pid, 532);
        assert!(summary.entries[0].is_hot);
        assert!(summary.entries[0].via_descendant);
        assert_eq!(summary.entries[0].shell_pane, Some(pane2));
        assert_eq!(summary.primary_name.as_str(), "rustc");

        // Pane 1's grandchild `node-worker` inherits Pane 1 with `is_alt_screen = true`.
        let node_worker = summary.entries.iter().find(|e| e.pid == 521).unwrap();
        assert!(node_worker.via_descendant);
        assert_eq!(node_worker.shell_pane, Some(pane1));
        assert!(node_worker.shell_pane.unwrap().is_alt_screen);

        // Pane 1 root itself also carries `Some(pane1)`.
        let pane1_root = summary.entries.iter().find(|e| e.pid == 501).unwrap();
        assert!(!pane1_root.via_descendant);
        assert_eq!(pane1_root.shell_pane, Some(pane1));
    }

    #[test]
    fn folder_processes_hot_thresholds_cumulative_hot_and_ordering() {
        let arena = Inline::<262_144>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();

        let folder = "/Volumes/Projects/blaze";

        // Case 1: Cumulative hot folder (no single process >= 25% CPU or >= 15% GPU,
        //         but 3 processes at 10% CPU each => total_cpu_pct = 30% >= 25%!).
        let mut engine = SysMonEngine::new_in(alloc);
        let make_cum_snap = |ts: u64, mult: u64| {
            let mut procs = Vec::new_in(alloc);
            procs.push(make_proc_with_cwd(alloc, 10, 1, "w1", folder, "/bin/w1", 100_000_000 * mult, 0, 100, 1));
            procs.push(make_proc_with_cwd(alloc, 11, 1, "w2", folder, "/bin/w2", 100_000_000 * mult, 0, 500, 1));
            procs.push(make_proc_with_cwd(alloc, 12, 1, "w3", folder, "/bin/w3", 100_000_000 * mult, 0, 500, 1));
            RawSnapshot {
                timestamp_ns: ts,
                wall_time_secs: 0,
                cores: Vec::new_in(alloc),
                mem: RawMemSample::default(),
                gpu: RawGpuSample::empty_in(alloc),
                processes: procs,
                load_avg: [0.0; 3],
                uptime_secs: 1,
            }
        };
        engine.ingest(make_cum_snap(1_000_000_000, 1));
        engine.ingest(make_cum_snap(2_000_000_000, 2));

        let cum = engine.folder_processes_in(folder, 999, &[], alloc);
        assert_eq!(cum.entries.len(), 3);
        assert!(cum.entries.iter().all(|e| !e.is_hot));
        assert!((cum.total_cpu_pct - 30.0).abs() < 1e-3);
        assert!(cum.is_hot);
        // Same CPU+GPU (10%): tie-break by mem desc (500 before 100), then pid asc (11 before 12).
        assert_eq!(cum.entries[0].pid, 11);
        assert_eq!(cum.entries[1].pid, 12);
        assert_eq!(cum.entries[2].pid, 10);

        // Case 2: GPU-hot process (0% CPU, 15% GPU => is_hot = true) MUST rank ahead of
        //         non-hot process with higher CPU+GPU sum (20% CPU + 0% GPU = 20% > 15%)!
        let mut engine2 = SysMonEngine::new_in(alloc);
        let make_hot_order_snap = |ts: u64, mult: u64| {
            let mut procs = Vec::new_in(alloc);
            // Warm CPU process: 20% CPU, 0% GPU -> NOT hot.
            procs.push(make_proc_with_cwd(alloc, 20, 1, "warm-cpu", folder, "/bin/a", 200_000_000 * mult, 0, 9000, 1));
            // Hot GPU process: 0% CPU, 15% GPU -> HOT (gpu >= 15.0).
            procs.push(make_proc_with_cwd(alloc, 21, 1, "gpu-shader", folder, "/bin/b", 0, 150_000_000 * mult, 100, 1));
            // Hot CPU process: 25% CPU, 0% GPU -> HOT (cpu >= 25.0).
            procs.push(make_proc_with_cwd(alloc, 22, 1, "hot-cpu", folder, "/bin/c", 250_000_000 * mult, 0, 100, 1));
            RawSnapshot {
                timestamp_ns: ts,
                wall_time_secs: 0,
                cores: Vec::new_in(alloc),
                mem: RawMemSample::default(),
                gpu: RawGpuSample::empty_in(alloc),
                processes: procs,
                load_avg: [0.0; 3],
                uptime_secs: 1,
            }
        };
        engine2.ingest(make_hot_order_snap(1_000_000_000, 1));
        engine2.ingest(make_hot_order_snap(2_000_000_000, 2));

        let ordered = engine2.folder_processes_in(folder, 999, &[], alloc);
        assert!(ordered.is_hot);
        assert_eq!(ordered.entries.len(), 3);
        // Hot entries first: hot-cpu (25%) > gpu-shader (15%), then non-hot warm-cpu (20%).
        assert_eq!(ordered.entries[0].pid, 22);
        assert!(ordered.entries[0].is_hot);
        assert_eq!(ordered.entries[1].pid, 21);
        assert!(ordered.entries[1].is_hot);
        assert_eq!(ordered.entries[2].pid, 20);
        assert!(!ordered.entries[2].is_hot);
        assert_eq!(ordered.primary_name.as_str(), "hot-cpu");

        // Empty folder returns empty summary safely.
        let empty = engine2.folder_processes_in("", 999, &[], alloc);
        assert!(empty.entries.is_empty());
        assert!(!empty.is_hot);
        assert_eq!(empty.primary_name.as_str(), "");
    }

    #[test]
    fn folder_processes_uptime_and_parent_name_edge_cases() {
        let arena = Inline::<262_144>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();

        let folder = "/Volumes/Projects/blaze";
        let mut engine = SysMonEngine::new_in(alloc);

        // Case 1: wall_time_secs > 0 with normal, zero, future start times, and missing parent.
        let mut procs = Vec::new_in(alloc);
        let mut normal = make_proc_with_cwd(alloc, 10, 9999, "orphan", folder, "/bin/orphan", 0, 0, 100, 1);
        normal.start_time_secs = 1_000;
        procs.push(normal);

        let mut zero_start = make_proc_with_cwd(alloc, 11, 10, "zero-start", folder, "/bin/z", 0, 0, 100, 1);
        zero_start.start_time_secs = 0;
        procs.push(zero_start);

        let mut future_start = make_proc_with_cwd(alloc, 12, 10, "future-start", folder, "/bin/f", 0, 0, 100, 1);
        future_start.start_time_secs = 5_000;
        procs.push(future_start);

        engine.ingest(RawSnapshot {
            timestamp_ns: 1_000_000_000,
            wall_time_secs: 1_250,
            cores: Vec::new_in(alloc),
            mem: RawMemSample::default(),
            gpu: RawGpuSample::empty_in(alloc),
            processes: procs,
            load_avg: [0.0; 3],
            uptime_secs: 1,
        });

        let summary = engine.folder_processes_in(folder, 999, &[], alloc);
        assert_eq!(summary.entries.len(), 3);

        let e10 = summary.entries.iter().find(|e| e.pid == 10).unwrap();
        assert_eq!(e10.uptime_secs, 250);
        assert_eq!(e10.parent_name.as_str(), "");

        let e11 = summary.entries.iter().find(|e| e.pid == 11).unwrap();
        assert_eq!(e11.uptime_secs, 0);
        assert_eq!(e11.parent_name.as_str(), "orphan");

        let e12 = summary.entries.iter().find(|e| e.pid == 12).unwrap();
        assert_eq!(e12.uptime_secs, 0);
        assert_eq!(e12.parent_name.as_str(), "orphan");

        // Case 2: wall_time_secs == 0 yields uptime_secs == 0 even when start_time_secs > 0.
        let mut procs2 = Vec::new_in(alloc);
        let mut p = make_proc_with_cwd(alloc, 20, 0, "solo", folder, "/bin/solo", 0, 0, 100, 1);
        p.start_time_secs = 1_000;
        procs2.push(p);

        engine.ingest(RawSnapshot {
            timestamp_ns: 2_000_000_000,
            wall_time_secs: 0,
            cores: Vec::new_in(alloc),
            mem: RawMemSample::default(),
            gpu: RawGpuSample::empty_in(alloc),
            processes: procs2,
            load_avg: [0.0; 3],
            uptime_secs: 2,
        });

        let summary2 = engine.folder_processes_in(folder, 999, &[], alloc);
        assert_eq!(summary2.entries.len(), 1);
        assert_eq!(summary2.entries[0].uptime_secs, 0);
        assert_eq!(summary2.entries[0].parent_name.as_str(), "");
    }

    #[test]
    fn tree_subtree_rollups_sum_cpu_gpu_mem_disk_and_threads() {
        let arena = Inline::<262_144>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();
        let mut engine = SysMonEngine::new_in(alloc);

        let snap = |ts_ns: u64, tick: u64| {
            let mut procs = Vec::new_in(alloc);
            let mut p1 = make_proc_with_cwd(alloc, 100, 1, "app", "/", "/Applications/App.app/Contents/MacOS/App", 100_000_000 * tick, 0, 1_000, 4);
            p1.disk_read_bytes = 500 * tick;
            let mut p2 = make_proc_with_cwd(alloc, 101, 100, "helper", "/", "/Applications/App.app/Contents/MacOS/Helper", 200_000_000 * tick, 50_000_000 * tick, 2_500, 6);
            p2.disk_write_bytes = 1_500 * tick;
            let mut p3 = make_proc_with_cwd(alloc, 102, 101, "worker", "/", "/Applications/App.app/Contents/MacOS/Worker", 50_000_000 * tick, 100_000_000 * tick, 500, 3);
            p3.disk_read_bytes = 250 * tick;
            procs.push(p1);
            procs.push(p2);
            procs.push(p3);
            RawSnapshot {
                timestamp_ns: ts_ns,
                wall_time_secs: 1000 + tick,
                cores: Vec::new_in(alloc),
                mem: RawMemSample::default(),
                gpu: RawGpuSample::empty_in(alloc),
                processes: procs,
                load_avg: [0.0; 3],
                uptime_secs: 10 + tick,
            }
        };

        engine.ingest(snap(1_000_000_000, 1));
        engine.ingest(snap(2_000_000_000, 2));

        let root = engine.rows().iter().find(|r| r.pid == 100).unwrap();
        assert!((root.subtree_cpu_pct - 35.0).abs() < 0.1, "got {}", root.subtree_cpu_pct);
        assert!((root.subtree_gpu_pct - 15.0).abs() < 0.1, "got {}", root.subtree_gpu_pct);
        assert_eq!(root.subtree_mem_bytes, 4_000);
        assert_eq!(root.subtree_disk_bps, 2_250);
        assert_eq!(root.subtree_threads, 13);

        let helper = engine.rows().iter().find(|r| r.pid == 101).unwrap();
        assert_eq!(helper.subtree_mem_bytes, 3_000);
        assert_eq!(helper.subtree_threads, 9);
        assert_eq!(helper.subtree_disk_bps, 1_750);
    }

    #[test]
    fn ancestor_mask_tracks_continuing_and_last_sibling_branches() {
        let arena = Inline::<262_144>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();
        let mut engine = SysMonEngine::new_in(alloc);
        engine.set_sort(SortColumn::Pid); // deterministic ascending PID order

        // Tree:
        // 1 (depth 0, last root)                             -> mask 0b00
        // ├─ 10 (depth 1, continuing)                        -> mask 0b00
        // │  ├─ 11 (depth 2, continuing; lvl0=1)             -> mask 0b01
        // │  │  ├─ 13 (depth 3, continuing; lvl0=1, lvl1=1)  -> mask 0b11
        // │  │  └─ 14 (depth 3, last;       lvl0=1, lvl1=1)  -> mask 0b11
        // │  └─ 12 (depth 2, last;       lvl0=1)             -> mask 0b01
        // │     └─ 15 (depth 3, last;    lvl0=1, lvl1=0)     -> mask 0b01
        // └─ 20 (depth 1, last)                              -> mask 0b00
        //    ├─ 21 (depth 2, continuing; lvl0=0)             -> mask 0b00
        //    │  └─ 23 (depth 3, last;    lvl0=0, lvl1=1)     -> mask 0b10
        //    └─ 22 (depth 2, last;       lvl0=0)             -> mask 0b00
        //       └─ 24 (depth 3, last;    lvl0=0, lvl1=0)     -> mask 0b00
        let mut procs = Vec::new_in(alloc);
        procs.push(make_proc(alloc, 1, 0, "root", "/sbin/launchd", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 10, 1, "a", "/bin/a", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 11, 10, "a1", "/bin/a1", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 13, 11, "a1a", "/bin/a1a", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 14, 11, "a1b", "/bin/a1b", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 12, 10, "a2", "/bin/a2", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 15, 12, "a2a", "/bin/a2a", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 20, 1, "b", "/bin/b", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 21, 20, "b1", "/bin/b1", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 23, 21, "b1a", "/bin/b1a", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 22, 20, "b2", "/bin/b2", 0, 0, 10, 0, 0, 1));
        procs.push(make_proc(alloc, 24, 22, "b2a", "/bin/b2a", 0, 0, 10, 0, 0, 1));

        engine.ingest(RawSnapshot {
            timestamp_ns: 1_000_000_000,
            wall_time_secs: 0,
            cores: Vec::new_in(alloc),
            mem: RawMemSample::default(),
            gpu: RawGpuSample::empty_in(alloc),
            processes: procs,
            load_avg: [0.0; 3],
            uptime_secs: 1,
        });

        let expected: &[(u32, u16, bool, u64, &str)] = &[
            (1, 0, true, 0b00, ""),
            (10, 1, false, 0b00, "├─ "),
            (11, 2, false, 0b01, "│  ├─ "),
            (13, 3, false, 0b11, "│  │  ├─ "),
            (14, 3, true, 0b11, "│  │  └─ "),
            (12, 2, true, 0b01, "│  └─ "),
            (15, 3, true, 0b01, "│     └─ "),
            (20, 1, true, 0b00, "└─ "),
            (21, 2, false, 0b00, "   ├─ "),
            (23, 3, true, 0b10, "   │  └─ "),
            (22, 2, true, 0b00, "   └─ "),
            (24, 3, true, 0b00, "      └─ "),
        ];
        assert_eq!(engine.rows().len(), expected.len());
        for (row, &(pid, depth, is_last, mask, prefix)) in engine.rows().iter().zip(expected.iter()) {
            assert_eq!(row.pid, pid);
            assert_eq!(row.depth, depth, "pid {pid}");
            assert_eq!(row.is_last_sibling, is_last, "pid {pid}");
            assert_eq!(row.ancestor_mask, mask, "pid {pid}");
            assert_eq!(row.tree_prefix.as_str(), prefix, "pid {pid}");
        }

        // Flat mode always zeroes `ancestor_mask`.
        engine.toggle_tree_mode();
        assert!(engine.rows().iter().all(|r| r.ancestor_mask == 0));
    }

    #[test]
    fn folder_processes_origin_walks_through_login_to_host_app() {
        // 1. Unit checks for `app_display_name`.
        assert_eq!(app_display_name("/Applications/Ghostty.app"), "Ghostty");
        assert_eq!(app_display_name("/Applications/Ghostty.app/"), "Ghostty");
        assert_eq!(app_display_name("/Applications/Visual Studio Code.APP///"), "Visual Studio Code");
        assert_eq!(app_display_name("/System/Applications/Utilities/Terminal.app"), "Terminal");
        assert_eq!(app_display_name("Ghostty.app"), "Ghostty");
        assert_eq!(app_display_name("/usr/bin/zsh"), "zsh");
        assert_eq!(app_display_name(""), "");
        assert_eq!(app_display_name("/"), "");

        let arena = Inline::<262_144>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();
        let mut engine = SysMonEngine::new_in(alloc);
        let folder = "/Volumes/Projects/blaze";
        let own_pid = 900;

        let pane1 = ShellPanePid {
            pid: 901,
            pane_id: 1,
            block_id: 10,
            pane_number: 1,
            is_alt_screen: false,
        };

        let mut procs = Vec::new_in(alloc);
        // launchd
        procs.push(make_proc_with_cwd(alloc, 1, 0, "launchd", "/", "/sbin/launchd", 0, 0, 10, 1));

        // Case A: Finder.app -> Ghostty.app (ghostty) -> login -> nu (cwd = folder)
        // Must pick nearest `.app` (Ghostty, not Finder) and skip `login`.
        procs.push(make_proc_with_cwd(
            alloc,
            5,
            1,
            "Finder",
            "/",
            "/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder",
            0,
            0,
            100,
            1,
        ));
        procs.push(make_proc_with_cwd(
            alloc,
            10,
            5,
            "ghostty",
            "/",
            "/Applications/Ghostty.app/Contents/MacOS/ghostty",
            0,
            0,
            200,
            4,
        ));
        procs.push(make_proc_with_cwd(alloc, 11, 10, "login", "/", "/usr/bin/login", 0, 0, 50, 1));
        procs.push(make_proc_with_cwd(alloc, 12, 11, "nu", folder, "/opt/homebrew/bin/nu", 0, 0, 300, 2));

        // Case B: Antigravity.app -> language_server -> zsh -> cargo (cwd = folder)
        procs.push(make_proc_with_cwd(
            alloc,
            20,
            1,
            "Antigravity",
            "/",
            "/Applications/Antigravity.app/Contents/MacOS/Antigravity",
            0,
            0,
            500,
            8,
        ));
        procs.push(make_proc_with_cwd(
            alloc,
            21,
            20,
            "language_server",
            "/",
            "/Applications/Antigravity.app/Contents/Resources/app/bin/language_server",
            0,
            0,
            400,
            6,
        ));
        procs.push(make_proc_with_cwd(alloc, 22, 21, "zsh", folder, "/bin/zsh", 0, 0, 100, 1));
        procs.push(make_proc_with_cwd(alloc, 23, 22, "cargo", folder, "/usr/bin/cargo", 0, 0, 800, 4));

        // Case C: tmux -> zsh -> rg (no `.app` ancestor)
        procs.push(make_proc_with_cwd(alloc, 30, 1, "tmux", "/", "/opt/homebrew/bin/tmux", 0, 0, 80, 1));
        procs.push(make_proc_with_cwd(alloc, 31, 30, "zsh", "/tmp", "/bin/zsh", 0, 0, 60, 1));
        procs.push(make_proc_with_cwd(alloc, 32, 31, "rg", folder, "/opt/homebrew/bin/rg", 0, 0, 90, 2));

        // Case D: blaze-shell (own_pid, shell_pane = Some(pane1)) -> zsh -> claude
        procs.push(make_proc_with_cwd(
            alloc,
            own_pid,
            10,
            "blaze-shell",
            folder,
            "/Applications/BlazeShell.app/Contents/MacOS/blaze-shell",
            0,
            0,
            600,
            4,
        ));
        procs.push(make_proc_with_cwd(alloc, 901, own_pid, "zsh", folder, "/bin/zsh", 0, 0, 70, 1));
        procs.push(make_proc_with_cwd(alloc, 902, 901, "claude", folder, "/usr/local/bin/claude", 0, 0, 700, 4));

        engine.ingest(RawSnapshot {
            timestamp_ns: 1_000_000_000,
            wall_time_secs: 1_700_000_000,
            cores: Vec::new_in(alloc),
            mem: RawMemSample::default(),
            gpu: RawGpuSample::empty_in(alloc),
            processes: procs,
            load_avg: [0.0; 3],
            uptime_secs: 100,
        });

        let summary = engine.folder_processes_in(folder, own_pid, &[pane1], alloc);

        let nu = summary.entries.iter().find(|e| e.pid == 12).unwrap();
        assert_eq!(nu.origin_app_name.as_str(), "Ghostty");
        assert_eq!(
            nu.origin_app_bundle_path.as_ref().map(|s| s.as_str()),
            Some("/Applications/Ghostty.app")
        );
        assert_eq!(nu.origin_chain.as_str(), "Ghostty");

        let cargo = summary.entries.iter().find(|e| e.pid == 23).unwrap();
        assert_eq!(cargo.origin_app_name.as_str(), "Antigravity");
        assert_eq!(
            cargo.origin_app_bundle_path.as_ref().map(|s| s.as_str()),
            Some("/Applications/Antigravity.app")
        );
        assert_eq!(cargo.origin_chain.as_str(), "Antigravity › language_server › zsh");

        let rg = summary.entries.iter().find(|e| e.pid == 32).unwrap();
        assert_eq!(rg.origin_app_name.as_str(), "zsh");
        assert_eq!(rg.origin_app_bundle_path, None);
        assert_eq!(rg.origin_chain.as_str(), "tmux › zsh");

        let claude = summary.entries.iter().find(|e| e.pid == 902).unwrap();
        assert_eq!(claude.shell_pane, Some(pane1));
        assert_eq!(claude.origin_app_name.as_str(), "blaze-shell");
        assert_eq!(
            claude.origin_app_bundle_path.as_ref().map(|s| s.as_str()),
            Some("/Applications/BlazeShell.app")
        );
        assert_eq!(claude.origin_chain.as_str(), "blaze-shell › zsh");
    }
}
