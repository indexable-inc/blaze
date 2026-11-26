//! bench_wave: one-command A/B wave benchmark + Rule 68 smoke runner for blaze.
//!
//!     blaze run -q -p scripts --bin bench_wave -- '{"old": "<base>", "new": "<cand>"}'
//!     blaze run -q -p scripts --bin bench_wave -- '{"new": "<cand>", "workloads": ["smoke", "cold_hello", "mod500"]}'
//!     blaze run -q -p scripts --bin bench_wave -- selftest
//!
//! Every run isolates its homes under `/tmp/blaze-bench-wave-<pid>` and removes the tree on exit
//! (AGENTS.md rule 39), APFS/reflink-clones `$HOME/.blaze/{v2/toolchain,v2/sources,toolchains,pins,
//! native,library,versions,t,noted}` (never symlinks, since `bstd::fs` opens with `O_NOFOLLOW`),
//! alternates `old` and `new` order across trials under `BLAZE_NO_DAEMON=1` with a strict
//! per-command timeout (rule 69), parses `.blaze/v2/profile.txt` for peak RSS and `stage
//! Names`/`Heads`/`Bodies` times, and prints a GitHub-Flavored Markdown scorecard naming host
//! core count with per-core metrics (rule 84) plus ready-to-paste Rule 68 smoke output.

// `#[expect(blaze::..)]` names a blaze lint; inert under rustc.
#![feature(register_tool)]
#![register_tool(blaze)]
#![forbid(unsafe_code)]

const DOC: &str = r#"bench_wave: A/B wave benchmark and Rule 68 smoke runner for blaze.

    blaze run -q -p scripts --bin bench_wave -- '{"old": "<baseline>", "new": "<candidate>"}'
    blaze run -q -p scripts --bin bench_wave -- '{"new": "<candidate>"}'
    blaze run -q -p scripts --bin bench_wave -- selftest

The input is one JSON object (rule 42, unknown fields refused):

    old           optional path to baseline blaze binary (omitted -> single-binary mode)
    new           required path to candidate blaze binary
    trials        alternating A/B trials per workload (default 3, capped 1..=20)
    hello_world   path to hello-world fixture (default "/Volumes/Projects/tmp/hello-world")
    ripgrep       optional path to ripgrep checkout (benchmarks crates/regex cold + 1-file edit)
    workloads     subset of ["smoke", "cold_hello", "mod500", "ripgrep", "build", "noop_startup"]
                  (default ["smoke", "cold_hello", "mod500"])
    timeout_secs  per-command timeout in seconds (default 120, capped 1..=3600; rule 69)
    record_kb     optional bool (default false): upsert measured `new` rows into
                  `<kb_root>/agents/kb/baselines.jsonl`
    kb_root       optional repo root for KB baselines (default ".")
"#;

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt::Write as _,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use blaze_script::{
    TempDir,
    child::{self, Ended, Measured},
    exit, expect, fail,
    input::{self, Fields, Input, InputError},
    py_round, read_text_lossy, run_within, utc_now,
};
use serde_json::{Map, Number, Value};

const SCRIPT: &str = "bench_wave";
const WORKLOADS: [&str; 6] = [
    "smoke",
    "cold_hello",
    "mod500",
    "ripgrep",
    "build",
    "noop_startup",
];
const DEFAULT_WORKLOADS: [&str; 3] = ["smoke", "cold_hello", "mod500"];
const DEFAULT_HELLO_WORLD: &str = "/Volumes/Projects/tmp/hello-world";
const DEFAULT_KB_ROOT: &str = ".";
const DEFAULT_TRIALS: i64 = 3;
const MAX_TRIALS: i64 = 20;
const DEFAULT_TIMEOUT_SECS: i64 = 120;
const MAX_TIMEOUT_SECS: i64 = 3600;
const GIT_HEAD_TIMEOUT: Duration = Duration::from_secs(5);
const MOD500_COUNT: usize = 500;
const NOOP_STARTUP_RUNS: usize = 10;
const REMOVE_RETRY_ATTEMPTS: usize = 5;
const REMOVE_RETRY_BACKOFF: Duration = Duration::from_millis(50);
const MAX_COPY_DEPTH: usize = 64;
const ERROR_TAIL_LINES: usize = 20;

/// Entries APFS/reflink-cloned from `$HOME/.blaze` into each isolated home (never symlinked).
const HOME_SEED_ENTRIES: [&str; 9] = [
    "v2/toolchain",
    "v2/sources",
    "toolchains",
    "pins",
    "native",
    "library",
    "versions",
    "t",
    "noted",
];

/// Typed CLI JSON input (Rule 42).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    old: Option<String>,
    new: String,
    trials: usize,
    hello_world: String,
    ripgrep: Option<String>,
    workloads: Vec<String>,
    workloads_explicit: bool,
    timeout_secs: u64,
    record_kb: bool,
    kb_root: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &[
        "old",
        "new",
        "trials",
        "hello_world",
        "ripgrep",
        "workloads",
        "timeout_secs",
        "record_kb",
        "kb_root",
    ];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        let old = f.str_or_null("old")?;
        if old.as_deref().is_some_and(|s| s.trim().is_empty()) {
            return Err(InputError(format!("{} must not be empty", f.at("old"))));
        }
        let new = f.str("new", None)?;
        if new.trim().is_empty() {
            return Err(InputError(format!("{} must not be empty", f.at("new"))));
        }
        let trials_raw = f.int("trials", Some(DEFAULT_TRIALS))?;
        if !(1..=MAX_TRIALS).contains(&trials_raw) {
            return Err(InputError(format!(
                "{} must be in 1..={MAX_TRIALS}, not {trials_raw}",
                f.at("trials")
            )));
        }
        let trials = usize::try_from(trials_raw)
            .map_err(|_| InputError(format!("{} is out of range: {trials_raw}", f.at("trials"))))?;
        let hello_world = f.str("hello_world", Some(DEFAULT_HELLO_WORLD))?;
        if hello_world.trim().is_empty() {
            return Err(InputError(format!("{} must not be empty", f.at("hello_world"))));
        }
        let ripgrep = f.str_or_null("ripgrep")?;
        if ripgrep.as_deref().is_some_and(|s| s.trim().is_empty()) {
            return Err(InputError(format!("{} must not be empty", f.at("ripgrep"))));
        }
        let workloads_explicit = f.get("workloads").is_some();
        let workloads = f.ones_of("workloads", &WORKLOADS, &DEFAULT_WORKLOADS)?;
        let distinct = workloads
            .iter()
            .enumerate()
            .all(|(at, name)| !workloads[..at].contains(name));
        if workloads.is_empty() || !distinct {
            return Err(InputError(format!(
                "{} names each workload at most once, and one at least",
                f.at("workloads")
            )));
        }
        let timeout_raw = f.int("timeout_secs", Some(DEFAULT_TIMEOUT_SECS))?;
        if !(1..=MAX_TIMEOUT_SECS).contains(&timeout_raw) {
            return Err(InputError(format!(
                "{} must be in 1..={MAX_TIMEOUT_SECS}, not {timeout_raw}",
                f.at("timeout_secs")
            )));
        }
        let timeout_secs = u64::try_from(timeout_raw).map_err(|_| {
            InputError(format!("{} is out of range: {timeout_raw}", f.at("timeout_secs")))
        })?;
        let record_kb = f.bool("record_kb", Some(false))?;
        let kb_root = f.str("kb_root", Some(DEFAULT_KB_ROOT))?;
        if kb_root.trim().is_empty() {
            return Err(InputError(format!("{} must not be empty", f.at("kb_root"))));
        }
        Ok(Args {
            old,
            new,
            trials,
            hello_world,
            ripgrep,
            workloads,
            workloads_explicit,
            timeout_secs,
            record_kb,
            kb_root,
        })
    }
}

impl Args {
    /// Active workloads: honours explicit `workloads`, and when `workloads` was omitted while
    /// `ripgrep` was provided and exists on disk, appends `"ripgrep"` automatically.
    fn effective_workloads(&self) -> Vec<String> {
        let mut list = self.workloads.clone();
        if !self.workloads_explicit
            && self
                .ripgrep
                .as_deref()
                .is_some_and(|p| Path::new(p).is_dir())
            && !list.iter().any(|w| w == "ripgrep")
        {
            list.push("ripgrep".to_owned());
        }
        list
    }
}

/// Captured host environment variables read once at startup.
struct HostEnv {
    user_home: PathBuf,
    cargo_home: OsString,
    path: OsString,
    sdkroot: Option<OsString>,
}

impl HostEnv {
    #[expect(
        clippy::disallowed_methods,
        reason = "bench_wave reads HOME, PATH, CARGO_HOME and SDKROOT once at startup to seed isolated homes and build minimal child environments"
    )]
    fn capture() -> Result<HostEnv, String> {
        let user_home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| "HOME must be set to seed isolated benchmark homes".to_owned())?;
        let path = std::env::var_os("PATH")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| "PATH must be set for child commands".to_owned())?;
        let cargo_home = std::env::var_os("CARGO_HOME")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| user_home.join(".cargo").into_os_string());
        let sdkroot = std::env::var_os("SDKROOT").filter(|v| !v.is_empty());
        Ok(HostEnv { user_home, cargo_home, path, sdkroot })
    }

    /// Minimal child environment pointing `HOME` at `isolated_root` (so v2 `blaze` uses
    /// `<isolated_root>/.blaze`), `BLAZE_HOME` at `<isolated_root>/.blaze`, and `BLAZE_NO_DAEMON=1`.
    fn child_env(&self, isolated_root: &Path) -> Vec<(&'static str, OsString)> {
        let mut env = Vec::with_capacity(8);
        env.push(("HOME", isolated_root.as_os_str().to_os_string()));
        env.push(("BLAZE_HOME", isolated_root.join(".blaze").into_os_string()));
        env.push(("CARGO_HOME", self.cargo_home.clone()));
        env.push(("PATH", self.path.clone()));
        env.push(("TERM", OsString::from("dumb")));
        env.push(("LANG", OsString::from("C.UTF-8")));
        env.push(("BLAZE_NO_DAEMON", OsString::from("1")));
        if let Some(sdkroot) = &self.sdkroot {
            env.push(("SDKROOT", sdkroot.clone()));
        }
        env
    }
}

/// RAII temporary work tree under `/tmp/blaze-bench-wave-<pid>`, removed on drop (Rule 39).
struct WorkRoot {
    path: PathBuf,
}

/// Whether `err` is `ENOTEMPTY` (`DirectoryNotEmpty`, macOS os error 66, Linux os error 39).
fn is_dir_not_empty(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::DirectoryNotEmpty
        || matches!(err.raw_os_error(), Some(66 | 39))
}

/// Recursively make a directory tree owner-writable (needed for read-only cloned toolchain/native
/// directories) and remove it cleanly, retrying briefly when a detached background `blaze gc`
/// leaves a transient `DirectoryNotEmpty` (os error 66).
fn remove_tree_writable(path: &Path) -> std::io::Result<()> {
    let mut chmod_done = false;
    let mut retries = 0usize;
    loop {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied && !chmod_done => {
                chmod_done = true;
                let _ = std::process::Command::new("/bin/chmod")
                    .args(["-R", "u+w"])
                    .arg(path)
                    .status();
            }
            Err(err) if is_dir_not_empty(&err) && retries < REMOVE_RETRY_ATTEMPTS => {
                retries += 1;
                #[expect(
                    clippy::disallowed_methods,
                    reason = "bench_wave is a standalone script retrying temp-dir cleanup after a detached child exits"
                )]
                std::thread::sleep(REMOVE_RETRY_BACKOFF);
            }
            Err(err) => return Err(err),
        }
    }
}

impl WorkRoot {
    #[expect(
        blaze::tmp_path,
        reason = "bench_wave explicitly isolates wave runs under /tmp/blaze-bench-wave-<pid> and removes the tree in Drop (Rule 39)"
    )]
    fn create() -> Result<WorkRoot, String> {
        let path = PathBuf::from(format!("/tmp/blaze-bench-wave-{}", std::process::id()));
        if path.exists() {
            remove_tree_writable(&path)
                .map_err(|e| format!("failed to clear stale work dir {}: {e}", path.display()))?;
        }
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("failed to create work dir {}: {e}", path.display()))?;
        Ok(WorkRoot { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkRoot {
    fn drop(&mut self) {
        if let Err(err) = remove_tree_writable(&self.path) {
            write_stderr(&format!(
                "{SCRIPT}: warning: failed to remove {}: {err}\n",
                self.path.display()
            ));
        }
    }
}

/// Host logical core count for per-core metrics (Rule 84).
#[expect(
    clippy::disallowed_methods,
    reason = "bench_wave reports the host core count for Rule 84 per-core metrics and does not run inside blaze's worker pool"
)]
fn host_cores() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

fn write_stdout(text: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    if let Err(err) = out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        fail(SCRIPT, &format!("stdout write failed: {err}"));
    }
}

fn write_stderr(text: &str) {
    use std::io::Write as _;
    let mut err = std::io::stderr().lock();
    if err.write_all(text.as_bytes()).and_then(|()| err.flush()).is_err() {
        // Nothing left to report stderr write failures to.
    }
}

fn push_line(out: &mut String, args: std::fmt::Arguments<'_>) {
    if out.write_fmt(args).is_ok() {
        out.push('\n');
    }
}

/// Wall and CPU durations (milliseconds) parsed from a `stage <Name>` line in `.blaze/v2/profile.txt`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct StageTime {
    wall_ms: f64,
    cpu_ms: f64,
}

/// Metrics extracted from `.blaze/v2/profile.txt`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct ProfileMetrics {
    peak_rss_bytes: Option<u64>,
    names: Option<StageTime>,
    heads: Option<StageTime>,
    bodies: Option<StageTime>,
}

/// Parse a byte magnitude like `"42.3 MB"`, `"1.2 GB"`, `"512 kB"`, `"8192 B"`, `"128 MiB"`.
fn parse_byte_size(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace();
    let num_str = parts.next()?;
    let unit_raw = parts.next()?;
    let unit = unit_raw.trim_end_matches([',', ';', ')', '·']);
    let value: f64 = num_str.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let scale: f64 = match unit {
        "B" => 1.0,
        "kB" | "KB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "TB" => 1_000_000_000_000.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = (value * scale).round();
    if !bytes.is_finite() || bytes < 0.0 || bytes >= (u64::MAX as f64) {
        return None;
    }
    Some(bytes as u64)
}

/// Parse a labeled duration like `"wall 0.4 s"` or `"cpu 12.5 ms"` into milliseconds.
fn parse_labeled_ms(segment: &str, label: &str) -> Option<f64> {
    let rest = segment.trim().strip_prefix(label)?;
    let mut parts = rest.split_whitespace();
    let value: f64 = parts.next()?.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let unit = parts
        .next()
        .unwrap_or("s")
        .trim_end_matches([',', ';', ')', '·']);
    let factor = match unit {
        "s" => 1_000.0,
        "ms" => 1.0,
        "us" | "µs" => 0.001,
        "ns" => 0.000_001,
        _ => return None,
    };
    Some(value * factor)
}

/// Parse a `stage <Name>[ (running)]: tasks ... · wall <X> s · cpu <Y> s` line.
fn parse_stage_status_line(line: &str, stage_name: &str) -> Option<StageTime> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("stage ")?.strip_prefix(stage_name)?;
    let rest = rest.strip_prefix(" (running)").unwrap_or(rest);
    let body = rest.strip_prefix(':')?;
    let mut wall_ms = None;
    let mut cpu_ms = None;
    for part in body.split('·') {
        if wall_ms.is_none()
            && let Some(ms) = parse_labeled_ms(part, "wall ")
        {
            wall_ms = Some(ms);
        }
        if cpu_ms.is_none()
            && let Some(ms) = parse_labeled_ms(part, "cpu ")
        {
            cpu_ms = Some(ms);
        }
    }
    match (wall_ms, cpu_ms) {
        (Some(w), Some(c)) => Some(StageTime { wall_ms: w, cpu_ms: c }),
        _ => None,
    }
}

/// Parse a row of `engine stage times`: `Names: <phases> <wall_ms> <cpu_ms> ...`.
fn parse_engine_stage_row(line: &str, stage_name: &str) -> Option<StageTime> {
    let rest = line.trim().strip_prefix(stage_name)?.strip_prefix(':')?;
    let mut cols = rest.split_whitespace();
    let _phases: u64 = cols.next()?.parse().ok()?;
    let wall_ms: f64 = cols.next()?.parse().ok()?;
    let cpu_ms: f64 = cols.next()?.parse().ok()?;
    if wall_ms.is_finite() && cpu_ms.is_finite() && wall_ms >= 0.0 && cpu_ms >= 0.0 {
        Some(StageTime { wall_ms, cpu_ms })
    } else {
        None
    }
}

/// Parse `.blaze/v2/profile.txt` for peak RSS (`most resident ...`) and `stage
/// Names`/`Heads`/`Bodies` wall & cpu times (falling back to `engine stage times` rows if no
/// `stage <Name>:` status line appeared).
fn parse_profile(text: &str) -> ProfileMetrics {
    let mut out = ProfileMetrics::default();
    let mut in_stage_times = false;
    let mut table_names = None;
    let mut table_heads = None;
    let mut table_bodies = None;

    for line in text.lines() {
        if out.peak_rss_bytes.is_none()
            && let Some(idx) = line.find("most resident ")
        {
            let after = &line[idx + "most resident ".len()..];
            if let Some(bytes) = parse_byte_size(after) {
                out.peak_rss_bytes = Some(bytes);
            }
        }
        if out.names.is_none()
            && let Some(st) = parse_stage_status_line(line, "Names")
        {
            out.names = Some(st);
        }
        if out.heads.is_none()
            && let Some(st) = parse_stage_status_line(line, "Heads")
        {
            out.heads = Some(st);
        }
        if out.bodies.is_none()
            && let Some(st) = parse_stage_status_line(line, "Bodies")
        {
            out.bodies = Some(st);
        }

        let trimmed = line.trim();
        if trimmed.starts_with("engine stage times") {
            in_stage_times = true;
            continue;
        }
        if in_stage_times {
            if !line.starts_with(' ') {
                in_stage_times = false;
            } else {
                if table_names.is_none() {
                    table_names = parse_engine_stage_row(line, "Names");
                }
                if table_heads.is_none() {
                    table_heads = parse_engine_stage_row(line, "Heads");
                }
                if table_bodies.is_none() {
                    table_bodies = parse_engine_stage_row(line, "Bodies");
                }
            }
        }
    }

    if out.names.is_none() {
        out.names = table_names;
    }
    if out.heads.is_none() {
        out.heads = table_heads;
    }
    if out.bodies.is_none() {
        out.bodies = table_bodies;
    }
    out
}

/// One measured invocation of `blaze`.
#[derive(Debug, Clone, Copy)]
struct Sample {
    wall_ms: f64,
    peak_rss_bytes: u64,
    profile: ProfileMetrics,
}

/// Aggregated median metrics for one binary on one workload.
#[derive(Debug, Clone, Copy, Default)]
struct SideSummary {
    wall_ms: f64,
    peak_rss_bytes: u64,
    names: Option<StageTime>,
    heads: Option<StageTime>,
    bodies: Option<StageTime>,
}

/// One row of the wave benchmark scorecard.
#[derive(Debug, Clone)]
struct RowSummary {
    workload: String,
    work_bytes: u64,
    old: Option<SideSummary>,
    new: SideSummary,
}

fn median_f64(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len() & 1 == 1 {
        Some(sorted[mid])
    } else {
        Some((sorted[mid - 1] + sorted[mid]) * 0.5)
    }
}

fn median_u64(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len() & 1 == 1 {
        Some(sorted[mid])
    } else {
        let sum = u128::from(sorted[mid - 1]) + u128::from(sorted[mid]);
        Some(u64::try_from(sum / 2).unwrap_or(u64::MAX))
    }
}

fn median_stage(values: &[Option<StageTime>]) -> Option<StageTime> {
    let mut walls = Vec::new();
    let mut cpus = Vec::new();
    for st in values.iter().flatten() {
        walls.push(st.wall_ms);
        cpus.push(st.cpu_ms);
    }
    match (median_f64(&walls), median_f64(&cpus)) {
        (Some(wall_ms), Some(cpu_ms)) => Some(StageTime { wall_ms, cpu_ms }),
        _ => None,
    }
}

fn summarize_samples(samples: &[Sample]) -> SideSummary {
    let walls: Vec<f64> = samples.iter().map(|s| s.wall_ms).collect();
    let rsses: Vec<u64> = samples.iter().map(|s| s.peak_rss_bytes).collect();
    let names: Vec<Option<StageTime>> = samples.iter().map(|s| s.profile.names).collect();
    let heads: Vec<Option<StageTime>> = samples.iter().map(|s| s.profile.heads).collect();
    let bodies: Vec<Option<StageTime>> = samples.iter().map(|s| s.profile.bodies).collect();
    SideSummary {
        wall_ms: median_f64(&walls).unwrap_or(0.0),
        peak_rss_bytes: median_u64(&rsses).unwrap_or(0),
        names: median_stage(&names),
        heads: median_stage(&heads),
        bodies: median_stage(&bodies),
    }
}

fn speedup(old_ms: f64, new_ms: f64) -> Option<f64> {
    if old_ms > 0.0 && new_ms > 0.0 && old_ms.is_finite() && new_ms.is_finite() {
        Some(old_ms / new_ms)
    } else {
        None
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "-".to_owned();
    }
    const UNITS: [(f64, &str); 4] = [
        (1_000_000_000_000.0, "TB"),
        (1_000_000_000.0, "GB"),
        (1_000_000.0, "MB"),
        (1_000.0, "kB"),
    ];
    let val = bytes as f64;
    for (unit, label) in UNITS {
        if val >= unit {
            return format!("{:.1} {label}", val / unit);
        }
    }
    format!("{bytes} B")
}

fn format_stages(side: &SideSummary) -> String {
    let mut parts = Vec::new();
    for (tag, st) in [("N", side.names), ("H", side.heads), ("B", side.bodies)] {
        if let Some(st) = st {
            parts.push(format!("{tag}:{:.0}/{:.0}ms", st.wall_ms, st.cpu_ms));
        }
    }
    if parts.is_empty() { "-".to_owned() } else { parts.join(" ") }
}

/// Rule 84 per-core metric: `MB/s/core` (or `kB/s/core` for tiny fixtures) plus `core-ms`.
fn format_per_core(wall_ms: f64, cores: usize, work_bytes: u64) -> String {
    if !wall_ms.is_finite() || wall_ms <= 0.0 || cores == 0 {
        return "-".to_owned();
    }
    let core_ms = wall_ms * (cores as f64);
    if work_bytes > 0 {
        let secs = wall_ms / 1_000.0;
        let mb_per_s_per_core = (work_bytes as f64 / 1_000_000.0) / secs / (cores as f64);
        if mb_per_s_per_core >= 0.01 {
            format!("{mb_per_s_per_core:.3} MB/s/core ({core_ms:.1} core-ms)")
        } else {
            let kb_per_s_per_core = mb_per_s_per_core * 1_000.0;
            format!("{kb_per_s_per_core:.2} kB/s/core ({core_ms:.1} core-ms)")
        }
    } else {
        format!("{core_ms:.1} core-ms")
    }
}

/// Render the GitHub-Flavored Markdown benchmark scorecard (Rule 84).
fn render_markdown(
    platform: &str,
    cores: usize,
    trials: usize,
    old_bin: Option<&str>,
    new_bin: &str,
    rows: &[RowSummary],
) -> String {
    let mut out = String::new();
    push_line(
        &mut out,
        format_args!("## Wave Benchmark Scorecard ({platform}, {cores} cores, {trials} trials)"),
    );
    out.push('\n');
    if let Some(old) = old_bin {
        push_line(&mut out, format_args!("- **Baseline (`old`)**: `{old}`"));
    }
    push_line(&mut out, format_args!("- **Candidate (`new`)**: `{new_bin}`"));
    out.push('\n');

    if old_bin.is_some() {
        out.push_str(
            "| Workload | Old Wall (ms) | New Wall (ms) | Speedup (old/new) | Old RSS | New RSS | Per-Core (new) | Stages (new: wall/cpu) |\n",
        );
        out.push_str("|---|---:|---:|---:|---:|---:|---|---|\n");
        for row in rows {
            let (old_wall, old_rss, sp) = match row.old {
                Some(old) => (
                    format!("{:.1}", old.wall_ms),
                    format_bytes(old.peak_rss_bytes),
                    speedup(old.wall_ms, row.new.wall_ms)
                        .map_or_else(|| "-".to_owned(), |s| format!("{s:.2}x")),
                ),
                None => ("-".to_owned(), "-".to_owned(), "-".to_owned()),
            };
            push_line(
                &mut out,
                format_args!(
                    "| {} | {} | {:.1} | {} | {} | {} | {} | {} |",
                    row.workload,
                    old_wall,
                    row.new.wall_ms,
                    sp,
                    old_rss,
                    format_bytes(row.new.peak_rss_bytes),
                    format_per_core(row.new.wall_ms, cores, row.work_bytes),
                    format_stages(&row.new),
                ),
            );
        }
    } else {
        out.push_str(
            "| Workload | Wall (ms) | Peak RSS | Per-Core | Stages (wall/cpu) |\n",
        );
        out.push_str("|---|---:|---:|---|---|\n");
        for row in rows {
            push_line(
                &mut out,
                format_args!(
                    "| {} | {:.1} | {} | {} | {} |",
                    row.workload,
                    row.new.wall_ms,
                    format_bytes(row.new.peak_rss_bytes),
                    format_per_core(row.new.wall_ms, cores, row.work_bytes),
                    format_stages(&row.new),
                ),
            );
        }
    }
    out
}

/// Aggregated results for the `"noop_startup"` workload (10 alternating warm no-op `blaze check`
/// runs + 10 alternating `blaze --version` runs + binary sizes in bytes).
#[derive(Debug, Clone)]
struct NoopStartupSummary {
    runs: usize,
    check_row: RowSummary,
    version_row: RowSummary,
    old_bin_bytes: Option<u64>,
    new_bin_bytes: u64,
}

fn format_mb(bytes: u64) -> String {
    format!("{:.2} MB", bytes as f64 / 1_000_000.0)
}

fn format_size_delta(old_bytes: u64, new_bytes: u64) -> String {
    let delta_mb = (new_bytes as f64 - old_bytes as f64) / 1_000_000.0;
    if old_bytes > 0 {
        let pct = (new_bytes as f64 - old_bytes as f64) * 100.0 / (old_bytes as f64);
        format!("{delta_mb:+.2} MB ({pct:+.1}%)")
    } else {
        format!("{delta_mb:+.2} MB")
    }
}

/// Render the concise `"noop_startup"` summary (high-precision median ms, speedup, and binary size
/// in MB).
fn render_noop_startup_section(
    platform: &str,
    cores: usize,
    summary: &NoopStartupSummary,
) -> String {
    let mut out = String::new();
    let runs = summary.runs;
    if let (Some(old_check), Some(old_ver), Some(old_bytes)) = (
        summary.check_row.old,
        summary.version_row.old,
        summary.old_bin_bytes,
    ) {
        push_line(
            &mut out,
            format_args!(
                "## No-op Startup & Binary Size ({platform}, {cores} cores, {runs} alternating runs)"
            ),
        );
        out.push('\n');
        out.push_str("| Metric | Old | New | Speedup / Delta |\n");
        out.push_str("|---|---:|---:|---:|\n");
        let check_sp = speedup(old_check.wall_ms, summary.check_row.new.wall_ms)
            .map_or_else(|| "-".to_owned(), |s| format!("{s:.2}x"));
        let ver_sp = speedup(old_ver.wall_ms, summary.version_row.new.wall_ms)
            .map_or_else(|| "-".to_owned(), |s| format!("{s:.2}x"));
        push_line(
            &mut out,
            format_args!(
                "| warm no-op `blaze check` (median) | {:.2} ms | {:.2} ms | {check_sp} |",
                old_check.wall_ms, summary.check_row.new.wall_ms
            ),
        );
        push_line(
            &mut out,
            format_args!(
                "| `blaze --version` (median) | {:.2} ms | {:.2} ms | {ver_sp} |",
                old_ver.wall_ms, summary.version_row.new.wall_ms
            ),
        );
        push_line(
            &mut out,
            format_args!(
                "| binary size | {} | {} | {} |",
                format_mb(old_bytes),
                format_mb(summary.new_bin_bytes),
                format_size_delta(old_bytes, summary.new_bin_bytes),
            ),
        );
    } else {
        push_line(
            &mut out,
            format_args!("## No-op Startup & Binary Size ({platform}, {cores} cores, {runs} runs)"),
        );
        out.push('\n');
        out.push_str("| Metric | New |\n");
        out.push_str("|---|---:|\n");
        push_line(
            &mut out,
            format_args!(
                "| warm no-op `blaze check` (median) | {:.2} ms |",
                summary.check_row.new.wall_ms
            ),
        );
        push_line(
            &mut out,
            format_args!(
                "| `blaze --version` (median) | {:.2} ms |",
                summary.version_row.new.wall_ms
            ),
        );
        push_line(
            &mut out,
            format_args!("| binary size | {} |", format_mb(summary.new_bin_bytes)),
        );
    }
    out
}

/// One step of Rule 68 smoke (`check`, `run`, `test`).
#[derive(Debug, Clone)]
struct SmokeStep {
    command: String,
    passed: bool,
    status_label: String,
    wall_ms: f64,
    output_lines: Vec<String>,
    failure_note: Option<String>,
}

/// Format Rule 68 smoke output ready to paste directly into a pull request.
fn render_smoke_quote(
    binary: &str,
    platform: &str,
    cores: usize,
    steps: &[SmokeStep],
) -> String {
    let mut out = String::new();
    let all_ok = steps.iter().all(|s| s.passed);
    let verdict = if all_ok { "PASS" } else { "FAILED" };
    push_line(
        &mut out,
        format_args!("## Rule 68 Smoke ({verdict} — {platform}, {cores} cores)"),
    );
    out.push('\n');
    out.push_str("```text\n");
    push_line(&mut out, format_args!("binary: {binary}"));
    push_line(&mut out, format_args!("host: {platform} ({cores} cores)"));
    for step in steps {
        push_line(&mut out, format_args!("$ {}", step.command));
        for line in &step.output_lines {
            push_line(&mut out, format_args!("{line}"));
        }
        let mark = if step.passed { "ok" } else { "FAILED" };
        push_line(
            &mut out,
            format_args!("[{mark}: {}, {:.1} ms]", step.status_label, step.wall_ms),
        );
        if let Some(why) = &step.failure_note {
            push_line(&mut out, format_args!("  reason: {why}"));
        }
    }
    push_line(&mut out, format_args!("verdict: {verdict}"));
    out.push_str("```\n");
    out
}

/// APFS-clone (`cp -Rc` on macOS) or reflink-copy (`cp -a --reflink=auto` on Linux) one entry.
fn clone_entry(src: &Path, dst: &Path, timeout: Duration) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create parent {}: {e}", parent.display()))?;
    }
    let src_s = src
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 source path: {}", src.display()))?;
    let dst_s = dst
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 destination path: {}", dst.display()))?;
    let cwd = dst.parent().unwrap_or_else(|| Path::new("/"));
    let mac_argv = ["/bin/cp", "-Rc", src_s, dst_s];
    let linux_argv = ["cp", "-a", "--reflink=auto", src_s, dst_s];
    let argv: &[&str] = if cfg!(target_os = "macos") { &mac_argv } else { &linux_argv };
    match run_within(argv, cwd, timeout)? {
        None => Err(format!(
            "cloning {} -> {} timed out after {}s",
            src.display(),
            dst.display(),
            timeout.as_secs()
        )),
        Some(ran) if ran.status == 0 => Ok(()),
        Some(ran) => Err(format!(
            "cloning {} -> {} failed (exit {}): {}",
            src.display(),
            dst.display(),
            ran.status,
            ran.err().trim()
        )),
    }
}

/// Prepare a fresh isolated home `<isolated_root>/.blaze` seeded by APFS/reflink cloning
/// `$HOME/.blaze/{v2/toolchain,v2/sources,toolchains,pins,native,library,versions,t,noted}`.
/// Never creates symlinks (since `bstd::fs` uses `O_NOFOLLOW`).
fn seed_isolated_home(
    user_home: &Path,
    isolated_root: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let src_blaze = user_home.join(".blaze");
    let dst_blaze = isolated_root.join(".blaze");
    std::fs::create_dir_all(&dst_blaze)
        .map_err(|e| format!("failed to create {}: {e}", dst_blaze.display()))?;
    for rel in HOME_SEED_ENTRIES {
        let src = src_blaze.join(rel);
        if src.symlink_metadata().is_ok() {
            let dst = dst_blaze.join(rel);
            clone_entry(&src, &dst, timeout)?;
        }
    }
    Ok(())
}

/// Recursively copy regular files and directories from `src` to `dst`, skipping directory names in
/// `exclude` and never creating symlinks.
fn copy_tree_excluding(
    src: &Path,
    dst: &Path,
    exclude: &[&str],
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_COPY_DEPTH {
        return Err(format!(
            "directory nesting exceeded {MAX_COPY_DEPTH} while copying {}",
            src.display()
        ));
    }
    std::fs::create_dir_all(dst)
        .map_err(|e| format!("failed to create {}: {e}", dst.display()))?;
    let entries = std::fs::read_dir(src)
        .map_err(|e| format!("failed to read dir {}: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("dir entry error in {}: {e}", src.display()))?;
        let name = entry.file_name();
        if name.to_str().is_some_and(|s| exclude.contains(&s)) {
            continue;
        }
        let ftype = entry
            .file_type()
            .map_err(|e| format!("file_type error for {}: {e}", entry.path().display()))?;
        let from = entry.path();
        let to = dst.join(&name);
        if ftype.is_dir() {
            copy_tree_excluding(&from, &to, exclude, depth + 1)?;
        } else if ftype.is_file() {
            std::fs::copy(&from, &to).map_err(|e| {
                format!("failed to copy {} -> {}: {e}", from.display(), to.display())
            })?;
        }
    }
    Ok(())
}

/// Total `.rs` source bytes under `root` (skipping `.blaze`, `target`, `.git`).
fn count_rust_bytes(root: &Path, depth: usize) -> u64 {
    if depth > MAX_COPY_DEPTH {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|s| matches!(s, ".blaze" | "target" | ".git"))
        {
            continue;
        }
        let Ok(ftype) = entry.file_type() else {
            continue;
        };
        if ftype.is_dir() {
            total = total.saturating_add(count_rust_bytes(&entry.path(), depth + 1));
        } else if ftype.is_file()
            && entry.path().extension() == Some(OsStr::new("rs"))
            && let Ok(meta) = entry.metadata()
        {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// Generate a synthetic `count`-module Rust crate in `dir` for cold + 1-file edit benchmarks.
fn generate_mod500(dir: &Path, count: usize) -> Result<(), String> {
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&src_dir)
        .map_err(|e| format!("failed to create {}: {e}", src_dir.display()))?;
    let cargo_toml = "[package]\nname = \"mod500\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n";
    std::fs::write(dir.join("Cargo.toml"), cargo_toml)
        .map_err(|e| format!("failed to write Cargo.toml in {}: {e}", dir.display()))?;
    let mut lib_rs = String::with_capacity(count * 64);
    for i in 0..count {
        push_line(&mut lib_rs, format_args!("pub mod m{i:03};"));
        write_mod500_module(&src_dir, i, 0)?;
    }
    lib_rs.push_str("\npub fn total(seed: u64) -> u64 {\n    let mut acc = 0u64;\n");
    for i in 0..count {
        push_line(
            &mut lib_rs,
            format_args!("    acc = acc.wrapping_add(m{i:03}::compute(seed));"),
        );
    }
    lib_rs.push_str("    acc\n}\n");
    std::fs::write(src_dir.join("lib.rs"), lib_rs)
        .map_err(|e| format!("failed to write src/lib.rs in {}: {e}", dir.display()))?;
    Ok(())
}

fn write_mod500_module(src_dir: &Path, idx: usize, salt: u64) -> Result<(), String> {
    let mut body = String::with_capacity(512);
    push_line(&mut body, format_args!("#[derive(Clone, Copy, Debug, PartialEq, Eq)]"));
    push_line(&mut body, format_args!("pub struct Item{idx:03} {{"));
    push_line(&mut body, format_args!("    pub value: u64,"));
    push_line(&mut body, format_args!("    pub tag: u32,"));
    push_line(&mut body, format_args!("}}"));
    body.push('\n');
    push_line(&mut body, format_args!("impl Item{idx:03} {{"));
    push_line(&mut body, format_args!("    pub const fn new(seed: u64) -> Self {{"));
    push_line(
        &mut body,
        format_args!("        Self {{ value: seed.wrapping_add({idx}), tag: {idx} }}"),
    );
    push_line(&mut body, format_args!("    }}"));
    body.push('\n');
    push_line(&mut body, format_args!("    pub const fn score(&self) -> u64 {{"));
    push_line(
        &mut body,
        format_args!("        self.value.wrapping_mul(31).wrapping_add(self.tag as u64)"),
    );
    push_line(&mut body, format_args!("    }}"));
    push_line(&mut body, format_args!("}}"));
    body.push('\n');
    push_line(&mut body, format_args!("pub fn compute(x: u64) -> u64 {{"));
    push_line(&mut body, format_args!("    let item = Item{idx:03}::new(x);"));
    push_line(&mut body, format_args!("    item.score().wrapping_add({salt})"));
    push_line(&mut body, format_args!("}}"));
    let path = src_dir.join(format!("m{idx:03}.rs"));
    std::fs::write(&path, body).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// Run a child command with isolated env, output logs, and strict timeout (Rule 69).
fn run_bounded_cmd(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, OsString)],
    log_stem: &Path,
    timeout: Duration,
) -> Result<Ended, String> {
    if let Some(parent) = log_stem.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create log dir {}: {e}", parent.display()))?;
    }
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(program.display().to_string());
    for arg in args {
        argv.push((*arg).to_owned());
    }
    child::run_logged(&argv, cwd, env, log_stem, timeout)
}

fn read_profile_from(candidates: &[&Path]) -> ProfileMetrics {
    for root in candidates {
        let path = root.join(".blaze").join("v2").join("profile.txt");
        if let Ok(text) = read_text_lossy(&path) {
            return parse_profile(&text);
        }
    }
    ProfileMetrics::default()
}

fn tail_lines(path: &Path, limit: usize) -> String {
    match read_text_lossy(path) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(limit);
            lines[start..].join("\n")
        }
        Err(_) => String::new(),
    }
}

/// Execute `blaze` for a benchmark trial, assert exit 0, and return wall/RSS/profile metrics.
fn measure_blaze(
    bin: &Path,
    args: &[&str],
    cwd: &Path,
    profile_roots: &[&Path],
    env: &[(&str, OsString)],
    log_stem: &Path,
    timeout: Duration,
    label: &str,
) -> Result<Sample, String> {
    match run_bounded_cmd(bin, args, cwd, env, log_stem, timeout)? {
        Ended::TimedOut { after } => Err(format!(
            "{label}: timed out after {}s (cmd: {} {})",
            after.as_secs(),
            bin.display(),
            args.join(" ")
        )),
        Ended::Exited(Measured { wall_ns, rss_bytes, status, .. }) => {
            if status != 0 {
                let err_tail = tail_lines(&log_stem.with_extension("err"), ERROR_TAIL_LINES);
                let out_tail = tail_lines(&log_stem.with_extension("out"), ERROR_TAIL_LINES);
                return Err(format!(
                    "{label}: exited {status} (cmd: {} {})\nstderr:\n{err_tail}\nstdout:\n{out_tail}",
                    bin.display(),
                    args.join(" ")
                ));
            }
            let profile = read_profile_from(profile_roots);
            let peak_rss_bytes = profile.peak_rss_bytes.unwrap_or(rss_bytes);
            Ok(Sample {
                wall_ms: wall_ns as f64 / 1_000_000.0,
                peak_rss_bytes,
                profile,
            })
        }
    }
}

/// Run exact Rule 68 smoke (`check`, `run` asserting `Hello, world!`, `test` asserting exit 0).
fn run_smoke(
    bin: &Path,
    hello_fixture: &Path,
    work_root: &Path,
    host_env: &HostEnv,
    timeout: Duration,
) -> Result<Vec<SmokeStep>, String> {
    if !hello_fixture.is_dir() {
        return Err(format!(
            "hello_world fixture directory does not exist: {}",
            hello_fixture.display()
        ));
    }
    let smoke_dir = work_root.join("smoke");
    let iso_home = smoke_dir.join("home");
    let proj_dir = smoke_dir.join("proj");
    let log_dir = smoke_dir.join("logs");
    seed_isolated_home(&host_env.user_home, &iso_home, timeout)?;
    copy_tree_excluding(hello_fixture, &proj_dir, &[".blaze", "target"], 0)?;
    let env = host_env.child_env(&iso_home);

    let mut steps = Vec::with_capacity(3);
    let specs: [(&str, &[&str], Option<&str>); 3] = [
        ("check", &["check"], None),
        ("run", &["run"], Some("Hello, world!")),
        ("test", &["test"], None),
    ];
    for (name, cmd_args, must_contain) in specs {
        let log_stem = log_dir.join(name);
        let command = format!("{} {}", bin.display(), cmd_args.join(" "));
        match run_bounded_cmd(bin, cmd_args, &proj_dir, &env, &log_stem, timeout)? {
            Ended::TimedOut { after } => {
                steps.push(SmokeStep {
                    command,
                    passed: false,
                    status_label: format!("TIMEOUT {}s", after.as_secs()),
                    wall_ms: after.as_secs_f64() * 1_000.0,
                    output_lines: Vec::new(),
                    failure_note: Some(format!("command timed out after {}s", after.as_secs())),
                });
            }
            Ended::Exited(Measured { wall_ns, status, .. }) => {
                let stdout = read_text_lossy(&log_stem.with_extension("out")).unwrap_or_default();
                let stderr = read_text_lossy(&log_stem.with_extension("err")).unwrap_or_default();
                let mut output_lines: Vec<String> =
                    stdout.lines().map(str::to_owned).collect();
                if output_lines.is_empty() {
                    output_lines.extend(stderr.lines().map(str::to_owned));
                }
                let mut failure_note = None;
                if status != 0 {
                    failure_note = Some(format!("exited {status}"));
                } else if let Some(needle) = must_contain
                    && !stdout.contains(needle)
                {
                    failure_note = Some(format!("stdout did not contain `{needle}`"));
                }
                let passed = failure_note.is_none();
                let status_label = if status == -1 {
                    "signal".to_owned()
                } else {
                    format!("exit {status}")
                };
                steps.push(SmokeStep {
                    command,
                    passed,
                    status_label,
                    wall_ms: wall_ns as f64 / 1_000_000.0,
                    output_lines,
                    failure_note,
                });
            }
        }
    }
    Ok(steps)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

impl Side {
    fn tag(self) -> &'static str {
        match self {
            Side::Old => "old",
            Side::New => "new",
        }
    }
}

fn trial_order(trial: usize, has_old: bool) -> &'static [Side] {
    if !has_old {
        &[Side::New]
    } else if trial & 1 == 0 {
        &[Side::Old, Side::New]
    } else {
        &[Side::New, Side::Old]
    }
}

fn bench_cold_hello(
    old_bin: Option<&Path>,
    new_bin: &Path,
    trials: usize,
    hello_fixture: &Path,
    work_root: &Path,
    host_env: &HostEnv,
    timeout: Duration,
) -> Result<RowSummary, String> {
    if !hello_fixture.is_dir() {
        return Err(format!(
            "hello_world fixture directory does not exist: {}",
            hello_fixture.display()
        ));
    }
    let work_bytes = count_rust_bytes(hello_fixture, 0);
    let mut old_samples = Vec::new();
    let mut new_samples = Vec::new();
    for trial in 0..trials {
        for &side in trial_order(trial, old_bin.is_some()) {
            let bin = match side {
                Side::Old => old_bin.ok_or_else(|| "missing old binary".to_owned())?,
                Side::New => new_bin,
            };
            let run_dir = work_root
                .join("cold_hello")
                .join(format!("t{trial}-{}", side.tag()));
            let iso_home = run_dir.join("home");
            let proj_dir = run_dir.join("proj");
            seed_isolated_home(&host_env.user_home, &iso_home, timeout)?;
            copy_tree_excluding(hello_fixture, &proj_dir, &[".blaze", "target"], 0)?;
            let env = host_env.child_env(&iso_home);
            let sample = measure_blaze(
                bin,
                &["check"],
                &proj_dir,
                &[proj_dir.as_path()],
                &env,
                &run_dir.join("logs").join("check"),
                timeout,
                &format!("cold_hello[t{trial} {}]", side.tag()),
            )?;
            match side {
                Side::Old => old_samples.push(sample),
                Side::New => new_samples.push(sample),
            }
            if let Err(err) = remove_tree_writable(&run_dir) {
                write_stderr(&format!(
                    "{SCRIPT}: warning: failed to clean {}: {err}\n",
                    run_dir.display()
                ));
            }
        }
    }
    Ok(RowSummary {
        workload: "cold_hello (check)".to_owned(),
        work_bytes,
        old: old_bin.map(|_| summarize_samples(&old_samples)),
        new: summarize_samples(&new_samples),
    })
}

fn bench_mod500(
    old_bin: Option<&Path>,
    new_bin: &Path,
    trials: usize,
    work_root: &Path,
    host_env: &HostEnv,
    timeout: Duration,
) -> Result<[RowSummary; 2], String> {
    let mut old_cold = Vec::new();
    let mut new_cold = Vec::new();
    let mut old_edit = Vec::new();
    let mut new_edit = Vec::new();
    let mut work_bytes = 0u64;

    for trial in 0..trials {
        for &side in trial_order(trial, old_bin.is_some()) {
            let bin = match side {
                Side::Old => old_bin.ok_or_else(|| "missing old binary".to_owned())?,
                Side::New => new_bin,
            };
            let run_dir = work_root
                .join("mod500")
                .join(format!("t{trial}-{}", side.tag()));
            let iso_home = run_dir.join("home");
            let proj_dir = run_dir.join("proj");
            seed_isolated_home(&host_env.user_home, &iso_home, timeout)?;
            generate_mod500(&proj_dir, MOD500_COUNT)?;
            if work_bytes == 0 {
                work_bytes = count_rust_bytes(&proj_dir, 0);
            }
            let env = host_env.child_env(&iso_home);
            let cold = measure_blaze(
                bin,
                &["check"],
                &proj_dir,
                &[proj_dir.as_path()],
                &env,
                &run_dir.join("logs").join("cold"),
                timeout,
                &format!("mod500 cold[t{trial} {}]", side.tag()),
            )?;
            write_mod500_module(&proj_dir.join("src"), MOD500_COUNT - 1, 1000 + (trial as u64))?;
            let edit = measure_blaze(
                bin,
                &["check"],
                &proj_dir,
                &[proj_dir.as_path()],
                &env,
                &run_dir.join("logs").join("edit"),
                timeout,
                &format!("mod500 1-file edit[t{trial} {}]", side.tag()),
            )?;
            match side {
                Side::Old => {
                    old_cold.push(cold);
                    old_edit.push(edit);
                }
                Side::New => {
                    new_cold.push(cold);
                    new_edit.push(edit);
                }
            }
            if let Err(err) = remove_tree_writable(&run_dir) {
                write_stderr(&format!(
                    "{SCRIPT}: warning: failed to clean {}: {err}\n",
                    run_dir.display()
                ));
            }
        }
    }
    Ok([
        RowSummary {
            workload: "mod500 (cold)".to_owned(),
            work_bytes,
            old: old_bin.map(|_| summarize_samples(&old_cold)),
            new: summarize_samples(&new_cold),
        },
        RowSummary {
            workload: "mod500 (1-file edit)".to_owned(),
            work_bytes,
            old: old_bin.map(|_| summarize_samples(&old_edit)),
            new: summarize_samples(&new_edit),
        },
    ])
}

/// Pick a Rust source file inside `crate_dir/src` for a harmless 1-file edit.
fn pick_edit_target(crate_dir: &Path) -> Result<PathBuf, String> {
    let lib_rs = crate_dir.join("src").join("lib.rs");
    if lib_rs.is_file() {
        return Ok(lib_rs);
    }
    let src_dir = crate_dir.join("src");
    let entries = std::fs::read_dir(&src_dir)
        .map_err(|e| format!("failed to read {}: {e}", src_dir.display()))?;
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension() == Some(OsStr::new("rs")) {
            candidates.push(path);
        }
    }
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| format!("no .rs file found under {}", src_dir.display()))
}

fn bench_ripgrep(
    old_bin: Option<&Path>,
    new_bin: &Path,
    trials: usize,
    ripgrep_root: &Path,
    work_root: &Path,
    host_env: &HostEnv,
    timeout: Duration,
) -> Result<[RowSummary; 2], String> {
    let regex_sub = ripgrep_root.join("crates").join("regex");
    if !regex_sub.is_dir() {
        return Err(format!(
            "ripgrep checkout missing crates/regex directory: {}",
            regex_sub.display()
        ));
    }
    let work_bytes = count_rust_bytes(&regex_sub, 0);
    let mut old_cold = Vec::new();
    let mut new_cold = Vec::new();
    let mut old_edit = Vec::new();
    let mut new_edit = Vec::new();

    for trial in 0..trials {
        for &side in trial_order(trial, old_bin.is_some()) {
            let bin = match side {
                Side::Old => old_bin.ok_or_else(|| "missing old binary".to_owned())?,
                Side::New => new_bin,
            };
            let run_dir = work_root
                .join("ripgrep")
                .join(format!("t{trial}-{}", side.tag()));
            let iso_home = run_dir.join("home");
            let rg_copy = run_dir.join("ripgrep");
            seed_isolated_home(&host_env.user_home, &iso_home, timeout)?;
            copy_tree_excluding(ripgrep_root, &rg_copy, &[".blaze", "target", ".git"], 0)?;
            let crate_dir = rg_copy.join("crates").join("regex");
            // Scope the temp copy's root manifest to a virtual workspace over `crates/regex` and
            // `crates/matcher`, keeping `rg_copy/Cargo.lock` and `[workspace.package]` intact.
            std::fs::write(
                rg_copy.join("Cargo.toml"),
                "[workspace]\nmembers = [\"crates/regex\", \"crates/matcher\"]\nresolver = \"2\"\n\n[workspace.package]\nedition = \"2024\"\nrust-version = \"1.96\"\n",
            )
            .map_err(|e| format!("failed to write scoped ripgrep Cargo.toml: {e}"))?;
            let env = host_env.child_env(&iso_home);
            let profile_roots = [rg_copy.as_path(), crate_dir.as_path()];

            let cold = measure_blaze(
                bin,
                &["check"],
                &rg_copy,
                &profile_roots,
                &env,
                &run_dir.join("logs").join("cold"),
                timeout,
                &format!("ripgrep crates/regex cold[t{trial} {}]", side.tag()),
            )?;

            let edit_file = pick_edit_target(&crate_dir)?;
            let mut text = std::fs::read_to_string(&edit_file)
                .map_err(|e| format!("failed to read {}: {e}", edit_file.display()))?;
            push_line(
                &mut text,
                format_args!(
                    "\n#[allow(dead_code)]\nfn __blaze_bench_wave_edit() -> u32 {{ {} }}",
                    trial + 1
                ),
            );
            std::fs::write(&edit_file, text)
                .map_err(|e| format!("failed to edit {}: {e}", edit_file.display()))?;

            let edit = measure_blaze(
                bin,
                &["check"],
                &rg_copy,
                &profile_roots,
                &env,
                &run_dir.join("logs").join("edit"),
                timeout,
                &format!("ripgrep crates/regex 1-file edit[t{trial} {}]", side.tag()),
            )?;

            match side {
                Side::Old => {
                    old_cold.push(cold);
                    old_edit.push(edit);
                }
                Side::New => {
                    new_cold.push(cold);
                    new_edit.push(edit);
                }
            }
            if let Err(err) = remove_tree_writable(&run_dir) {
                write_stderr(&format!(
                    "{SCRIPT}: warning: failed to clean {}: {err}\n",
                    run_dir.display()
                ));
            }
        }
    }

    Ok([
        RowSummary {
            workload: "ripgrep/crates/regex (cold)".to_owned(),
            work_bytes,
            old: old_bin.map(|_| summarize_samples(&old_cold)),
            new: summarize_samples(&new_cold),
        },
        RowSummary {
            workload: "ripgrep/crates/regex (1-file edit)".to_owned(),
            work_bytes,
            old: old_bin.map(|_| summarize_samples(&old_edit)),
            new: summarize_samples(&new_edit),
        },
    ])
}

fn bench_build(
    old_bin: Option<&Path>,
    new_bin: &Path,
    trials: usize,
    hello_fixture: &Path,
    work_root: &Path,
    host_env: &HostEnv,
    timeout: Duration,
) -> Result<RowSummary, String> {
    if !hello_fixture.is_dir() {
        return Err(format!(
            "hello_world fixture directory does not exist: {}",
            hello_fixture.display()
        ));
    }
    let work_bytes = count_rust_bytes(hello_fixture, 0);
    let mut old_samples = Vec::new();
    let mut new_samples = Vec::new();

    for trial in 0..trials {
        for &side in trial_order(trial, old_bin.is_some()) {
            let bin = match side {
                Side::Old => old_bin.ok_or_else(|| "missing old binary".to_owned())?,
                Side::New => new_bin,
            };
            let run_dir = work_root
                .join("build")
                .join(format!("t{trial}-{}", side.tag()));
            let iso_home = run_dir.join("home");
            let proj_dir = run_dir.join("proj");
            let out_bin = run_dir.join("built-bin");
            seed_isolated_home(&host_env.user_home, &iso_home, timeout)?;
            copy_tree_excluding(hello_fixture, &proj_dir, &[".blaze", "target"], 0)?;
            let env = host_env.child_env(&iso_home);
            let out_s = out_bin
                .to_str()
                .ok_or_else(|| format!("non-UTF-8 output path: {}", out_bin.display()))?;
            let sample = measure_blaze(
                bin,
                &["build", "-o", out_s],
                &proj_dir,
                &[proj_dir.as_path()],
                &env,
                &run_dir.join("logs").join("build"),
                timeout,
                &format!("build[t{trial} {}]", side.tag()),
            )?;
            if !out_bin.is_file() {
                return Err(format!(
                    "build[t{trial} {}]: expected output binary at {}",
                    side.tag(),
                    out_bin.display()
                ));
            }
            // Verify built executable runs cleanly under the same timeout.
            match run_bounded_cmd(
                &out_bin,
                &[],
                &proj_dir,
                &env,
                &run_dir.join("logs").join("built-run"),
                timeout,
            )? {
                Ended::TimedOut { after } => {
                    return Err(format!(
                        "build[t{trial} {}]: built binary timed out after {}s",
                        side.tag(),
                        after.as_secs()
                    ));
                }
                Ended::Exited(Measured { status, .. }) if status != 0 => {
                    return Err(format!(
                        "build[t{trial} {}]: built binary exited {status}",
                        side.tag()
                    ));
                }
                Ended::Exited(_) => {}
            }
            match side {
                Side::Old => old_samples.push(sample),
                Side::New => new_samples.push(sample),
            }
            if let Err(err) = remove_tree_writable(&run_dir) {
                write_stderr(&format!(
                    "{SCRIPT}: warning: failed to clean {}: {err}\n",
                    run_dir.display()
                ));
            }
        }
    }

    Ok(RowSummary {
        workload: "build (hello-world)".to_owned(),
        work_bytes,
        old: old_bin.map(|_| summarize_samples(&old_samples)),
        new: summarize_samples(&new_samples),
    })
}

struct NoopSide {
    proj_dir: PathBuf,
    log_dir: PathBuf,
    env: Vec<(&'static str, OsString)>,
}

fn setup_noop_side(
    base_dir: &Path,
    tag: &str,
    bin: &Path,
    hello_fixture: &Path,
    host_env: &HostEnv,
    timeout: Duration,
) -> Result<NoopSide, String> {
    let side_dir = base_dir.join(tag);
    let iso_home = side_dir.join("home");
    let proj_dir = side_dir.join("proj");
    let log_dir = side_dir.join("logs");
    seed_isolated_home(&host_env.user_home, &iso_home, timeout)?;
    copy_tree_excluding(hello_fixture, &proj_dir, &[".blaze", "target"], 0)?;
    let env = host_env.child_env(&iso_home);
    // Warmup outside timing to populate this binary's store & certificate.
    let _warmup = measure_blaze(
        bin,
        &["check"],
        &proj_dir,
        &[proj_dir.as_path()],
        &env,
        &log_dir.join("warmup"),
        timeout,
        &format!("noop_startup warmup [{tag}]"),
    )?;
    Ok(NoopSide { proj_dir, log_dir, env })
}

fn bench_noop_startup(
    old_bin: Option<&Path>,
    new_bin: &Path,
    hello_fixture: &Path,
    work_root: &Path,
    host_env: &HostEnv,
    timeout: Duration,
) -> Result<NoopStartupSummary, String> {
    if !hello_fixture.is_dir() {
        return Err(format!(
            "hello_world fixture directory does not exist: {}",
            hello_fixture.display()
        ));
    }
    let new_bin_bytes = std::fs::metadata(new_bin)
        .map_err(|e| format!("failed to stat new binary {}: {e}", new_bin.display()))?
        .len();
    let old_bin_bytes = match old_bin {
        Some(bin) => Some(
            std::fs::metadata(bin)
                .map_err(|e| format!("failed to stat old binary {}: {e}", bin.display()))?
                .len(),
        ),
        None => None,
    };

    let work_bytes = count_rust_bytes(hello_fixture, 0);
    let base_dir = work_root.join("noop_startup");
    // Each side gets its own isolated home & warmed checkout so differing `build_id`s between
    // `old` and `new` never invalidate each other's standing certificate during alternating runs.
    let old_side = match old_bin {
        Some(bin) => Some(setup_noop_side(&base_dir, "old", bin, hello_fixture, host_env, timeout)?),
        None => None,
    };
    let new_side = setup_noop_side(&base_dir, "new", new_bin, hello_fixture, host_env, timeout)?;

    let mut old_check = Vec::with_capacity(NOOP_STARTUP_RUNS);
    let mut new_check = Vec::with_capacity(NOOP_STARTUP_RUNS);
    for run in 0..NOOP_STARTUP_RUNS {
        for &side in trial_order(run, old_bin.is_some()) {
            let (bin, side_env) = match side {
                Side::Old => (
                    old_bin.ok_or_else(|| "missing old binary".to_owned())?,
                    old_side.as_ref().ok_or_else(|| "missing old noop side".to_owned())?,
                ),
                Side::New => (new_bin, &new_side),
            };
            let sample = measure_blaze(
                bin,
                &["check"],
                &side_env.proj_dir,
                &[side_env.proj_dir.as_path()],
                &side_env.env,
                &side_env.log_dir.join(format!("check-{run}")),
                timeout,
                &format!("noop_startup check[r{run} {}]", side.tag()),
            )?;
            match side {
                Side::Old => old_check.push(sample),
                Side::New => new_check.push(sample),
            }
        }
    }

    let mut old_version = Vec::with_capacity(NOOP_STARTUP_RUNS);
    let mut new_version = Vec::with_capacity(NOOP_STARTUP_RUNS);
    for run in 0..NOOP_STARTUP_RUNS {
        for &side in trial_order(run, old_bin.is_some()) {
            let (bin, side_env) = match side {
                Side::Old => (
                    old_bin.ok_or_else(|| "missing old binary".to_owned())?,
                    old_side.as_ref().ok_or_else(|| "missing old noop side".to_owned())?,
                ),
                Side::New => (new_bin, &new_side),
            };
            let sample = measure_blaze(
                bin,
                &["--version"],
                &side_env.proj_dir,
                &[],
                &side_env.env,
                &side_env.log_dir.join(format!("version-{run}")),
                timeout,
                &format!("noop_startup --version[r{run} {}]", side.tag()),
            )?;
            match side {
                Side::Old => old_version.push(sample),
                Side::New => new_version.push(sample),
            }
        }
    }

    if let Err(err) = remove_tree_writable(&base_dir) {
        write_stderr(&format!(
            "{SCRIPT}: warning: failed to clean {}: {err}\n",
            base_dir.display()
        ));
    }

    Ok(NoopStartupSummary {
        runs: NOOP_STARTUP_RUNS,
        check_row: RowSummary {
            workload: "noop_startup (warm check)".to_owned(),
            work_bytes,
            old: old_bin.map(|_| summarize_samples(&old_check)),
            new: summarize_samples(&new_check),
        },
        version_row: RowSummary {
            workload: "noop_startup (--version)".to_owned(),
            work_bytes: 0,
            old: old_bin.map(|_| summarize_samples(&old_version)),
            new: summarize_samples(&new_version),
        },
        old_bin_bytes,
        new_bin_bytes,
    })
}

fn canonical_binary(label: &str, raw: &str) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(raw)
        .map_err(|e| format!("{label} binary `{raw}` not accessible: {e}"))?;
    if !path.is_file() {
        return Err(format!("{label} binary `{}` is not a file", path.display()));
    }
    Ok(path)
}

fn run_wave_inner(args: &Args, work_root: &WorkRoot) -> Result<i32, String> {
    let host_env = HostEnv::capture()?;
    let new_bin = canonical_binary("new", &args.new)?;
    let old_bin = match &args.old {
        Some(raw) => Some(canonical_binary("old", raw)?),
        None => None,
    };
    let timeout = Duration::from_secs(args.timeout_secs);
    let cores = host_cores();
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let hello_fixture = Path::new(&args.hello_world);
    let workloads = args.effective_workloads();

    let mut smoke_steps: Option<Vec<SmokeStep>> = None;
    let mut noop_startup: Option<NoopStartupSummary> = None;
    let mut rows: Vec<RowSummary> = Vec::new();
    let mut exit_code = 0i32;

    for workload in &workloads {
        match workload.as_str() {
            "smoke" => {
                let steps =
                    run_smoke(&new_bin, hello_fixture, work_root.path(), &host_env, timeout)?;
                if steps.iter().any(|s| !s.passed) {
                    exit_code = 1;
                }
                smoke_steps = Some(steps);
            }
            "cold_hello" => {
                rows.push(bench_cold_hello(
                    old_bin.as_deref(),
                    &new_bin,
                    args.trials,
                    hello_fixture,
                    work_root.path(),
                    &host_env,
                    timeout,
                )?);
            }
            "mod500" => {
                let [cold, edit] = bench_mod500(
                    old_bin.as_deref(),
                    &new_bin,
                    args.trials,
                    work_root.path(),
                    &host_env,
                    timeout,
                )?;
                rows.push(cold);
                rows.push(edit);
            }
            "ripgrep" => match &args.ripgrep {
                Some(rg) if Path::new(rg).is_dir() => {
                    let [cold, edit] = bench_ripgrep(
                        old_bin.as_deref(),
                        &new_bin,
                        args.trials,
                        Path::new(rg),
                        work_root.path(),
                        &host_env,
                        timeout,
                    )?;
                    rows.push(cold);
                    rows.push(edit);
                }
                Some(rg) => {
                    write_stderr(&format!(
                        "{SCRIPT}: skipping ripgrep workload: path does not exist: {rg}\n"
                    ));
                }
                None => {
                    write_stderr(&format!(
                        "{SCRIPT}: skipping ripgrep workload: `ripgrep` path not provided\n"
                    ));
                }
            },
            "build" => {
                rows.push(bench_build(
                    old_bin.as_deref(),
                    &new_bin,
                    args.trials,
                    hello_fixture,
                    work_root.path(),
                    &host_env,
                    timeout,
                )?);
            }
            "noop_startup" => {
                let summary = bench_noop_startup(
                    old_bin.as_deref(),
                    &new_bin,
                    hello_fixture,
                    work_root.path(),
                    &host_env,
                    timeout,
                )?;
                rows.push(summary.check_row.clone());
                rows.push(summary.version_row.clone());
                noop_startup = Some(summary);
            }
            other => {
                return Err(format!("unknown workload `{other}`"));
            }
        }
    }

    let mut report = String::new();
    let new_display = new_bin.display().to_string();
    if !rows.is_empty() {
        let old_display = old_bin.as_ref().map(|p| p.display().to_string());
        report.push_str(&render_markdown(
            &platform,
            cores,
            args.trials,
            old_display.as_deref(),
            &new_display,
            &rows,
        ));
    }
    if let Some(summary) = &noop_startup {
        if !report.is_empty() {
            report.push('\n');
        }
        report.push_str(&render_noop_startup_section(&platform, cores, summary));
    }
    if let Some(steps) = &smoke_steps {
        if !report.is_empty() {
            report.push('\n');
        }
        report.push_str(&render_smoke_quote(&new_display, &platform, cores, steps));
    }
    write_stdout(&report);
    if args.record_kb && !rows.is_empty() {
        let kb_root = Path::new(&args.kb_root);
        let commit = git_head_short(kb_root);
        let date = utc_date_from_iso(&utc_now());
        let meta = KbRecordMeta {
            machine: &platform,
            cores,
            commit: &commit,
            date: &date,
            trials: args.trials,
            noop_runs: noop_startup.as_ref().map_or(NOOP_STARTUP_RUNS, |s| s.runs),
            noop_bin_bytes: noop_startup.as_ref().map(|s| s.new_bin_bytes),
        };
        let (path, written) = upsert_kb_baselines(kb_root, &rows, &meta)?;
        write_stderr(&format!(
            "{SCRIPT}: recorded {written} baseline row(s) to {}\n",
            path.display()
        ));
    }
    Ok(exit_code)
}

/// Context needed to serialize measured `new` workload rows into `agents/kb/baselines.jsonl`.
struct KbRecordMeta<'a> {
    machine: &'a str,
    cores: usize,
    commit: &'a str,
    date: &'a str,
    trials: usize,
    noop_runs: usize,
    noop_bin_bytes: Option<u64>,
}

/// Extract `YYYY-MM-DD` from an ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
fn utc_date_from_iso(iso: &str) -> String {
    iso.get(..10)
        .filter(|s| {
            let b = s.as_bytes();
            b.len() == 10
                && b[4] == b'-'
                && b[7] == b'-'
                && b.iter()
                    .enumerate()
                    .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        })
        .map_or_else(|| "1970-01-01".to_owned(), str::to_owned)
}

/// Short git HEAD commit hash in `root`, or `"unknown"` when `root` is not a git checkout
/// or `git` fails/times out (Rule 69).
fn git_head_short(root: &Path) -> String {
    if !root.is_dir() {
        return "unknown".to_owned();
    }
    let Ok(Some(ran)) = run_within(
        &["git", "rev-parse", "--short", "HEAD"],
        root,
        GIT_HEAD_TIMEOUT,
    ) else {
        return "unknown".to_owned();
    };
    if ran.status != 0 {
        return "unknown".to_owned();
    }
    let trimmed = ran.out().trim().to_owned();
    if trimmed.is_empty()
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        "unknown".to_owned()
    } else {
        trimmed
    }
}

/// Canonical KB workload slug for `agents/kb/baselines.jsonl` (safe for `<machine>-<workload>-<commit>` IDs).
fn kb_workload_slug(label: &str) -> String {
    match label {
        "cold_hello (check)" => return "cold_hello".to_owned(),
        "mod500 (cold)" => return "mod500_cold".to_owned(),
        "mod500 (1-file edit)" => return "mod500_edit".to_owned(),
        "ripgrep/crates/regex (cold)" => return "ripgrep_cold".to_owned(),
        "ripgrep/crates/regex (1-file edit)" => return "ripgrep_edit".to_owned(),
        "build (hello-world)" => return "build_hello".to_owned(),
        "noop_startup (warm check)" => return "noop_startup_warm_check".to_owned(),
        "noop_startup (--version)" => return "noop_startup_version".to_owned(),
        _ => {}
    }
    let mut slug = String::with_capacity(label.len());
    let mut prev_sep = true;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_sep = false;
        } else if !prev_sep {
            slug.push('_');
            prev_sep = true;
        }
    }
    while slug.ends_with('_') {
        slug.pop();
    }
    if slug.is_empty() {
        "workload".to_owned()
    } else {
        slug
    }
}

fn json_f64(value: f64) -> Result<Value, String> {
    let finite = if value.is_finite() { value } else { 0.0 };
    let rounded = py_round(finite, 2);
    let clean = if rounded == 0.0 { 0.0 } else { rounded };
    Number::from_f64(clean)
        .map(Value::Number)
        .ok_or_else(|| format!("non-finite float cannot be serialized to JSON: {value}"))
}

fn breakdown_item(component: &str, value: f64, unit: &str) -> Result<Value, String> {
    let mut obj = Map::with_capacity(3);
    obj.insert("component".to_owned(), Value::String(component.to_owned()));
    obj.insert("value".to_owned(), json_f64(value)?);
    obj.insert("unit".to_owned(), Value::String(unit.to_owned()));
    Ok(Value::Object(obj))
}

/// Build one canonical JSONL line for `<kb_root>/agents/kb/baselines.jsonl` from a measured `new` row.
fn build_baseline_entry(
    row: &RowSummary,
    meta: &KbRecordMeta<'_>,
) -> Result<(String, String), String> {
    let workload = kb_workload_slug(&row.workload);
    let id = format!("{}-{workload}-{}", meta.machine, meta.commit);
    let wall_ms = json_f64(row.new.wall_ms)?;
    let rss_mb = json_f64(row.new.peak_rss_bytes as f64 / 1_000_000.0)?;

    let mut breakdown = Vec::new();
    for (stage_name, stage) in [
        ("stage_names", row.new.names),
        ("stage_heads", row.new.heads),
        ("stage_bodies", row.new.bodies),
    ] {
        if let Some(st) = stage {
            breakdown.push(breakdown_item(&format!("{stage_name}_wall"), st.wall_ms, "ms")?);
            breakdown.push(breakdown_item(&format!("{stage_name}_cpu"), st.cpu_ms, "ms")?);
        }
    }
    if workload.starts_with("noop_startup")
        && let Some(bin_bytes) = meta.noop_bin_bytes
    {
        breakdown.push(breakdown_item(
            "binary_size",
            bin_bytes as f64 / 1_000_000.0,
            "MB",
        )?);
    }

    let per_core = format_per_core(row.new.wall_ms, meta.cores, row.work_bytes);
    let floor_note = if workload.starts_with("noop_startup") {
        if let Some(bin_bytes) = meta.noop_bin_bytes {
            format!(
                "bench_wave ({} runs, {} cores, binary {}): {per_core}",
                meta.noop_runs,
                meta.cores,
                format_mb(bin_bytes)
            )
        } else {
            format!("bench_wave ({} runs, {} cores): {per_core}", meta.noop_runs, meta.cores)
        }
    } else {
        format!("bench_wave ({} trials, {} cores): {per_core}", meta.trials, meta.cores)
    };

    let mut obj = Map::with_capacity(10);
    obj.insert("id".to_owned(), Value::String(id.clone()));
    obj.insert("machine".to_owned(), Value::String(meta.machine.to_owned()));
    obj.insert("cores".to_owned(), Value::Number(Number::from(meta.cores as u64)));
    obj.insert("commit".to_owned(), Value::String(meta.commit.to_owned()));
    obj.insert("date".to_owned(), Value::String(meta.date.to_owned()));
    obj.insert("workload".to_owned(), Value::String(workload));
    obj.insert("wall_ms".to_owned(), wall_ms);
    obj.insert("rss_mb".to_owned(), rss_mb);
    obj.insert("breakdown".to_owned(), Value::Array(breakdown));
    obj.insert("floor_note".to_owned(), Value::String(floor_note));

    let line = serde_json::to_string(&Value::Object(obj))
        .map_err(|e| format!("failed to serialize KB baseline `{id}`: {e}"))?;
    Ok((id, line))
}

/// Upsert measured `new` workload rows into `<kb_root>/agents/kb/baselines.jsonl`, keeping
/// lines canonically sorted by `id` (1 JSON object per line, trailing newline) so `kb check` passes.
fn upsert_kb_baselines(
    kb_root: &Path,
    rows: &[RowSummary],
    meta: &KbRecordMeta<'_>,
) -> Result<(PathBuf, usize), String> {
    let kb_dir = kb_root.join("agents").join("kb");
    let path = kb_dir.join("baselines.jsonl");
    let mut by_id: BTreeMap<String, String> = BTreeMap::new();

    if path.exists() {
        let existing = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        for (idx, raw_line) in existing.lines().enumerate() {
            let trimmed = raw_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let line_no = idx + 1;
            let val: Value = serde_json::from_str(trimmed)
                .map_err(|e| format!("{}:{line_no}: invalid JSON line: {e}", path.display()))?;
            let id = val
                .as_object()
                .and_then(|obj| obj.get("id"))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    format!(
                        "{}:{line_no}: baseline row must be a JSON object with non-empty string `id`",
                        path.display()
                    )
                })?
                .to_owned();
            let canonical = serde_json::to_string(&val).map_err(|e| {
                format!("{}:{line_no}: failed to serialize baseline row: {e}", path.display())
            })?;
            if by_id.contains_key(&id) {
                return Err(format!("{}:{line_no}: duplicate baseline id `{id}`", path.display()));
            }
            by_id.insert(id, canonical);
        }
    }

    let mut written = 0usize;
    for row in rows {
        let (id, line) = build_baseline_entry(row, meta)?;
        by_id.insert(id, line);
        written += 1;
    }

    std::fs::create_dir_all(&kb_dir)
        .map_err(|e| format!("failed to create {}: {e}", kb_dir.display()))?;
    let mut out = String::new();
    for line in by_id.values() {
        out.push_str(line);
        out.push('\n');
    }
    let tmp_path = kb_dir.join(format!(".baselines.jsonl.tmp-{}", std::process::id()));
    std::fs::write(&tmp_path, &out)
        .map_err(|e| format!("failed to write {}: {e}", tmp_path.display()))?;
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        if std::fs::remove_file(&tmp_path).is_err() {
            // Best-effort cleanup of sibling temp file before reporting the rename error.
        }
        return Err(format!(
            "failed to rename {} -> {}: {e}",
            tmp_path.display(),
            path.display()
        ));
    }
    Ok((path, written))
}

fn run_wave(args: &Args) -> ! {
    let work_root = match WorkRoot::create() {
        Ok(root) => root,
        Err(why) => fail(SCRIPT, &why),
    };
    let result = run_wave_inner(args, &work_root);
    // Drop work_root explicitly before `exit`, which skips destructors (Rule 39).
    drop(work_root);
    match result {
        Ok(code) => exit(code),
        Err(why) => fail(SCRIPT, &why),
    }
}

fn parse_args_json(raw: &str) -> Result<Args, InputError> {
    input::read(raw).and_then(|v| input::of::<Args>(&v, ""))
}

fn check(holds: bool, what: &str) {
    expect(SCRIPT, holds, what);
}

fn approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6
}

fn run_selftest_cases() {
    // 1. JSON input parsing: defaults, full A/B, and rejections.
    let minimal = parse_args_json(r#"{"new": "/bin/blaze-new"}"#)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("minimal input failed: {}", e.0)));
    check(
        minimal.old.is_none()
            && minimal.new == "/bin/blaze-new"
            && minimal.trials == 3
            && minimal.hello_world == DEFAULT_HELLO_WORLD
            && minimal.ripgrep.is_none()
            && minimal.workloads == ["smoke", "cold_hello", "mod500"]
            && minimal.timeout_secs == 120
            && !minimal.record_kb
            && minimal.kb_root == DEFAULT_KB_ROOT,
        "minimal JSON input applies all documented defaults",
    );

    let full = parse_args_json(
        r#"{
            "old": "/a/blaze",
            "new": "/b/blaze",
            "trials": 7,
            "hello_world": "/custom/hw",
            "ripgrep": "/custom/rg",
            "workloads": ["smoke", "cold_hello", "mod500", "ripgrep", "build", "noop_startup"],
            "timeout_secs": 45,
            "record_kb": true,
            "kb_root": "/custom/kb"
        }"#,
    )
    .unwrap_or_else(|e| fail(SCRIPT, &format!("full input failed: {}", e.0)));
    check(
        full.old.as_deref() == Some("/a/blaze")
            && full.new == "/b/blaze"
            && full.trials == 7
            && full.hello_world == "/custom/hw"
            && full.ripgrep.as_deref() == Some("/custom/rg")
            && full.workloads
                == ["smoke", "cold_hello", "mod500", "ripgrep", "build", "noop_startup"]
            && full.timeout_secs == 45
            && full.record_kb
            && full.kb_root == "/custom/kb",
        "full A/B JSON input parses every field including noop_startup, record_kb and kb_root",
    );

    let noop_only = parse_args_json(r#"{"new": "/b/blaze", "workloads": ["noop_startup"]}"#)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("noop_startup input failed: {}", e.0)));
    check(
        noop_only.workloads == ["noop_startup"],
        "noop_startup workload parses standalone",
    );

    let unknown_err = parse_args_json(r#"{"new": "/b/blaze", "unknown_flag": 1}"#);
    check(
        matches!(&unknown_err, Err(InputError(msg)) if msg.contains("unknown_flag")),
        "unknown JSON field is rejected naming the field",
    );

    for bad in [
        r#"{}"#,
        r#"{"new": ""}"#,
        r#"{"old": "", "new": "/b/blaze"}"#,
        r#"{"new": "/b/blaze", "trials": 0}"#,
        r#"{"new": "/b/blaze", "trials": 21}"#,
        r#"{"new": "/b/blaze", "trials": true}"#,
        r#"{"new": "/b/blaze", "timeout_secs": 0}"#,
        r#"{"new": "/b/blaze", "timeout_secs": 9999}"#,
        r#"{"new": "/b/blaze", "workloads": []}"#,
        r#"{"new": "/b/blaze", "workloads": ["nope"]}"#,
        r#"{"new": "/b/blaze", "workloads": ["export"]}"#,
        r#"{"new": "/b/blaze", "workloads": ["smoke", "smoke"]}"#,
        r#"{"new": "/b/blaze", "record_kb": "yes"}"#,
        r#"{"new": "/b/blaze", "kb_root": ""}"#,
        r#"{"new": "/b/blaze", "kb_root": 123}"#,
    ] {
        check(
            parse_args_json(bad).is_err(),
            &format!("invalid JSON input rejected: {bad}"),
        );
    }

    // 2. Profile metric parser: RSS + stage Names/Heads/Bodies extraction + fallbacks.
    let sample_profile = "\
phases       process start to runtime start 1000 · runtime start to main 2000 · in main 500000000
meter        accounted of the process's totals: · instr 1.2 G of 1.3 G (92%) · heap bytes 12.0 MB of 12.1 MB (99%) · most resident 84.5 MB
stages at the end, 1.4 s:
stage Units: tasks 3 done / 3 started / 3 known · wall 0.1 s · cpu 0.1 s
stage Names: tasks 14 done / 14 started / 14 known · wall 0.4 s · cpu 1.2 s
stage Heads: tasks 14 done / 14 started / 14 known · wall 0.1 s · cpu 0.3 s
stage Bodies: tasks 500 done / 500 started / 500 known · wall 0.8 s · cpu 6.4 s
";
    let parsed = parse_profile(sample_profile);
    check(
        parsed.peak_rss_bytes == Some(84_500_000),
        &format!("peak RSS extracted from `most resident 84.5 MB`: {:?}", parsed.peak_rss_bytes),
    );
    check(
        parsed
            .names
            .is_some_and(|s| approx_eq(s.wall_ms, 400.0) && approx_eq(s.cpu_ms, 1200.0)),
        &format!("stage Names wall & cpu extracted: {:?}", parsed.names),
    );
    check(
        parsed
            .heads
            .is_some_and(|s| approx_eq(s.wall_ms, 100.0) && approx_eq(s.cpu_ms, 300.0)),
        &format!("stage Heads wall & cpu extracted: {:?}", parsed.heads),
    );
    check(
        parsed
            .bodies
            .is_some_and(|s| approx_eq(s.wall_ms, 800.0) && approx_eq(s.cpu_ms, 6400.0)),
        &format!("stage Bodies wall & cpu extracted: {:?}", parsed.bodies),
    );

    let fallback_profile = "\
meter        accounted of the process's totals: · most resident unavailable (not supported)
engine stage times (stage: phases, wall ms, task ms summed, merge ms, put ms, tasks timed)
  Names:    2      123.4      456.7      1.2      3.4      14
  Heads:    1       45.6       89.0      0.5      1.1      14
  Bodies:   1      812.3     6401.2      2.1      4.5     500
Names ran 0.0 s beside Scripts
";
    let fb = parse_profile(fallback_profile);
    check(
        fb.peak_rss_bytes.is_none()
            && fb.names.is_some_and(|s| approx_eq(s.wall_ms, 123.4) && approx_eq(s.cpu_ms, 456.7))
            && fb.heads.is_some_and(|s| approx_eq(s.wall_ms, 45.6) && approx_eq(s.cpu_ms, 89.0))
            && fb.bodies.is_some_and(|s| approx_eq(s.wall_ms, 812.3) && approx_eq(s.cpu_ms, 6401.2)),
        "profile parser handles unavailable RSS and `engine stage times` table fallback",
    );
    check(
        parse_byte_size("1.5 GB") == Some(1_500_000_000)
            && parse_byte_size("512 kB") == Some(512_000)
            && parse_byte_size("64 MiB") == Some(64 * 1024 * 1024)
            && parse_byte_size("512 B") == Some(512)
            && parse_byte_size("unavailable (x)").is_none(),
        "byte size parser handles decimal and binary units",
    );

    // 3. Median, speedup, per-core units, and Markdown table + Rule 68 smoke formatting.
    check(
        median_f64(&[]).is_none()
            && median_f64(&[30.0, 10.0, 20.0]).is_some_and(|m| approx_eq(m, 20.0))
            && median_f64(&[40.0, 10.0, 30.0, 20.0]).is_some_and(|m| approx_eq(m, 25.0)),
        "median_f64 computes exact medians for odd and even inputs",
    );
    check(
        median_u64(&[]).is_none()
            && median_u64(&[300, 100, 200]) == Some(200)
            && median_u64(&[400, 100, 300, 200]) == Some(250),
        "median_u64 computes exact medians for odd and even inputs",
    );
    check(
        speedup(200.0, 100.0).is_some_and(|s| approx_eq(s, 2.0))
            && speedup(0.0, 100.0).is_none()
            && speedup(100.0, 0.0).is_none(),
        "speedup computes old/new and rejects zero/non-positive denominators",
    );
    check(
        trial_order(0, true) == [Side::Old, Side::New].as_slice()
            && trial_order(1, true) == [Side::New, Side::Old].as_slice()
            && trial_order(0, false) == [Side::New].as_slice(),
        "A/B trial ordering alternates old/new across trials",
    );

    let ab_rows = [RowSummary {
        workload: "mod500 (cold)".to_owned(),
        work_bytes: 200_000,
        old: Some(SideSummary {
            wall_ms: 200.0,
            peak_rss_bytes: 120_000_000,
            names: Some(StageTime { wall_ms: 40.0, cpu_ms: 160.0 }),
            heads: Some(StageTime { wall_ms: 20.0, cpu_ms: 80.0 }),
            bodies: Some(StageTime { wall_ms: 100.0, cpu_ms: 600.0 }),
        }),
        new: SideSummary {
            wall_ms: 100.0,
            peak_rss_bytes: 80_000_000,
            names: Some(StageTime { wall_ms: 20.0, cpu_ms: 80.0 }),
            heads: Some(StageTime { wall_ms: 10.0, cpu_ms: 40.0 }),
            bodies: Some(StageTime { wall_ms: 50.0, cpu_ms: 300.0 }),
        },
    }];
    let md_ab = render_markdown(
        "macos-aarch64",
        16,
        3,
        Some("/old/blaze"),
        "/new/blaze",
        &ab_rows,
    );
    check(
        md_ab.contains("16 cores")
            && md_ab.contains("| mod500 (cold) | 200.0 | 100.0 | 2.00x | 120.0 MB | 80.0 MB |")
            && md_ab.contains("0.125 MB/s/core (1600.0 core-ms)")
            && md_ab.contains("N:20/80ms H:10/40ms B:50/300ms"),
        &format!("A/B markdown table formats cores, speedup, RSS, per-core metric and stages:\n{md_ab}"),
    );

    let md_single = render_markdown("linux-aarch64", 128, 3, None, "/new/blaze", &ab_rows);
    check(
        md_single.contains("128 cores")
            && md_single.contains("| Workload | Wall (ms) | Peak RSS | Per-Core | Stages (wall/cpu) |")
            && !md_single.contains("Old Wall"),
        "single-binary markdown table omits baseline columns",
    );

    let noop_ab = NoopStartupSummary {
        runs: NOOP_STARTUP_RUNS,
        check_row: RowSummary {
            workload: "noop_startup (warm check)".to_owned(),
            work_bytes: 120,
            old: Some(SideSummary { wall_ms: 18.4, peak_rss_bytes: 12_000_000, ..SideSummary::default() }),
            new: SideSummary { wall_ms: 9.2, peak_rss_bytes: 10_000_000, ..SideSummary::default() },
        },
        version_row: RowSummary {
            workload: "noop_startup (--version)".to_owned(),
            work_bytes: 0,
            old: Some(SideSummary { wall_ms: 3.1, peak_rss_bytes: 4_000_000, ..SideSummary::default() }),
            new: SideSummary { wall_ms: 1.55, peak_rss_bytes: 3_500_000, ..SideSummary::default() },
        },
        old_bin_bytes: Some(52_400_000),
        new_bin_bytes: 48_100_000,
    };
    let noop_md_ab = render_noop_startup_section("macos-aarch64", 16, &noop_ab);
    check(
        noop_md_ab.contains("10 alternating runs")
            && noop_md_ab.contains("| warm no-op `blaze check` (median) | 18.40 ms | 9.20 ms | 2.00x |")
            && noop_md_ab.contains("| `blaze --version` (median) | 3.10 ms | 1.55 ms | 2.00x |")
            && noop_md_ab.contains("| binary size | 52.40 MB | 48.10 MB | -4.30 MB (-8.2%) |"),
        &format!("noop_startup A/B section formats medians, speedups, and binary sizes:\n{noop_md_ab}"),
    );

    let noop_single = NoopStartupSummary {
        runs: NOOP_STARTUP_RUNS,
        check_row: RowSummary { old: None, ..noop_ab.check_row.clone() },
        version_row: RowSummary { old: None, ..noop_ab.version_row.clone() },
        old_bin_bytes: None,
        new_bin_bytes: 48_100_000,
    };
    let noop_md_single = render_noop_startup_section("linux-aarch64", 128, &noop_single);
    check(
        noop_md_single.contains("10 runs")
            && noop_md_single.contains("| warm no-op `blaze check` (median) | 9.20 ms |")
            && noop_md_single.contains("| `blaze --version` (median) | 1.55 ms |")
            && noop_md_single.contains("| binary size | 48.10 MB |")
            && !noop_md_single.contains("Speedup"),
        &format!("noop_startup single-binary section omits baseline columns:\n{noop_md_single}"),
    );

    check(
        is_dir_not_empty(&std::io::Error::from(std::io::ErrorKind::DirectoryNotEmpty))
            && is_dir_not_empty(&std::io::Error::from_raw_os_error(66))
            && is_dir_not_empty(&std::io::Error::from_raw_os_error(39))
            && !is_dir_not_empty(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        "is_dir_not_empty recognizes DirectoryNotEmpty and macOS/Linux ENOTEMPTY",
    );

    let quote = render_smoke_quote(
        "/new/blaze",
        "macos-aarch64",
        16,
        &[SmokeStep {
            command: "/new/blaze run".to_owned(),
            passed: true,
            status_label: "exit 0".to_owned(),
            wall_ms: 42.5,
            output_lines: vec!["Hello, world!".to_owned()],
            failure_note: None,
        }],
    );
    check(
        quote.contains("PASS")
            && quote.contains("$ /new/blaze run")
            && quote.contains("Hello, world!")
            && quote.contains("[ok: exit 0, 42.5 ms]"),
        "Rule 68 smoke quote formats command, stdout, and verdict",
    );

    // 4. Timeout enforcement: a hanging child is killed promptly at the deadline.
    let tmp = TempDir::new("bench-wave-selftest-")
        .unwrap_or_else(|e| fail(SCRIPT, &format!("TempDir failed: {e}")));
    let log_stem = tmp.path().join("timeout");
    let began = Instant::now();
    let ended = run_bounded_cmd(
        Path::new("/bin/sh"),
        &["-c", "sleep 30"],
        tmp.path(),
        &[],
        &log_stem,
        Duration::from_millis(200),
    )
    .unwrap_or_else(|e| fail(SCRIPT, &format!("timeout test failed to spawn: {e}")));
    let elapsed = began.elapsed();
    // Also verify mod500 generation in the same temp dir before dropping it.
    let mod_dir = tmp.path().join("mod5");
    if let Err(why) = generate_mod500(&mod_dir, 5) {
        drop(tmp);
        fail(SCRIPT, &format!("generate_mod500 failed: {why}"));
    }
    let generated_ok = mod_dir.join("Cargo.toml").is_file()
        && mod_dir.join("src").join("lib.rs").is_file()
        && mod_dir.join("src").join("m004.rs").is_file()
        && count_rust_bytes(&mod_dir, 0) > 0;
    let removed_ok = remove_tree_writable(&mod_dir).is_ok()
        && !mod_dir.exists()
        && remove_tree_writable(&mod_dir).is_ok();

    // 5. KB baseline recording (`record_kb`): write, canonical sorting, and upsert by `id`.
    check(
        utc_date_from_iso("2026-11-26T09:07:07Z") == "2026-11-26"
            && utc_date_from_iso("not-a-date") == "1970-01-01",
        "utc_date_from_iso extracts YYYY-MM-DD and guards malformed input",
    );
    check(
        git_head_short(Path::new("/nonexistent/bench-wave-kb-selftest")) == "unknown",
        "git_head_short falls back to `unknown` for missing/non-git roots",
    );
    check(
        kb_workload_slug("cold_hello (check)") == "cold_hello"
            && kb_workload_slug("mod500 (cold)") == "mod500_cold"
            && kb_workload_slug("mod500 (1-file edit)") == "mod500_edit"
            && kb_workload_slug("ripgrep/crates/regex (cold)") == "ripgrep_cold"
            && kb_workload_slug("ripgrep/crates/regex (1-file edit)") == "ripgrep_edit"
            && kb_workload_slug("noop_startup (warm check)") == "noop_startup_warm_check"
            && kb_workload_slug("noop_startup (--version)") == "noop_startup_version",
        "kb_workload_slug produces canonical ID-safe slugs for all wave workloads",
    );

    let kb_root = tmp.path().join("kb-repo");
    let kb_meta = KbRecordMeta {
        machine: "macos-aarch64",
        cores: 18,
        commit: "abc1234",
        date: "2026-11-26",
        trials: 3,
        noop_runs: NOOP_STARTUP_RUNS,
        noop_bin_bytes: Some(48_100_000),
    };
    let initial_rows = [
        RowSummary {
            workload: "ripgrep/crates/regex (cold)".to_owned(),
            work_bytes: 500_000,
            old: None,
            new: SideSummary {
                wall_ms: 420.126,
                peak_rss_bytes: 95_400_000,
                names: Some(StageTime { wall_ms: 30.2, cpu_ms: 110.4 }),
                heads: None,
                bodies: Some(StageTime { wall_ms: 210.5, cpu_ms: 1400.0 }),
            },
        },
        RowSummary {
            workload: "cold_hello (check)".to_owned(),
            work_bytes: 120,
            old: None,
            new: SideSummary {
                wall_ms: 47.8,
                peak_rss_bytes: 18_200_000,
                ..SideSummary::default()
            },
        },
        noop_ab.check_row.clone(),
        noop_ab.version_row.clone(),
        ab_rows[0].clone(),
    ];
    let (kb_path, first_written) = match upsert_kb_baselines(&kb_root, &initial_rows, &kb_meta) {
        Ok(res) => res,
        Err(e) => {
            drop(tmp);
            fail(SCRIPT, &format!("initial upsert_kb_baselines failed: {e}"));
        }
    };
    let first_text = match std::fs::read_to_string(&kb_path) {
        Ok(text) => text,
        Err(e) => {
            drop(tmp);
            fail(SCRIPT, &format!("failed to read {}: {e}", kb_path.display()));
        }
    };
    let first_lines: Vec<&str> = first_text.lines().collect();
    let mut first_ids = Vec::with_capacity(first_lines.len());
    let mut schema_ok = first_written == initial_rows.len()
        && first_lines.len() == initial_rows.len()
        && first_text.ends_with('\n');
    for line in &first_lines {
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(line) else {
            schema_ok = false;
            break;
        };
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        let expected_keys = [
            "id",
            "machine",
            "cores",
            "commit",
            "date",
            "workload",
            "wall_ms",
            "rss_mb",
            "breakdown",
            "floor_note",
        ];
        if keys != expected_keys {
            schema_ok = false;
            break;
        }
        let id = obj.get("id").and_then(Value::as_str).unwrap_or("");
        let cores_ok = obj.get("cores").and_then(Value::as_i64) == Some(18);
        let wall_ok = matches!(obj.get("wall_ms"), Some(Value::Number(n)) if n.is_f64());
        let rss_ok = matches!(obj.get("rss_mb"), Some(Value::Number(n)) if n.is_f64());
        let breakdown_ok = obj.get("breakdown").is_some_and(Value::is_array);
        let note_ok = obj
            .get("floor_note")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if id.is_empty() || !cores_ok || !wall_ok || !rss_ok || !breakdown_ok || !note_ok {
            schema_ok = false;
            break;
        }
        first_ids.push(id.to_owned());
    }
    let first_sorted = first_ids.windows(2).all(|w| w[0] < w[1]);
    let has_expected_first_ids = first_ids
        == [
            "macos-aarch64-cold_hello-abc1234",
            "macos-aarch64-mod500_cold-abc1234",
            "macos-aarch64-noop_startup_version-abc1234",
            "macos-aarch64-noop_startup_warm_check-abc1234",
            "macos-aarch64-ripgrep_cold-abc1234",
        ];

    // Second write: upsert `noop_startup (warm check)` with a new wall_ms and add `mod500 (1-file edit)`.
    let upsert_rows = [
        RowSummary {
            workload: "noop_startup (warm check)".to_owned(),
            work_bytes: 120,
            old: None,
            new: SideSummary {
                wall_ms: 4.69,
                peak_rss_bytes: 7_900_000,
                ..SideSummary::default()
            },
        },
        RowSummary {
            workload: "mod500 (1-file edit)".to_owned(),
            work_bytes: 200_000,
            old: None,
            new: SideSummary {
                wall_ms: 14.25,
                peak_rss_bytes: 62_000_000,
                ..SideSummary::default()
            },
        },
    ];
    let (_, second_written) = match upsert_kb_baselines(&kb_root, &upsert_rows, &kb_meta) {
        Ok(res) => res,
        Err(e) => {
            drop(tmp);
            fail(SCRIPT, &format!("upsert_kb_baselines update failed: {e}"));
        }
    };
    let second_text = match std::fs::read_to_string(&kb_path) {
        Ok(text) => text,
        Err(e) => {
            drop(tmp);
            fail(SCRIPT, &format!("failed to re-read {}: {e}", kb_path.display()));
        }
    };
    let second_lines: Vec<&str> = second_text.lines().collect();
    let mut second_ids = Vec::with_capacity(second_lines.len());
    let mut updated_warm_check_ok = false;
    for line in &second_lines {
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(line)
            && let Some(id) = obj.get("id").and_then(Value::as_str)
        {
            if id == "macos-aarch64-noop_startup_warm_check-abc1234" {
                let wall_eq = obj
                    .get("wall_ms")
                    .and_then(Value::as_f64)
                    .is_some_and(|w| approx_eq(w, 4.69));
                let rss_eq = obj
                    .get("rss_mb")
                    .and_then(Value::as_f64)
                    .is_some_and(|r| approx_eq(r, 7.9));
                let has_bin_size = obj
                    .get("breakdown")
                    .and_then(Value::as_array)
                    .is_some_and(|arr| {
                        arr.iter().any(|item| {
                            item.get("component").and_then(Value::as_str) == Some("binary_size")
                                && item.get("unit").and_then(Value::as_str) == Some("MB")
                        })
                    });
                updated_warm_check_ok = wall_eq && rss_eq && has_bin_size;
            }
            second_ids.push(id.to_owned());
        }
    }
    let second_sorted = second_ids.windows(2).all(|w| w[0] < w[1]);
    let upsert_count_ok = second_written == 2 && second_ids.len() == 6;

    // Corrupt existing baselines.jsonl must fail loudly (Rule 3 & Rule 19).
    let bad_kb_root = tmp.path().join("bad-kb-repo");
    let bad_kb_dir = bad_kb_root.join("agents").join("kb");
    let bad_setup_ok = std::fs::create_dir_all(&bad_kb_dir).is_ok()
        && std::fs::write(bad_kb_dir.join("baselines.jsonl"), "{not-json}\n").is_ok();
    let bad_kb_rejected = bad_setup_ok
        && upsert_kb_baselines(&bad_kb_root, &upsert_rows, &kb_meta)
            .is_err_and(|msg| msg.contains("baselines.jsonl:1:"));

    drop(tmp);

    check(
        ended == Ended::TimedOut { after: Duration::from_millis(200) }
            && elapsed < Duration::from_secs(5),
        &format!("hanging child is killed at timeout ({ended:?}, elapsed {elapsed:?})"),
    );
    check(generated_ok, "mod500 fixture generator writes valid crate layout");
    check(removed_ok, "remove_tree_writable removes populated and already-absent directories");
    check(
        schema_ok && first_sorted && has_expected_first_ids,
        &format!(
            "record_kb writes canonical sorted baselines.jsonl (ids={first_ids:?}):\n{first_text}"
        ),
    );
    check(
        upsert_count_ok && second_sorted && updated_warm_check_ok,
        &format!(
            "record_kb upserts existing id and preserves sorted baselines.jsonl (ids={second_ids:?}):\n{second_text}"
        ),
    );
    check(
        bad_kb_rejected,
        "record_kb refuses malformed existing baselines.jsonl naming file and line",
    );
}

fn selftest() {
    run_selftest_cases();
    write_stdout(&format!("{SCRIPT} selftest: ok\n"));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => {
            let parsed = input::parse::<Args>(one, SCRIPT);
            run_wave(&parsed);
        }
        _ => {
            write_stderr(&format!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}"));
            exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selftest_passes() {
        run_selftest_cases();
    }
}
