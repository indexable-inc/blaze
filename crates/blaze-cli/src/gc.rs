//! `blaze gc [--max-gb <N> | --max-bytes <N>] [--dry-run] [--all-formats] [--background]`:
//! bounded disk-space maintenance for the global plain store under `<HOME>/.blaze/v2/store`.
//!
//! - Compacts/sweeps segments and runs root-level LRU eviction (down to the 75% low watermark
//!   whenever committed and live bytes exceed the 85% high watermark of the budget, default
//!   12 GiB; configurable via `BLAZE_STORE_MAX_BYTES`, `BLAZE_STORE_MAX_GB`, or `--max-gb`).
//! - Removes stale old-format store directories (`f5s2`, legacy format-4 files directly in
//!   `v2/store/`) untouched for longer than `forms::EXPIRY_DAYS` (or all other-format stores
//!   immediately with `--all-formats`) cleanly via `bstd::fs`, never touching the active
//!   `STORE_DIR` (`f5s3`) or `CACHEDIR.TAG`.
//! - When spawned detached by `after_answer_printed` (`--background`), lowers thread QoS to
//!   `QOS_CLASS_BACKGROUND` (macOS) / nice 19 + idle IO (Linux) and acquires a non-blocking
//!   exclusive advisory lock (`v2/store/gc.lock`) so at most one background GC runs at a time
//!   and foreground CLI exit is never delayed.

use core::fmt;

use blaze_cas::plain::{DEFAULT_MAX_STORE_BYTES, ExclusiveLock, Liveness, MIN_MAX_STORE_BYTES, NoPidCheck, OsPidCheck, PidCheck, Settings, Sliced};
use blaze_core::FileName;
use blaze_keyed::store::StoreError;
use bstd::args::Args;
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::entry::Status;
use bstd::env::{Env, VarName};
use bstd::fs::{Dir, Fs, ReadWrite};
use bstd::host::Host;
use bstd::out::Out;
use bstd::path::Path;
use bstd::rng::Rng;

use crate::CliError;
use crate::clean::{SweepPlant, remove_other};
use crate::store::plain_place::{Facts, PlaceError, STORE_DIR, STORE_ROOT, StoreAlloc, forms};
use crate::store::{Home, StoreFailure, TRUSTED_BUILDER, boot_fact, nonce};

/// Environment variable configuring the store byte budget directly in bytes (`0` = unlimited).
pub(crate) const ENV_MAX_BYTES: &str = "BLAZE_STORE_MAX_BYTES";

/// Environment variable configuring the store byte budget in GiB (`0` = unlimited).
pub(crate) const ENV_MAX_GB: &str = "BLAZE_STORE_MAX_GB";

/// Lock file beneath `v2/store/` ensuring at most one GC process runs at a time.
const GC_LOCK_LEAF: &str = "gc.lock";

/// Maximum compaction/sweep slices per `blaze gc` invocation (rule 18: bounded progress).
const GC_SLICES_MOST: u32 = 1024;
/// Maximum manifest-refresh retries when a compaction slice aborts due to a concurrent
/// foreground commit touching a candidate segment or renewing a root across worktrees (rule 18).
const GC_CONFLICT_RETRIES_MOST: u32 = 8;

/// Arguments passed to the detached background GC helper.
#[cfg(unix)]
const BG_GC_ARGS: [&str; 2] = ["gc", "--background"];

/// Usage text for `blaze gc`.
const USAGE: &str = "`blaze gc` takes [--max-gb <N>] [--max-bytes <N>] [--dry-run] [--all-formats] [--background]";

/// Why `blaze gc` or budget configuration refused (rule 19).
#[derive(Debug)]
pub enum GcError {
    /// Invalid budget in an environment variable or CLI flag.
    InvalidBudget { source: &'static str, why: &'static str },
    /// Failed to acquire the exclusive GC lock file.
    Lock(StoreError),
    /// Failed to lower thread QoS to background class on macOS.
    #[cfg(target_os = "macos")]
    Qos(blaze_core::threads::qos::QosError),
    /// Failed to lower thread CPU/IO priority for background GC on Linux.
    #[cfg(target_os = "linux")]
    Priority { call: &'static str, errno: blaze_sys::Errno },
}

impl fmt::Display for GcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GcError::InvalidBudget { source, why } => write!(f, "store budget {source}: {why}"),
            GcError::Lock(error) => write!(f, "blaze gc: lock under .blaze/{STORE_ROOT}/{GC_LOCK_LEAF}: {error}"),
            #[cfg(target_os = "macos")]
            GcError::Qos(error) => write!(f, "blaze gc --background: {error}"),
            #[cfg(target_os = "linux")]
            GcError::Priority { call, errno } => write!(f, "blaze gc --background: {call} failed with {errno}"),
        }
    }
}

/// Parsed options for `blaze gc`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct GcOpts {
    /// Explicit budget override in bytes (`Some(0)` = unlimited; `None` = env or default).
    pub(crate) max_store_bytes: Option<u64>,
    pub(crate) dry_run: bool,
    pub(crate) all_formats: bool,
    pub(crate) background: bool,
}

/// Summary of one `blaze gc` execution (used for output and tests).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct GcSummary {
    /// True if another GC process already held `gc.lock` and this run exited early.
    pub(crate) skipped_locked: bool,
    pub(crate) bytes_before: u64,
    pub(crate) bytes_after: u64,
    pub(crate) cap_bytes: u64,
    pub(crate) slices: u32,
    pub(crate) evicted_roots: u32,
    pub(crate) files_unlinked: u32,
    pub(crate) other_forms_removed: u32,
    pub(crate) other_forms_bytes: u64,
    pub(crate) aux_files_removed: u32,
    pub(crate) aux_bytes_freed: u64,
}

/// Parses `BLAZE_STORE_MAX_BYTES` and `BLAZE_STORE_MAX_GB` values cleanly at the CLI edge.
/// - `0` means unlimited (`Some(0)` -> `Settings::cap_bytes()` yields `u64::MAX`).
/// - Non-zero values are clamped to at least [`MIN_MAX_STORE_BYTES`] (1 GiB).
/// - `BLAZE_STORE_MAX_BYTES` takes precedence over `BLAZE_STORE_MAX_GB`.
/// - Unset/empty yields `Ok(None)` (uses [`DEFAULT_MAX_STORE_BYTES`] = 12 GiB).
pub(crate) fn parse_budget_values(max_bytes: Option<&str>, max_gb: Option<&str>) -> Result<Option<u64>, GcError> {
    if let Some(raw) = max_bytes.map(str::trim).filter(|s| !s.is_empty()) {
        let parsed = raw
            .parse::<u64>()
            .map_err(|_| GcError::InvalidBudget { source: ENV_MAX_BYTES, why: "must be a non-negative integer number of bytes (0 = unlimited)" })?;
        return Ok(Some(if parsed == 0 { 0 } else { parsed.max(MIN_MAX_STORE_BYTES) }));
    }
    if let Some(raw) = max_gb.map(str::trim).filter(|s| !s.is_empty()) {
        let gb = raw
            .parse::<u64>()
            .map_err(|_| GcError::InvalidBudget { source: ENV_MAX_GB, why: "must be a non-negative integer number of GiB (0 = unlimited)" })?;
        return Ok(Some(if gb == 0 { 0 } else { gb.saturating_mul(1 << 30).max(MIN_MAX_STORE_BYTES) }));
    }
    Ok(None)
}

/// Reads the store byte budget from `bstd::env::Env` at the CLI edge (rule 75).
pub(crate) fn budget_from_env(env: Option<&Env>) -> Result<Option<u64>, CliError> {
    let Some(env) = env else { return Ok(None) };
    let read_var = |name: &'static str| -> Option<&str> {
        let key = VarName::new(name).ok()?;
        env.get(&key).ok().flatten()
    };
    parse_budget_values(read_var(ENV_MAX_BYTES), read_var(ENV_MAX_GB)).map_err(CliError::Gc)
}

/// Parses CLI arguments after `blaze gc`.
pub(crate) fn parse_args<'a>(mut words: impl Iterator<Item = &'a str>) -> Result<GcOpts, CliError> {
    let mut opts = GcOpts::default();
    while let Some(word) = words.next() {
        match word {
            "--dry-run" => opts.dry_run = true,
            "--all-formats" => opts.all_formats = true,
            "--background" => opts.background = true,
            "--max-gb" => {
                let raw = words.next().ok_or(CliError::Usage(USAGE))?;
                opts.max_store_bytes = Some(parse_max_gb_flag(raw)?);
            }
            "--max-bytes" => {
                let raw = words.next().ok_or(CliError::Usage(USAGE))?;
                opts.max_store_bytes = Some(parse_max_bytes_flag(raw)?);
            }
            other => {
                if let Some(rest) = other.strip_prefix("--max-gb=") {
                    opts.max_store_bytes = Some(parse_max_gb_flag(rest)?);
                } else if let Some(rest) = other.strip_prefix("--max-bytes=") {
                    opts.max_store_bytes = Some(parse_max_bytes_flag(rest)?);
                } else {
                    return Err(CliError::Usage(USAGE));
                }
            }
        }
    }
    Ok(opts)
}

fn parse_max_gb_flag(raw: &str) -> Result<u64, CliError> {
    let gb = raw
        .trim()
        .parse::<u64>()
        .map_err(|_| CliError::Gc(GcError::InvalidBudget { source: "--max-gb", why: "must be a non-negative integer number of GiB (0 = unlimited)" }))?;
    Ok(if gb == 0 { 0 } else { gb.saturating_mul(1 << 30).max(MIN_MAX_STORE_BYTES) })
}

fn parse_max_bytes_flag(raw: &str) -> Result<u64, CliError> {
    raw.trim()
        .parse::<u64>()
        .map_err(|_| CliError::Gc(GcError::InvalidBudget { source: "--max-bytes", why: "must be a non-negative integer number of bytes (0 = unlimited)" }))
}

/// Detached background GC spawner carried by [`Home`] so `after_answer_printed` can spawn
/// `blaze gc --background` in a new session (`POSIX_SPAWN_SETSID` / `setsid`) without blocking
/// CLI exit.
pub(crate) enum BgGc {
    None,
    #[cfg(unix)]
    Detached {
        spawner: bstd::procs::Spawner,
        exe: blaze_own::String<StoreAlloc>,
        home: blaze_own::String<StoreAlloc>,
        max_store_bytes: Option<u64>,
    },
}

impl BgGc {
    /// Captures the detached spawner when running on a real native Unix host with a known
    /// executable and `HOME`; returns [`BgGc::None`] in tests or simulated worlds.
    #[cfg(unix)]
    pub(crate) fn detached(cx: &Cx, host: &Host, home: Option<&str>, max_store_bytes: Option<u64>, alloc: StoreAlloc) -> BgGc {
        if cfg!(test) || cx.is_simulated() {
            return BgGc::None;
        }
        let (Ok(spawner), Ok(exe_path), Some(home_path)) = (host.spawner(), host.exe(), home) else {
            return BgGc::None;
        };
        let mut exe = blaze_own::String::new_in(alloc);
        exe.push_str(&exe_path);
        let mut home_str = blaze_own::String::new_in(alloc);
        home_str.push_str(home_path);
        BgGc::Detached { spawner, exe, home: home_str, max_store_bytes }
    }

    #[cfg(not(unix))]
    pub(crate) fn detached(_cx: &Cx, _host: &Host, _home: Option<&str>, _max_store_bytes: Option<u64>, _alloc: StoreAlloc) -> BgGc {
        BgGc::None
    }

    /// Whether this handle can spawn a detached background GC helper.
    pub(crate) fn is_detached(&self) -> bool {
        !matches!(self, BgGc::None)
    }

    /// Spawns detached `blaze gc --background` in a new session with `/dev/null` stdio when no
    /// other GC process currently holds `v2/store/gc.lock`. Returns `true` if a child was spawned,
    /// `false` if skipped (non-detached handle or GC already active). Never waits for the child.
    pub(crate) fn spawn(&self, cx: &Cx, blaze_dir: &Dir<ReadWrite>, err: &Out) -> bool {
        match self {
            BgGc::None => false,
            #[cfg(unix)]
            BgGc::Detached { spawner, exe, home, max_store_bytes } => {
                // Fast non-blocking probe of `gc.lock`: skip spawning a duplicate `blaze gc`
                // process on every rapid edit while an existing background GC is already running.
                match gc_lock_is_free(cx, blaze_dir) {
                    Ok(true) => {}
                    Ok(false) => return false,
                    Err(error) => {
                        let _ = err.line(cx, format_args!("blaze: warning: failed to probe background gc lock: {error}"));
                        return false;
                    }
                }
                let frame = cx.frame();
                let budget_text = max_store_bytes.map(|b| bstd::mem::format_in!(frame, "{b}"));
                let two_vars = budget_text.as_ref().map(|val| [("HOME", home.as_str()), (ENV_MAX_BYTES, val.as_str())]);
                let one_var = [("HOME", home.as_str())];
                let env: &[(&str, &str)] = match &two_vars {
                    Some(pair) => pair.as_slice(),
                    None => one_var.as_slice(),
                };
                let spec = bstd::procs::DetachedSpec { program: exe.as_str(), args: &BG_GC_ARGS, env, dir: home.as_str() };
                if let Err(error) = spawner.start_detached(cx, &spec) {
                    let _ = err.line(cx, format_args!("blaze: warning: failed to start detached background gc: {error}"));
                    return false;
                }
                true
            }
        }
    }
}

/// Non-blocking check of whether `v2/store/gc.lock` is currently unheld by another GC process.
/// Immediately releases the probe lock if free so a newly spawned `blaze gc --background` can
/// acquire it; returns `Ok(false)` when contended (another GC is already running), or `Err` on
/// lock-path/I/O failure.
pub(crate) fn gc_lock_is_free(cx: &Cx, blaze_dir: &Dir<ReadWrite>) -> Result<bool, CliError> {
    Ok(try_acquire_gc_lock(cx, blaze_dir)?.is_some())
}

/// Acquires the non-blocking exclusive GC lock at `<HOME>/.blaze/v2/store/gc.lock`.
/// Returns `Ok(None)` when another GC process already holds the lock.
pub(crate) fn try_acquire_gc_lock(cx: &Cx, blaze_dir: &Dir<ReadWrite>) -> Result<Option<ExclusiveLock<bstd::io::LentDriver>>, CliError> {
    let store_root = Path::new(STORE_ROOT)?;
    let driver = blaze_dir.lend(&store_root);
    let frame = cx.frame();
    let lock_path = bstd::mem::format_in!(frame, "{}/{GC_LOCK_LEAF}", driver.dir().as_str());
    let file = FileName::new(lock_path.as_str()).ok_or(CliError::Text { what: "gc lock path is not a valid relative file name" })?;
    ExclusiveLock::try_acquire(driver, file).map_err(|error| CliError::Gc(GcError::Lock(error)))
}

/// Top-level entry for `blaze gc`.
#[expect(clippy::too_many_arguments, reason = "gc command edge: each argument one specific capability")]
pub(crate) fn run(
    cx: &Cx,
    args: &Args,
    fs: &Fs,
    env: Option<&Env>,
    host: &Host,
    clock: &Clock,
    rng: &mut Rng,
    out: &Out,
    err: &Out,
    heap: &'static blaze_own::Heap,
) -> Result<Status, CliError> {
    let mut opts = parse_args(args.each().skip(2))?;
    if opts.max_store_bytes.is_none() {
        opts.max_store_bytes = budget_from_env(env)?;
    }
    let home_str = env.and_then(|env| env.get(&VarName::new("HOME").ok()?).ok().flatten());
    let _summary = run_with(cx, fs, host, clock, rng, home_str, opts, out, err, heap)?;
    Ok(Status::OK)
}

/// Core `blaze gc` execution, parameterized by `home_str` and `opts` for direct testing on both
/// native and simulated worlds.
#[expect(clippy::too_many_arguments, reason = "gc runner edge: each argument one specific capability")]
pub(crate) fn run_with(
    cx: &Cx,
    fs: &Fs,
    host: &Host,
    clock: &Clock,
    rng: &mut Rng,
    home_str: Option<&str>,
    opts: GcOpts,
    out: &Out,
    err: &Out,
    heap: &'static blaze_own::Heap,
) -> Result<GcSummary, CliError> {
    #[cfg(target_os = "macos")]
    let _qos = if opts.background {
        Some(blaze_core::threads::qos::Lowered::at_most(blaze_core::threads::qos::Qos::Background).map_err(|e| CliError::Gc(GcError::Qos(e)))?)
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    if opts.background && !cx.is_simulated() {
        // Lower CPU priority to nice 19 and I/O priority to idle class via raw Linux syscalls in
        // `blaze-sys` (zero libc imports) so detached `blaze gc --background` on 128-core servers
        // never steals CPU or disk bandwidth from foreground compiles.
        blaze_sys::linux::task::setpriority(blaze_sys::linux::task::PriorityOf::Process, 0, 19)
            .map_err(|errno| CliError::Gc(GcError::Priority { call: "setpriority(PRIO_PROCESS, 0, 19)", errno }))?;
        blaze_sys::linux::task::ioprio_set(blaze_sys::linux::task::IoPriority::IDLE)
            .map_err(|errno| CliError::Gc(GcError::Priority { call: "ioprio_set(IOPRIO_CLASS_IDLE)", errno }))?;
    }

    let home = Home::of(cx, fs, home_str)?;
    let settings = Settings {
        max_store_bytes: opts.max_store_bytes,
        trusted_writer: TRUSTED_BUILDER,
        ..Settings::default()
    };
    let cap_bytes = settings.cap_bytes();
    let now_ns = clock.wall().as_ns();

    // Acquire non-blocking exclusive GC lock unless --dry-run.
    let _lock = if !opts.dry_run {
        match try_acquire_gc_lock(cx, home.dir())? {
            Some(lock) => Some(lock),
            None => {
                if !opts.background {
                    out.line(cx, format_args!("blaze gc: another gc process is already running"))?;
                }
                return Ok(GcSummary { skipped_locked: true, cap_bytes, ..GcSummary::default() });
            }
        }
    } else {
        None
    };

    let store_path = Path::new(STORE_DIR)?;
    let store_exists = home.dir().stat(cx, &store_path).map_err(|error| CliError::Store(StoreFailure::Look { folder: STORE_DIR, error }))?.is_some();

    let boot = host.boot().ok().map(|b| boot_fact(&b));
    let facts = Facts { boot, nonce: nonce(rng) };

    if opts.dry_run {
        return run_dry(cx, &home, &settings, facts, store_exists, now_ns, opts.all_formats, out, heap);
    }

    let mut summary = GcSummary { cap_bytes, ..GcSummary::default() };

    if store_exists {
        let mut remaining_slices = GC_SLICES_MOST;
        let mut totals = Sliced::default();
        // Open the store ONCE: on a compaction conflict (`slice.aborted`), `Plain` refreshes its
        // manifest in-place and maps only delta segments from the concurrent writer so all
        // existing segment `mmap`s and `ReachCache` survive across conflict retries.
        let lent = home.open_store_with_liveness(&settings, facts, Liveness::Reached, heap.alloc())?;
        if !cx.is_simulated() {
            lent.set_pid(host.pid());
        }
        let pid_check: &dyn PidCheck = if cx.is_simulated() { &NoPidCheck } else { &OsPidCheck };
        let _reclaimed = lent.reclaim_dead_leases(pid_check);
        summary.bytes_before = lent.total_bytes();
        for attempt in 0..=GC_CONFLICT_RETRIES_MOST {
            let ran = if opts.background {
                lent.compact(remaining_slices)
            } else {
                lent.compact_foreground(remaining_slices)
            }
            .map_err(|error| CliError::Store(StoreFailure::Place(PlaceError::Lent(error))))?;
            let mut aborted = false;
            for slice in &ran {
                if slice.aborted {
                    aborted = true;
                } else {
                    totals = totals.and(*slice);
                    if slice.segments > 0 || slice.evicted_roots > 0 || slice.removed > 0 || slice.orphans > 0 {
                        summary.slices += 1;
                        remaining_slices = remaining_slices.saturating_sub(1);
                    }
                }
            }
            if aborted && remaining_slices > 0 && attempt < GC_CONFLICT_RETRIES_MOST {
                // `Plain` already adopted the newer manifest and mapped delta segments in-place;
                // re-probe any newly discovered same-boot leases and retry compaction directly.
                let _ = lent.reclaim_dead_leases(pid_check);
                continue;
            }
            break;
        }
        // Ensure any older retired files whose `GRACE_NS` elapsed before or during compaction
        // are unlinked via a zero-conflict metadata-only slice before exiting.
        if lent
            .plan_gc()
            .map_err(|error| CliError::Store(StoreFailure::Place(PlaceError::Lent(error))))?
            .retired_ready
            > 0
        {
            let cleanup = if opts.background {
                lent.compact(1)
            } else {
                lent.compact_foreground(1)
            }
            .map_err(|error| CliError::Store(StoreFailure::Place(PlaceError::Lent(error))))?;
            for slice in &cleanup {
                if !slice.aborted {
                    totals = totals.and(*slice);
                    if slice.segments > 0 || slice.evicted_roots > 0 || slice.removed > 0 || slice.orphans > 0 {
                        summary.slices += 1;
                    }
                }
            }
        }
        // Seal unsealed local segments so post-reboot opens verify them in O(1) without full
        // BLAKE3 re-hashing. Up to 2 passes: pass 1 stamps any earlier-boot segments checked
        // during `map_all` onto the current boot; pass 2 barrier-seals them.
        for _ in 0..2u8 {
            let sealed = if opts.background {
                lent.seal()
            } else {
                lent.seal_foreground()
            }
            .map_err(|error| CliError::Store(StoreFailure::Place(PlaceError::Lent(error))))?;
            if !sealed || !lent.needs_seal() {
                break;
            }
        }
        summary.bytes_after = lent.total_bytes();
        drop(lent);
        summary.evicted_roots = totals.evicted_roots;
        summary.files_unlinked = totals.removed;
    }

    // Clean stale old-format store directories (e.g. f5s2, legacy format <=4 files in v2/store/).
    let root = Path::new(STORE_ROOT)?;
    let found = forms::list(cx, home.dir(), &root)?;
    for form in found.iter() {
        if !opts.all_formats && !forms::expired(form, now_ns) {
            continue;
        }
        let gone = remove_other(cx, home.dir(), &root, form, SweepPlant::None).map_err(CliError::Clean)?;
        summary.other_forms_removed += 1;
        summary.other_forms_bytes = summary.other_forms_bytes.saturating_add(gone.bytes);
        if !opts.background {
            out.line(
                cx,
                format_args!(
                    "removed old-format store ~/.blaze/{}{}: {} files, {} freed",
                    gone.path,
                    if matches!(form.kind, forms::FormKind::Legacy) { " (format <=4)" } else { "" },
                    gone.files,
                    blaze_report::ui::fmt_bytes(gone.bytes),
                ),
            )?;
        }
    }

    // Sweep dead-PID telemetry rings / build summaries (`v2/runs/`), orphaned test temp dirs
    // (`v2/test/tmp/`), and expired legacy run record files (`v2/run/records/`).
    let probe = OsPidProbe;
    let aux = sweep_aux_with(cx, home.dir(), now_ns, &probe, false, AuxSweepPlant::None);
    summary.aux_files_removed = aux.files;
    summary.aux_bytes_freed = aux.bytes;

    if !opts.background {
        let active_freed = summary.bytes_before.saturating_sub(summary.bytes_after);
        let budget_label = format_budget(cx, summary.cap_bytes);
        out.line(
            cx,
            format_args!(
                "blaze gc: store ~/.blaze/{STORE_DIR}: {} -> {} ({} freed; budget {}); {} slices, {} roots evicted, {} files unlinked",
                blaze_report::ui::fmt_bytes(summary.bytes_before),
                blaze_report::ui::fmt_bytes(summary.bytes_after),
                blaze_report::ui::fmt_bytes(active_freed),
                budget_label.as_str(),
                summary.slices,
                summary.evicted_roots,
                summary.files_unlinked,
            ),
        )?;
        if summary.other_forms_removed > 0 {
            out.line(
                cx,
                format_args!(
                    "blaze gc: removed {} old-format store(s), {} freed",
                    summary.other_forms_removed,
                    blaze_report::ui::fmt_bytes(summary.other_forms_bytes),
                ),
            )?;
        }
        if summary.aux_files_removed > 0 {
            out.line(
                cx,
                format_args!(
                    "blaze gc: swept {} stale run/temp file(s), {} freed",
                    summary.aux_files_removed,
                    blaze_report::ui::fmt_bytes(summary.aux_bytes_freed),
                ),
            )?;
        }
    }
    let _ = err;
    Ok(summary)
}

#[expect(clippy::too_many_arguments, reason = "dry-run gc helper: each argument one capability or option")]
fn run_dry(
    cx: &Cx,
    home: &Home,
    settings: &Settings,
    facts: Facts,
    store_exists: bool,
    now_ns: u64,
    all_formats: bool,
    out: &Out,
    heap: &'static blaze_own::Heap,
) -> Result<GcSummary, CliError> {
    let cap_bytes = settings.cap_bytes();
    let budget_label = format_budget(cx, cap_bytes);
    let mut summary = GcSummary { cap_bytes, ..GcSummary::default() };

    if store_exists {
        let lent = home.open_store_with_liveness(settings, facts, Liveness::Reached, heap.alloc())?;
        let pid_check: &dyn PidCheck = if cx.is_simulated() { &NoPidCheck } else { &OsPidCheck };
        let _reclaimed = lent.reclaim_dead_leases(pid_check);
        let plan = lent.plan_gc().map_err(|error| CliError::Store(StoreFailure::Place(PlaceError::Lent(error))))?;
        summary.bytes_before = plan.total_bytes;
        summary.bytes_after = plan.total_bytes.saturating_sub(plan.reclaimable_bytes);
        summary.evicted_roots = plan.lru_evict_roots;
        summary.files_unlinked = plan.retired_ready;
        out.line(
            cx,
            format_args!(
                "blaze gc (dry-run): store ~/.blaze/{STORE_DIR}: {} total, {} live, ~{} reclaimable (budget {}, watermarks {}/{})",
                blaze_report::ui::fmt_bytes(plan.total_bytes),
                blaze_report::ui::fmt_bytes(plan.live_bytes),
                blaze_report::ui::fmt_bytes(plan.reclaimable_bytes),
                budget_label.as_str(),
                format_budget(cx, plan.low_watermark_bytes).as_str(),
                format_budget(cx, plan.high_watermark_bytes).as_str(),
            ),
        )?;
        out.line(
            cx,
            format_args!(
                "blaze gc (dry-run): {} segments, {} roots ({} expired, {} to LRU-evict), {} retired files ready to unlink",
                plan.segments, plan.total_roots, plan.expired_roots, plan.lru_evict_roots, plan.retired_ready,
            ),
        )?;
    } else {
        out.line(
            cx,
            format_args!("blaze gc (dry-run): store ~/.blaze/{STORE_DIR}: absent (budget {})", budget_label.as_str()),
        )?;
    }

    let root = Path::new(STORE_ROOT)?;
    let found = forms::list(cx, home.dir(), &root)?;
    for form in found.iter() {
        let would_remove = all_formats || forms::expired(form, now_ns);
        if would_remove {
            summary.other_forms_removed += 1;
            summary.other_forms_bytes = summary.other_forms_bytes.saturating_add(form.bytes);
        }
        let action = if would_remove { "would remove" } else { "kept within grace period" };
        match form.kind {
            forms::FormKind::Legacy => out.line(
                cx,
                format_args!(
                    "blaze gc (dry-run): {action} ~/.blaze/{STORE_ROOT} (format <=4 · {} files · {})",
                    form.files,
                    blaze_report::ui::fmt_bytes(form.bytes),
                ),
            )?,
            forms::FormKind::Folder(name) => out.line(
                cx,
                format_args!(
                    "blaze gc (dry-run): {action} ~/.blaze/{STORE_ROOT}/{} ({} files · {})",
                    name.as_str(),
                    form.files,
                    blaze_report::ui::fmt_bytes(form.bytes),
                ),
            )?,
        }
    }

    let probe = OsPidProbe;
    let aux = sweep_aux_with(cx, home.dir(), now_ns, &probe, true, AuxSweepPlant::None);
    summary.aux_files_removed = aux.files;
    summary.aux_bytes_freed = aux.bytes;
    if aux.files > 0 {
        out.line(
            cx,
            format_args!(
                "blaze gc (dry-run): would sweep {} stale run/temp file(s), {}",
                aux.files,
                blaze_report::ui::fmt_bytes(aux.bytes),
            ),
        )?;
    }

    Ok(summary)
}

fn format_budget<'f>(cx: &'f Cx, bytes: u64) -> bstd::mem::String<&'f bstd::mem::Frame> {
    let frame = cx.frame();
    if bytes == u64::MAX {
        bstd::mem::format_in!(frame, "unlimited")
    } else {
        bstd::mem::format_in!(frame, "{}", blaze_report::ui::fmt_bytes(bytes))
    }
}

// ---- Auxiliary directory sweeps (`v2/runs`, `v2/test/tmp`, `v2/run/records`) ----

/// Runs directory beneath `<HOME>/.blaze`.
const RUNS_DIR: &str = "v2/runs";
/// Test temporary root beneath `<HOME>/.blaze`.
const TEST_TMP_DIR: &str = "v2/test/tmp";
/// Legacy run records / native dylib directory beneath `<HOME>/.blaze`.
const RUN_RECORDS_DIR: &str = "v2/run/records";
/// Native unit work root (`<cmd>/{pkg,out,deps,facts,jobs}/...`) beneath `<HOME>/.blaze`.
const NATIVE_WORK_DIR: &str = "v2/native-work";
/// Build-script emitted file cache beneath `<HOME>/.blaze`.
const BUILD_SCRIPT_FILES_DIR: &str = "v2/build-script/files";
/// Fetched dependency source records beneath `<HOME>/.blaze`.
const SOURCES_DIR: &str = "v2/sources";
/// Compiled Wasmtime proc-macro module cache beneath `<HOME>/.blaze`.
const PROC_MACRO_COMPILED_DIR: &str = "v2/proc-macro/compiled";
/// Proc-macro expansion record cache beneath `<HOME>/.blaze`.
const PROC_MACRO_EXPANSIONS_DIR: &str = "v2/proc-macro/expansions";
/// Retired legacy directories beneath `<HOME>/.blaze` that this blaze never reads or writes
/// (layout.rs `Scope::All` retired rows; store.rs `OLD_HOME`).
const LEGACY_RETIRED_DIRS: [&str; 3] = ["v2/shared", "v2/own", "v2/daemon"];
/// Scratch temporary directory root (`~/.blaze/t/`) holding `bd-*` / `*-<pid>-*` scratch dirs.
const SCRATCH_TMP_DIR: &str = "t";
/// Toolchain shelf root beneath `<HOME>/.blaze`.
const TOOLCHAIN_DIR: &str = "v2/toolchain";
/// Remaining rebuildable cache directories (never touching `modules/` bootstrap dirs) expired at
/// [`CACHE_EXPIRY_NS`] (`ROOT_LIFE_NS` = 14 days, matching store root lifetime).
const CACHE_EXPIRY_DIRS: [&str; 4] = [
    "v2/build-script/compiled",
    "v2/build-script/records",
    BUILD_SCRIPT_FILES_DIR,
    "v2/sdk-answers",
];
/// Tightened size caps for auxiliary directories under `<HOME>/.blaze/` so total disk footprint
/// stays bounded. When a directory exceeds its cap, oldest-mtime entries evict down to 75%.
pub(crate) const BUILD_SCRIPT_FILES_MAX_BYTES: u64 = 4 << 30;
pub(crate) const NATIVE_WORK_MAX_BYTES: u64 = 2 << 30;
pub(crate) const RUN_RECORDS_MAX_BYTES: u64 = 2 << 30;
pub(crate) const SOURCES_MAX_BYTES: u64 = 2 << 30;
pub(crate) const PROC_MACRO_MAX_BYTES: u64 = 1 << 30;
/// Stale age threshold (24 hours) for non-PID scratch directories under `~/.blaze/t/`.
pub(crate) const SCRATCH_STALE_NS: u64 = 24 * 3600 * 1_000_000_000;
/// Low-watermark percentage (75%) after an auxiliary LRU sweep (matches store GC watermarks).
const AUX_LOW_WATERMARK_PCT: u64 = 75;
/// Total directory entries visited across one auxiliary size-budget scan (rule 18).
const AUX_BUDGET_VISIT_MOST: usize = 65_536;
/// Maximum file/directory removal operations performed across one auxiliary GC pass (rule 18:
/// shared cap prevents nested-loop multiplication across directory trees).
pub(crate) const AUX_OPS_MOST: usize = 16_384;

/// Size-budget caps (in bytes; `0` = age-only) for the three largest auxiliary directories.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AuxCaps {
    pub(crate) build_script_files: u64,
    pub(crate) native_work: u64,
    pub(crate) run_records: u64,
}

impl AuxCaps {
    pub(crate) const DEFAULT: AuxCaps = AuxCaps {
        build_script_files: BUILD_SCRIPT_FILES_MAX_BYTES,
        native_work: NATIVE_WORK_MAX_BYTES,
        run_records: RUN_RECORDS_MAX_BYTES,
    };
}

/// Bounded budgets for one auxiliary GC sweep (rule 18: bounded progress).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AuxLimits {
    pub(crate) caps: AuxCaps,
    /// Shared maximum removal/tree-expansion operations across the entire auxiliary sweep.
    pub(crate) max_ops: usize,
    /// Maximum directory entries visited during byte-budget scans per capped directory.
    pub(crate) max_visits: usize,
}

impl AuxLimits {
    pub(crate) const DEFAULT: AuxLimits = AuxLimits {
        caps: AuxCaps::DEFAULT,
        max_ops: AUX_OPS_MOST,
        max_visits: AUX_BUDGET_VISIT_MOST,
    };
}

/// Target low-watermark byte budget (75% of `cap`) for auxiliary LRU eviction.
const fn aux_low_watermark(cap: u64) -> u64 {
    (cap / 100)
        .saturating_mul(AUX_LOW_WATERMARK_PCT)
        .saturating_add(((cap % 100).saturating_mul(AUX_LOW_WATERMARK_PCT)) / 100)
}
/// Maximum bytes read when verifying the active `PINNED` toolchain manifest (typically ~64 KiB).
const TOOLCHAIN_MANIFEST_MOST: u64 = 8 << 20;
/// Expiry window for cache directories and superseded toolchain packs (14 days = `ROOT_LIFE_NS`).
pub(crate) const CACHE_EXPIRY_NS: u64 = blaze_cas::plain::ROOT_LIFE_NS;

/// Maximum entries inspected per auxiliary directory in one GC pass (rule 18).
const AUX_SWEEP_MOST: usize = 4096;
/// Maximum depth when removing an orphaned temp directory iteratively (rule 18).
const TREE_DEPTH_MOST: usize = 32;
/// Maximum bytes read from a `.meta` file when checking its PID.
const META_READ_MOST: u64 = 64 << 10;
/// Grace period for dead-PID telemetry rings while a live viewer holds `viewer.lock` (4x viewer's
/// 15s `collect_after`).
pub(crate) const VIEWER_DEAD_GRACE_NS: u64 = 60 * 1_000_000_000;
/// Fallback age threshold for `.telem` / temp dirs whose `.meta` or PID is missing/unparseable.
pub(crate) const ORPHAN_GRACE_NS: u64 = 3600 * 1_000_000_000;
/// Maximum historical `.build.json` summaries retained in `v2/runs/` (matches viewer's `builds_most`).
const BUILDS_KEEP_MOST: usize = 64;
/// Maximum age for `.build.json` summaries and unreferenced `v2/run/records/` files (7 days).
pub(crate) const AUX_EXPIRY_NS: u64 = 7 * 86_400 * 1_000_000_000;

/// Process liveness state reported by [`PidProbe`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PidState {
    Alive,
    Dead,
    Unknown,
}

/// Process liveness seam (rule 78) so auxiliary GC sweeps run deterministically under `SimWorld`.
pub(crate) trait PidProbe {
    fn probe(&self, pid: u32) -> PidState;
}

/// Native Unix PID probe via `kill(pid, 0)` (safe, allocation-free).
pub(crate) struct OsPidProbe;

impl PidProbe for OsPidProbe {
    fn probe(&self, pid: u32) -> PidState {
        #[cfg(unix)]
        {
            let Ok(pid_i32) = i32::try_from(pid) else {
                return PidState::Unknown;
            };
            if pid_i32 <= 0 {
                return PidState::Dead;
            }
            match blaze_sys::posix::signal::kill(pid_i32, 0) {
                Ok(()) | Err(blaze_sys::Errno::Perm) => PidState::Alive,
                Err(blaze_sys::Errno::Srch) => PidState::Dead,
                Err(_) => PidState::Unknown,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            PidState::Unknown
        }
    }
}

/// Planted bug controls for auxiliary GC DST tests (rule 43).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AuxSweepPlant {
    None,
    /// Planted bug: treats `PidState::Alive` as `Dead`, proving the test oracle catches live-run
    /// deletion.
    #[cfg(test)]
    IgnorePidAlive,
    /// Planted bug: ignores the active `PINNED` toolchain pack set, proving the test oracle
    /// catches active-pack deletion.
    #[cfg(test)]
    IgnoreActiveToolchain,
}

/// Counts from an auxiliary directory sweep.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct AuxSummary {
    pub(crate) files: u32,
    pub(crate) bytes: u64,
}

/// Pure parser for `pid=<u32>` inside a run's `.meta` file (`viewer/pump.rs` format).
pub(crate) fn parse_meta_pid(bytes: &[u8]) -> Option<u32> {
    let text = core::str::from_utf8(bytes).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("pid=") {
            let pid = rest.trim().parse::<u32>().ok()?;
            return (pid > 0).then_some(pid);
        }
    }
    None
}

/// Pure parser for leading `<pid>-...` directory names under `v2/test/tmp/` and `v2/native-work/`.
pub(crate) fn parse_pid_prefix(name: &str) -> Option<u32> {
    let (head, _) = name.split_once('-')?;
    let pid = head.parse::<u32>().ok()?;
    (pid > 0).then_some(pid)
}

/// Pure parser for a process ID embedded in a `v2/native-work/` entry name:
/// - `<64-hex>-<pid>-<seq>` or `<64-hex>-<pid>-<seq>.lock` (native unit run folders/locks,
///   `blaze-proc-macro/src/native_unit.rs`);
/// - `<pid>-<rest>` (PID-prefixed temp directories).
/// Returns `None` for legacy key-only names without a PID (e.g. `oldkey`).
pub(crate) fn parse_native_work_pid(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".lock").unwrap_or(name);
    if let Some(prefix) = stem.get(..64)
        && stem.as_bytes().get(64) == Some(&b'-')
        && parse_hex_32(prefix).is_some()
        && let Some(rest) = stem.get(65..)
        && let Some((pid_str, seq_str)) = rest.split_once('-')
        && seq_str.parse::<u64>().is_ok()
    {
        let pid = pid_str.parse::<u32>().ok()?;
        return (pid > 0).then_some(pid);
    }
    parse_pid_prefix(stem)
}

/// Pure parser for a process ID embedded in a `~/.blaze/t/` scratch entry name:
/// - `<64-hex>-<pid>-<seq>[.lock]` or `<pid>-<rest>` (same as [`parse_native_work_pid`]);
/// - `<prefix>-<pid>-<suffix>[.lock]` (e.g. `bd-4242-0`, `work-4242-tmp`).
pub(crate) fn parse_scratch_pid(name: &str) -> Option<u32> {
    if let Some(pid) = parse_native_work_pid(name) {
        return Some(pid);
    }
    let stem = name.strip_suffix(".lock").unwrap_or(name);
    let (head, _tail) = stem.rsplit_once('-')?;
    if let Some((_, pid_str)) = head.rsplit_once('-')
        && let Ok(pid) = pid_str.parse::<u32>()
        && pid > 0
    {
        return Some(pid);
    }
    if let Some((_, rest)) = stem.split_once('-')
        && let Some((pid_str, _)) = rest.split_once('-')
        && let Ok(pid) = pid_str.parse::<u32>()
        && pid > 0
    {
        return Some(pid);
    }
    None
}

fn effective_pid_state(pid_state: Option<PidState>, plant: AuxSweepPlant) -> Option<PidState> {
    match (pid_state, plant) {
        #[cfg(test)]
        (Some(PidState::Alive), AuxSweepPlant::IgnorePidAlive) => Some(PidState::Dead),
        (state, _) => state,
    }
}

/// Sans-IO decision for whether a run's `.telem`/`.meta` pair should be swept.
pub(crate) fn should_sweep_run(
    pid_state: Option<PidState>,
    viewer_active: bool,
    mtime_ns: u64,
    now_ns: u64,
    plant: AuxSweepPlant,
) -> bool {
    let age_ns = now_ns.saturating_sub(mtime_ns);
    match effective_pid_state(pid_state, plant) {
        Some(PidState::Alive) => false,
        Some(PidState::Dead) => !viewer_active || age_ns >= VIEWER_DEAD_GRACE_NS,
        Some(PidState::Unknown) | None => age_ns >= ORPHAN_GRACE_NS,
    }
}

/// Probes whether a live telemetry viewer currently holds `LOCK_SH` on `<HOME>/.blaze/v2/runs/viewer.lock`.
/// Fast path: 1 `stat` syscall returning `false` when `viewer.lock` does not exist (never creates it).
pub(crate) fn viewer_lock_is_held(cx: &Cx, blaze_dir: &Dir<ReadWrite>) -> bool {
    let frame = cx.frame();
    let rel_str = bstd::mem::format_in!(frame, "{RUNS_DIR}/{}", blaze_launch::telemetry::VIEWER_LOCK_LEAF);
    let Ok(rel) = Path::new(rel_str.as_str()) else {
        return false;
    };
    match blaze_dir.stat(cx, &rel) {
        Ok(None) => return false,
        Ok(Some(_)) => {}
        Err(_) => return true,
    }
    let Ok(runs_root) = Path::new(RUNS_DIR) else {
        return true;
    };
    let driver = blaze_dir.lend(&runs_root);
    let lock_path = bstd::mem::format_in!(frame, "{}/{}", driver.dir().as_str(), blaze_launch::telemetry::VIEWER_LOCK_LEAF);
    let Some(file) = FileName::new(lock_path.as_str()) else {
        return true;
    };
    match ExclusiveLock::try_acquire(driver, file) {
        Ok(Some(lock)) => {
            drop(lock);
            false
        }
        Ok(None) | Err(_) => true,
    }
}

#[inline]
fn take_op(ops_budget: &mut usize) -> bool {
    if *ops_budget == 0 {
        return false;
    }
    *ops_budget -= 1;
    true
}

/// Shared bounded state for one auxiliary GC sweep (rule 18: one shared `ops_budget` prevents
/// nested directory loops from multiplying work).
struct AuxSweepCtx {
    now_ns: u64,
    limits: AuxLimits,
    sources_cap: u64,
    proc_macro_cap: u64,
    dry_run: bool,
    plant: AuxSweepPlant,
    ops_budget: usize,
    summary: AuxSummary,
}

impl AuxSweepCtx {
    fn new(now_ns: u64, limits: AuxLimits, sources_cap: u64, proc_macro_cap: u64, dry_run: bool, plant: AuxSweepPlant) -> Self {
        Self { now_ns, limits, sources_cap, proc_macro_cap, dry_run, plant, ops_budget: limits.max_ops, summary: AuxSummary::default() }
    }

    #[inline]
    fn exhausted(&self) -> bool {
        self.ops_budget == 0
    }

    fn remove_file(&mut self, cx: &Cx, blaze_dir: &Dir<ReadWrite>, path: &Path, len: u64) {
        use bstd::fs::Durable;
        if !take_op(&mut self.ops_budget) {
            return;
        }
        if self.dry_run || blaze_dir.remove(cx, path, Durable::No).is_ok() {
            self.summary.files = self.summary.files.saturating_add(1);
            self.summary.bytes = self.summary.bytes.saturating_add(len);
        }
    }

    fn remove_tree(&mut self, cx: &Cx, blaze_dir: &Dir<ReadWrite>, path: &Path) -> AuxSummary {
        let gone = remove_tree_budgeted(cx, blaze_dir, path, self.dry_run, &mut self.ops_budget);
        self.summary.files = self.summary.files.saturating_add(gone.files);
        self.summary.bytes = self.summary.bytes.saturating_add(gone.bytes);
        gone
    }
}

/// Best-effort bounded iterative removal of a directory tree beneath `root` (handles files,
/// symlinks, and special nodes like sockets/FIFOs left by sandboxed tests in `$TMPDIR`).
pub(crate) fn remove_tree_best_effort(cx: &Cx, root: &Dir<ReadWrite>, path: &Path) -> AuxSummary {
    let mut ops_budget = AUX_SWEEP_MOST;
    remove_tree_budgeted(cx, root, path, false, &mut ops_budget)
}

fn remove_tree_budgeted(cx: &Cx, root: &Dir<ReadWrite>, start: &Path, dry_run: bool, ops_budget: &mut usize) -> AuxSummary {
    use bstd::fs::{Durable, EntryKind};
    let mut summary = AuxSummary::default();
    if *ops_budget == 0 {
        return summary;
    }
    let frame = cx.frame();
    let mut stack: blaze_own::Vec<(bstd::mem::String<&bstd::mem::Frame>, bool, usize), &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    stack.push((bstd::mem::format_in!(frame, "{}", start.as_str()), false, 0));
    while let Some((dir_str, expanded, depth)) = stack.pop() {
        let Ok(dir_path) = Path::new(dir_str.as_str()) else {
            continue;
        };
        if expanded {
            if !dry_run {
                let _ = root.remove_dir(cx, &dir_path, Durable::No);
            }
            continue;
        }
        if !take_op(ops_budget) {
            continue;
        }
        stack.push((dir_str, true, depth));
        let Ok(Some(entries)) = root.entries(cx, &dir_path) else {
            continue;
        };
        for i in 0..entries.len() {
            if *ops_budget == 0 {
                break;
            }
            let Ok(child) = entries.path(i) else {
                continue;
            };
            match entries.kind(i) {
                EntryKind::Dir => {
                    if depth < TREE_DEPTH_MOST {
                        stack.push((bstd::mem::format_in!(frame, "{}", child.as_str()), false, depth + 1));
                    }
                }
                EntryKind::File | EntryKind::Link | EntryKind::Other | EntryKind::Unknown => {
                    if !take_op(ops_budget) {
                        break;
                    }
                    let len = entries.len_of(i);
                    if dry_run || root.remove(cx, &child, Durable::No).is_ok() {
                        summary.files = summary.files.saturating_add(1);
                        summary.bytes = summary.bytes.saturating_add(len);
                    }
                }
                EntryKind::Gone => {}
            }
        }
    }
    summary
}

/// One candidate entry considered by an auxiliary directory's size-budget LRU sweep.
struct AuxEntry {
    mtime_ns: u64,
    bytes: u64,
    is_dir: bool,
    path: Path,
}

/// Bounded iterative byte-size measurement for a directory tree beneath `root`, charging a shared
/// `visit_budget` so one GC pass never performs unbounded directory traversal (rule 18).
/// Returns `None` if `visit_budget` ran out mid-tree so callers never enqueue undercounted
/// `0`-byte placeholders into LRU eviction.
fn measure_tree_bytes(cx: &Cx, root: &Dir<ReadWrite>, start: &Path, visit_budget: &mut usize) -> Option<u64> {
    use bstd::fs::EntryKind;
    if *visit_budget == 0 {
        return None;
    }
    let frame = cx.frame();
    let mut stack: blaze_own::Vec<(bstd::mem::String<&bstd::mem::Frame>, usize), &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    stack.push((bstd::mem::format_in!(frame, "{}", start.as_str()), 0));
    let mut total = 0u64;
    while let Some((dir_str, depth)) = stack.pop() {
        if *visit_budget == 0 {
            return None;
        }
        *visit_budget -= 1;
        let Ok(dir_path) = Path::new(dir_str.as_str()) else {
            continue;
        };
        let Ok(Some(entries)) = root.entries(cx, &dir_path) else {
            continue;
        };
        for i in 0..entries.len() {
            if *visit_budget == 0 {
                return None;
            }
            match entries.kind(i) {
                EntryKind::Dir => {
                    if depth < TREE_DEPTH_MOST
                        && let Ok(child) = entries.path(i)
                    {
                        stack.push((bstd::mem::format_in!(frame, "{}", child.as_str()), depth + 1));
                    }
                }
                EntryKind::File | EntryKind::Link | EntryKind::Other | EntryKind::Unknown => {
                    *visit_budget -= 1;
                    total = total.saturating_add(entries.len_of(i));
                }
                EntryKind::Gone => {}
            }
        }
    }
    Some(total)
}

/// Evicts oldest-mtime entries from `entries` when `total_bytes > cap_bytes`, reclaiming down to
/// the 75% low watermark (`aux_low_watermark(cap_bytes)`). Deterministic tie-breaking by path.
fn evict_aux_lru(
    cx: &Cx,
    blaze_dir: &Dir<ReadWrite>,
    entries: &mut blaze_own::Vec<AuxEntry, &bstd::mem::Frame>,
    mut total_bytes: u64,
    cap_bytes: u64,
    ctx: &mut AuxSweepCtx,
) {
    if cap_bytes == 0 || total_bytes <= cap_bytes || ctx.exhausted() {
        return;
    }
    let target = aux_low_watermark(cap_bytes);
    entries.sort_by(|a, b| a.mtime_ns.cmp(&b.mtime_ns).then_with(|| a.path.as_str().cmp(b.path.as_str())));
    for entry in entries.iter() {
        if total_bytes <= target || ctx.exhausted() {
            break;
        }
        if entry.is_dir {
            let gone = ctx.remove_tree(cx, blaze_dir, &entry.path);
            total_bytes = total_bytes.saturating_sub(entry.bytes.max(gone.bytes));
        } else {
            ctx.remove_file(cx, blaze_dir, &entry.path, entry.bytes);
            total_bytes = total_bytes.saturating_sub(entry.bytes);
        }
    }
}

/// Sweeps stale files in `v2/runs/`, `v2/test/tmp/`, `v2/run/records/`, `v2/native-work/`,
/// `v2/proc-macro/{compiled,expansions}`, `v2/build-script/{compiled,records,files}`,
/// `v2/sdk-answers/`, and superseded loose packs in `v2/toolchain/`.
pub(crate) fn sweep_aux_with(
    cx: &Cx,
    blaze_dir: &Dir<ReadWrite>,
    now_ns: u64,
    probe: &impl PidProbe,
    dry_run: bool,
    plant: AuxSweepPlant,
) -> AuxSummary {
    sweep_aux_with_caps(cx, blaze_dir, now_ns, AuxCaps::DEFAULT, probe, dry_run, plant)
}

/// [`sweep_aux_with`] with explicit size-budget [`AuxCaps`] (parameterized for deterministic tests).
pub(crate) fn sweep_aux_with_caps(
    cx: &Cx,
    blaze_dir: &Dir<ReadWrite>,
    now_ns: u64,
    caps: AuxCaps,
    probe: &impl PidProbe,
    dry_run: bool,
    plant: AuxSweepPlant,
) -> AuxSummary {
    sweep_aux_with_limits(cx, blaze_dir, now_ns, AuxLimits { caps, ..AuxLimits::DEFAULT }, probe, dry_run, plant)
}

/// [`sweep_aux_with`] with explicit [`AuxLimits`] (size caps + bounded per-sweep work cap).
pub(crate) fn sweep_aux_with_limits(
    cx: &Cx,
    blaze_dir: &Dir<ReadWrite>,
    now_ns: u64,
    limits: AuxLimits,
    probe: &impl PidProbe,
    dry_run: bool,
    plant: AuxSweepPlant,
) -> AuxSummary {
    sweep_aux_inner(cx, blaze_dir, now_ns, limits, SOURCES_MAX_BYTES, PROC_MACRO_MAX_BYTES, probe, dry_run, plant)
}

#[expect(clippy::too_many_arguments, reason = "internal aux sweep entry parameterized by all directory caps")]
fn sweep_aux_inner(
    cx: &Cx,
    blaze_dir: &Dir<ReadWrite>,
    now_ns: u64,
    limits: AuxLimits,
    sources_cap: u64,
    proc_macro_cap: u64,
    probe: &impl PidProbe,
    dry_run: bool,
    plant: AuxSweepPlant,
) -> AuxSummary {
    let mut ctx = AuxSweepCtx::new(now_ns, limits, sources_cap, proc_macro_cap, dry_run, plant);
    sweep_runs(cx, blaze_dir, probe, &mut ctx);
    sweep_test_tmp(cx, blaze_dir, probe, &mut ctx);
    sweep_scratch_tmp(cx, blaze_dir, probe, &mut ctx);
    sweep_legacy_dirs(cx, blaze_dir, &mut ctx);
    sweep_run_records(cx, blaze_dir, &mut ctx);
    sweep_native_work(cx, blaze_dir, probe, &mut ctx);
    sweep_expired_dirs_shared_cap(
        cx,
        blaze_dir,
        &[PROC_MACRO_COMPILED_DIR, PROC_MACRO_EXPANSIONS_DIR],
        CACHE_EXPIRY_NS,
        ctx.proc_macro_cap,
        &mut ctx,
    );
    sweep_expired_dirs_shared_cap(cx, blaze_dir, &[SOURCES_DIR], CACHE_EXPIRY_NS, ctx.sources_cap, &mut ctx);
    for dir in CACHE_EXPIRY_DIRS {
        if ctx.exhausted() {
            break;
        }
        let cap = if dir == BUILD_SCRIPT_FILES_DIR { limits.caps.build_script_files } else { 0 };
        sweep_expired_dirs_shared_cap(cx, blaze_dir, &[dir], CACHE_EXPIRY_NS, cap, &mut ctx);
    }
    sweep_toolchain_packs_ctx(cx, blaze_dir, blaze_toolchain::PINNED, &mut ctx);
    ctx.summary
}

fn sweep_runs(cx: &Cx, blaze_dir: &Dir<ReadWrite>, probe: &impl PidProbe, ctx: &mut AuxSweepCtx) {
    use bstd::fs::EntryKind;
    if ctx.exhausted() {
        return;
    }
    let Ok(runs_root) = Path::new(RUNS_DIR) else {
        return;
    };
    let Ok(Some(entries)) = blaze_dir.entries(cx, &runs_root) else {
        return;
    };
    let viewer_active = viewer_lock_is_held(cx, blaze_dir);
    let frame = cx.frame();
    let mut builds: blaze_own::Vec<(u64, Path, u64), &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    let mut alive_stems: blaze_own::Vec<bstd::mem::String<&bstd::mem::Frame>, &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    let mut telem_stems: blaze_own::Vec<bstd::mem::String<&bstd::mem::Frame>, &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    let limit = entries.len().min(AUX_SWEEP_MOST.min(ctx.limits.max_visits));
    let mut meta_buf = bstd::mem::Bytes::new_in(frame, "gc run meta read");

    // Pass 1: sweep dead/orphaned `.telem` + `.meta` pairs and record alive stems.
    for i in 0..limit {
        if ctx.exhausted() {
            return;
        }
        if entries.kind(i) != EntryKind::File {
            continue;
        }
        let name = entries.name(i);
        if name == blaze_launch::telemetry::VIEWER_LOCK_LEAF || name.ends_with(".bstd-new") {
            continue;
        }
        let Some(stem) = name.strip_suffix(".telem") else {
            continue;
        };
        telem_stems.push(bstd::mem::format_in!(frame, "{stem}"));
        let Ok(telem_path) = entries.path(i) else {
            continue;
        };
        let telem_len = entries.len_of(i);
        let telem_mtime = blaze_dir.stat(cx, &telem_path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
        let meta_rel_str = bstd::mem::format_in!(frame, "{RUNS_DIR}/{stem}.meta");
        let meta_path = Path::new(meta_rel_str.as_str()).ok();
        let (meta_stat, pid_state) = match meta_path.as_ref() {
            Some(mp) => {
                let st = blaze_dir.stat(cx, mp).ok().flatten();
                let pid = if st.as_ref().is_some_and(|s| s.len <= META_READ_MOST) {
                    meta_buf.clear();
                    blaze_dir.read_all(cx, mp, &mut meta_buf).ok().and_then(|_| parse_meta_pid(meta_buf.as_slice()))
                } else {
                    None
                };
                (st, pid.map(|p| probe.probe(p)))
            }
            None => (None, None),
        };
        let mtime_ns = meta_stat.as_ref().map_or(telem_mtime, |s| telem_mtime.max(s.stamp.modified_ns));
        if should_sweep_run(pid_state, viewer_active, mtime_ns, ctx.now_ns, ctx.plant) {
            ctx.remove_file(cx, blaze_dir, &telem_path, telem_len);
            if let (Some(mp), Some(st)) = (meta_path.as_ref(), meta_stat) {
                ctx.remove_file(cx, blaze_dir, mp, st.len);
            }
        } else if pid_state == Some(PidState::Alive) {
            alive_stems.push(bstd::mem::format_in!(frame, "{stem}"));
        }
    }

    // Pass 2: sweep orphaned `.meta` (no sibling `.telem`) and collect `.build.json` summaries.
    for i in 0..limit {
        if ctx.exhausted() {
            return;
        }
        if entries.kind(i) != EntryKind::File {
            continue;
        }
        let name = entries.name(i);
        if let Some(stem) = name.strip_suffix(".meta") {
            if telem_stems.iter().any(|s| s.as_str() == stem) {
                continue;
            }
            let Ok(meta_path) = entries.path(i) else {
                continue;
            };
            let meta_len = entries.len_of(i);
            let mtime_ns = blaze_dir.stat(cx, &meta_path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
            let pid_state = if meta_len <= META_READ_MOST {
                meta_buf.clear();
                blaze_dir.read_all(cx, &meta_path, &mut meta_buf).ok().and_then(|_| parse_meta_pid(meta_buf.as_slice())).map(|p| probe.probe(p))
            } else {
                None
            };
            if should_sweep_run(pid_state, viewer_active, mtime_ns, ctx.now_ns, ctx.plant) {
                ctx.remove_file(cx, blaze_dir, &meta_path, meta_len);
            }
        } else if let Some(stem) = name.strip_suffix(".build.json") {
            if alive_stems.iter().any(|s| s.as_str() == stem) {
                continue;
            }
            let Ok(path) = entries.path(i) else {
                continue;
            };
            let len = entries.len_of(i);
            let mtime_ns = blaze_dir.stat(cx, &path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
            builds.push((mtime_ns, path, len));
        }
    }

    // Keep at most the newest `BUILDS_KEEP_MOST` `.build.json` summaries within `AUX_EXPIRY_NS`.
    builds.sort_by(|a, b| b.0.cmp(&a.0));
    for (idx, (mtime_ns, path, len)) in builds.iter().enumerate() {
        if ctx.exhausted() {
            break;
        }
        let expired = ctx.now_ns.saturating_sub(*mtime_ns) >= AUX_EXPIRY_NS;
        if idx >= BUILDS_KEEP_MOST || expired {
            ctx.remove_file(cx, blaze_dir, path, *len);
        }
    }
}

fn sweep_test_tmp(cx: &Cx, blaze_dir: &Dir<ReadWrite>, probe: &impl PidProbe, ctx: &mut AuxSweepCtx) {
    use bstd::fs::EntryKind;
    if ctx.exhausted() {
        return;
    }
    let Ok(tmp_root) = Path::new(TEST_TMP_DIR) else {
        return;
    };
    let Ok(Some(entries)) = blaze_dir.entries(cx, &tmp_root) else {
        return;
    };
    let limit = entries.len().min(AUX_SWEEP_MOST.min(ctx.limits.max_visits));
    for i in 0..limit {
        if ctx.exhausted() {
            break;
        }
        if entries.kind(i) != EntryKind::Dir {
            continue;
        }
        let Ok(path) = entries.path(i) else {
            continue;
        };
        let mtime_ns = blaze_dir.stat(cx, &path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
        let pid_state = parse_pid_prefix(entries.name(i)).map(|p| probe.probe(p));
        if should_sweep_run(pid_state, false, mtime_ns, ctx.now_ns, ctx.plant) {
            let _ = ctx.remove_tree(cx, blaze_dir, &path);
        }
    }
}

/// Sweeps dead-PID or >24h stale scratch entries (`bd-*` / `*-<pid>-*` / `<pid>-*`) under
/// `~/.blaze/t/` safely without ever touching live PIDs.
fn sweep_scratch_tmp(cx: &Cx, blaze_dir: &Dir<ReadWrite>, probe: &impl PidProbe, ctx: &mut AuxSweepCtx) {
    use bstd::fs::EntryKind;
    if ctx.exhausted() {
        return;
    }
    let Ok(scratch_root) = Path::new(SCRATCH_TMP_DIR) else {
        return;
    };
    let Ok(Some(entries)) = blaze_dir.entries(cx, &scratch_root) else {
        return;
    };
    let limit = entries.len().min(AUX_SWEEP_MOST.min(ctx.limits.max_visits));
    for i in 0..limit {
        if ctx.exhausted() {
            break;
        }
        let name = entries.name(i);
        if name.ends_with(".bstd-new") {
            continue;
        }
        let pid = parse_scratch_pid(name);
        if pid.is_none() && !name.starts_with("bd-") {
            continue;
        }
        let pid_state = effective_pid_state(pid.map(|p| probe.probe(p)), ctx.plant);
        if pid_state == Some(PidState::Alive) {
            continue;
        }
        let Ok(path) = entries.path(i) else {
            continue;
        };
        let dead = pid_state == Some(PidState::Dead);
        let stale = if dead {
            true
        } else {
            let mtime_ns = blaze_dir.stat(cx, &path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
            ctx.now_ns.saturating_sub(mtime_ns) >= SCRATCH_STALE_NS
        };
        if !stale {
            continue;
        }
        match entries.kind(i) {
            EntryKind::Dir => {
                let _ = ctx.remove_tree(cx, blaze_dir, &path);
            }
            EntryKind::File | EntryKind::Link | EntryKind::Other | EntryKind::Unknown => {
                ctx.remove_file(cx, blaze_dir, &path, entries.len_of(i));
            }
            EntryKind::Gone => {}
        }
    }
}

/// Sweeps retired legacy directories (`v2/shared`, `v2/own`, `v2/daemon`) that this blaze never
/// reads or writes, checking `EntryKind::Dir` so symlinks are never followed.
fn sweep_legacy_dirs(cx: &Cx, blaze_dir: &Dir<ReadWrite>, ctx: &mut AuxSweepCtx) {
    use bstd::fs::EntryKind;
    if ctx.exhausted() {
        return;
    }
    let Ok(v2_root) = Path::new("v2") else {
        return;
    };
    let Ok(Some(entries)) = blaze_dir.entries(cx, &v2_root) else {
        return;
    };
    let limit = entries.len().min(AUX_SWEEP_MOST.min(ctx.limits.max_visits));
    for i in 0..limit {
        if ctx.exhausted() {
            break;
        }
        if entries.kind(i) != EntryKind::Dir {
            continue;
        }
        let Ok(path) = entries.path(i) else {
            continue;
        };
        if LEGACY_RETIRED_DIRS.contains(&path.as_str()) {
            let _ = ctx.remove_tree(cx, blaze_dir, &path);
        }
    }
}

fn sweep_run_records(cx: &Cx, blaze_dir: &Dir<ReadWrite>, ctx: &mut AuxSweepCtx) {
    use bstd::fs::EntryKind;
    if ctx.exhausted() {
        return;
    }
    let Ok(rec_root) = Path::new(RUN_RECORDS_DIR) else {
        return;
    };
    let Ok(Some(entries)) = blaze_dir.entries(cx, &rec_root) else {
        return;
    };
    let cap_bytes = ctx.limits.caps.run_records;
    let frame = cx.frame();
    let mut kept: blaze_own::Vec<AuxEntry, &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    let mut total_bytes = 0u64;
    let limit = entries.len().min(ctx.limits.max_visits);
    for i in 0..limit {
        if ctx.exhausted() {
            return;
        }
        if entries.kind(i) != EntryKind::File {
            continue;
        }
        let name = entries.name(i);
        if name.ends_with(".bstd-new") {
            continue;
        }
        let Ok(path) = entries.path(i) else {
            continue;
        };
        let len = entries.len_of(i);
        let mtime_ns = blaze_dir.stat(cx, &path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
        if ctx.now_ns.saturating_sub(mtime_ns) >= AUX_EXPIRY_NS {
            ctx.remove_file(cx, blaze_dir, &path, len);
        } else if cap_bytes > 0 {
            total_bytes = total_bytes.saturating_add(len);
            kept.push(AuxEntry { mtime_ns, bytes: len, is_dir: false, path });
        }
    }
    evict_aux_lru(cx, blaze_dir, &mut kept, total_bytes, cap_bytes, ctx);
}

/// Sweeps stale entries under `v2/native-work/`:
/// - Top-level PID-prefixed temp dirs (`<pid>-...`) whose PID is dead;
/// - Dead-PID jobserver FIFOs under `<cmd>/jobs/<pid>` (never touching live FIFOs or applying LRU);
/// - Expired shim snapshots under `<cmd>/fact-shim/` past `CACHE_EXPIRY_NS` (never LRU-evicted
///   so an active command's shared shim binary is never deleted mid-build);
/// - Work trees/locks under `<cmd>/{pkg,out,deps,facts}/<key>-<pid>-<seq>[.lock]`: live PIDs are
///   ALWAYS preserved (never expired or LRU-evicted), dead PIDs are swept immediately as crash
///   debris, and legacy non-PID entries use 7-day expiry plus oldest-mtime LRU above `cap_bytes`.
fn sweep_native_work(cx: &Cx, blaze_dir: &Dir<ReadWrite>, probe: &impl PidProbe, ctx: &mut AuxSweepCtx) {
    use bstd::fs::EntryKind;
    if ctx.exhausted() {
        return;
    }
    let Ok(work_root) = Path::new(NATIVE_WORK_DIR) else {
        return;
    };
    let Ok(Some(cmds)) = blaze_dir.entries(cx, &work_root) else {
        return;
    };
    let cap_bytes = ctx.limits.caps.native_work;
    let frame = cx.frame();
    let mut kept: blaze_own::Vec<AuxEntry, &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    let mut total_bytes = 0u64;
    let mut visit_budget = ctx.limits.max_visits;
    let cmd_limit = cmds.len().min(64);
    for c in 0..cmd_limit {
        if ctx.exhausted() {
            return;
        }
        if cmds.kind(c) != EntryKind::Dir {
            continue;
        }
        let Ok(cmd_path) = cmds.path(c) else {
            continue;
        };
        if let Some(pid) = parse_pid_prefix(cmds.name(c)) {
            if effective_pid_state(Some(probe.probe(pid)), ctx.plant) == Some(PidState::Dead) {
                let _ = ctx.remove_tree(cx, blaze_dir, &cmd_path);
            }
            continue;
        }
        let Ok(Some(buckets)) = blaze_dir.entries(cx, &cmd_path) else {
            continue;
        };
        let bucket_limit = buckets.len().min(64);
        for b in 0..bucket_limit {
            if ctx.exhausted() {
                return;
            }
            if buckets.kind(b) != EntryKind::Dir {
                continue;
            }
            let bucket_name = buckets.name(b);
            let Ok(bucket_path) = buckets.path(b) else {
                continue;
            };
            let Ok(Some(items)) = blaze_dir.entries(cx, &bucket_path) else {
                continue;
            };
            let item_limit = items.len().min(AUX_SWEEP_MOST.min(ctx.limits.max_visits));
            if bucket_name == "jobs" {
                for i in 0..item_limit {
                    if ctx.exhausted() {
                        return;
                    }
                    let Ok(item_path) = items.path(i) else {
                        continue;
                    };
                    let mtime_ns = blaze_dir.stat(cx, &item_path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
                    let pid = items.name(i).parse::<u32>().ok().filter(|p| *p > 0);
                    let pid_state = pid.map(|p| probe.probe(p));
                    if should_sweep_run(pid_state, false, mtime_ns, ctx.now_ns, ctx.plant) {
                        ctx.remove_file(cx, blaze_dir, &item_path, items.len_of(i));
                    }
                }
            } else if bucket_name == "fact-shim" {
                // Content-addressed shim binaries are shared across all scripts of a running
                // command: never subject to LRU eviction, only expired past `CACHE_EXPIRY_NS`.
                for i in 0..item_limit {
                    if ctx.exhausted() {
                        return;
                    }
                    if items.name(i).ends_with(".bstd-new") {
                        continue;
                    }
                    let Ok(item_path) = items.path(i) else {
                        continue;
                    };
                    let mtime_ns = blaze_dir.stat(cx, &item_path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
                    if ctx.now_ns.saturating_sub(mtime_ns) < CACHE_EXPIRY_NS {
                        continue;
                    }
                    match items.kind(i) {
                        EntryKind::Dir => {
                            let _ = ctx.remove_tree(cx, blaze_dir, &item_path);
                        }
                        EntryKind::File | EntryKind::Link => {
                            ctx.remove_file(cx, blaze_dir, &item_path, items.len_of(i));
                        }
                        _ => {}
                    }
                }
            } else {
                for i in 0..item_limit {
                    if ctx.exhausted() {
                        return;
                    }
                    let name = items.name(i);
                    if name.ends_with(".bstd-new") {
                        continue;
                    }
                    let pid_state = effective_pid_state(parse_native_work_pid(name).map(|p| probe.probe(p)), ctx.plant);
                    if pid_state == Some(PidState::Alive) {
                        continue;
                    }
                    let Ok(item_path) = items.path(i) else {
                        continue;
                    };
                    let dead_pid = pid_state == Some(PidState::Dead);
                    let mtime_ns = if dead_pid {
                        0
                    } else {
                        blaze_dir.stat(cx, &item_path).ok().flatten().map_or(0, |s| s.stamp.modified_ns)
                    };
                    let remove_now = dead_pid || ctx.now_ns.saturating_sub(mtime_ns) >= AUX_EXPIRY_NS;
                    match items.kind(i) {
                        EntryKind::Dir => {
                            if remove_now {
                                let _ = ctx.remove_tree(cx, blaze_dir, &item_path);
                            } else if cap_bytes > 0
                                && let Some(bytes) = measure_tree_bytes(cx, blaze_dir, &item_path, &mut visit_budget)
                            {
                                total_bytes = total_bytes.saturating_add(bytes);
                                kept.push(AuxEntry { mtime_ns, bytes, is_dir: true, path: item_path });
                            }
                        }
                        EntryKind::File | EntryKind::Link => {
                            let len = items.len_of(i);
                            if remove_now {
                                ctx.remove_file(cx, blaze_dir, &item_path, len);
                            } else if cap_bytes > 0 {
                                total_bytes = total_bytes.saturating_add(len);
                                kept.push(AuxEntry { mtime_ns, bytes: len, is_dir: false, path: item_path });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    evict_aux_lru(cx, blaze_dir, &mut kept, total_bytes, cap_bytes, ctx);
}

/// Sweeps files and subdirectories directly under every directory in `rel_dirs` untouched past
/// `expiry_ns`, and when `cap_bytes > 0`, also runs a combined oldest-mtime LRU sweep across
/// `rel_dirs` down to 75% of `cap_bytes`.
fn sweep_expired_dirs_shared_cap(
    cx: &Cx,
    blaze_dir: &Dir<ReadWrite>,
    rel_dirs: &[&str],
    expiry_ns: u64,
    cap_bytes: u64,
    ctx: &mut AuxSweepCtx,
) {
    use bstd::fs::EntryKind;
    if ctx.exhausted() {
        return;
    }
    let frame = cx.frame();
    let mut kept: blaze_own::Vec<AuxEntry, &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    let mut total_bytes = 0u64;
    let mut visit_budget = ctx.limits.max_visits;
    // Flat content-addressed areas like `v2/build-script/files` and `v2/sources` can hold tens of
    // thousands of files: scan up to `max_visits` when size-budgeted so entries past 4096 are still accounted.
    let entry_cap = if cap_bytes > 0 { ctx.limits.max_visits } else { AUX_SWEEP_MOST.min(ctx.limits.max_visits) };
    for &rel_dir in rel_dirs {
        if ctx.exhausted() {
            return;
        }
        let Ok(dir_path) = Path::new(rel_dir) else {
            continue;
        };
        let Ok(Some(entries)) = blaze_dir.entries(cx, &dir_path) else {
            continue;
        };
        let limit = entries.len().min(entry_cap);
        for i in 0..limit {
            if ctx.exhausted() {
                return;
            }
            let name = entries.name(i);
            if name.ends_with(".bstd-new") {
                continue;
            }
            let Ok(path) = entries.path(i) else {
                continue;
            };
            let mtime_ns = blaze_dir.stat(cx, &path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
            let expired = ctx.now_ns.saturating_sub(mtime_ns) >= expiry_ns;
            match entries.kind(i) {
                EntryKind::Dir => {
                    if expired {
                        let _ = ctx.remove_tree(cx, blaze_dir, &path);
                    } else if cap_bytes > 0
                        && let Some(bytes) = measure_tree_bytes(cx, blaze_dir, &path, &mut visit_budget)
                    {
                        total_bytes = total_bytes.saturating_add(bytes);
                        kept.push(AuxEntry { mtime_ns, bytes, is_dir: true, path });
                    }
                }
                EntryKind::File | EntryKind::Link => {
                    let len = entries.len_of(i);
                    if expired {
                        ctx.remove_file(cx, blaze_dir, &path, len);
                    } else if cap_bytes > 0 {
                        total_bytes = total_bytes.saturating_add(len);
                        kept.push(AuxEntry { mtime_ns, bytes: len, is_dir: false, path });
                    }
                }
                _ => {}
            }
        }
    }
    evict_aux_lru(cx, blaze_dir, &mut kept, total_bytes, cap_bytes, ctx);
}

/// Pure parser for a 64-lowercase-hex digest filename (`[u8; 32]`).
pub(crate) fn parse_hex_32(name: &str) -> Option<[u8; 32]> {
    let bytes = name.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let nibble = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        }
    };
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        let hi = nibble(*pair.first()?)?;
        let lo = nibble(*pair.get(1)?)?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

/// Sweeps superseded loose 64-hex pack/manifest files under `v2/toolchain/` that are NOT
/// referenced by `pinned`'s verified manifest and have been untouched past [`CACHE_EXPIRY_NS`].
/// Never touches `clang/`, any subdirectory, non-64-hex files, or anything when the active
/// manifest is absent/unreadable.
#[cfg(test)]
pub(crate) fn sweep_toolchain_packs(
    cx: &Cx,
    blaze_dir: &Dir<ReadWrite>,
    pinned: blaze_toolchain::ToolchainId,
    now_ns: u64,
    dry_run: bool,
    plant: AuxSweepPlant,
) -> AuxSummary {
    let mut ctx = AuxSweepCtx::new(now_ns, AuxLimits::DEFAULT, SOURCES_MAX_BYTES, PROC_MACRO_MAX_BYTES, dry_run, plant);
    sweep_toolchain_packs_ctx(cx, blaze_dir, pinned, &mut ctx);
    ctx.summary
}

fn sweep_toolchain_packs_ctx(cx: &Cx, blaze_dir: &Dir<ReadWrite>, pinned: blaze_toolchain::ToolchainId, ctx: &mut AuxSweepCtx) {
    use bstd::fs::EntryKind;
    if ctx.exhausted() {
        return;
    }
    let frame = cx.frame();
    let pinned_hex_bytes = crate::fetch::key_hex(&blaze_ids::ContentId(pinned.0));
    let Ok(pinned_hex) = core::str::from_utf8(&pinned_hex_bytes) else {
        return;
    };
    let manifest_rel = bstd::mem::format_in!(frame, "{TOOLCHAIN_DIR}/{pinned_hex}");
    let Ok(manifest_path) = Path::new(manifest_rel.as_str()) else {
        return;
    };
    let Some(st) = blaze_dir.stat(cx, &manifest_path).ok().flatten() else {
        return;
    };
    if st.len > TOOLCHAIN_MANIFEST_MOST {
        return;
    }
    let mut buf = bstd::mem::Bytes::new_in(frame, "gc toolchain manifest read");
    if blaze_dir.read_all(cx, &manifest_path, &mut buf).is_err() {
        return;
    }
    let Ok(view) = blaze_toolchain::ToolchainView::open(buf.as_slice(), pinned) else {
        return;
    };
    let mut keep: blaze_own::Vec<[u8; 32], &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    #[cfg(test)]
    let ignore_active = ctx.plant == AuxSweepPlant::IgnoreActiveToolchain;
    #[cfg(not(test))]
    let ignore_active = {
        let _ = ctx.plant;
        false
    };
    if !ignore_active {
        keep.push(pinned.0);
        for krate in view.crates() {
            keep.push(krate.pack);
        }
        keep.sort_unstable();
        keep.dedup();
    }
    let Ok(tc_root) = Path::new(TOOLCHAIN_DIR) else {
        return;
    };
    let Ok(Some(entries)) = blaze_dir.entries(cx, &tc_root) else {
        return;
    };
    let limit = entries.len().min(AUX_SWEEP_MOST.min(ctx.limits.max_visits));
    for i in 0..limit {
        if ctx.exhausted() {
            break;
        }
        if entries.kind(i) != EntryKind::File {
            continue;
        }
        let Some(digest) = parse_hex_32(entries.name(i)) else {
            continue;
        };
        if keep.binary_search(&digest).is_ok() {
            continue;
        }
        let Ok(path) = entries.path(i) else {
            continue;
        };
        let mtime_ns = blaze_dir.stat(cx, &path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
        if ctx.now_ns.saturating_sub(mtime_ns) < CACHE_EXPIRY_NS {
            continue;
        }
        ctx.remove_file(cx, blaze_dir, &path, entries.len_of(i));
    }
}

/// Default budget constant re-exported for tests.
#[cfg(test)]
pub(crate) const DEFAULT_BUDGET: u64 = DEFAULT_MAX_STORE_BYTES;

#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;

#[cfg(test)]
mod unified_gc_tests {
    use core::time::Duration;

    use bstd::fs::{Content, Durable};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::path::Path;
    use bstd::test::BUDGET;

    use super::{
        AuxLimits, AuxSweepPlant, BUILD_SCRIPT_FILES_MAX_BYTES, CACHE_EXPIRY_NS, DEFAULT_BUDGET,
        NATIVE_WORK_MAX_BYTES, PROC_MACRO_MAX_BYTES, PidProbe, PidState, RUN_RECORDS_MAX_BYTES,
        SCRATCH_STALE_NS, SOURCES_MAX_BYTES, forms, parse_scratch_pid, sweep_aux_inner, sweep_aux_with,
    };
    use crate::store::Home;

    struct FixedPids<'a>(&'a [u32]);

    impl PidProbe for FixedPids<'_> {
        fn probe(&self, pid: u32) -> PidState {
            if self.0.contains(&pid) { PidState::Alive } else { PidState::Dead }
        }
    }

    #[test]
    fn unified_gc_budget_and_expiry_constants_are_tightened() {
        assert_eq!(DEFAULT_BUDGET, 12 << 30);
        assert_eq!(BUILD_SCRIPT_FILES_MAX_BYTES, 4 << 30);
        assert_eq!(NATIVE_WORK_MAX_BYTES, 2 << 30);
        assert_eq!(RUN_RECORDS_MAX_BYTES, 2 << 30);
        assert_eq!(SOURCES_MAX_BYTES, 2 << 30);
        assert_eq!(PROC_MACRO_MAX_BYTES, 1 << 30);
        assert_eq!(forms::EXPIRY_DAYS, 3);
    }

    #[test]
    fn gc_sweeps_sources_proc_macro_lru_legacy_dirs_and_scratch_tmp_protecting_live_pids() {
        assert_eq!(parse_scratch_pid("bd-1001-0"), Some(1001));
        assert_eq!(parse_scratch_pid("bd-2002-foo-bar"), Some(2002));
        assert_eq!(parse_scratch_pid("work-3003-9"), Some(3003));
        assert_eq!(parse_scratch_pid("4004-tmp"), Some(4004));
        assert_eq!(
            parse_scratch_pid("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef-5005-1"),
            Some(5005)
        );
        assert_eq!(parse_scratch_pid("bd-orphan"), None);
        assert_eq!(parse_scratch_pid("bd-0-1"), None);
        assert_eq!(parse_scratch_pid("notes.txt"), None);

        let io = Io::simulated(SimWorld::new(88, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let fs = io.need_fs().expect("fs");
        let clock = io.need_clock().expect("clock");
        let home = Home::of(&cx, &fs, Some("/h")).expect("home");
        let put = |rel: &str, bytes: &[u8]| {
            home.dir().publish(&cx, &Path::new(rel).expect("path"), Content::bytes(bytes), Durable::No).expect("publish");
        };

        // 1. Legacy retired dirs (`v2/shared`, `v2/own`, `v2/daemon`).
        put("v2/shared/old.seg", &[1u8; 40]);
        put("v2/own/old.pack", &[2u8; 40]);
        put("v2/daemon/1234.sock", &[3u8; 16]);

        // 2. Scratch dirs under `~/.blaze/t/`: live PID, dead PID, no-PID `bd-*`, and unrelated file.
        put("t/bd-1001-0/a.o", &[4u8; 32]);
        put("t/work-1001-1/b.o", &[4u8; 32]);
        put("t/bd-2002-0/dead.o", &[5u8; 32]);
        put("t/work-2002-1/dead.o", &[5u8; 32]);
        put("t/bd-orphan/stale.o", &[6u8; 32]);
        put("t/keep.txt", b"not-a-scratch-dir");

        // 3. `v2/sources` (3 x 100B age-ordered) and `v2/proc-macro/{compiled,expansions}` (3 x 100B
        //    combined age-ordered), plus bootstrap `v2/proc-macro/modules/` (never touched).
        put("v2/proc-macro/modules/boot.wasm", &[9u8; 100]);
        put("v2/sources/src-old", &[1u8; 100]);
        put("v2/proc-macro/compiled/pm-old.cwasm", &[2u8; 100]);
        clock.sleep(&cx, Duration::from_millis(50)).expect("sleep 1");
        put("v2/sources/src-mid", &[3u8; 100]);
        put("v2/proc-macro/expansions/pm-mid.exp", &[4u8; 100]);
        clock.sleep(&cx, Duration::from_millis(50)).expect("sleep 2");
        put("v2/sources/src-new", &[5u8; 100]);
        put("v2/proc-macro/compiled/pm-new.cwasm", &[6u8; 100]);

        let now_ns = clock.wall().as_ns();
        let probe = FixedPids(&[1001]);

        // Dry-run mutates nothing.
        let dry = sweep_aux_inner(&cx, home.dir(), now_ns, AuxLimits::DEFAULT, 200, 200, &probe, true, AuxSweepPlant::None);
        assert_eq!(dry.files, 9, "3 legacy + 2 dead scratch + 2 old sources + 2 old proc-macro");
        assert!(home.dir().stat(&cx, &Path::new("v2/shared/old.seg").expect("p")).expect("s").is_some());

        // Real sweep at t=now_ns:
        // - sweeps all 3 legacy dirs;
        // - sweeps dead-PID scratch dirs (`bd-2002-0`, `work-2002-1`), keeps live-PID scratch dirs
        //   (`bd-1001-0`, `work-1001-1`), fresh `bd-orphan`, and `keep.txt`;
        // - LRU-evicts oldest 2 entries in `v2/sources` (cap 200 -> low 150) and oldest 2 across
        //   `v2/proc-macro/{compiled,expansions}` (cap 200 -> low 150), keeping newest + modules.
        let swept = sweep_aux_inner(&cx, home.dir(), now_ns, AuxLimits::DEFAULT, 200, 200, &probe, false, AuxSweepPlant::None);
        assert_eq!(swept.files, 9);
        for gone in [
            "v2/shared",
            "v2/own",
            "v2/daemon",
            "t/bd-2002-0",
            "t/work-2002-1",
            "v2/sources/src-old",
            "v2/sources/src-mid",
            "v2/proc-macro/compiled/pm-old.cwasm",
            "v2/proc-macro/expansions/pm-mid.exp",
        ] {
            assert!(home.dir().stat(&cx, &Path::new(gone).expect("p")).expect("s").is_none(), "expected swept: {gone}");
        }
        for kept in [
            "t/bd-1001-0/a.o",
            "t/work-1001-1/b.o",
            "t/bd-orphan/stale.o",
            "t/keep.txt",
            "v2/sources/src-new",
            "v2/proc-macro/compiled/pm-new.cwasm",
            "v2/proc-macro/modules/boot.wasm",
        ] {
            assert!(home.dir().stat(&cx, &Path::new(kept).expect("p")).expect("s").is_some(), "expected kept: {kept}");
        }

        // Past 24h (`SCRATCH_STALE_NS`): `t/bd-orphan` is swept while live PIDs and `keep.txt` survive!
        let after_24h = now_ns.saturating_add(SCRATCH_STALE_NS).saturating_add(1_000_000_000);
        let stale = sweep_aux_with(&cx, home.dir(), after_24h, &probe, false, AuxSweepPlant::None);
        assert_eq!(stale.files, 1, "only >24h stale bd-orphan swept");
        assert!(home.dir().stat(&cx, &Path::new("t/bd-orphan").expect("p")).expect("s").is_none());
        assert!(home.dir().stat(&cx, &Path::new("t/bd-1001-0/a.o").expect("p")).expect("s").is_some(), "live PID scratch survives >24h");
        assert!(home.dir().stat(&cx, &Path::new("t/work-1001-1/b.o").expect("p")).expect("s").is_some(), "live PID scratch survives >24h");
        assert!(home.dir().stat(&cx, &Path::new("t/keep.txt").expect("p")).expect("s").is_some(), "non-scratch file untouched");

        // Past 14d (`CACHE_EXPIRY_NS`): remaining `v2/sources` and `v2/proc-macro/compiled` expire,
        // while `v2/proc-macro/modules/boot.wasm` and live PID scratch dirs still survive.
        let after_14d = now_ns.saturating_add(CACHE_EXPIRY_NS).saturating_add(1_000_000_000);
        let expired = sweep_aux_with(&cx, home.dir(), after_14d, &probe, false, AuxSweepPlant::None);
        assert_eq!(expired.files, 2, "expired src-new + pm-new.cwasm swept at 14d");
        assert!(home.dir().stat(&cx, &Path::new("v2/sources/src-new").expect("p")).expect("s").is_none());
        assert!(home.dir().stat(&cx, &Path::new("v2/proc-macro/compiled/pm-new.cwasm").expect("p")).expect("s").is_none());
        assert!(home.dir().stat(&cx, &Path::new("v2/proc-macro/modules/boot.wasm").expect("p")).expect("s").is_some());

        // Planted-bug control (rule 43): ignoring PidState::Alive deletes the 2 live scratch dirs.
        let planted = sweep_aux_with(&cx, home.dir(), after_14d, &probe, false, AuxSweepPlant::IgnorePidAlive);
        assert_eq!(planted.files, 2, "planted bug control must delete both live-PID scratch dirs");
        assert!(home.dir().stat(&cx, &Path::new("t/bd-1001-0").expect("p")).expect("s").is_none());
        assert!(home.dir().stat(&cx, &Path::new("t/work-1001-1").expect("p")).expect("s").is_none());
    }
}
