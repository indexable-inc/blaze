//! `blaze gc [--max-gb <N> | --max-bytes <N>] [--dry-run] [--all-formats] [--background]`:
//! bounded disk-space maintenance for the global plain store under `<HOME>/.blaze/v2/store`.
//!
//! - Compacts/sweeps segments and runs root-level LRU eviction (down to the 75% low watermark
//!   whenever committed and live bytes exceed the 85% high watermark of the budget, default
//!   32 GiB; configurable via `BLAZE_STORE_MAX_BYTES`, `BLAZE_STORE_MAX_GB`, or `--max-gb`).
//! - Removes stale old-format store directories (`f5s2`, legacy format-4 files directly in
//!   `v2/store/`) untouched for longer than `forms::EXPIRY_DAYS` (or all other-format stores
//!   immediately with `--all-formats`) cleanly via `bstd::fs`, never touching the active
//!   `STORE_DIR` (`f5s3`) or `CACHEDIR.TAG`.
//! - When spawned detached by `after_answer_printed` (`--background`), lowers thread QoS to
//!   `QOS_CLASS_BACKGROUND` (macOS) / nice 19 + idle IO (Linux) and acquires a non-blocking
//!   exclusive advisory lock (`v2/store/gc.lock`) so at most one background GC runs at a time
//!   and foreground CLI exit is never delayed.

use core::fmt;

use blaze_cas::plain::{DEFAULT_MAX_STORE_BYTES, ExclusiveLock, Liveness, MIN_MAX_STORE_BYTES, Settings, Sliced};
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
}

impl fmt::Display for GcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GcError::InvalidBudget { source, why } => write!(f, "store budget {source}: {why}"),
            GcError::Lock(error) => write!(f, "blaze gc: lock under .blaze/{STORE_ROOT}/{GC_LOCK_LEAF}: {error}"),
            #[cfg(target_os = "macos")]
            GcError::Qos(error) => write!(f, "blaze gc --background: {error}"),
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
/// - Unset/empty yields `Ok(None)` (uses [`DEFAULT_MAX_STORE_BYTES`] = 32 GiB).
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
        let lent = home.open_store_with_liveness(&settings, facts, Liveness::Reached, heap.alloc())?;
        summary.bytes_before = lent.total_bytes();
        let ran = if opts.background {
            lent.compact(GC_SLICES_MOST)
        } else {
            lent.compact_foreground(GC_SLICES_MOST)
        }
        .map_err(|error| CliError::Store(StoreFailure::Place(PlaceError::Lent(error))))?;
        let mut totals = Sliced::default();
        for slice in &ran {
            if !slice.aborted {
                totals = totals.and(*slice);
                if slice.segments > 0 || slice.evicted_roots > 0 || slice.removed > 0 {
                    summary.slices += 1;
                }
            }
        }
        summary.evicted_roots = totals.evicted_roots;
        summary.files_unlinked = totals.removed;
        let _ = if opts.background {
            lent.seal()
        } else {
            lent.seal_foreground()
        }
        .map_err(|error| CliError::Store(StoreFailure::Place(PlaceError::Lent(error))))?;
        summary.bytes_after = lent.total_bytes();
        drop(lent);
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

/// Pure parser for leading `<pid>-...` directory names under `v2/test/tmp/`.
pub(crate) fn parse_pid_prefix(name: &str) -> Option<u32> {
    let (head, _) = name.split_once('-')?;
    let pid = head.parse::<u32>().ok()?;
    (pid > 0).then_some(pid)
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
    let effective = match (pid_state, plant) {
        #[cfg(test)]
        (Some(PidState::Alive), AuxSweepPlant::IgnorePidAlive) => Some(PidState::Dead),
        (state, _) => state,
    };
    match effective {
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

/// Best-effort bounded iterative removal of a directory tree beneath `root` (handles files,
/// symlinks, and special nodes like sockets/FIFOs left by sandboxed tests in `$TMPDIR`).
pub(crate) fn remove_tree_best_effort(cx: &Cx, root: &Dir<ReadWrite>, path: &Path) -> AuxSummary {
    remove_tree_impl(cx, root, path, false)
}

fn remove_tree_impl(cx: &Cx, root: &Dir<ReadWrite>, start: &Path, dry_run: bool) -> AuxSummary {
    use bstd::fs::{Durable, EntryKind};
    let frame = cx.frame();
    let mut summary = AuxSummary::default();
    let mut stack: blaze_own::Vec<(bstd::mem::String<&bstd::mem::Frame>, bool, usize), &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    stack.push((bstd::mem::format_in!(frame, "{}", start.as_str()), false, 0));
    let mut visited = 0usize;
    while let Some((dir_str, expanded, depth)) = stack.pop() {
        if visited >= AUX_SWEEP_MOST {
            break;
        }
        let Ok(dir_path) = Path::new(dir_str.as_str()) else {
            continue;
        };
        if expanded {
            if !dry_run {
                let _ = root.remove_dir(cx, &dir_path, Durable::No);
            }
            continue;
        }
        visited += 1;
        stack.push((dir_str, true, depth));
        let Ok(Some(entries)) = root.entries(cx, &dir_path) else {
            continue;
        };
        for i in 0..entries.len() {
            if visited >= AUX_SWEEP_MOST {
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
                    visited += 1;
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

/// Sweeps stale files in `v2/runs/`, `v2/test/tmp/`, and `v2/run/records/`.
pub(crate) fn sweep_aux_with(
    cx: &Cx,
    blaze_dir: &Dir<ReadWrite>,
    now_ns: u64,
    probe: &impl PidProbe,
    dry_run: bool,
    plant: AuxSweepPlant,
) -> AuxSummary {
    let mut total = AuxSummary::default();
    let runs = sweep_runs(cx, blaze_dir, now_ns, probe, dry_run, plant);
    let tmps = sweep_test_tmp(cx, blaze_dir, now_ns, probe, dry_run, plant);
    let recs = sweep_run_records(cx, blaze_dir, now_ns, dry_run);
    total.files = runs.files.saturating_add(tmps.files).saturating_add(recs.files);
    total.bytes = runs.bytes.saturating_add(tmps.bytes).saturating_add(recs.bytes);
    total
}

fn sweep_runs(
    cx: &Cx,
    blaze_dir: &Dir<ReadWrite>,
    now_ns: u64,
    probe: &impl PidProbe,
    dry_run: bool,
    plant: AuxSweepPlant,
) -> AuxSummary {
    use bstd::fs::{Durable, EntryKind};
    let mut out = AuxSummary::default();
    let Ok(runs_root) = Path::new(RUNS_DIR) else {
        return out;
    };
    let Ok(Some(entries)) = blaze_dir.entries(cx, &runs_root) else {
        return out;
    };
    let viewer_active = viewer_lock_is_held(cx, blaze_dir);
    let frame = cx.frame();
    let mut builds: blaze_own::Vec<(u64, Path, u64), &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    let mut alive_stems: blaze_own::Vec<bstd::mem::String<&bstd::mem::Frame>, &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    let mut telem_stems: blaze_own::Vec<bstd::mem::String<&bstd::mem::Frame>, &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
    let limit = entries.len().min(AUX_SWEEP_MOST);
    let mut meta_buf = bstd::mem::Bytes::new_in(frame, "gc run meta read");

    // Pass 1: sweep dead/orphaned `.telem` + `.meta` pairs and record alive stems.
    for i in 0..limit {
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
        if should_sweep_run(pid_state, viewer_active, mtime_ns, now_ns, plant) {
            if dry_run || blaze_dir.remove(cx, &telem_path, Durable::No).is_ok() {
                out.files = out.files.saturating_add(1);
                out.bytes = out.bytes.saturating_add(telem_len);
            }
            if let (Some(mp), Some(st)) = (meta_path.as_ref(), meta_stat) {
                if dry_run || blaze_dir.remove(cx, mp, Durable::No).is_ok() {
                    out.files = out.files.saturating_add(1);
                    out.bytes = out.bytes.saturating_add(st.len);
                }
            }
        } else if pid_state == Some(PidState::Alive) {
            alive_stems.push(bstd::mem::format_in!(frame, "{stem}"));
        }
    }

    // Pass 2: sweep orphaned `.meta` (no sibling `.telem`) and collect `.build.json` summaries.
    for i in 0..limit {
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
            if should_sweep_run(pid_state, viewer_active, mtime_ns, now_ns, plant) {
                if dry_run || blaze_dir.remove(cx, &meta_path, Durable::No).is_ok() {
                    out.files = out.files.saturating_add(1);
                    out.bytes = out.bytes.saturating_add(meta_len);
                }
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
        let expired = now_ns.saturating_sub(*mtime_ns) >= AUX_EXPIRY_NS;
        if idx >= BUILDS_KEEP_MOST || expired {
            if dry_run || blaze_dir.remove(cx, path, Durable::No).is_ok() {
                out.files = out.files.saturating_add(1);
                out.bytes = out.bytes.saturating_add(*len);
            }
        }
    }
    out
}

fn sweep_test_tmp(
    cx: &Cx,
    blaze_dir: &Dir<ReadWrite>,
    now_ns: u64,
    probe: &impl PidProbe,
    dry_run: bool,
    plant: AuxSweepPlant,
) -> AuxSummary {
    use bstd::fs::EntryKind;
    let mut out = AuxSummary::default();
    let Ok(tmp_root) = Path::new(TEST_TMP_DIR) else {
        return out;
    };
    let Ok(Some(entries)) = blaze_dir.entries(cx, &tmp_root) else {
        return out;
    };
    let limit = entries.len().min(AUX_SWEEP_MOST);
    for i in 0..limit {
        if entries.kind(i) != EntryKind::Dir {
            continue;
        }
        let Ok(path) = entries.path(i) else {
            continue;
        };
        let mtime_ns = blaze_dir.stat(cx, &path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
        let pid_state = parse_pid_prefix(entries.name(i)).map(|p| probe.probe(p));
        if should_sweep_run(pid_state, false, mtime_ns, now_ns, plant) {
            let gone = remove_tree_impl(cx, blaze_dir, &path, dry_run);
            out.files = out.files.saturating_add(gone.files);
            out.bytes = out.bytes.saturating_add(gone.bytes);
        }
    }
    out
}

fn sweep_run_records(cx: &Cx, blaze_dir: &Dir<ReadWrite>, now_ns: u64, dry_run: bool) -> AuxSummary {
    use bstd::fs::{Durable, EntryKind};
    let mut out = AuxSummary::default();
    let Ok(rec_root) = Path::new(RUN_RECORDS_DIR) else {
        return out;
    };
    let Ok(Some(entries)) = blaze_dir.entries(cx, &rec_root) else {
        return out;
    };
    let limit = entries.len().min(AUX_SWEEP_MOST);
    for i in 0..limit {
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
        let mtime_ns = blaze_dir.stat(cx, &path).ok().flatten().map_or(0, |s| s.stamp.modified_ns);
        if now_ns.saturating_sub(mtime_ns) >= AUX_EXPIRY_NS {
            let len = entries.len_of(i);
            if dry_run || blaze_dir.remove(cx, &path, Durable::No).is_ok() {
                out.files = out.files.saturating_add(1);
                out.bytes = out.bytes.saturating_add(len);
            }
        }
    }
    out
}

/// Default budget constant re-exported for tests.
#[cfg(test)]
pub(crate) const DEFAULT_BUDGET: u64 = DEFAULT_MAX_STORE_BYTES;

#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;
