//! `blaze debug diff A B`: what changed from build A to build B. Each phase's time (every span
//! of a name, its whole wall time), the units whose time changed most (a unit's
//! `unit names`/`bodies`/`codegen`/`link`/`store` spans), and the error kinds B said that A
//! did not, and those gone.
//!
//! A side is a build of the history (its number), read through the pinned DuckDB, or a build
//! log as the client printed it (a path): its errors read by [`errors::from_log`] and its
//! phases from the `phases.tsv` of the `BLAZE_PROFILE` directory the log names, for builds from
//! before the history kept errors. A log has no unit's time; the diff says so.

use std::{
    collections::BTreeMap,
    fmt::{self, Write as _},
    fs, io,
    path::{Path, PathBuf},
};

use arrow_array::{Array, RecordBatch, cast::AsArray, types::Int64Type};

use crate::{
    errors,
    sql::{self, Query, SqlError},
};

/// The spans a unit's time is the sum of: its phases, the span kinds blaze-cargo's
/// `serve/unit/compile.rs` opens.
pub const UNIT_PHASES: [&str; 5] = crate::span::UNIT_PHASES;

/// Rows of phases and of units the diff shows at most, most changed first.
pub const LIMIT: usize = 10;

/// What the client prints after the profile of a build: `blaze: profile of N processes in
/// <dir>: trace.json ...` (lib.rs).
const PROFILE_SAYS: &str = "blaze: profile of ";

/// One side of a diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Side {
    /// A build of the history.
    Build(u64),
    /// A build's log.
    Log(PathBuf),
}

impl Side {
    /// A build's number, or the path of a build's log.
    pub fn parse(word: &str) -> Side {
        match word.parse() {
            Ok(build) => Side::Build(build),
            Err(_) => Side::Log(PathBuf::from(word)),
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Side::Build(build) => write!(f, "build {build}"),
            Side::Log(path) => write!(f, "{}", path.display()),
        }
    }
}

/// How often spans of a name ran, and their whole time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    pub count: u64,
    pub ns: i64,
}

/// What a diff compares of a build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Each span name's cost; `Err` why it was not recorded.
    pub phases: Result<BTreeMap<String, Cost>, String>,
    /// Each unit's time, by its crate; `Err` why it was not recorded.
    pub units: Result<BTreeMap<String, i64>, String>,
    /// How many errors of each kind.
    pub errors: BTreeMap<String, u64>,
}

/// A build that recorded nothing yet: no phase, unit or error.
impl Default for Summary {
    fn default() -> Summary {
        Summary { phases: Ok(BTreeMap::new()), units: Ok(BTreeMap::new()), errors: BTreeMap::new() }
    }
}

#[derive(Debug)]
pub enum DiffError {
    Read { path: PathBuf, error: io::Error },
    Sql(SqlError),
    /// The history has no build of that number.
    NoBuild { build: u64, history: PathBuf },
    /// DuckDB's answer is not of the columns asked.
    Answer(String),
    /// A side is a build of the history, and no DuckDB was given.
    NoDuckdb { side: Side },
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiffError::Read { path, error } => write!(f, "blaze debug diff: reading {}: {error}", path.display()),
            DiffError::Sql(error) => write!(f, "blaze debug diff: {error}"),
            DiffError::NoBuild { build, history } => {
                write!(f, "blaze debug diff: the history in {} has no build {build}", history.display())
            }
            DiffError::Answer(what) => write!(f, "blaze debug diff: DuckDB's answer {what}"),
            DiffError::NoDuckdb { side } => {
                write!(f, "blaze debug diff: {side} is read from the history, which needs the pinned DuckDB")
            }
        }
    }
}

impl std::error::Error for DiffError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DiffError::Read { error, .. } => Some(error),
            DiffError::Sql(error) => Some(error),
            _ => None,
        }
    }
}

/// The diff of `a` and `b`, each a build of the history in `history` (read by `duckdb`) or a
/// build's log, as [`render`] prints it.
pub fn run(a: &Side, b: &Side, duckdb: Option<&Path>, history: &Path) -> Result<String, DiffError> {
    let mut builds: Vec<u64> = [a, b]
        .iter()
        .filter_map(|it| match it {
            Side::Build(build) => Some(*build),
            Side::Log(_) => None,
        })
        // alloc: two numbers, once a diff
        .collect();
    builds.dedup();
    // alloc: empty, allocates nothing
    let mut of_history = Vec::new();
    if !builds.is_empty() {
        let side = if matches!(a, Side::Build(_)) { a } else { b };
        // clone: a side, on an error
        let duckdb = duckdb.ok_or_else(|| DiffError::NoDuckdb { side: side.clone() })?;
        of_history = of_builds(duckdb, history, &builds)?;
    }
    let summary = |side: &Side| match side {
        Side::Build(build) => {
            let found = of_history.iter().find(|it: &&(u64, Summary, bool)| it.0 == *build);
            // clone: a build's summary, once a side; `diff 41 41` takes it twice
            Ok(found.map(|it| it.1.clone()).unwrap_or_default())
        }
        Side::Log(path) => of_log(path),
    };
    let (of_a, of_b) = (summary(a)?, summary(b)?);
    Ok(render(a, &of_a, b, &of_b, LIMIT))
}

/// The summaries of `builds` of the history in `history`, in one run of `duckdb`, each with
/// its number and in their order (the last field: the history has its `builds` row). A build
/// the history has no `builds` row of is an error.
pub fn of_builds(duckdb: &Path, history: &Path, builds: &[u64]) -> Result<Vec<(u64, Summary, bool)>, DiffError> {
    // alloc: a query, once a diff
    let list = builds.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
    // alloc: a query, once a diff
    let phases = UNIT_PHASES.map(|it| format!("'{it}'")).join(", ");
    // alloc: a query, once a diff
    let sql = format!(
        "SELECT 'build' AS part, CAST(build_id AS BIGINT) AS build, NULL AS key, CAST(count(*) AS BIGINT) AS count, \
         CAST(NULL AS BIGINT) AS ns FROM builds WHERE build_id IN ({list}) GROUP BY build_id \
         UNION ALL \
         SELECT 'phase', CAST(build_id AS BIGINT), CAST(name AS VARCHAR), CAST(count(*) AS BIGINT), \
         CAST(sum(dur_ns) AS BIGINT) FROM spans WHERE build_id IN ({list}) AND query_kind IS NULL \
         GROUP BY build_id, name \
         UNION ALL \
         SELECT 'unit', CAST(build_id AS BIGINT), CAST(coalesce(crate, '') AS VARCHAR), CAST(count(*) AS BIGINT), \
         CAST(sum(dur_ns) AS BIGINT) FROM spans WHERE build_id IN ({list}) AND name IN ({phases}) \
         GROUP BY build_id, crate \
         UNION ALL \
         SELECT 'error', CAST(build_id AS BIGINT), CAST(kind AS VARCHAR), CAST(count(*) AS BIGINT), \
         CAST(NULL AS BIGINT) FROM errors WHERE build_id IN ({list}) GROUP BY build_id, kind"
    );
    let batches = sql::run(&Query { duckdb, history, sql: &sql }).map_err(DiffError::Sql)?;
    // alloc: two summaries, once a diff
    let mut out: Vec<(u64, Summary, bool)> = builds.iter().map(|it| (*it, Summary::default(), false)).collect();
    for batch in &batches {
        read_answer(batch, &mut out)?;
    }
    if let Some((build, ..)) = out.iter().find(|it| !it.2) {
        // alloc: an error
        return Err(DiffError::NoBuild { build: *build, history: history.to_owned() });
    }
    Ok(out)
}

fn read_answer(batch: &RecordBatch, out: &mut [(u64, Summary, bool)]) -> Result<(), DiffError> {
    let column = |name: &str| {
        batch
            .column_by_name(name)
            // alloc: an error
            .ok_or_else(|| DiffError::Answer(format!("has no column {name}")))
    };
    let (part, build, key, count, ns) = (column("part")?, column("build")?, column("key")?, column("count")?, column("ns")?);
    // dyn: an Arrow column, as DuckDB's answer gives it, once a diff
    let text = |array: &dyn Array, name: &str, row: usize| -> Result<Option<String>, DiffError> {
        if array.is_null(row) {
            return Ok(None);
        }
        match (array.as_string_opt::<i32>(), array.as_string_view_opt()) {
            // alloc: a row of DuckDB's answer, once a diff
            (Some(it), _) => Ok(Some(it.value(row).to_owned())),
            // alloc: a row of DuckDB's answer, once a diff
            (None, Some(it)) => Ok(Some(it.value(row).to_owned())),
            // alloc: an error
            (None, None) => Err(DiffError::Answer(format!("has column {name} of type {}, not text", array.data_type()))),
        }
    };
    // dyn: an Arrow column, as DuckDB's answer gives it, once a diff
    let int = |array: &dyn Array, name: &str, row: usize| -> Result<Option<i64>, DiffError> {
        if array.is_null(row) {
            return Ok(None);
        }
        match array.as_primitive_opt::<Int64Type>() {
            Some(it) => Ok(Some(it.value(row))),
            // alloc: an error
            None => Err(DiffError::Answer(format!("has column {name} of type {}, not BIGINT", array.data_type()))),
        }
    };
    for row in 0..batch.num_rows() {
        let build = int(build.as_ref(), "build", row)?.unwrap_or(0) as u64;
        let key = text(key.as_ref(), "key", row)?.unwrap_or_default();
        let count = int(count.as_ref(), "count", row)?.unwrap_or(0) as u64;
        let ns = int(ns.as_ref(), "ns", row)?.unwrap_or(0);
        let Some((_, summary, known)) = out.iter_mut().find(|it| it.0 == build) else {
            // alloc: an error
            return Err(DiffError::Answer(format!("has a row of build {build}, which was not asked")));
        };
        match text(part.as_ref(), "part", row)?.as_deref() {
            Some("build") => *known = true,
            Some("phase") => {
                if let Ok(phases) = &mut summary.phases {
                    phases.insert(key, Cost { count, ns });
                }
            }
            Some("unit") => {
                if let Ok(units) = &mut summary.units {
                    units.insert(key, ns);
                }
            }
            Some("error") => {
                summary.errors.insert(key, count);
            }
            // alloc: an error
            other => return Err(DiffError::Answer(format!("has a row of part {other:?}"))),
        }
    }
    Ok(())
}

/// The summary of a build's log at `path`: its errors, and the phases of the `phases.tsv` in
/// the profile directory it names.
pub fn of_log(path: &Path) -> Result<Summary, DiffError> {
    let read = |path: &Path| {
        // alloc: an error
        fs::read_to_string(path).map_err(|error| DiffError::Read { path: path.to_owned(), error })
    };
    let log = read(path)?;
    let mut errors = BTreeMap::new();
    for row in errors::from_log(&log) {
        *errors.entry(row.kind).or_default() += 1;
    }
    let profile = log.lines().rev().find_map(|line| {
        let rest = line.strip_prefix(PROFILE_SAYS)?;
        let (_, rest) = rest.split_once(" processes in ")?;
        let (dir, _) = rest.split_once(": trace.json")?;
        Some(PathBuf::from(dir))
    });
    let phases = match profile {
        Some(dir) => Ok(phases_tsv(&read(&dir.join("phases.tsv"))?)),
        // alloc: a note, once a diff
        None => Err("its log names no BLAZE_PROFILE directory".to_owned()),
    };
    // alloc: a note, once a diff
    let units = Err("a log has no unit's time; a build of the history does".to_owned());
    Ok(Summary { phases, units, errors })
}

/// The phases of a profile's `phases.tsv`: `phase`, `count`, `wall_ms`, `cpu_ms`.
fn phases_tsv(text: &str) -> BTreeMap<String, Cost> {
    let mut out = BTreeMap::new();
    for line in text.lines().skip(1) {
        let mut cells = line.split('\t');
        let (Some(name), Some(count), Some(wall_ms)) = (cells.next(), cells.next(), cells.next()) else { continue };
        let (Ok(count), Ok(wall_ms)) = (count.parse::<u64>(), wall_ms.parse::<f64>()) else { continue };
        // alloc: a row, once a diff
        out.insert(name.to_owned(), Cost { count, ns: (wall_ms * 1e6).round() as i64 });
    }
    out
}

/// What [`run`] prints: the phases and units whose time changed most, at most `limit` of each,
/// and the error kinds added and gone.
pub fn render(a: &Side, of_a: &Summary, b: &Side, of_b: &Summary, limit: usize) -> String {
    // alloc: the diff's text, once a diff
    let mut out = format!("blaze debug diff: {a} -> {b}\n");
    let ms = |ns: i64| ns as f64 / 1e6;
    match (&of_a.phases, &of_b.phases) {
        (Ok(pa), Ok(pb)) => {
            let rows = changed(pa, pb, |it| it.ns, |x, y| x.count != y.count);
            table(&mut out, "phases", "every span of each name, wall time", &rows, limit, |out, (_, x, y)| {
                let _ = write!(
                    out,
                    "{:>10.1} ms -> {:>10.1} ms {:>+10.1} ms   {} -> {} times",
                    ms(x.ns),
                    ms(y.ns),
                    ms(y.ns - x.ns),
                    x.count,
                    y.count
                );
            });
        }
        (Err(why), _) => _ = writeln!(out, "phases: not recorded for {a}: {why}"),
        (_, Err(why)) => _ = writeln!(out, "phases: not recorded for {b}: {why}"),
    }
    match (&of_a.units, &of_b.units) {
        (Ok(ua), Ok(ub)) => {
            let rows = changed(ua, ub, |it| *it, |_, _| false);
            table(&mut out, "units", "their unit phases' time", &rows, limit, |out, (_, x, y)| {
                let _ = write!(out, "{:>10.1} ms -> {:>10.1} ms {:>+10.1} ms", ms(*x), ms(*y), ms(y - x));
            });
        }
        (Err(why), _) => _ = writeln!(out, "units: not recorded for {a}: {why}"),
        (_, Err(why)) => _ = writeln!(out, "units: not recorded for {b}: {why}"),
    }
    let kinds = |from: &BTreeMap<String, u64>, to: &BTreeMap<String, u64>| -> Vec<(String, u64)> {
        // clone: an error kind's name, once a diff
        from.iter().filter(|(kind, _)| !to.contains_key(*kind)).map(|(kind, n)| (kind.clone(), *n)).collect()
    };
    for (what, rows) in [("added", kinds(&of_b.errors, &of_a.errors)), ("gone", kinds(&of_a.errors, &of_b.errors))] {
        if rows.is_empty() {
            let _ = writeln!(out, "errors {what}: none");
            continue;
        }
        let _ = writeln!(out, "errors {what}:");
        let width = rows.iter().map(|it| it.0.len()).max().unwrap_or(0);
        for (kind, n) in rows {
            let _ = writeln!(out, "  {kind:<width$} {n}");
        }
    }
    out
}

/// The keys whose value changed from `a` to `b` (a key on one side only counts the other as
/// the default), with both values, most changed first by `ns`; ties by name. `also` says a
/// row changed though its time did not.
fn changed<'a, T: Default + Copy>(
    a: &'a BTreeMap<String, T>,
    b: &'a BTreeMap<String, T>,
    ns: impl Fn(&T) -> i64,
    also: impl Fn(&T, &T) -> bool,
) -> Vec<(&'a str, T, T)> {
    // alloc: the diff's rows, once a diff
    let mut rows: Vec<(&str, T, T)> = a
        .keys()
        .chain(b.keys().filter(|it| !a.contains_key(*it)))
        .map(|key| (key.as_str(), a.get(key).copied().unwrap_or_default(), b.get(key).copied().unwrap_or_default()))
        .filter(|(_, x, y)| ns(x) != ns(y) || also(x, y))
        // alloc: the diff's rows, once a diff
        .collect();
    rows.sort_by(|x, y| (ns(&y.2) - ns(&y.1)).abs().cmp(&(ns(&x.2) - ns(&x.1)).abs()).then(x.0.cmp(y.0)));
    rows
}

fn table<T>(
    out: &mut String,
    what: &str,
    of: &str,
    rows: &[(&str, T, T)],
    limit: usize,
    cells: impl Fn(&mut String, &(&str, T, T)),
) {
    if rows.is_empty() {
        let _ = writeln!(out, "{what}: none changed");
        return;
    }
    let shown = &rows[..rows.len().min(limit)];
    let _ = writeln!(out, "{what}, most changed first ({of}):");
    let width = shown.iter().map(|it| it.0.len()).max().unwrap_or(0);
    for row in shown {
        let _ = write!(out, "  {:<width$} ", row.0);
        cells(out, row);
        out.push('\n');
    }
    if rows.len() > shown.len() {
        let _ = writeln!(out, "  and {} more", rows.len() - shown.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map<T: Copy>(rows: &[(&str, T)]) -> BTreeMap<String, T> {
        rows.iter().map(|(key, value)| ((*key).to_owned(), *value)).collect()
    }

    /// Two builds: `unit codegen` and the unit `app` 250 ms slower, `mir` and `serde` the same,
    /// a derive panic added and a missing native library gone.
    fn fixture() -> (Summary, Summary) {
        let a = Summary {
            phases: Ok(map(&[
                ("unit codegen", Cost { count: 4, ns: 1_000_000_000 }),
                ("mir", Cost { count: 10, ns: 500_000_000 }),
            ])),
            units: Ok(map(&[("app", 600_000_000), ("serde", 400_000_000)])),
            errors: map(&[("link-native-missing", 2), ("E0277", 1)]),
        };
        let b = Summary {
            phases: Ok(map(&[
                ("unit codegen", Cost { count: 4, ns: 1_250_000_000 }),
                ("mir", Cost { count: 10, ns: 500_000_000 }),
            ])),
            units: Ok(map(&[("app", 850_000_000), ("serde", 400_000_000)])),
            errors: map(&[("E0277", 1), ("derive-panic", 3)]),
        };
        (a, b)
    }

    #[test]
    fn a_diff_prints_exactly_what_changed() {
        let (a, b) = fixture();
        let said = render(&Side::Build(41), &a, &Side::Build(42), &b, LIMIT);
        let want = "\
blaze debug diff: build 41 -> build 42
phases, most changed first (every span of each name, wall time):
  unit codegen     1000.0 ms ->     1250.0 ms     +250.0 ms   4 -> 4 times
units, most changed first (their unit phases' time):
  app      600.0 ms ->      850.0 ms     +250.0 ms
errors added:
  derive-panic 3
errors gone:
  link-native-missing 2
";
        assert_eq!(said, want);
        let same = render(&Side::Build(41), &a, &Side::Build(41), &a, LIMIT);
        assert_eq!(
            same,
            "blaze debug diff: build 41 -> build 41\nphases: none changed\nunits: none changed\n\
             errors added: none\nerrors gone: none\n"
        );
    }

    #[test]
    fn past_the_limit_the_rest_are_counted() {
        let a = Summary { units: Ok(map(&[("a", 1), ("b", 2), ("c", 3)])), ..Summary::default() };
        let said = render(&Side::Build(1), &a, &Side::Build(2), &Summary::default(), 2);
        assert!(said.contains("  c ") && said.contains("  b ") && !said.contains("  a "), "{said}");
        assert!(said.contains("  and 1 more\n"), "{said}");
    }

    #[test]
    fn a_side_is_a_build_or_a_log() {
        assert_eq!(Side::parse("12"), Side::Build(12));
        assert_eq!(Side::parse("/tmp/self-0151.log"), Side::Log(PathBuf::from("/tmp/self-0151.log")));
    }

    /// A log's errors and the phases of the profile it names; it has no unit's time.
    #[test]
    fn a_log_is_read_with_the_profile_it_names() {
        let dir = std::env::temp_dir().join(format!("blaze-diff-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("profile")).unwrap();
        fs::write(dir.join("profile/phases.tsv"), "phase\tcount\twall_ms\tcpu_ms\nmir\t3\t12.500\t10.000\n").unwrap();
        let log = format!(
            "error: proc-macro derive panicked\n\nerror: could not compile `app` (lib) due to 1 previous error\n\
             blaze: profile of 1 processes in {}: trace.json (ui.perfetto.dev), phases.tsv, functions.tsv, passes.tsv\n",
            dir.join("profile").display()
        );
        fs::write(dir.join("build.log"), log).unwrap();
        let summary = of_log(&dir.join("build.log")).unwrap();
        assert_eq!(summary.errors, map(&[("derive-panic", 1)]));
        assert_eq!(summary.phases, Ok(map(&[("mir", Cost { count: 3, ns: 12_500_000 })])));
        assert!(summary.units.is_err());
        fs::write(dir.join("bare.log"), "error[E0411]: cannot find type `Self`\n").unwrap();
        let bare = of_log(&dir.join("bare.log")).unwrap();
        assert_eq!(bare.errors, map(&[("E0411", 1)]));
        assert_eq!(bare.phases, Err("its log names no BLAZE_PROFILE directory".to_owned()));
        let said = render(&Side::Log(dir.join("build.log")), &summary, &Side::Log(dir.join("bare.log")), &bare, LIMIT);
        assert!(said.contains("phases: not recorded for ") && said.contains("bare.log: its log names no"), "{said}");
        assert!(said.contains("errors added:\n  E0411 1\nerrors gone:\n  derive-panic 1\n"), "{said}");
        _ = fs::remove_dir_all(&dir);
    }

    /// Two builds flushed to a history, diffed through the pinned DuckDB: the unit phase and
    /// the error that only the second has.
    #[test]
    #[ignore = "needs the pinned DuckDB: BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb"]
    fn two_builds_of_the_history_diff_through_duckdb() {
        use crate::history::{BuildRow, CAP_BYTES, History, Outcome};
        let duckdb = PathBuf::from(std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI"));
        crate::install();
        let dir = std::env::temp_dir().join(format!("blaze-diff-duckdb-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        let mut history = History::open(&dir, CAP_BYTES).unwrap();
        for (build, ms) in [(81u32, 1), (82, 30)] {
            std::thread::spawn(move || {
                let _build = crate::flight::building(build);
                let _unit = tracing::info_span!(target: "blaze", "unit codegen", krate = "app").entered();
                blaze_graph::spin::busy(std::time::Duration::from_millis(ms));
            })
            .join()
            .unwrap();
            let row = BuildRow {
                build_id: u64::from(build),
                commit: None,
                workspace: "/work/app".into(),
                command: "build".into(),
                started_ns: 0,
                dur_ns: Some(1),
                started_unix_ns: None,
                outcome: Outcome::Failed,
                hits: None,
                misses: None,
                joined: None,
                tree: None,
                blaze: None,
            };
            history.build_ended(row);
            if build == 82 {
                let mut rows = Vec::new();
                let of = errors::Of { build_id: 82, krate: Some("app"), unit: Some("app (lib)"), ..errors::Of::default() };
                errors::rows(errors::Said { message: "proc-macro derive panicked", ..errors::Said::default() }, of, &mut rows);
                history.errored(rows);
            }
            history.flush(Some(u64::from(build))).unwrap();
        }
        let said = run(&Side::Build(81), &Side::Build(82), Some(&duckdb), &dir).unwrap();
        assert!(said.contains("\n  unit codegen "), "{said}");
        assert!(said.contains("units, most changed first") && said.contains("\n  app "), "{said}");
        assert!(said.ends_with("errors added:\n  derive-panic 1\nerrors gone: none\n"), "{said}");
        let missing = run(&Side::Build(81), &Side::Build(99), Some(&duckdb), &dir).unwrap_err();
        assert!(matches!(missing, DiffError::NoBuild { build: 99, .. }), "{missing}");
        _ = fs::remove_dir_all(&dir);
    }

    /// The diff of the two self-build logs Andrew named (round 31, and the 214-crate
    /// regression), printed: a look, not a check.
    #[test]
    #[ignore = "reads /tmp/blaze-lanes/self-0151.log and self-0209PST.log; run with --ignored --nocapture"]
    fn the_self_build_logs_diff() {
        let (a, b) = (Side::parse("/tmp/blaze-lanes/self-0151.log"), Side::parse("/tmp/blaze-lanes/self-0209PST.log"));
        println!("{}", run(&a, &b, None, Path::new("/nowhere")).unwrap());
    }
}
