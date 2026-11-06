//! The profile's gate: what a profiled build's history must hold before anyone may trust it
//! (docs/profiling.md, "The gate"). Each check passes, fails, or says why it could not run; the
//! verdict passes only when every check passed, so a check that could not run is never read as
//! clean. CI runs it over the history its self-build wrote (`harness/profile_gate.py`), and
//! over one fixture a check (`fixtures/gate/<check>/`) that the check must fail.
//!
//! The checks, and their one threshold each:
//! - `workers_covered`: per pool worker, the share of the build's wall time inside no span and
//!   not idle is under [`UNCOVERED_MAX_PCT`].
//! - `blocks_in_waits`: no sample of a worker blocked in the kernel is outside a `wait` span.
//! - `waits_say_what`: every `wait` span says what it awaits.
//! - `rows_lost`: the recorder lost at most [`ROWS_LOST_MAX`] spans and rows in the build.
//! - `samples_in_spans`: at least [`SAMPLES_IN_SPANS_MIN_PCT`] of the workers' samples name a
//!   span that was open when they were taken.
//! - `children_in_parents`: every span lies inside its parent, to [`CLOCK_SLACK_NS`].
//! - `self_cpu_in_duration`: no span's self CPU is negative or more than its duration.
//! - `cpu_matches_process`: the spans' self CPU summed is within [`CPU_DIFF_MAX_PCT`] of the
//!   daemon's own CPU over the build (`builds.daemon_user_ns` + `daemon_system_ns`, getrusage).
//! - `slow_deterministic`: `blaze slow --json` of the build is the same bytes twice.
//! - `merged_rows_match`: every merged file of the build holds the rows its merge read
//!   ([`crate::history::MERGED_ROWS`]).
//! - `merges_exact`: merging the build's files again holds exactly their rows, as a multiset.
//! - `lock_waits`: no one named lock's waits (`lock_waits`, blaze_core::lock) sum to more than
//!   [`LOCK_WAIT_MAX_PCT`] of the build's pool-worker time. Set from the 2026-11-05
//!   `flight::intern` convoy and the 1% uncovered budget; re-pick from the first ten gate
//!   builds.
//!
//! A check that saw nothing to judge (no pool worker switched, no sample was taken or none off
//! the CPU, no wait span) could not run, never a pass: a build profiled with the sampler off
//! would otherwise pass `blocks_in_waits` by having no samples.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use arrow_array::{
    Array, RecordBatch,
    cast::AsArray,
    types::{Float64Type, Int64Type},
};
use parquet::{
    arrow::ArrowWriter,
    file::metadata::{PageIndexPolicy, ParquetMetaDataReader},
};
use serde_derive::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    history::{self, compact::Name},
    schema, slow,
    sql::{self, Query, SqlError},
    time::load::Source,
    views::{self, ViewError},
};

/// The most of a build's wall time a pool worker may spend inside no span, in percent.
pub const UNCOVERED_MAX_PCT: f64 = 1.0;

/// The most spans and rows the recorder may lose in a build.
pub const ROWS_LOST_MAX: i64 = 0;

/// The least share of a pool worker's samples that must name a span open as they were taken,
/// in percent.
pub const SAMPLES_IN_SPANS_MIN_PCT: f64 = 99.0;

/// The most the spans' self CPU summed may differ from the process's own CPU over the build,
/// in percent of the process's.
pub const CPU_DIFF_MAX_PCT: f64 = 5.0;

/// The most of the build's pool-worker time (its workers times its wall time, as
/// `workers_covered` counts it) one named lock's waits may sum to, in percent. Set from the
/// 2026-11-05 `flight::intern` convoy (60 threads behind one lock) and the 1% uncovered
/// budget; re-pick from the first ten gate builds' `lock_waits`.
pub const LOCK_WAIT_MAX_PCT: f64 = 2.0;

/// How far two clock readings that should agree may differ: a span's start and its parent's,
/// a sample's time and its span's ends.
pub const CLOCK_SLACK_NS: i64 = 10_000;

/// The most raw blocks a verdict asks for.
const NAMED_MAX: u64 = 50;

/// The most offenders a check names; its count is of all of them.
const SHOWN_MAX: usize = 10;

/// The version of [`Verdict`]'s JSON: what `harness/profile_gate.py` reads.
pub const VERDICT_VERSION: u32 = 1;

/// A check of the gate, as its JSON, the fixtures' directories and
/// `harness/profile-gate-known.toml` name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckName {
    WorkersCovered,
    BlocksInWaits,
    WaitsSayWhat,
    RowsLost,
    SamplesInSpans,
    ChildrenInParents,
    SelfCpuInDuration,
    CpuMatchesProcess,
    SlowDeterministic,
    MergedRowsMatch,
    MergesExact,
    LockWaits,
}

impl CheckName {
    /// Every check, in the order a verdict says them.
    pub const ALL: [CheckName; 12] = [
        CheckName::WorkersCovered,
        CheckName::BlocksInWaits,
        CheckName::WaitsSayWhat,
        CheckName::RowsLost,
        CheckName::SamplesInSpans,
        CheckName::ChildrenInParents,
        CheckName::SelfCpuInDuration,
        CheckName::CpuMatchesProcess,
        CheckName::SlowDeterministic,
        CheckName::MergedRowsMatch,
        CheckName::MergesExact,
        CheckName::LockWaits,
    ];

    pub fn name(self) -> &'static str {
        match self {
            CheckName::WorkersCovered => "workers_covered",
            CheckName::BlocksInWaits => "blocks_in_waits",
            CheckName::WaitsSayWhat => "waits_say_what",
            CheckName::RowsLost => "rows_lost",
            CheckName::SamplesInSpans => "samples_in_spans",
            CheckName::ChildrenInParents => "children_in_parents",
            CheckName::SelfCpuInDuration => "self_cpu_in_duration",
            CheckName::CpuMatchesProcess => "cpu_matches_process",
            CheckName::SlowDeterministic => "slow_deterministic",
            CheckName::MergedRowsMatch => "merged_rows_match",
            CheckName::MergesExact => "merges_exact",
            CheckName::LockWaits => "lock_waits",
        }
    }
}

impl fmt::Display for CheckName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What one check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Passed,
    Failed,
    /// It could not run: the history lacks what it reads, or its query failed. Never a pass.
    Unchecked,
}

/// One check's outcome, a line saying it, and the offenders or the error, one a line.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Check {
    pub name: CheckName,
    pub outcome: Outcome,
    pub said: String,
    pub detail: Vec<String>,
}

impl Check {
    fn new(name: CheckName, outcome: Outcome, said: String) -> Check {
        // alloc: a check's lines, once a check
        Check { name, outcome, said, detail: Vec::new() }
    }

    fn with(mut self, detail: Vec<String>) -> Check {
        self.detail = detail;
        self
    }

    fn unchecked(name: CheckName, why: impl fmt::Display) -> Check {
        // alloc: a check's line, once a check
        Check::new(name, Outcome::Unchecked, format!("could not run: {why}"))
    }
}

/// What the gate found in one build: every check of [`CheckName::ALL`], in its order.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Verdict {
    pub version: u32,
    pub build: i64,
    /// The build's `builds.tree` and `builds.blaze`: the exact tree and binary measured.
    pub tree: Option<String>,
    pub blaze: Option<String>,
    /// The daemon's process the build ran in; `None` in rows of a blaze before `builds.process`.
    pub process: Option<i64>,
    pub checks: Vec<Check>,
}

impl Verdict {
    /// Every check passed: none failed and none could not run.
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|it| it.outcome == Outcome::Passed)
    }

    pub fn check(&self, name: CheckName) -> Option<&Check> {
        self.checks.iter().find(|it| it.name == name)
    }

    fn named(&self, outcome: Outcome) -> Vec<&'static str> {
        // alloc: a verdict's summary, once a verdict
        self.checks.iter().filter(|it| it.outcome == outcome).map(|it| it.name.name()).collect()
    }

    /// One line: the checks that failed and those that could not run.
    pub fn summary(&self) -> String {
        let (failed, unchecked) = (self.named(Outcome::Failed), self.named(Outcome::Unchecked));
        // alloc: a verdict's summary, once a verdict
        let none = |it: &[&str]| if it.is_empty() { "none".to_owned() } else { it.join(", ") };
        // alloc: a verdict's summary, once a verdict
        format!(
            "build {}: failed: {}; could not run: {}",
            self.build,
            none(&failed),
            none(&unchecked)
        )
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tree = self.tree.as_deref().unwrap_or("not recorded");
        let blaze = self.blaze.as_deref().unwrap_or("not recorded");
        let count = |outcome| self.checks.iter().filter(|it| it.outcome == outcome).count();
        writeln!(
            f,
            "build {} (tree {tree}, blaze {blaze}): {} passed, {} failed, {} could not run",
            self.build,
            count(Outcome::Passed),
            count(Outcome::Failed),
            count(Outcome::Unchecked)
        )?;
        for check in &self.checks {
            let mark = match check.outcome {
                Outcome::Passed => "ok  ",
                Outcome::Failed => "FAIL",
                Outcome::Unchecked => "----",
            };
            writeln!(f, "{mark} {}: {}", check.name, check.said)?;
            for line in &check.detail {
                writeln!(f, "       {line}")?;
            }
        }
        let unchecked = self.named(Outcome::Unchecked);
        match unchecked.is_empty() {
            true => writeln!(f, "could not run: none"),
            false => writeln!(f, "could not run: {}", unchecked.join(", ")),
        }
    }
}

/// Why the gate gave no verdict at all.
#[derive(Debug)]
pub enum GateError {
    Sql(SqlError),
    /// The history has no `builds` row of an ended build for the build asked for (`None`: for
    /// any build).
    NoBuild {
        history: PathBuf,
        build: Option<u64>,
    },
    /// The build's summary had no row, or did not write.
    Summary {
        path: PathBuf,
        why: String,
    },
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateError::Sql(error) => write!(f, "the profile's gate: {error}"),
            GateError::NoBuild { history, build: Some(build) } => {
                write!(f, "the profile's gate: {} has no ended build {build}", history.display())
            }
            GateError::NoBuild { history, build: None } => {
                write!(f, "the profile's gate: {} has no build that ended", history.display())
            }
            GateError::Summary { path, why } => {
                write!(f, "the profile's gate: the build's summary {}: {why}", path.display())
            }
        }
    }
}

impl std::error::Error for GateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GateError::Sql(error) => Some(error),
            GateError::NoBuild { .. } | GateError::Summary { .. } => None,
        }
    }
}

impl From<SqlError> for GateError {
    fn from(error: SqlError) -> GateError {
        GateError::Sql(error)
    }
}

/// The build a verdict is of (and `blaze debug locks`'s, [`crate::locks::load`]): its `builds`
/// row's window and process.
#[derive(Clone, Debug)]
pub(crate) struct Build {
    pub(crate) build: u64,
    pub(crate) t0: i64,
    pub(crate) t1: i64,
    pub(crate) process: Option<u32>,
    pub(crate) tree: Option<String>,
    pub(crate) blaze: Option<String>,
}

impl Build {
    /// ` AND process = P`, or nothing when the build's row names no process.
    pub(crate) fn of(&self) -> String {
        // alloc: a condition, once a verdict
        self.process.map_or(String::new(), |it| format!(" AND process = {it}"))
    }

    /// The build's spans: its own, and its process's of no build begun in its window (as
    /// `slow::load::facts` takes them).
    fn spans(&self) -> String {
        let Build { build, t0, t1, .. } = *self;
        let of = self.of();
        // alloc: a condition, once a check
        format!(
            "(build_id = {build} OR (build_id IS NULL AND start_ns >= {t0} AND start_ns < {t1}{of}))"
        )
    }

    /// The build's folded spans, chosen alike by when the first began.
    fn folded(&self) -> String {
        let Build { build, t0, t1, .. } = *self;
        let of = self.of();
        // alloc: a condition, once a check
        format!(
            "(build_id = {build} OR (build_id IS NULL AND from_ns >= {t0} AND from_ns < {t1}{of}))"
        )
    }

    /// The build's events (and the views over them, `lock_waits` and `stalls`): its own, and
    /// its process's of no build in its window, as [`Build::spans`] takes spans.
    pub(crate) fn events(&self) -> String {
        let Build { build, t0, t1, .. } = *self;
        let of = self.of();
        // alloc: a condition, once a check
        format!("(build_id = {build} OR (build_id IS NULL AND t_ns >= {t0} AND t_ns < {t1}{of}))")
    }

    /// The pool workers of the build: the threads that ran or idled jobs in its window, as
    /// `uncovered_per_worker` finds them.
    pub(crate) fn workers(&self) -> String {
        let Build { t0, t1, .. } = *self;
        let of = self.of();
        // alloc: a query's part, once a check
        format!(
            "(SELECT DISTINCT process, thread FROM switches WHERE t_ns BETWEEN {t0} AND {t1} AND event <> 'ready'{of})"
        )
    }
}

/// The gate over `build` of the history in `history` (`None`: the last build that ended), run
/// by the DuckDB at `duckdb`. Each check that cannot run is [`Outcome::Unchecked`] with why;
/// only a history with no such build is an error.
pub fn check(duckdb: &Path, history: &Path, asked: Option<u64>) -> Result<Verdict, GateError> {
    let it = build(duckdb, history, asked)?;
    // The SQL checks in one DuckDB: the views are made once (`sql::run_all`).
    let asks = sql_checks(&it);
    // alloc: the checks' queries, once a verdict
    let sqls: Vec<&str> =
        asks.iter().flat_map(|(_, sqls, _)| sqls.iter().map(String::as_str)).collect();
    let mut replies = answers(duckdb, history, &sqls).into_iter();
    // alloc: a verdict's checks, once a verdict
    let mut checks = Vec::with_capacity(CheckName::ALL.len());
    for (name, sqls, judge) in asks {
        // alloc: a check's answers, once a check
        let mine: Vec<Result<Vec<RecordBatch>, String>> =
            replies.by_ref().take(sqls.len()).collect();
        // alloc: a check's answers, once a check
        let answered: Result<Vec<Vec<RecordBatch>>, String> = mine.into_iter().collect();
        checks.push(match answered {
            Ok(batches) => judge(&it, &batches).unwrap_or_else(|why| Check::unchecked(name, why)),
            Err(why) => Check::unchecked(name, why),
        });
    }
    checks.push(slow_deterministic(duckdb, history, it.build));
    checks.push(merged_rows_match(history, it.build));
    checks.push(merges_exact(duckdb, history, it.build));
    checks.sort_by_key(|check| CheckName::ALL.iter().position(|it| *it == check.name));
    Ok(Verdict {
        version: VERDICT_VERSION,
        build: i64::try_from(it.build).unwrap_or(i64::MAX),
        tree: it.tree,
        blaze: it.blaze,
        process: it.process.map(i64::from),
        checks,
    })
}

/// The build's `builds` row: the last ended one, or `asked`'s.
pub(crate) fn build(duckdb: &Path, history: &Path, asked: Option<u64>) -> Result<Build, GateError> {
    let which = match asked {
        // alloc: a query, once a verdict
        Some(build) => format!("build_id = {build}"),
        // alloc: a query's part, once a verdict
        None => "true".to_owned(),
    };
    // alloc: a query, once a verdict
    let sql = format!(
        "SELECT CAST(build_id AS BIGINT) AS build, CAST(started_ns AS BIGINT) AS t0, CAST(dur_ns AS BIGINT) AS dur, \
         CAST(process AS BIGINT) AS process, CAST(tree AS VARCHAR) AS tree, CAST(blaze AS VARCHAR) AS blaze \
         FROM builds WHERE {which} AND dur_ns IS NOT NULL ORDER BY build_id DESC, started_ns DESC LIMIT 1"
    );
    let row = sql::run(&Query { duckdb, history, scope: sql::Scope::All, sql: &sql })?;
    let (Some(build), Some(t0), Some(dur)) =
        (first_i64(&row, 0), first_i64(&row, 1), first_i64(&row, 2))
    else {
        // alloc: an error, once a verdict
        return Err(GateError::NoBuild { history: history.to_owned(), build: asked });
    };
    Ok(Build {
        build: u64::try_from(build).unwrap_or(0),
        t0,
        t1: t0 + dur,
        process: first_i64(&row, 3).and_then(|it| u32::try_from(it).ok()),
        tree: nullable(&row, 4),
        blaze: nullable(&row, 5),
    })
}

/// A check's judgement of its queries' answers, in the order it asked them; `Err` when the
/// answers are not what it asked for.
type Judge = fn(&Build, &[Vec<RecordBatch>]) -> Result<Check, String>;

/// The checks the history's SQL answers: each check's name, its queries and its judge.
fn sql_checks(it: &Build) -> Vec<(CheckName, Vec<String>, Judge)> {
    let Build { build, t0, t1, .. } = *it;
    let (spans, folded, workers, of) = (it.spans(), it.folded(), it.workers(), it.of());
    let events = it.events();
    let mut args = Map::new();
    args.insert("build".into(), Value::from(build));
    let uncovered = views::find("uncovered_per_worker").and_then(|it| views::render(it, &args));
    args.insert("limit".into(), Value::from(NAMED_MAX));
    let raw = views::find("raw_blocks").and_then(|it| views::render(it, &args));
    let view = |name: &str, it: Result<String, ViewError>| {
        // alloc: a query, once a verdict; a broken view is a query DuckDB refuses, naming it
        it.unwrap_or_else(|error| {
            format!(
                "SELECT error('the view {name} would not render: {}')",
                quoted(&error.to_string())
            )
        })
    };
    let slack = CLOCK_SLACK_NS;
    // A child and its parent, by (process, span_id), over the parent's whole process.
    // alloc: a query's part, once a verdict
    let joined = format!(
        "WITH c AS (SELECT process, span_id, parent, name, start_ns, start_ns + dur_ns AS end_ns FROM spans WHERE {spans} AND parent IS NOT NULL), \
         p AS (SELECT process, span_id, name, start_ns, start_ns + dur_ns AS end_ns FROM spans WHERE process IN (SELECT DISTINCT process FROM c)), \
         j AS (SELECT c.span_id, c.name, p.name AS parent_name, p.start_ns AS parent_start, \
               greatest(p.start_ns - c.start_ns, c.end_ns - p.end_ns) AS over_ns \
               FROM c LEFT JOIN p ON p.process = c.process AND p.span_id = c.parent)"
    );
    // alloc: the checks' queries, once a verdict
    vec![
        (
            CheckName::WorkersCovered,
            vec![format!(
                "SELECT CAST(thread AS BIGINT) AS thread, CAST(thread_name AS VARCHAR) AS thread_name, \
                 CAST(uncovered_s AS DOUBLE) AS seconds, CAST(uncovered_pct AS DOUBLE) AS pct \
                 FROM ({}) ORDER BY pct DESC, thread",
                view("uncovered_per_worker", uncovered)
            )],
            workers_covered as Judge,
        ),
        (
            CheckName::BlocksInWaits,
            vec![
                format!(
                    "SELECT CAST(thread AS BIGINT) AS thread, CAST(t_ns AS BIGINT) AS t_ns, CAST(frames AS VARCHAR) AS frames FROM ({})",
                    view("raw_blocks", raw)
                ),
                format!(
                    "SELECT CAST(count(*) FILTER (WHERE cpu = 'off') AS BIGINT) AS off, CAST(count(*) AS BIGINT) AS samples \
                     FROM samples WHERE t_ns BETWEEN {t0} AND {t1}{of}"
                ),
            ],
            blocks_in_waits as Judge,
        ),
        (
            CheckName::WaitsSayWhat,
            vec![format!(
                "SELECT CAST(count(*) AS BIGINT) AS waits, CAST(count(*) FILTER (WHERE awaits IS NULL) AS BIGINT) AS unsaid \
                 FROM spans WHERE {spans} AND name = 'wait'"
            )],
            waits_say_what as Judge,
        ),
        (
            CheckName::RowsLost,
            vec![slow::load::recorder_sql(build, t0, t1, &of)],
            rows_lost as Judge,
        ),
        (
            CheckName::SamplesInSpans,
            vec![format!(
                "WITH s AS (SELECT row_number() OVER () AS id, samples.process, samples.t_ns, samples.span_id \
                 FROM samples JOIN {workers} w USING (process, thread) WHERE samples.t_ns BETWEEN {t0} AND {t1}), \
                 found AS (SELECT s.id, s.t_ns >= spans.start_ns - {slack} AND s.t_ns <= spans.start_ns + spans.dur_ns + {slack} AS inside \
                 FROM s JOIN spans ON spans.process = s.process AND spans.span_id = s.span_id) \
                 SELECT CAST(count(*) AS BIGINT) AS samples, \
                 CAST(count(*) FILTER (WHERE s.span_id IS NULL) AS BIGINT) AS no_span, \
                 CAST(count(*) FILTER (WHERE found.inside = false) AS BIGINT) AS outside, \
                 CAST(count(*) FILTER (WHERE s.span_id IS NOT NULL AND found.id IS NULL) AS BIGINT) AS no_row \
                 FROM s LEFT JOIN found ON found.id = s.id"
            )],
            samples_in_spans as Judge,
        ),
        (
            CheckName::ChildrenInParents,
            vec![
                format!(
                    "{joined} SELECT CAST(count(*) AS BIGINT) AS children, \
                     CAST(count(*) FILTER (WHERE parent_start IS NULL) AS BIGINT) AS orphans, \
                     CAST(count(*) FILTER (WHERE over_ns > {slack}) AS BIGINT) AS over FROM j"
                ),
                format!(
                    "{joined} SELECT CAST(name AS VARCHAR) AS name, CAST(parent_name AS VARCHAR) AS parent_name, \
                     CAST(over_ns AS BIGINT) AS over_ns FROM j WHERE over_ns > {slack} ORDER BY over_ns DESC, span_id LIMIT {SHOWN_MAX}"
                ),
            ],
            children_in_parents as Judge,
        ),
        (
            CheckName::SelfCpuInDuration,
            vec![
                format!(
                    "SELECT CAST((SELECT count(*) FROM spans WHERE {spans} AND self_cpu_ns IS NOT NULL) \
                     + (SELECT count(*) FROM folded WHERE {folded} AND self_cpu_ns IS NOT NULL) AS BIGINT) AS counted"
                ),
                format!(
                    "SELECT * FROM (\
                     SELECT CAST(name AS VARCHAR) AS name, CAST(self_cpu_ns AS BIGINT) AS self_cpu_ns, CAST(dur_ns AS BIGINT) AS dur_ns \
                     FROM spans WHERE {spans} AND (self_cpu_ns < 0 OR self_cpu_ns > dur_ns + {slack}) \
                     UNION ALL \
                     SELECT CAST(name || ' (folded)' AS VARCHAR), CAST(self_cpu_ns AS BIGINT), CAST(dur_ns AS BIGINT) \
                     FROM folded WHERE {folded} AND (self_cpu_ns < 0 OR self_cpu_ns > dur_ns + {slack} * spans)) \
                     ORDER BY self_cpu_ns - dur_ns DESC, name"
                ),
            ],
            self_cpu_in_duration as Judge,
        ),
        (
            CheckName::CpuMatchesProcess,
            vec![format!(
                "SELECT \
                 (SELECT CAST(count(*) AS BIGINT) FROM builds WHERE build_id = {build} AND dur_ns IS NOT NULL \
                  AND daemon_user_ns IS NOT NULL AND daemon_system_ns IS NOT NULL{of}) AS has_cpu, \
                 (SELECT CAST(max(daemon_user_ns + daemon_system_ns) AS DOUBLE) FROM builds WHERE build_id = {build} AND dur_ns IS NOT NULL{of}) AS process_ns, \
                 (SELECT CAST(coalesce(sum(self_cpu_ns), 0) AS DOUBLE) FROM spans WHERE {spans}) \
                 + (SELECT CAST(coalesce(sum(self_cpu_ns), 0) AS DOUBLE) FROM folded WHERE {folded}) AS spans_ns, \
                 (SELECT CAST(count(self_cpu_ns) AS BIGINT) FROM spans WHERE {spans}) AS counted"
            )],
            cpu_matches_process as Judge,
        ),
        (
            CheckName::LockWaits,
            vec![format!(
                "SELECT CAST(lock AS VARCHAR) AS lock, CAST(sum(wait_ns) AS DOUBLE) AS wait_ns, \
                 CAST(count(*) AS BIGINT) AS waits, \
                 CAST((SELECT count(*) FROM {workers} AS w) AS DOUBLE) * {} AS worker_ns \
                 FROM lock_waits WHERE {events} GROUP BY lock ORDER BY wait_ns DESC, lock",
                t1 - t0
            )],
            lock_waits as Judge,
        ),
    ]
}

/// Each of `sqls`' answers, or why it has none. They run in one DuckDB; when that fails, each
/// runs alone so the error is the query's own and the others still answer.
fn answers(duckdb: &Path, history: &Path, sqls: &[&str]) -> Vec<Result<Vec<RecordBatch>, String>> {
    match sql::run_all(duckdb, history, sql::Scope::All, sqls) {
        // alloc: the checks' answers, once a verdict
        Ok(all) => all.into_iter().map(Ok).collect(),
        Err(_) => sqls
            .iter()
            // alloc: an error, once a check
            .map(|&sql| {
                sql::run(&Query { duckdb, history, scope: sql::Scope::All, sql })
                    .map_err(|error| first_line(&error.to_string()))
            })
            // alloc: the checks' answers, once a verdict
            .collect(),
    }
}

fn workers_covered(_: &Build, answers: &[Vec<RecordBatch>]) -> Result<Check, String> {
    let rows = &answers[0];
    let (threads, names, seconds, pcts) =
        (i64s(rows, 0)?, strings(rows, 1)?, f64s(rows, 2)?, f64s(rows, 3)?);
    let name = CheckName::WorkersCovered;
    let Some(&worst) = pcts.first() else {
        return Ok(Check::unchecked(name, "the build has no pool worker's switches"));
    };
    // alloc: a check's offenders, once a check
    let over: Vec<usize> = (0..pcts.len()).filter(|&i| pcts[i] >= UNCOVERED_MAX_PCT).collect();
    let detail = over
        .iter()
        .take(SHOWN_MAX)
        .map(|&i| {
            // alloc: a failure's line, once a failure
            format!(
                "worker {} ({}) was inside no span for {:.2} s, {:.2}% of the build",
                threads[i], names[i], seconds[i], pcts[i]
            )
        })
        // alloc: a check's lines, once a check
        .collect();
    let outcome = if over.is_empty() { Outcome::Passed } else { Outcome::Failed };
    // alloc: a check's line, once a check
    let said = format!(
        "{} of {} workers inside no span for {UNCOVERED_MAX_PCT}% or more of the build; the worst {worst:.2}%",
        over.len(),
        pcts.len()
    );
    Ok(Check::new(name, outcome, said).with(detail))
}

fn blocks_in_waits(_: &Build, answers: &[Vec<RecordBatch>]) -> Result<Check, String> {
    let (threads, at, frames) =
        (i64s(&answers[0], 0)?, i64s(&answers[0], 1)?, strings(&answers[0], 2)?);
    let (off, samples) =
        (first_i64(&answers[1], 0).unwrap_or(0), first_i64(&answers[1], 1).unwrap_or(0));
    let name = CheckName::BlocksInWaits;
    if samples == 0 {
        return Ok(Check::unchecked(
            name,
            "the build has no CPU samples (was the sampler on? `blaze debug dump` says its state)",
        ));
    }
    if threads.is_empty() && off == 0 {
        return Ok(Check::unchecked(
            name,
            format_args!(
                "none of the build's {samples} CPU samples is of a blocked thread (cpu = 'off'), so no block was seen to judge"
            ),
        ));
    }
    let detail = (0..threads.len())
        .take(SHOWN_MAX)
        // alloc: a failure's line, once a failure
        .map(|i| format!("worker {} blocked at {} ns with no wait span; route it through blaze_graph::wait: {}", threads[i], at[i], frames[i]))
        // alloc: a check's lines, once a check
        .collect();
    let many = if threads.len() as u64 >= NAMED_MAX { " or more" } else { "" };
    let outcome = if threads.is_empty() { Outcome::Passed } else { Outcome::Failed };
    // alloc: a check's line, once a check
    let said = format!(
        "{}{many} samples of a worker blocked outside every wait span, of {off} off-CPU samples",
        threads.len()
    );
    Ok(Check::new(name, outcome, said).with(detail))
}

fn waits_say_what(_: &Build, answers: &[Vec<RecordBatch>]) -> Result<Check, String> {
    let (waits, unsaid) =
        (first_i64(&answers[0], 0).unwrap_or(0), first_i64(&answers[0], 1).unwrap_or(0));
    let name = CheckName::WaitsSayWhat;
    if waits == 0 {
        return Ok(Check::unchecked(name, "the build has no wait span"));
    }
    let outcome = if unsaid == 0 { Outcome::Passed } else { Outcome::Failed };
    // alloc: a check's line, once a check
    Ok(Check::new(
        name,
        outcome,
        format!("{unsaid} of {waits} wait spans say nothing of what they await"),
    ))
}

fn rows_lost(_: &Build, answers: &[Vec<RecordBatch>]) -> Result<Check, String> {
    // alloc: an error, once a check
    let recorder = slow::load::recorder(&answers[0]).map_err(|error| error.to_string())?;
    let name = CheckName::RowsLost;
    if recorder.spans_lost.is_none() && recorder.rows_skipped.is_none() {
        return Ok(Check::unchecked(name, "the history has no loss counter in the build's window"));
    }
    let lost = recorder.lost();
    let outcome = if lost <= ROWS_LOST_MAX { Outcome::Passed } else { Outcome::Failed };
    // alloc: a check's line, once a check
    let said = format!(
        "{lost} lost (at most {ROWS_LOST_MAX}): {} spans the recorder could not open, {} rows a ring wrote over ({} of them while the writer was behind)",
        recorder.spans_lost.unwrap_or(0),
        recorder.rows_skipped.unwrap_or(0),
        recorder.rows_behind.unwrap_or(0)
    );
    Ok(Check::new(name, outcome, said))
}

fn samples_in_spans(_: &Build, answers: &[Vec<RecordBatch>]) -> Result<Check, String> {
    let row = &answers[0];
    let at = |i| first_i64(row, i).unwrap_or(0);
    let (samples, no_span, outside, no_row) = (at(0), at(1), at(2), at(3));
    let name = CheckName::SamplesInSpans;
    if samples == 0 {
        return Ok(Check::unchecked(name, "the build has no sample of a pool worker"));
    }
    let tied = samples - no_span - outside;
    let pct = 100.0 * tied as f64 / samples as f64;
    let outcome = if pct >= SAMPLES_IN_SPANS_MIN_PCT { Outcome::Passed } else { Outcome::Failed };
    // alloc: a check's line, once a check
    let said = format!(
        "{pct:.2}% of {samples} worker samples name a span open as they were taken (at least {SAMPLES_IN_SPANS_MIN_PCT}%)"
    );
    // alloc: a check's lines, once a check
    let detail = vec![
        // alloc: a line, once a check
        format!("{no_span} name no span"),
        // alloc: a line, once a check
        format!("{outside} name a span that was not open then (to {CLOCK_SLACK_NS} ns)"),
        // alloc: a line, once a check
        format!("{no_row} name a span with no row (folded, or lost): counted as tied"),
    ];
    Ok(Check::new(name, outcome, said).with(detail))
}

fn lock_waits(_: &Build, answers: &[Vec<RecordBatch>]) -> Result<Check, String> {
    let rows = &answers[0];
    let (locks, waited, waits, worker) =
        (strings(rows, 0)?, f64s(rows, 1)?, i64s(rows, 2)?, f64s(rows, 3)?);
    let name = CheckName::LockWaits;
    // No lock waited for is a pass, unlike the checks that saw nothing to judge: a lock that is
    // never contended records no row (its fast path records nothing), so a build with no
    // contention has no rows and is exactly the clean case. A blaze older than the named locks
    // writes none either, and passes for the same reason: it has nothing this check weighs.
    let Some(&worker_ns) = worker.first() else {
        // alloc: a check's line, once a check
        return Ok(Check::new(name, Outcome::Passed, "no named lock was waited for".to_owned()));
    };
    if worker_ns <= 0.0 {
        return Ok(Check::unchecked(
            name,
            format_args!(
                "{} named locks were waited for, and the build has no pool worker's switches to weigh their waits against",
                locks.len()
            ),
        ));
    }
    let pct = |i: usize| 100.0 * waited[i] / worker_ns;
    // alloc: a check's offenders, once a check
    let over: Vec<usize> = (0..locks.len()).filter(|&i| pct(i) > LOCK_WAIT_MAX_PCT).collect();
    let detail = over
        .iter()
        .take(SHOWN_MAX)
        .map(|&i| {
            // alloc: a failure's line, once a failure
            format!(
                "lock {} was waited for {:.3} s over {} waits, {:.2}% of the pool workers' time: `blaze debug locks --build N` says where and what to use instead",
                locks[i],
                waited[i] / 1e9,
                waits[i],
                pct(i)
            )
        })
        // alloc: a check's lines, once a check
        .collect();
    let outcome = if over.is_empty() { Outcome::Passed } else { Outcome::Failed };
    // alloc: a check's line, once a check
    let said = format!(
        "{} of {} named locks waited for over {LOCK_WAIT_MAX_PCT}% of the pool workers' {:.2} s; the worst, {}, {:.2}%",
        over.len(),
        locks.len(),
        worker_ns / 1e9,
        locks[0],
        pct(0)
    );
    Ok(Check::new(name, outcome, said).with(detail))
}

fn children_in_parents(_: &Build, answers: &[Vec<RecordBatch>]) -> Result<Check, String> {
    let at = |i| first_i64(&answers[0], i).unwrap_or(0);
    let (children, orphans, over) = (at(0), at(1), at(2));
    let name = CheckName::ChildrenInParents;
    if children == orphans {
        return Ok(Check::unchecked(
            name,
            format_args!("none of the build's {children} child spans has its parent's row"),
        ));
    }
    let (names, parents, overs) =
        (strings(&answers[1], 0)?, strings(&answers[1], 1)?, i64s(&answers[1], 2)?);
    // alloc: a check's lines, once a check
    let mut detail: Vec<String> = (0..names.len())
        // alloc: a failure's line, once a failure
        .map(|i| format!("{} overhangs its parent {} by {} ns", names[i], parents[i], overs[i]))
        .collect();
    if orphans > 0 {
        // alloc: a line, once a check
        detail.push(format!(
            "{orphans} children whose parent has no row (folded, lost or still open) not checked"
        ));
    }
    let outcome = if over == 0 { Outcome::Passed } else { Outcome::Failed };
    // alloc: a check's line, once a check
    let said = format!(
        "{over} of {} child spans outside their parent by more than {CLOCK_SLACK_NS} ns",
        children - orphans
    );
    Ok(Check::new(name, outcome, said).with(detail))
}

fn self_cpu_in_duration(_: &Build, answers: &[Vec<RecordBatch>]) -> Result<Check, String> {
    let counted = first_i64(&answers[0], 0).unwrap_or(0);
    let name = CheckName::SelfCpuInDuration;
    if counted == 0 {
        return Ok(Check::unchecked(name, "no span of the build counts its CPU"));
    }
    let (names, cpu, dur) =
        (strings(&answers[1], 0)?, i64s(&answers[1], 1)?, i64s(&answers[1], 2)?);
    let detail = (0..names.len())
        .take(SHOWN_MAX)
        // alloc: a failure's line, once a failure
        .map(|i| format!("{}: self CPU {} ns in {} ns open", names[i], cpu[i], dur[i]))
        // alloc: a check's lines, once a check
        .collect();
    let outcome = if names.is_empty() { Outcome::Passed } else { Outcome::Failed };
    // alloc: a check's line, once a check
    let said = format!(
        "{} of {counted} counted spans have self CPU below 0 or above their duration",
        names.len()
    );
    Ok(Check::new(name, outcome, said).with(detail))
}

fn cpu_matches_process(it: &Build, answers: &[Vec<RecordBatch>]) -> Result<Check, String> {
    let row = &answers[0];
    let name = CheckName::CpuMatchesProcess;
    let has_cpu = first_i64(row, 0).unwrap_or(0);
    if it.process.is_none() {
        return Ok(Check::unchecked(name, "the build's row names no process"));
    }
    if has_cpu == 0 {
        return Ok(Check::unchecked(
            name,
            "the build's row has no `daemon_user_ns`/`daemon_system_ns` (a blaze before them, or Windows)",
        ));
    }
    if first_i64(row, 3).unwrap_or(0) == 0 {
        return Ok(Check::unchecked(name, "no span of the build counts its CPU"));
    }
    let process = first_f64(row, 1).unwrap_or(0.0);
    let spans = first_f64(row, 2).unwrap_or(0.0);
    if process <= 0.0 {
        return Ok(Check::unchecked(name, "the daemon used no CPU over the build by its row"));
    }
    let pct = 100.0 * (spans - process).abs() / process;
    let outcome = if pct <= CPU_DIFF_MAX_PCT { Outcome::Passed } else { Outcome::Failed };
    // alloc: a check's line, once a check
    let said = format!(
        "spans' self CPU {:.3} s against the daemon's {:.3} s: {pct:.2}% apart (at most {CPU_DIFF_MAX_PCT}%)",
        spans / 1e9,
        process / 1e9
    );
    Ok(Check::new(name, outcome, said))
}

/// `blaze slow --json`'s bytes of `build`, twice, compared.
fn slow_deterministic(duckdb: &Path, history: &Path, build: u64) -> Check {
    let name = CheckName::SlowDeterministic;
    let source = Source { duckdb, history };
    let once = || -> Result<String, String> {
        let report = slow::load::run(&source, Some(build))
            .map_err(|error| first_line(&error.to_string()))?;
        // The bytes `blaze slow --json` prints.
        // alloc: a report's JSON, twice a verdict
        Ok(serde_json::to_string(&report).map_err(|error| error.to_string())? + "\n")
    };
    let (first, second) = match (once(), once()) {
        (Ok(first), Ok(second)) => (first, second),
        // alloc: an error, once a verdict
        (Err(why), _) | (_, Err(why)) => {
            return Check::unchecked(name, format_args!("`blaze slow --json`: {why}"));
        }
    };
    match first_difference(&first, &second) {
        // alloc: a check's line, once a verdict
        None => Check::new(
            name,
            Outcome::Passed,
            format!("`blaze slow --json` gave the same {} bytes twice", first.len()),
        ),
        Some(at) => Check::new(
            name,
            Outcome::Failed,
            // alloc: a check's line, once a verdict
            format!(
                "`blaze slow --json` differs between two runs from byte {at} of {} and {}",
                first.len(),
                second.len()
            ),
        )
        // alloc: a check's lines, once a verdict
        .with(vec![around(&first, at), around(&second, at)]),
    }
}

/// The first byte at which `a` and `b` differ, or their shorter one's length when one is the
/// other's start; `None` when they are equal.
fn first_difference(a: &str, b: &str) -> Option<usize> {
    if a == b {
        return None;
    }
    let same = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    Some(same)
}

/// The 40 bytes of `text` each side of `at`, at char boundaries.
fn around(text: &str, at: usize) -> String {
    let mut from = at.saturating_sub(40);
    while !text.is_char_boundary(from) {
        from -= 1;
    }
    let mut to = (at + 40).min(text.len());
    while !text.is_char_boundary(to) {
        to += 1;
    }
    // alloc: a failure's line, once a failure
    format!("...{}...", &text[from..to])
}

/// The history's files of `build` its manifest lists, by table: their names and paths, in name
/// order, and the pin that keeps them on disk while the check reads them ([`history::Pin`]): a
/// file on disk the manifest does not list (a merge's retired input) is not part of the
/// history, and read beside the merged file would count its rows twice.
fn files_of(history: &Path, build: u64) -> Result<(history::Pin, Vec<(Name, PathBuf)>), String> {
    let pin = history::Pin::take(history).map_err(|error| error.to_string())?;
    // alloc: a build's files, once a check
    let mut files: Vec<(Name, PathBuf)> = pin
        .manifest()
        .all()
        .filter_map(|it| Some((Name::parse(it)?, history.join(it))))
        .filter(|(name, _)| name.build == build)
        .collect();
    files.sort_by_key(|(name, _)| (name.table, name.first, name.last, name.level));
    Ok((pin, files))
}

/// A Parquet file's footer row count and the value of its [`history::MERGED_ROWS`] key.
fn footer(path: &Path) -> Result<(i64, Option<String>), String> {
    let file =
        fs::File::open(path).map_err(|error| format!("opening {}: {error}", path.display()))?;
    let metadata = ParquetMetaDataReader::new()
        .with_page_index_policy(PageIndexPolicy::Skip)
        .parse_and_finish(&file)
        .map_err(|error| format!("reading the footer of {}: {error}", path.display()))?;
    let meta = metadata.file_metadata();
    let said = meta
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|it| it.key == history::MERGED_ROWS))
        .and_then(|it| it.value.clone());
    Ok((meta.num_rows(), said))
}

/// Every merged file of `build` holds the rows its merge read, as its metadata says.
fn merged_rows_match(history: &Path, build: u64) -> Check {
    let name = CheckName::MergedRowsMatch;
    let (_pin, files) = match files_of(history, build) {
        Ok(it) => it,
        Err(why) => return Check::unchecked(name, why),
    };
    let (mut keyed, mut unkeyed) = (0usize, 0usize);
    // alloc: a check's lines, once a check
    let mut wrong = Vec::new();
    for (_, path) in files.iter().filter(|(it, _)| it.level >= 1) {
        let (rows, said) = match footer(path) {
            Ok(it) => it,
            Err(why) => return Check::unchecked(name, why),
        };
        let Some(said) = said else {
            unkeyed += 1;
            continue;
        };
        keyed += 1;
        // alloc: a file's name, once a merged file
        let file = path
            .file_name()
            .map_or_else(|| path.display().to_string(), |it| it.to_string_lossy().into_owned());
        match said.parse::<i64>() {
            Ok(merged) if merged == rows => {}
            // alloc: a failure's line, once a failure
            Ok(merged) => wrong.push(format!("{file} holds {rows} rows; its merge read {merged}")),
            // alloc: a failure's line, once a failure
            Err(_) => {
                wrong.push(format!("{file} says {} = {said:?}, not a count", history::MERGED_ROWS))
            }
        }
    }
    if keyed == 0 {
        return Check::unchecked(
            name,
            format_args!(
                "the build has no merged file that says its merge's rows ({unkeyed} merged files without {})",
                history::MERGED_ROWS
            ),
        );
    }
    let outcome = if wrong.is_empty() { Outcome::Passed } else { Outcome::Failed };
    // alloc: a check's line, once a check
    let said = format!(
        "{} of {keyed} merged files hold other than the rows their merge read; {unkeyed} say nothing",
        wrong.len()
    );
    wrong.truncate(SHOWN_MAX);
    Check::new(name, outcome, said).with(wrong)
}

/// The build's files of each table merged again (a table of one file, with itself) into a
/// scratch directory: each merge's rows, by the footers and as a multiset of DuckDB's row
/// hashes, are exactly its inputs'.
fn merges_exact(duckdb: &Path, history: &Path, build: u64) -> Check {
    let name = CheckName::MergesExact;
    match merged_again(duckdb, history, build) {
        Ok(check) => check,
        Err(why) => Check::unchecked(name, why),
    }
}

/// The Parquet files `inputs` merged into `output` as the history's core merges them
/// ([`history::merge::Concat`]): their column chunks copied, the file written whole.
fn merge(
    output: &Path,
    inputs: &[PathBuf],
    properties: &parquet::file::properties::WriterProperties,
) -> Result<(), String> {
    // alloc: a merge's inputs, once a table
    let mut read = Vec::with_capacity(inputs.len());
    for input in inputs {
        // alloc: an input's bytes, once a merge
        let bytes = fs::read(input).map_err(|error| format!("reading {}: {error}", input.display()))?;
        let input = history::merge::Input::of_bytes(bytes::Bytes::from(bytes))
            .map_err(|error| format!("reading the footer of {}: {error}", input.display()))?;
        read.push(input);
    }
    let merged = history::merge::Concat::new(read, properties)
        .and_then(history::merge::Concat::all)
        .map_err(|failed| {
            let file = match failed.input {
                Some(at) => inputs[at].display().to_string(),
                None => output.display().to_string(),
            };
            // alloc: an error
            format!("merging into {}: {} {file}: {}", output.display(), failed.doing, failed.error)
        })?;
    fs::write(output, &merged).map_err(|error| format!("writing {}: {error}", output.display()))
}

fn merged_again(duckdb: &Path, history: &Path, build: u64) -> Result<Check, String> {
    let name = CheckName::MergesExact;
    let (_pin, files) = files_of(history, build)?;
    if files.is_empty() {
        return Ok(Check::unchecked(name, "the build has no file"));
    }
    let scratch = Scratch::new()?;
    let (empty, out) = (scratch.0.join("history"), scratch.0.join("merged"));
    for dir in [&empty, &out] {
        fs::create_dir_all(dir).map_err(|error| format!("making {}: {error}", dir.display()))?;
    }
    let writer = history::Writer::new();
    // alloc: a check's lines, once a check
    let (mut wrong, mut sqls, mut tables) = (Vec::new(), Vec::new(), Vec::new());
    for table in 0..schema::TABLES.len() {
        // alloc: a table's inputs, once a table
        let mut inputs: Vec<PathBuf> =
            files.iter().filter(|(it, _)| it.table == table).map(|(_, it)| it.clone()).collect();
        let Some(first) = inputs.first().cloned() else { continue };
        if inputs.len() == 1 {
            inputs.push(first);
        }
        // alloc: a path, once a table
        let output = out.join(format!("merged.{}.parquet", schema::TABLES[table].name));
        merge(&output, &inputs, writer.properties(table))?;
        let mut read = 0;
        for input in &inputs {
            read += footer(input)?.0;
        }
        let (rows, said) = footer(&output)?;
        let table_name = schema::TABLES[table].name;
        if rows != read || said.as_deref() != Some(read.to_string().as_str()) {
            // alloc: a failure's line, once a failure
            wrong.push(format!(
                "{table_name}: {} inputs hold {read} rows; their merge {rows}, and says {said:?}",
                inputs.len()
            ));
        }
        // alloc: a query's part, once an input
        let hash = |path: &Path| {
            format!(
                "SELECT sum(hash(x)) AS h FROM read_parquet('{}') x",
                quoted(&path.display().to_string())
            )
        };
        // alloc: a query's part, once a table
        let ins: Vec<String> = inputs.iter().map(|it| hash(it.as_path())).collect();
        // alloc: a query, once a table
        sqls.push(format!(
            "SELECT (SELECT CAST(count(*) AS BIGINT) FROM read_parquet('{out}') x) AS rows, \
             CAST(({}) AS VARCHAR) AS merged, CAST((SELECT sum(h) FROM ({})) AS VARCHAR) AS inputs",
            hash(&output),
            ins.join(" UNION ALL "),
            out = quoted(&output.display().to_string()),
        ));
        tables.push((table_name, read));
    }
    // alloc: the queries, once a check
    let asked: Vec<&str> = sqls.iter().map(String::as_str).collect();
    let answers = sql::run_all(duckdb, &empty, sql::Scope::All, &asked)
        .map_err(|error| first_line(&error.to_string()))?;
    for ((table, read), answer) in tables.iter().zip(&answers) {
        let rows = first_i64(answer, 0).unwrap_or(-1);
        let (merged, inputs) = (nullable(answer, 1), nullable(answer, 2));
        if rows != *read || merged != inputs {
            // alloc: a failure's line, once a failure
            wrong.push(format!("{table}: DuckDB reads {rows} rows of the merge, of {read}; row hashes summed {merged:?} against the inputs' {inputs:?}"));
        }
    }
    let outcome = if wrong.is_empty() { Outcome::Passed } else { Outcome::Failed };
    // alloc: a check's line, once a check
    let said =
        format!("{} of {} tables' merges differ from their inputs", wrong.len(), tables.len());
    wrong.truncate(SHOWN_MAX);
    Ok(Check::new(name, outcome, said).with(wrong))
}

/// A scratch directory under the system's temporary one, deleted when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch, String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        // alloc: a directory's path, once a check
        let dir = std::env::temp_dir().join(format!("blaze-gate-{}-{n}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).map_err(|error| format!("making {}: {error}", dir.display()))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A directory under the system's temporary one that is left behind is only space.
        _ = fs::remove_dir_all(&self.0);
    }
}

/// `SELECT * FROM build_summary` of `build`, the per-build summary CI keeps (views.sql), as the
/// Parquet file `out`.
pub fn summary(duckdb: &Path, history: &Path, build: i64, out: &Path) -> Result<(), GateError> {
    // alloc: a query, once a summary
    let sql = format!("SELECT * FROM build_summary WHERE build_id = {build} ORDER BY name");
    let batches = sql::run(&Query { duckdb, history, scope: sql::Scope::All, sql: &sql })?;
    let fail = |why: String| GateError::Summary { path: out.to_owned(), why };
    let Some(first) = batches.iter().find(|it| it.num_rows() > 0) else {
        return Err(fail(format!("build {build} has no row in build_summary")));
    };
    let file = fs::File::create(out).map_err(|error| fail(error.to_string()))?;
    let mut writer = ArrowWriter::try_new(file, first.schema(), None)
        .map_err(|error| fail(error.to_string()))?;
    for batch in &batches {
        writer.write(batch).map_err(|error| fail(error.to_string()))?;
    }
    writer.close().map_err(|error| fail(error.to_string()))?;
    Ok(())
}

/// `text` inside SQL single quotes.
fn quoted(text: &str) -> String {
    // alloc: a query's part
    text.replace('\'', "''")
}

fn first_line(text: &str) -> String {
    // alloc: an error's line
    text.lines().next().unwrap_or("").to_owned()
}

/// Column `at` of every batch, as the `BIGINT` the gate's SQL casts it to; null as 0.
pub(crate) fn i64s(batches: &[RecordBatch], at: usize) -> Result<Vec<i64>, String> {
    // alloc: a check's answer
    let mut out = Vec::new();
    for batch in batches {
        let column = column(batch, at)?
            .as_primitive_opt::<Int64Type>()
            .ok_or_else(|| wrong_type(batch, at, "BIGINT"))?;
        out.extend(column.iter().map(Option::unwrap_or_default));
    }
    Ok(out)
}

pub(crate) fn f64s(batches: &[RecordBatch], at: usize) -> Result<Vec<f64>, String> {
    // alloc: a check's answer
    let mut out = Vec::new();
    for batch in batches {
        let column = column(batch, at)?
            .as_primitive_opt::<Float64Type>()
            .ok_or_else(|| wrong_type(batch, at, "DOUBLE"))?;
        out.extend(column.iter().map(Option::unwrap_or_default));
    }
    Ok(out)
}

pub(crate) fn strings(batches: &[RecordBatch], at: usize) -> Result<Vec<String>, String> {
    // alloc: a check's answer
    let mut out = Vec::new();
    for batch in batches {
        let column = column(batch, at)?
            .as_string_opt::<i32>()
            .ok_or_else(|| wrong_type(batch, at, "VARCHAR"))?;
        // alloc: a check's answer
        out.extend(column.iter().map(|it| it.unwrap_or("?").to_owned()));
    }
    Ok(out)
}

fn column(batch: &RecordBatch, at: usize) -> Result<&dyn Array, String> {
    match at < batch.num_columns() {
        true => Ok(batch.column(at).as_ref()),
        // alloc: an error, a bug in the gate's SQL
        false => Err(format!(
            "the gate's query answered {} columns, not {}",
            batch.num_columns(),
            at + 1
        )),
    }
}

fn wrong_type(batch: &RecordBatch, at: usize, wanted: &str) -> String {
    // alloc: an error, a bug in the gate's SQL
    format!(
        "the gate's query answered column {} as {}, not {wanted}",
        batch.schema().field(at).name(),
        batch.column(at).data_type()
    )
}

/// Row 0's column `at`, a `BIGINT` or null.
fn first_i64(batches: &[RecordBatch], at: usize) -> Option<i64> {
    let batch = batches.iter().find(|it| it.num_rows() > 0)?;
    let column = batch.column(at).as_primitive_opt::<Int64Type>()?;
    column.is_valid(0).then(|| column.value(0))
}

fn first_f64(batches: &[RecordBatch], at: usize) -> Option<f64> {
    let batch = batches.iter().find(|it| it.num_rows() > 0)?;
    let column = batch.column(at).as_primitive_opt::<Float64Type>()?;
    column.is_valid(0).then(|| column.value(0))
}

/// Row 0's column `at`, a string or null.
fn nullable(batches: &[RecordBatch], at: usize) -> Option<String> {
    let batch = batches.iter().find(|it| it.num_rows() > 0)?;
    let column = batch.column(at).as_string_opt::<i32>()?;
    // alloc: a build's identity, once a check
    column.is_valid(0).then(|| column.value(0).to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn verdict(checks: Vec<Check>) -> Verdict {
        Verdict {
            version: VERDICT_VERSION,
            build: 24,
            tree: Some("9f2c".into()),
            blaze: None,
            process: Some(7),
            checks,
        }
    }

    #[test]
    fn a_verdict_says_each_check_and_lists_what_could_not_run() {
        let failed =
            Check::new(CheckName::WorkersCovered, Outcome::Failed, "1 of 8 workers".into()).with(
                vec![
                    "worker 7 (blaze-work-3) was inside no span for 77.00 s, 28.10% of the build"
                        .into(),
                ],
            );
        let unchecked = Check::unchecked(
            CheckName::CpuMatchesProcess,
            "the build's row has no `daemon_user_ns`",
        );
        let passed = Check::new(CheckName::RowsLost, Outcome::Passed, "0 lost".into());
        let it = verdict(vec![failed, unchecked, passed]);
        assert!(!it.passed());
        let said = it.to_string();
        assert!(
            said.starts_with(
                "build 24 (tree 9f2c, blaze not recorded): 1 passed, 1 failed, 1 could not run"
            ),
            "{said}"
        );
        assert!(
            said.contains("FAIL workers_covered: 1 of 8 workers\n       worker 7 (blaze-work-3)"),
            "{said}"
        );
        assert!(said.contains("---- cpu_matches_process: could not run:"), "{said}");
        assert!(said.ends_with("could not run: cpu_matches_process\n"), "{said}");
        assert_eq!(
            it.summary(),
            "build 24: failed: workers_covered; could not run: cpu_matches_process"
        );
        let json = serde_json::to_value(&it).unwrap();
        assert_eq!(json["checks"][1]["outcome"], "unchecked");
        assert_eq!(json["checks"][1]["name"], "cpu_matches_process");
    }

    #[test]
    fn a_check_that_could_not_run_fails_the_verdict() {
        let it = verdict(vec![Check::unchecked(CheckName::MergesExact, "the build has no file")]);
        assert!(!it.passed(), "a check that did not run is never read as clean");
        let clean =
            verdict(vec![Check::new(CheckName::RowsLost, Outcome::Passed, "0 lost".into())]);
        assert!(clean.passed());
    }

    #[test]
    fn check_names_are_their_json() {
        for name in CheckName::ALL {
            assert_eq!(serde_json::to_value(name).unwrap(), Value::from(name.name()));
            assert_eq!(
                serde_json::from_value::<CheckName>(Value::from(name.name())).unwrap(),
                name
            );
        }
    }

    #[test]
    fn the_first_difference_is_the_first_byte_that_differs() {
        assert_eq!(first_difference("abc", "abc"), None);
        assert_eq!(first_difference("abc", "abd"), Some(2));
        assert_eq!(first_difference("ab", "abc"), Some(2));
        assert_eq!(around("{\"a\":1}", 5), "...{\"a\":1}...");
    }

    /// The checks a fixture can make fail; `slow_deterministic` and `merges_exact` test blaze's
    /// own code over any history, so no file can make them fail (history.rs tests the merge).
    const FIXTURED: [CheckName; 10] = [
        CheckName::WorkersCovered,
        CheckName::BlocksInWaits,
        CheckName::WaitsSayWhat,
        CheckName::RowsLost,
        CheckName::SamplesInSpans,
        CheckName::ChildrenInParents,
        CheckName::SelfCpuInDuration,
        CheckName::CpuMatchesProcess,
        CheckName::MergedRowsMatch,
        CheckName::LockWaits,
    ];

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/gate")
    }

    #[test]
    fn every_check_a_file_can_fail_has_its_fixture() {
        for name in FIXTURED {
            let dir = fixtures().join(name.name());
            let builds = dir.join("1-00000001.builds.parquet");
            assert!(
                builds.is_file(),
                "{} has no fixture at {}: run fixtures/gate/make.py",
                name,
                builds.display()
            );
        }
    }

    /// Each fixture fails its own check: the gate's red cases, in seconds.
    #[test]
    #[ignore = "needs the pinned DuckDB: BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb"]
    fn each_fixture_fails_its_own_check() {
        let duckdb = PathBuf::from(
            std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI"),
        );
        for name in FIXTURED {
            let scratch = Scratch::new().unwrap();
            for entry in fs::read_dir(fixtures().join(name.name())).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().is_some_and(|it| it == "parquet") {
                    fs::copy(&path, scratch.0.join(path.file_name().unwrap())).unwrap();
                }
            }
            let verdict =
                check(&duckdb, &scratch.0, None).unwrap_or_else(|error| panic!("{name}: {error}"));
            let it = verdict.check(name).unwrap();
            assert_eq!(
                it.outcome,
                Outcome::Failed,
                "{name}'s fixture does not fail it:\n{verdict}"
            );
        }
    }

    /// The gate over a profiled self-build: `BLAZE_PROFILE_FIXTURE` is the daemon's history
    /// directory (`<state>/profile`), `BLAZE_PROFILE_FIXTURE_BUILD` the build (default: the last
    /// that ended), `BLAZE_DUCKDB` the pinned DuckDB. CI runs the same through `blaze debug gate`
    /// (`harness/profile_gate.py`).
    #[test]
    #[ignore = "needs a profiled self-build and the pinned DuckDB: BLAZE_PROFILE_FIXTURE=<state>/profile BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb"]
    fn a_profiled_self_build_passes_the_gate() {
        let duckdb = PathBuf::from(
            std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI"),
        );
        let history = PathBuf::from(
            std::env::var_os("BLAZE_PROFILE_FIXTURE")
                .expect("BLAZE_PROFILE_FIXTURE names a profiled build's history"),
        );
        let build = std::env::var("BLAZE_PROFILE_FIXTURE_BUILD")
            .ok()
            .map(|it| it.parse().expect("BLAZE_PROFILE_FIXTURE_BUILD is a build's number"));
        let verdict = check(&duckdb, &history, build).unwrap_or_else(|error| panic!("{error}"));
        assert!(verdict.passed(), "{verdict}");
        assert!(
            verdict.tree.is_some() && verdict.blaze.is_some(),
            "a measurement names its tree and binary: {verdict}"
        );
    }
}
