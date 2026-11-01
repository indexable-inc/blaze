//! The profile's gate: what a profiled build must hold so that every moment of every pool worker
//! is accounted for (docs/profiling.md, "Utilization"). Per worker, the share of the build's wall
//! time inside no span and before the pool's first word of it stays under
//! [`UNCOVERED_MAX_PCT`]; no sample of a worker blocked in the kernel (a mutex, a condition,
//! kevent, a read) is outside a `wait` span, since such a block bypassed `blaze_graph::wait`; and
//! every `wait` span says what it awaits. Run over a profiled self-build by the fixture test
//! below; each failure names the worker, or the blocked stack.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use arrow_array::{
    Array, RecordBatch,
    cast::AsArray,
    types::{Float64Type, Int64Type},
};
use serde_json::{Map, Value};

use crate::{
    sql::{self, Query, SqlError},
    views::{self, ViewError},
};

/// The most of a build's wall time a pool worker may spend inside no span, in percent.
pub const UNCOVERED_MAX_PCT: f64 = 1.0;

/// The most raw blocks a verdict names; the count is of all of them.
const NAMED_MAX: u64 = 50;

/// A worker outside every span for too much of the build.
#[derive(Clone, Debug, PartialEq)]
pub struct Uncovered {
    pub thread: i64,
    pub thread_name: String,
    pub seconds: f64,
    pub pct: f64,
}

/// A sample of a worker blocked in the kernel with no `wait` span open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBlock {
    pub thread: i64,
    pub t_ns: i64,
    pub frames: String,
}

/// What the gate found in one build.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub build: i64,
    /// The build's `builds.tree` and `builds.blaze`: the exact tree and binary measured.
    pub tree: Option<String>,
    pub blaze: Option<String>,
    pub uncovered: Vec<Uncovered>,
    /// The first [`NAMED_MAX`] raw blocks, by time.
    pub raw: Vec<RawBlock>,
    /// `wait` spans with no `awaits`.
    pub unsaid_waits: i64,
}

impl Verdict {
    pub fn passed(&self) -> bool {
        self.uncovered.is_empty() && self.raw.is_empty() && self.unsaid_waits == 0
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tree = self.tree.as_deref().unwrap_or("not recorded");
        let blaze = self.blaze.as_deref().unwrap_or("not recorded");
        if self.passed() {
            return write!(
                f,
                "build {} (tree {tree}, blaze {blaze}): every worker's time is accounted for",
                self.build
            );
        }
        writeln!(f, "build {} (tree {tree}, blaze {blaze}) fails the profile's gate:", self.build)?;
        for it in &self.uncovered {
            writeln!(
                f,
                "- worker {} ({}) was inside no span for {:.2} s, {:.2}% of the build (at most {UNCOVERED_MAX_PCT}%)",
                it.thread, it.thread_name, it.seconds, it.pct
            )?;
        }
        for it in &self.raw {
            writeln!(
                f,
                "- worker {} blocked at {} ns with no wait span saying on what; route it through blaze_graph::wait: {}",
                it.thread, it.t_ns, it.frames
            )?;
        }
        if self.unsaid_waits > 0 {
            writeln!(f, "- {} wait spans say nothing of what they await", self.unsaid_waits)?;
        }
        Ok(())
    }
}

/// Why the gate gave no verdict.
#[derive(Debug)]
pub enum GateError {
    Sql(SqlError),
    /// A view the gate reads would not render.
    View(ViewError),
    /// The history has no `builds` row for the build asked for (`None`: for any build).
    NoBuild { history: PathBuf, build: Option<u64> },
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateError::Sql(error) => write!(f, "the profile's gate: {error}"),
            GateError::View(error) => write!(f, "the profile's gate: {error}"),
            GateError::NoBuild { history, build: Some(build) } => {
                write!(f, "the profile's gate: {} has no builds row for build {build}", history.display())
            }
            GateError::NoBuild { history, build: None } => {
                write!(f, "the profile's gate: {} has no build that ended", history.display())
            }
        }
    }
}

impl std::error::Error for GateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GateError::Sql(error) => Some(error),
            GateError::View(error) => Some(error),
            GateError::NoBuild { .. } => None,
        }
    }
}

impl From<SqlError> for GateError {
    fn from(error: SqlError) -> GateError {
        GateError::Sql(error)
    }
}

impl From<ViewError> for GateError {
    fn from(error: ViewError) -> GateError {
        GateError::View(error)
    }
}

/// The gate over `build` of the history in `history` (`None`: the last build that ended), run
/// by the DuckDB at `duckdb`.
pub fn check(duckdb: &Path, history: &Path, asked: Option<u64>) -> Result<Verdict, GateError> {
    let run = |sql: &str| sql::run(&Query { duckdb, history, sql });
    let which = match asked {
        // alloc: a query, once a check
        Some(build) => format!("build_id = {build}"),
        // alloc: a query, once a check
        None => "build_id = (SELECT max(build_id) FROM builds)".to_owned(),
    };
    // alloc: a query, once a check
    let row = run(&format!(
        "SELECT CAST(build_id AS BIGINT) AS build, CAST(tree AS VARCHAR) AS tree, CAST(blaze AS VARCHAR) AS blaze \
         FROM builds WHERE {which} LIMIT 1"
    ))?;
    let Some(&build) = i64s(&row, 0).first() else {
        // alloc: an error, once a check
        return Err(GateError::NoBuild { history: history.to_owned(), build: asked });
    };
    let (tree, blaze) = (nullable(&row, 1), nullable(&row, 2));
    let mut args = Map::new();
    args.insert("build".into(), Value::from(build));
    let uncovered = views::render(views::find("uncovered_per_worker")?, &args)?;
    // alloc: a query, once a check
    let uncovered = run(&format!(
        "SELECT CAST(thread AS BIGINT) AS thread, CAST(thread_name AS VARCHAR) AS thread_name, \
         CAST(uncovered_s AS DOUBLE) AS seconds, CAST(uncovered_pct AS DOUBLE) AS pct \
         FROM ({uncovered}) WHERE uncovered_pct >= {UNCOVERED_MAX_PCT:?} ORDER BY pct DESC"
    ))?;
    args.insert("limit".into(), Value::from(NAMED_MAX));
    let raw = views::render(views::find("raw_blocks")?, &args)?;
    // alloc: a query, once a check
    let raw = run(&format!(
        "SELECT CAST(thread AS BIGINT) AS thread, CAST(t_ns AS BIGINT) AS t_ns, CAST(frames AS VARCHAR) AS frames FROM ({raw})"
    ))?;
    // alloc: a query, once a check
    let unsaid = run(&format!(
        "SELECT CAST(count(*) AS BIGINT) AS n FROM spans WHERE build_id = {build} AND name = 'wait' AND awaits IS NULL"
    ))?;
    let (threads, names, seconds, pcts) =
        (i64s(&uncovered, 0), strings(&uncovered, 1), f64s(&uncovered, 2), f64s(&uncovered, 3));
    let (raw_threads, raw_at, raw_frames) = (i64s(&raw, 0), i64s(&raw, 1), strings(&raw, 2));
    Ok(Verdict {
        build,
        tree,
        blaze,
        uncovered: (0..threads.len())
            .map(|i| Uncovered {
                thread: threads[i],
                // clone: a worker's name, once a failure
                thread_name: names[i].clone(),
                seconds: seconds[i],
                pct: pcts[i],
            })
            // alloc: a check's failures, once a check
            .collect(),
        raw: (0..raw_threads.len())
            .map(|i| RawBlock {
                thread: raw_threads[i],
                t_ns: raw_at[i],
                // clone: a stack, once a failure
                frames: raw_frames[i].clone(),
            })
            // alloc: a check's failures, once a check
            .collect(),
        unsaid_waits: i64s(&unsaid, 0).first().copied().unwrap_or(0),
    })
}

/// Column `at` of every batch, as the `BIGINT` the gate's SQL casts it to; null as 0.
fn i64s(batches: &[RecordBatch], at: usize) -> Vec<i64> {
    batches
        .iter()
        .flat_map(|it| it.column(at).as_primitive::<Int64Type>().iter().map(Option::unwrap_or_default))
        // alloc: a check's answer
        .collect()
}

fn f64s(batches: &[RecordBatch], at: usize) -> Vec<f64> {
    batches
        .iter()
        .flat_map(|it| it.column(at).as_primitive::<Float64Type>().iter().map(Option::unwrap_or_default))
        // alloc: a check's answer
        .collect()
}

fn strings(batches: &[RecordBatch], at: usize) -> Vec<String> {
    batches
        .iter()
        // alloc: a check's answer
        .flat_map(|it| it.column(at).as_string::<i32>().iter().map(|it| it.unwrap_or("?").to_owned()))
        // alloc: a check's answer
        .collect()
}

/// Row 0 of column `at`, a string or null.
fn nullable(batches: &[RecordBatch], at: usize) -> Option<String> {
    let column = batches.first()?.column(at).as_string_opt::<i32>()?;
    // alloc: a build's identity, once a check
    column.is_valid(0).then(|| column.value(0).to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_failed_verdict_names_each_worker_and_each_blocked_stack() {
        let verdict = Verdict {
            build: 24,
            tree: Some("9f2c".into()),
            blaze: None,
            uncovered: vec![Uncovered { thread: 7, thread_name: "blaze-work-3".into(), seconds: 77.0, pct: 28.1 }],
            raw: vec![RawBlock {
                thread: 9,
                t_ns: 55_000_000_000,
                frames: "start > blaze_codegen::flight::Flights::wait > __psynch_cvwait".into(),
            }],
            unsaid_waits: 3,
        };
        assert!(!verdict.passed());
        let said = verdict.to_string();
        assert!(said.contains("worker 7 (blaze-work-3)") && said.contains("28.10%"), "{said}");
        assert!(said.contains("blaze_codegen::flight::Flights::wait > __psynch_cvwait"), "{said}");
        assert!(said.contains("3 wait spans"), "{said}");
        assert!(said.contains("build 24 (tree 9f2c, blaze not recorded)"), "{said}");
        let clean = Verdict { build: 25, tree: None, blaze: None, uncovered: vec![], raw: vec![], unsaid_waits: 0 };
        assert!(clean.passed());
        assert!(clean.to_string().contains("accounted for"));
    }

    /// The gate over a profiled self-build: `BLAZE_PROFILE_FIXTURE` is the daemon's history
    /// directory (`<state>/profile`), `BLAZE_PROFILE_FIXTURE_BUILD` the build (default: the last
    /// that ended), `BLAZE_DUCKDB` the pinned DuckDB.
    #[test]
    #[ignore = "needs a profiled self-build and the pinned DuckDB: BLAZE_PROFILE_FIXTURE=<state>/profile BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb"]
    fn a_profiled_self_build_has_no_worker_outside_spans_and_no_block_unsaid() {
        let duckdb = PathBuf::from(std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI"));
        let history = PathBuf::from(
            std::env::var_os("BLAZE_PROFILE_FIXTURE").expect("BLAZE_PROFILE_FIXTURE names a profiled build's history"),
        );
        let build = std::env::var("BLAZE_PROFILE_FIXTURE_BUILD")
            .ok()
            .map(|it| it.parse().expect("BLAZE_PROFILE_FIXTURE_BUILD is a build's number"));
        let verdict = check(&duckdb, &history, build).unwrap_or_else(|error| panic!("{error}"));
        assert!(verdict.passed(), "{verdict}");
        assert!(verdict.tree.is_some() && verdict.blaze.is_some(), "a measurement names its tree and binary: {verdict}");
    }
}
