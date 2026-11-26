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
    workloads     subset of ["smoke", "cold_hello", "mod500", "ripgrep", "build"]
                  (default ["smoke", "cold_hello", "mod500"])
    timeout_secs  per-command timeout in seconds (default 120, capped 1..=3600; rule 69)
"#;

use std::{
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
    read_text_lossy, run_within,
};

const SCRIPT: &str = "bench_wave";
const WORKLOADS: [&str; 5] = ["smoke", "cold_hello", "mod500", "ripgrep", "build"];
const DEFAULT_WORKLOADS: [&str; 3] = ["smoke", "cold_hello", "mod500"];
const DEFAULT_HELLO_WORLD: &str = "/Volumes/Projects/tmp/hello-world";
const DEFAULT_TRIALS: i64 = 3;
const MAX_TRIALS: i64 = 20;
const DEFAULT_TIMEOUT_SECS: i64 = 120;
const MAX_TIMEOUT_SECS: i64 = 3600;
const MOD500_COUNT: usize = 500;
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
        Ok(Args {
            old,
            new,
            trials,
            hello_world,
            ripgrep,
            workloads,
            workloads_explicit,
            timeout_secs,
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

/// Recursively make a directory tree owner-writable (needed for read-only cloned toolchain/native
/// directories) and remove it cleanly.
fn remove_tree_writable(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            let _ = std::process::Command::new("/bin/chmod")
                .args(["-R", "u+w"])
                .arg(path)
                .status();
            std::fs::remove_dir_all(path)
        }
        Err(err) => Err(err),
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
    if let Some(steps) = &smoke_steps {
        if !report.is_empty() {
            report.push('\n');
        }
        report.push_str(&render_smoke_quote(&new_display, &platform, cores, steps));
    }
    write_stdout(&report);
    Ok(exit_code)
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
            && minimal.timeout_secs == 120,
        "minimal JSON input applies all documented defaults",
    );

    let full = parse_args_json(
        r#"{
            "old": "/a/blaze",
            "new": "/b/blaze",
            "trials": 7,
            "hello_world": "/custom/hw",
            "ripgrep": "/custom/rg",
            "workloads": ["smoke", "cold_hello", "mod500", "ripgrep", "build"],
            "timeout_secs": 45
        }"#,
    )
    .unwrap_or_else(|e| fail(SCRIPT, &format!("full input failed: {}", e.0)));
    check(
        full.old.as_deref() == Some("/a/blaze")
            && full.new == "/b/blaze"
            && full.trials == 7
            && full.hello_world == "/custom/hw"
            && full.ripgrep.as_deref() == Some("/custom/rg")
            && full.workloads == ["smoke", "cold_hello", "mod500", "ripgrep", "build"]
            && full.timeout_secs == 45,
        "full A/B JSON input parses every field",
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
    drop(tmp);

    check(
        ended == Ended::TimedOut { after: Duration::from_millis(200) }
            && elapsed < Duration::from_secs(5),
        &format!("hanging child is killed at timeout ({ended:?}, elapsed {elapsed:?})"),
    );
    check(generated_ok, "mod500 fixture generator writes valid crate layout");
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
