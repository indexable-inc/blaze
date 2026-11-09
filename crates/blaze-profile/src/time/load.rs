//! The driver of [`super::Model`] over the history: reads a build's `builds`, `switches`,
//! `spans` and `events` rows through the pinned DuckDB ([`crate::sql`]), and writes the tables the model
//! derives back into the history as `<build>-00000000.<table>.parquet`, so `blaze debug sql`
//! reads them as the views `job_intervals`, `span_time`, `path_links` and `core_busy`.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use arrow_array::{
    Array, ArrayRef, Int64Array, PrimitiveArray, RecordBatch, StringArray, UInt32Array,
    UInt64Array,
    cast::AsArray,
    types::{Float64Type, Int64Type, UInt64Type},
};

use super::{
    CutGroup, Event, EventIn, Input, JOB_SPAN, LaneId, Model, PathRead, SpanCut, SpanIn, State,
    SwitchIn, TimeError, WAIT_SPAN, html,
    path::{self, Path as CriticalPath},
    report::{self, By},
    trace,
};
use crate::{batches, history, schema, schema::Table, sql};

/// Where the history is, and the DuckDB that reads it.
#[derive(Clone, Copy, Debug)]
pub struct Source<'a> {
    pub duckdb: &'a Path,
    pub history: &'a Path,
}

/// What `blaze debug time` asks for.
#[derive(Clone, Copy, Debug)]
pub struct Ask<'a> {
    /// `None`: the last build that ended or whose daemon died.
    pub build: Option<u64>,
    pub by: By,
    pub top: usize,
    /// Only rows whose key holds it.
    pub matching: Option<&'a str>,
    /// The cores' busy time over the build, in this many lines.
    pub cores: Option<usize>,
    pub html: Option<&'a Path>,
    pub trace: Option<&'a Path>,
}

/// `blaze debug time`: the build's model, its tables written into the history, and the report
/// printed; the page and the trace written where `ask` says.
pub fn run(source: &Source<'_>, ask: &Ask<'_>) -> Result<String, TimeError> {
    let Modelled { model, path, process: _, cut } = modelled(source, pick(source, ask.build)?)?;
    let written = write(source.history, &model, &path)?;
    // alloc: the report, once a report
    let mut out = String::new();
    report::summary(&model, &mut out);
    if cut.floor_ns > 0 {
        // Writing to a `String` does not fail.
        _ = writeln!(
            out,
            "spans read: {} of {}, past the bound of {SPAN_BUDGET}: those under {:.3} s ({} spans, {:.1} s summed) are not in the model \
             but for the critical path's lanes{}; the job and wait spans among them are summed by `blaze slow`",
            cut.read,
            cut.spans,
            cut.floor_ns as f64 / 1e9,
            cut.cut(),
            cut.cut_ns as f64 / 1e9,
            cut.path.map_or(String::new(), |it| format!(
                ", read down to {:.3} s ({} spans of {} lanes)",
                it.floor_ns as f64 / 1e9,
                it.spans,
                it.lanes
            )),
        );
    }
    out.push('\n');
    report::table(&report::rows(&model, ask.by, ask.matching), ask.by, ask.top, &mut out);
    out.push('\n');
    report::path(&model, &path, ask.top, &mut out);
    out.push('\n');
    report::scheduling(&model, &path, ask.top, &mut out);
    if let Some(lines) = ask.cores {
        out.push('\n');
        report::cores(&model, lines, &mut out);
    }
    if let Some(file) = ask.html {
        put(file, html::page(&model, &path, ask.top).as_bytes(), "writing the page")?;
        // Writing to a `String` does not fail.
        _ = writeln!(out, "\nthe page: {}", file.display());
    }
    if let Some(file) = ask.trace {
        put(file, &trace::json(&model, &path), "writing the trace")?;
        _ = writeln!(out, "the trace (ui.perfetto.dev): {}", file.display());
    }
    _ = writeln!(
        out,
        "\nfor blaze debug sql, the views job_intervals, span_time, path_links and core_busy of build {} are in:",
        model.build
    );
    for it in written {
        _ = writeln!(out, "  {}", it.display());
    }
    Ok(out)
}

fn put(path: &Path, bytes: &[u8], doing: &'static str) -> Result<(), TimeError> {
    // alloc: an error
    std::fs::write(path, bytes).map_err(|error| TimeError::Io {
        path: path.to_owned(),
        doing,
        error,
    })
}

pub(crate) fn query(
    source: &Source<'_>,
    sql: &str,
    doing: &'static str,
) -> Result<Vec<RecordBatch>, TimeError> {
    sql::run(&sql::Query {
        duckdb: source.duckdb,
        history: source.history,
        scope: sql::Scope::All,
        sql,
    })
    .map_err(|error| TimeError::Sql { doing, error })
}

/// Each of `sqls` in one DuckDB ([`sql::run_all`]) over every file: their answers, in order,
/// one for each.
pub(crate) fn query_many(
    source: &Source<'_>,
    sqls: &[&str],
    doing: &'static str,
) -> Result<Vec<Vec<RecordBatch>>, TimeError> {
    query_many_in(source, sql::Scope::All, sqls, doing)
}

/// [`query_many`] over the files of `scope` alone: a reader of one build reads the footers
/// of its files, not of every file ([`sql::Scope::Build`]).
pub(crate) fn query_many_in(
    source: &Source<'_>,
    scope: sql::Scope,
    sqls: &[&str],
    doing: &'static str,
) -> Result<Vec<Vec<RecordBatch>>, TimeError> {
    let answers = sql::run_all(source.duckdb, source.history, scope, sqls)
        .map_err(|error| TimeError::Sql { doing, error })?;
    if answers.len() != sqls.len() {
        return Err(TimeError::BadColumn {
            table: "answers",
            column: "count",
            why: "is not one answer for each query DuckDB was asked",
        });
    }
    Ok(answers)
}

/// [`query_many`] of a fixed number of queries.
pub(crate) fn query_all<const N: usize>(
    source: &Source<'_>,
    sqls: [&str; N],
    doing: &'static str,
) -> Result<[Vec<RecordBatch>; N], TimeError> {
    <[Vec<RecordBatch>; N]>::try_from(query_many(source, &sqls, doing)?).map_err(|_| {
        TimeError::BadColumn {
            table: "answers",
            column: "count",
            why: "is not one answer for each query DuckDB was asked",
        }
    })
}

/// A build to read and its window ([`pick`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pick {
    pub build: u64,
    /// When it started and how long it ran: to its end row, or to the last row its dead daemon
    /// wrote; `None` while it runs, or when the history has no row of it.
    pub row: Option<(i64, i64)>,
    /// The process of the daemon whose death ended it: its row says `crashed` (a postmortem
    /// read back the daemon's last rows, `flight::postmortem`), or its only row is its start
    /// and that process no longer writes the history ([`history::writes`]).
    pub died: Option<u32>,
}

/// The build's Chrome trace, as `blaze debug time --trace` writes it, its client's rows
/// beside its daemon's: what `BLAZE_PROFILE` exports from the history.
pub fn trace_of(source: &Source<'_>, build: u64) -> Result<Vec<u8>, TimeError> {
    let Rows { input, .. } = rows(source, pick(source, Some(build))?)?;
    let model = Model::build(input)?;
    let path = path::critical(&model);
    Ok(trace::json(&model, &path))
}

/// `build`, or the last build that ended or whose daemon died, and its window.
pub fn pick(source: &Source<'_>, build: Option<u64>) -> Result<Pick, TimeError> {
    // The builds a report may be of: `build`, or the last that ended and every one after it,
    // newest first; of each, its end row before its start row.
    let which = match build {
        // alloc: a query's part, once a report
        Some(it) => format!("build_id = {it}"),
        None => {
            "build_id >= coalesce((SELECT max(build_id) FROM builds WHERE dur_ns IS NOT NULL), 0)"
                .to_owned()
        } // alloc: as above
    };
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(build_id AS UBIGINT) AS build, CAST(started_ns AS BIGINT) AS started_ns, CAST(dur_ns AS BIGINT) AS dur_ns, \
         CAST(process AS BIGINT) AS process, CAST(outcome AS VARCHAR) AS outcome FROM (SELECT * FROM builds UNION ALL BY NAME \
         SELECT NULL::UINTEGER AS process WHERE false) WHERE {which} ORDER BY build_id DESC, dur_ns DESC NULLS LAST"
    );
    let batches =
        query(source, &sql, "finding the build: the last that ended or whose daemon died")?;
    let mut seen = None;
    for batch in &batches {
        const T: &str = "builds";
        let (ids, started, dur, process) = (
            uints(batch, T, "build")?,
            ints(batch, T, "started_ns")?,
            ints(batch, T, "dur_ns")?,
            ints(batch, T, "process")?,
        );
        let outcome = texts(batch, T, "outcome")?;
        for row in 0..batch.num_rows() {
            let Some(id) = get(ids, row, |it, row| it.value(row)) else { continue };
            // A build's first row in this order says how it ended; its others are its start.
            if seen == Some(id) {
                continue;
            }
            seen = Some(id);
            let start = get(started, row, |it, row| it.value(row));
            let pid =
                get(process, row, |it, row| it.value(row)).and_then(|it| u32::try_from(it).ok());
            if let (Some(start), Some(dur)) = (start, get(dur, row, |it, row| it.value(row))) {
                // A crashed build ran to the last row its daemon's postmortem read back.
                let crashed = get(outcome, row, |it, row| it.value(row))
                    == Some(history::Outcome::Crashed.name());
                return Ok(Pick {
                    build: id,
                    row: Some((start, dur)),
                    died: pid.filter(|_| crashed),
                });
            }
            match (start, pid) {
                (Some(start), Some(pid)) if !history::writes(source.history, pid) => {
                    let last = last_row(source, pid, start)?;
                    return Ok(Pick {
                        build: id,
                        row: Some((start, last.max(start) - start)),
                        died: Some(pid),
                    });
                }
                // Still running, in a daemon that writes the history: asked for by name, it is
                // read as far as its rows go; else the one before it is the report's.
                _ if build.is_some() => return Ok(Pick { build: id, row: None, died: None }),
                _ => {}
            }
        }
    }
    // A build asked for by name with no `builds` row is read by its tagged rows alone.
    build.map(|build| Pick { build, row: None, died: None }).ok_or(TimeError::NoBuild)
}

/// The time of the last row the process `pid` wrote from `since` on: a span's end, a switch, a
/// counter, an event, or a reading of its open spans.
fn last_row(source: &Source<'_>, pid: u32, since: i64) -> Result<i64, TimeError> {
    let at = |table: &str, time: &str| {
        // alloc: a query's part, once a report
        format!("(SELECT max({time}) FROM {table} WHERE process = {pid} AND {time} >= {since})")
    };
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(greatest({}, {}, {}, {}, {}) AS BIGINT) AS last_ns",
        at("spans", "start_ns + coalesce(dur_ns, 0)"),
        at("switches", "t_ns"),
        at("counters", "t_ns"),
        at("events", "t_ns"),
        at("open", "read_ns"),
    );
    let batches = query(source, &sql, "finding the last row of the daemon that died")?;
    for batch in &batches {
        let it = ints(batch, "builds", "last_ns")?;
        if batch.num_rows() > 0 {
            return Ok(get(it, 0, |it, row| it.value(row)).unwrap_or(since));
        }
    }
    Ok(since)
}

/// The rows of `build`, or of the last build that ended or whose daemon died.
pub fn load(source: &Source<'_>, build: Option<u64>) -> Result<Input, TimeError> {
    rows(source, pick(source, build)?).map(|it| it.input)
}

/// The most spans of one build the model reads, in both passes ([`modelled`]). Each costs the
/// model about half a KiB (its `SpanIn` with its strings, its time, parent and place; estimated,
/// 2026-11-04), so this bounds the spans to about 4 GiB; the 192 million of build 24
/// (2026-11-03) would have been about 90 GiB, and `blaze slow` passed 19 GiB and was killed. The
/// spans by name, which `blaze slow` counts in DuckDB, always count them all, and the job and
/// wait spans left out are summed there ([`CutGroup`]).
pub const SPAN_BUDGET: u64 = 8_000_000;

/// Of [`SPAN_BUDGET`], the most the second pass adds for the critical path's lanes; the first
/// pass reads at most the rest.
pub const PATH_BUDGET: u64 = 2_000_000;

/// The most stretches (a lane's run on one thread) whose spans the second pass reads: the lanes
/// past it, in path order, keep the first pass's spans alone ([`PathRead::lanes_unread`]).
pub const PATH_STRETCHES: usize = 65_536;

/// The most groups of job and wait spans under the floor a report holds ([`SpanCut::groups`]).
pub const CUT_GROUPS: usize = 64;

/// The floors a pass tries, shortest first: the first that keeps its spans within its budget is
/// taken (0: every span). Steps of 1, 2 and 5 so a build is read from the least floor that fits,
/// not ten times past it. None is under 100 us: the recorder folds what is shorter than
/// [`crate::flight::FOLD_BELOW`] (20 us) at record time. The last keeps only the spans the
/// record never ended: past it is [`TimeError::TooManySpans`], the hard bound, which a build
/// reaches only when its recorder left millions of spans open.
const FLOORS_NS: [i64; 24] = [
    0,
    100_000,
    200_000,
    500_000,
    1_000_000,
    2_000_000,
    5_000_000,
    10_000_000,
    20_000_000,
    50_000_000,
    100_000_000,
    200_000_000,
    500_000_000,
    1_000_000_000,
    2_000_000_000,
    5_000_000_000,
    10_000_000_000,
    20_000_000_000,
    50_000_000_000,
    100_000_000_000,
    200_000_000_000,
    500_000_000_000,
    1_000_000_000_000,
    i64::MAX,
];

/// The columns of `spans` a [`SpanIn`] reads ([`spans`]).
const SPAN_COLUMNS: &str = "CAST(process AS BIGINT) AS process, CAST(thread AS BIGINT) AS thread, CAST(span_id AS UBIGINT) AS span_id, \
     CAST(parent AS UBIGINT) AS parent, CAST(depth AS BIGINT) AS depth, CAST(name AS VARCHAR) AS name, \
     CAST(crate AS VARCHAR) AS krate, CAST(function AS VARCHAR) AS function, CAST(unit AS VARCHAR) AS unit, \
     CAST(awaits AS VARCHAR) AS awaits, CAST(owner AS UBIGINT) AS owner, CAST(job AS UBIGINT) AS job, \
     CAST(creator AS UBIGINT) AS creator, CAST(start_ns AS BIGINT) AS start_ns, CAST(dur_ns AS BIGINT) AS dur_ns, \
     CAST(self_cpu_ns AS BIGINT) AS self_cpu_ns, CAST(coalesce(folded_ns, 0) AS BIGINT) AS folded_ns";

/// The condition on a `spans` row that it is read at `floor` ([`SpanCut`]): at or past it, or
/// unended. Taken by duration alone, a span read keeps its parent: a child lies inside it.
/// [`cut_sql`]'s buckets say the same.
fn kept_at(floor: i64) -> String {
    if floor <= 0 {
        // alloc: a query's part, once a report
        return "true".to_owned();
    }
    // alloc: a query's part, once a floor a report
    format!("(dur_ns IS NULL OR dur_ns >= {floor})")
}

/// The first of [`FLOORS_NS`] a pass reads from, given `kept`, how many spans each keeps (fewer
/// at each higher floor): 0 when every span fits in `whole`, else the first whose spans fit in
/// `budget`; `None` when none does. The first pass has the whole [`SPAN_BUDGET`] when it is the
/// only pass (nothing cut, so no second), and leaves [`PATH_BUDGET`] to the second otherwise.
fn floor_within(kept: &[u64; FLOORS_NS.len()], whole: u64, budget: u64) -> Option<usize> {
    if kept[0] <= whole {
        return Some(0);
    }
    kept.iter().position(|&it| it <= budget)
}

/// The spans of `spans` (a query's `FROM` item) and their time by bucket: bucket `i` holds
/// those [`FLOORS_NS`]`[i]` keeps and the next floor does not, the last the unended ones every
/// floor keeps ([`kept_at`]). One expression and a group a row: half the CPU of a count filtered
/// for each floor (21.8 against 48.1 CPU-seconds over build 24's 192 million spans, 2026-11-04).
fn cut_sql(spans: &str) -> String {
    let last = FLOORS_NS.len() - 1;
    // alloc: a query, once a report
    let mut sql = format!("SELECT CAST(CASE WHEN dur_ns IS NULL THEN {last}");
    for (i, next) in FLOORS_NS[1..last].iter().enumerate() {
        // Writing to a `String` does not fail.
        _ = write!(sql, " WHEN dur_ns < {next} THEN {i}");
    }
    _ = write!(
        sql,
        " ELSE {} END AS UBIGINT) AS bucket, CAST(count(*) AS UBIGINT) AS spans, CAST(coalesce(sum(dur_ns), 0) AS BIGINT) AS dur_ns \
         FROM {spans} GROUP BY 1 ORDER BY 1",
        last - 1
    );
    sql
}

/// Each floor's spans kept and time left out, from the spans and time of each bucket
/// ([`cut_sql`]), and the spans all told.
fn by_floor(
    buckets: &[(usize, u64, i64)],
) -> ([u64; FLOORS_NS.len()], [i64; FLOORS_NS.len()], u64) {
    let (mut kept, mut cut_ns) = ([0; FLOORS_NS.len()], [0; FLOORS_NS.len()]);
    let mut total = 0;
    for &(bucket, spans, dur_ns) in buckets {
        total += spans;
        for (floor, (kept, cut_ns)) in kept.iter_mut().zip(cut_ns.iter_mut()).enumerate() {
            if bucket >= floor {
                *kept += spans;
            } else {
                *cut_ns += dur_ns;
            }
        }
    }
    (kept, cut_ns, total)
}

/// [`cut_sql`]'s answer: (bucket, spans, time) each.
fn buckets(batches: &[RecordBatch]) -> Result<Vec<(usize, u64, i64)>, TimeError> {
    // alloc: the buckets, once a pass a report
    let mut out = Vec::with_capacity(FLOORS_NS.len());
    for batch in batches {
        const T: &str = "spans";
        let (bucket, spans, dur) =
            (uints(batch, T, "bucket")?, uints(batch, T, "spans")?, ints(batch, T, "dur_ns")?);
        for row in 0..batch.num_rows() {
            let Some(at) = get(bucket, row, |it, row| it.value(row)) else { continue };
            let at = usize::try_from(at).unwrap_or(usize::MAX).min(FLOORS_NS.len() - 1);
            out.push((
                at,
                get(spans, row, |it, row| it.value(row)).unwrap_or(0),
                get(dur, row, |it, row| it.value(row)).unwrap_or(0),
            ));
        }
    }
    Ok(out)
}

/// The first pass's floor for `build`'s spans by [`cut_sql`]'s answer `batches`, or the hard
/// bound's refusal when not even its unended spans fit.
fn cut_of(batches: &[RecordBatch], build: u64) -> Result<SpanCut, TimeError> {
    let (kept, cut_ns, total) = by_floor(&buckets(batches)?);
    let Some(at) = floor_within(&kept, SPAN_BUDGET, SPAN_BUDGET - PATH_BUDGET) else {
        let last = FLOORS_NS.len() - 1;
        return Err(TimeError::TooManySpans {
            build,
            spans: total,
            read: kept[last],
            floor_ns: FLOORS_NS[last],
            budget: SPAN_BUDGET,
        });
    };
    Ok(SpanCut {
        floor_ns: FLOORS_NS[at],
        spans: total,
        read: kept[at],
        cut_ns: cut_ns[at],
        ..SpanCut::default()
    })
}

/// Build 24's buckets (2026-11-03, recorded before folding was on) on the old ladder's floors,
/// laid on this one: read whole only within the whole budget; past it, from the least floor that
/// leaves the second pass its share; and never refused while the unended spans fit.
#[cfg(test)]
#[test]
fn a_floor_is_the_least_that_keeps_the_spans_within_the_budget() {
    // Bucket 0 is everything under 100 us; 4 (1 ms), 7 (10 ms), 10 (100 ms) and 13 (1 s) are the
    // old floors' buckets; 23 is a few unended spans.
    let buckets = [
        (0, 175_430_112, 994_484_495_816),
        (1, 6_920_778, 2_516_678_571_008),
        (4, 2_642_107, 8_700_493_894_942),
        (7, 714_057, 17_909_316_998_858),
        (10, 42_229, 10_606_481_020_222),
        (13, 600_000, 69_084_468_020_044),
        (23, 1_283, 0),
    ];
    let (kept, cut_ns, total) = by_floor(&buckets);
    assert_eq!(total, 186_350_566);
    assert_eq!(kept[0], total);
    assert_eq!(kept[4], 2_642_107 + 714_057 + 42_229 + 600_000 + 1_283);
    assert_eq!(cut_ns[4], 994_484_495_816 + 2_516_678_571_008);
    assert!(
        kept.windows(2).all(|it| it[0] >= it[1]),
        "every floor keeps what the next does and more"
    );
    let first =
        |kept: &[u64; FLOORS_NS.len()]| floor_within(kept, SPAN_BUDGET, SPAN_BUDGET - PATH_BUDGET);
    // 4.0 M from 200 us on fits the first pass's 6 M; 10.9 M from 100 us does not.
    assert_eq!(first(&kept).map(|it| FLOORS_NS[it]), Some(200_000));
    let mut small = [0; FLOORS_NS.len()];
    small[0] = 7_000_000;
    assert_eq!(
        first(&small),
        Some(0),
        "a build within the whole budget is read whole, with no second pass"
    );
    // Build 9's shape (2026-11-04): 240 M spans, 10.2 M of them root job and wait spans that the
    // old floors always kept, now cut by duration like any other.
    let build_9 = [(0, 229_000_000, 0), (4, 9_000_000, 0), (13, 2_000_000, 0), (23, 4_096, 0)];
    let (kept, _, _) = by_floor(&build_9);
    assert_eq!(first(&kept).map(|it| FLOORS_NS[it]), Some(2_000_000), "{kept:?}");
    let mut open = [0; FLOORS_NS.len()];
    open.fill(SPAN_BUDGET + 1);
    assert_eq!(first(&open), None, "past the unended spans' floor is the hard bound's refusal");
    // The second pass always fits: its spans have ended, and the last floor keeps none of them.
    let path = [(0, 30_000_000, 0), (7, 1_500_000, 0)];
    let (kept, _, _) = by_floor(&path);
    assert_eq!(
        floor_within(&kept, PATH_BUDGET, PATH_BUDGET).map(|it| FLOORS_NS[it]),
        Some(100_000)
    );
}

/// A build's rows ([`rows`]).
#[derive(Debug)]
pub struct Rows {
    pub input: Input,
    /// The process whose rows are of the build; `None` when none is.
    pub process: Option<u32>,
    /// Which of its spans `input` holds: the first pass of [`SpanCut`].
    pub cut: SpanCut,
}

/// The build's spans as a query's `FROM` item: its own, and its process's of no build in
/// `window` (a condition on the row's time).
fn spans_from(build: u64, window: Option<(i64, i64)>, process: Option<u32>) -> String {
    // alloc: a condition, once a report
    let of = process.map_or(String::new(), |it| format!(" AND process = {it}"));
    of_build(
        "spans",
        build,
        SPANS_SINCE_2,
        window.map(|(t0, t1)| format!("start_ns >= {t0} AND start_ns < {t1}{of}")),
    )
}

/// The rows of the picked build, the process whose rows are of it, and which of its spans were
/// read ([`SpanCut`]: all of them, unless they pass [`SPAN_BUDGET`]; then those of the first
/// pass, and the job and wait spans under its floor summed).
pub fn rows(source: &Source<'_>, pick: Pick) -> Result<Rows, TimeError> {
    let Pick { build, row: build_row, died: _ } = pick;
    // Rows of no build in the build's window are its too: the recorder leaves `build_id` null on
    // a job's rows once its code let go of the build (most of a cold build's switches and spans
    // on 2026-11-02), and a reader that took only the tagged rows saw a tenth of the work.
    let window = build_row.map(|(start, dur)| (start, start + dur));
    // The rows of no build in the window are its daemon's alone: another process's there (a
    // daemon before a reboot, whose clock began again) are of none of its work.
    let process = build_process(source, build)?;
    let of_spans = spans_from(build, window, process);
    let window = window.map(|(t0, t1)| {
        (t0, t1, process.map_or(String::new(), |it| format!(" AND process = {it}")))
    });
    // alloc: a query, once a report
    let switches_sql = format!(
        "SELECT CAST(process AS BIGINT) AS process, CAST(thread AS BIGINT) AS thread, CAST(t_ns AS BIGINT) AS t_ns, \
         CAST(event AS VARCHAR) AS event, CAST(job AS UBIGINT) AS job, CAST(fiber AS BIGINT) AS fiber, \
         CAST(core AS BIGINT) AS core, CAST(wait_kind AS VARCHAR) AS wait_kind, CAST(wait_value AS UBIGINT) AS wait_value, \
         CAST(by_job AS UBIGINT) AS by_job, CAST(since_ns AS BIGINT) AS since_ns, CAST(began_as AS UBIGINT) AS began_as, \
         CAST(queued_ns AS BIGINT) AS queued_ns, CAST(cpu_ns AS BIGINT) AS cpu_ns FROM {}",
        of_build(
            "switches",
            build,
            SWITCHES_SINCE_2,
            window.as_ref().map(|(t0, t1, of)| format!("t_ns BETWEEN {t0} AND {t1}{of}"))
        )
    );
    // alloc: a query, once a report
    let events_of = format!(
        "events WHERE build_id = {build}{}",
        window.as_ref().map_or(String::new(), |(t0, t1, of)| format!(
            " OR (build_id IS NULL AND t_ns BETWEEN {t0} AND {t1}{of})"
        ))
    );
    // alloc: a query, once a report
    let events_sql = format!(
        "SELECT CAST(process AS BIGINT) AS process, CAST(thread AS BIGINT) AS thread, CAST(t_ns AS BIGINT) AS t_ns, \
         CAST(build_id AS UBIGINT) AS build_id, CAST(job AS UBIGINT) AS job, CAST(level AS VARCHAR) AS level, \
         CAST(target AS VARCHAR) AS target, CAST(name AS VARCHAR) AS name, CAST(message AS VARCHAR) AS message, \
         CAST(span_id AS UBIGINT) AS span_id, CAST(event_id AS UBIGINT) AS event_id FROM {events_of}"
    );
    // The fields of those events: a file older than `event_fields` (schema version 5) has none.
    // alloc: a query, once a report
    let fields_sql = format!(
        "SELECT CAST(f.process AS BIGINT) AS process, CAST(f.event_id AS UBIGINT) AS event_id, \
         CAST(f.key AS VARCHAR) AS key, CAST(f.kind AS VARCHAR) AS kind, CAST(f.value_i64 AS BIGINT) AS value_i64, \
         CAST(f.value_u64 AS UBIGINT) AS value_u64, CAST(f.value_f64 AS DOUBLE) AS value_f64, \
         CAST(f.value_bool AS BOOLEAN) AS value_bool, CAST(f.value_str AS VARCHAR) AS value_str \
         FROM event_fields f SEMI JOIN (SELECT process, event_id FROM {events_of}) e \
         ON f.process = e.process AND f.event_id = e.event_id ORDER BY f.process, f.event_id, f.ordinal"
    );
    // The switches, the spans' count by duration, the events and their fields in one DuckDB:
    // the views are made once ([`sql::run_all`]). The spans wait on the count's floor.
    let [switches_rows, cut_rows, events_rows, fields_rows] = query_all(
        source,
        [
            switches_sql.as_str(),
            cut_sql(&of_spans).as_str(),
            events_sql.as_str(),
            fields_sql.as_str(),
        ],
        "reading the build's switches, events and their fields, and counting its spans by duration",
    )?;
    let switches = switches(&switches_rows)?;
    let mut cut = cut_of(&cut_rows, build)?;
    let events = events(&events_rows, &fields_rows)?;
    // Their Arrow copies go before the spans come in.
    drop((switches_rows, cut_rows, events_rows, fields_rows));
    // alloc: a query, once a report
    let sql = format!("SELECT {SPAN_COLUMNS} FROM {of_spans} WHERE {}", kept_at(cut.floor_ns));
    let spans = if cut.floor_ns > 0 {
        // The job and wait spans under the floor, summed where they are: what each job ran and
        // what each wait was on, which the model no longer holds a row of.
        let (by_subject, by_name) =
            (groups_sql(&of_spans, cut.floor_ns, true), groups_sql(&of_spans, cut.floor_ns, false));
        let [spans_rows, groups_rows, totals_rows] = query_all(
            source,
            [sql.as_str(), by_subject.as_str(), by_name.as_str()],
            "reading the build's spans past the floor, and summing its job and wait spans under it",
        )?;
        cut.groups = groups(&groups_rows)?;
        cut.totals = groups(&totals_rows)?;
        drop((groups_rows, totals_rows));
        spans_of(&spans_rows)?
    } else {
        spans_of(&query(source, &sql, "reading the build's spans")?)?
    };
    // What was read, not what was counted: a flush between the two queries adds rows.
    cut.read = spans.len() as u64;
    Ok(Rows { input: Input { build, build_row, switches, spans, events }, process, cut })
}

/// The root job spans and the wait spans of `spans` (a `FROM` item) under `floor`, summed: by
/// name, crate and subject, the [`CUT_GROUPS`] that took the most time, or by name alone, all.
fn groups_sql(spans: &str, floor: i64, by_subject: bool) -> String {
    let (keys, limit) = match by_subject {
        true => (
            // alloc: a query's part, once a report
            format!(
                "CAST(crate AS VARCHAR) AS krate, CAST(CASE WHEN name = '{WAIT_SPAN}' THEN awaits ELSE function END AS VARCHAR) AS subject"
            ),
            // alloc: a query's part, once a report
            format!(" LIMIT {CUT_GROUPS}"),
        ),
        // alloc: a query's part, once a report
        false => (
            "CAST(NULL AS VARCHAR) AS krate, CAST(NULL AS VARCHAR) AS subject".to_owned(),
            String::new(),
        ),
    };
    // alloc: a query, once a report
    format!(
        "SELECT CAST(name AS VARCHAR) AS name, {keys}, CAST(count(*) AS UBIGINT) AS spans, CAST(sum(dur_ns) AS BIGINT) AS dur_ns, \
         CAST(max(dur_ns) AS BIGINT) AS max_ns, CAST(coalesce(sum(self_cpu_ns), 0) AS BIGINT) AS self_cpu_ns \
         FROM {spans} WHERE dur_ns < {floor} AND (name = '{WAIT_SPAN}' OR (depth = 0 AND name = '{JOB_SPAN}')) \
         GROUP BY 1, 2, 3 ORDER BY 5 DESC, 1, 2 NULLS FIRST, 3 NULLS FIRST{limit}"
    )
}

/// [`groups_sql`]'s answer.
fn groups(batches: &[RecordBatch]) -> Result<Vec<CutGroup>, TimeError> {
    const T: &str = "spans";
    // alloc: the groups, once a report
    let mut out = Vec::new();
    let text = |it: &StringArray, row: usize| get(it, row, |it, row| it.value(row).to_owned());
    for batch in batches {
        let (name, krate, subject) =
            (texts(batch, T, "name")?, texts(batch, T, "krate")?, texts(batch, T, "subject")?);
        let (spans, dur, max, cpu) = (
            uints(batch, T, "spans")?,
            ints(batch, T, "dur_ns")?,
            ints(batch, T, "max_ns")?,
            ints(batch, T, "self_cpu_ns")?,
        );
        for row in 0..batch.num_rows() {
            out.push(CutGroup {
                name: text(name, row).unwrap_or_default(),
                krate: text(krate, row),
                subject: text(subject, row),
                spans: get(spans, row, |it, row| it.value(row)).unwrap_or(0),
                dur_ns: get(dur, row, |it, row| it.value(row)).unwrap_or(0),
                max_ns: get(max, row, |it, row| it.value(row)).unwrap_or(0),
                self_cpu_ns: get(cpu, row, |it, row| it.value(row)).unwrap_or(0),
            });
        }
    }
    Ok(out)
}

/// A build's model and its critical path, and which of its spans the model holds ([`SpanCut`]).
#[derive(Debug)]
pub struct Modelled {
    pub model: Model,
    pub path: CriticalPath,
    /// The process whose rows are of the build; `None` when none is.
    pub process: Option<u32>,
    pub cut: SpanCut,
}

/// The model of the picked build and its critical path. Within [`SPAN_BUDGET`], of every span;
/// past it, in two passes: a first model of the switches and the first pass's spans ([`rows`])
/// gives a critical path, and the spans of the lanes it crosses are read again under the floor
/// (within [`PATH_BUDGET`]), so the final model's path is named by its lanes' every span (or every
/// one past the second floor), not by the long ones alone. The switches are read once: the first
/// model sorts them and leaves them to the second.
pub fn modelled(source: &Source<'_>, pick: Pick) -> Result<Modelled, TimeError> {
    let Rows { input, process, mut cut } = rows(source, pick)?;
    if cut.floor_ns <= 0 {
        let model = Model::build(input)?;
        let path = path::critical(&model);
        return Ok(Modelled { model, path, process, cut });
    }
    let Input { build, build_row, mut switches, spans, events } = input;
    // alloc: none: the first model lays no events
    let first = Model::of(build, build_row, &mut switches, spans, Vec::new())?;
    let crossed = crossed(&first, &path::critical(&first));
    let mut spans = first.into_spans();
    let first_read = spans.len();
    let window = build_row.map(|(start, dur)| (start, start + dur));
    let floor_ns = match crossed.stretches.is_empty() && crossed.jobs.is_empty() {
        true => cut.floor_ns,
        false => {
            let (more, floor_ns) = path_spans(
                source,
                &spans_from(build, window, process),
                build,
                cut.floor_ns,
                &crossed,
            )?;
            spans.extend(more);
            floor_ns
        }
    };
    // A span both passes read (a flush between them adds rows) once.
    // Stable: the first pass's row of a span read twice comes first and is the one kept.
    spans.sort_by_key(|it| (it.process, it.span_id));
    spans.dedup_by_key(|it| (it.process, it.span_id));
    let added = spans.len().saturating_sub(first_read) as u64;
    let model = Model::of(build, build_row, &mut switches, spans, events)?;
    drop(switches);
    let path = path::critical(&model);
    // alloc: once a report
    let mut on_path: Vec<LaneId> = path.links.iter().map(|it| model.lanes[it.lane].id).collect();
    on_path.sort_unstable();
    on_path.dedup();
    let lanes_unread =
        on_path.iter().filter(|&&it| crossed.lanes.binary_search(&it).is_err()).count() as u64;
    cut.read = model.spans.len() as u64;
    cut.path =
        Some(PathRead { floor_ns, lanes: crossed.lanes.len() as u64, spans: added, lanes_unread });
    Ok(Modelled { model, path, process, cut })
}

/// The lanes a critical path crosses, and where their spans are ([`modelled`]'s second pass).
#[derive(Debug, Default)]
struct Crossed {
    /// Sorted.
    lanes: Vec<LaneId>,
    /// (process, thread, from, to): each job lane's runs on their threads, each thread lane's
    /// life on its thread; sorted, and those of one thread that overlap or touch merged, so a
    /// span starts in at most one.
    stretches: Vec<(u32, u32, i64, i64)>,
    /// (process, number) of each job lane: its own and the one it had before it joined the
    /// graph, which its `job` span names. Sorted.
    jobs: Vec<(u32, u64)>,
}

/// [`Crossed`] of `path`: its lanes in path order (with the lanes of the waits it crossed), each
/// taken whole while the stretches stay within [`PATH_STRETCHES`].
fn crossed(model: &Model, path: &CriticalPath) -> Crossed {
    // alloc: once a report
    let mut taken = vec![false; model.lanes.len()];
    // alloc: once a report
    let mut seen = vec![false; model.lanes.len()];
    let mut out = Crossed::default();
    let lanes = path
        .links
        .iter()
        .flat_map(|it| [Some(it.lane), it.wakes.map(|w| model.waits[w].lane)])
        .flatten();
    for l in lanes {
        if std::mem::replace(&mut seen[l], true) {
            continue;
        }
        let lane = &model.lanes[l];
        let before = out.stretches.len();
        match lane.id {
            LaneId::Job { process, .. } => {
                for it in &lane.intervals {
                    if let State::Running { thread, .. } = it.state {
                        out.stretches.push((process, thread, it.from_ns, it.to_ns));
                    }
                }
            }
            // A thread lane's spans are its thread's from its first to its last: its stretches
            // between them are the first pass's floor's, not the thread's.
            LaneId::Thread { process, thread } => {
                out.stretches.push((process, thread, lane.begin_ns(), lane.end_ns() + 1))
            }
        }
        if out.stretches.len() > PATH_STRETCHES {
            out.stretches.truncate(before);
            continue;
        }
        taken[l] = true;
        out.lanes.push(lane.id);
    }
    for (&(process, number), &l) in &model.jobs {
        if taken[l] {
            out.jobs.push((process, number));
        }
    }
    out.jobs.sort_unstable();
    out.jobs.dedup();
    out.lanes.sort_unstable();
    out.stretches.sort_unstable();
    // alloc: once a report
    let mut merged: Vec<(u32, u32, i64, i64)> = Vec::with_capacity(out.stretches.len());
    for &(process, thread, from, to) in &out.stretches {
        match merged.last_mut() {
            Some(last) if (last.0, last.1) == (process, thread) && from <= last.3 => {
                last.3 = last.3.max(to)
            }
            _ => merged.push((process, thread, from, to)),
        }
    }
    out.stretches = merged;
    out
}

/// The spans of `crossed` in `spans` (a `FROM` item) under `floor`, the first pass's: the spans
/// that began in one of its stretches on its thread (a thread runs one fiber at a time, so a
/// span that began in a job's run is that job's), and its job lanes' `job` spans. A stretch
/// is matched by its thread and the 2^30 ns (about 1 s) buckets it covers, so DuckDB hashes the
/// stretches and streams the spans past them once: a range join would hold every span, and one
/// on the thread alone would test each against all its thread's stretches.
fn crossed_sql(spans: &str, floor: i64, crossed: &Crossed) -> String {
    // alloc: a query's parts, once a report
    let mut branches: Vec<String> = Vec::with_capacity(2);
    if !crossed.stretches.is_empty() {
        // alloc: a query's part, once a report
        let mut values = String::with_capacity(48 * crossed.stretches.len());
        for (i, (process, thread, from, to)) in crossed.stretches.iter().enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            // Writing to a `String` does not fail.
            _ = write!(
                values,
                "{sep}({process}::BIGINT, {thread}::BIGINT, {from}::BIGINT, {to}::BIGINT)"
            );
        }
        // alloc: a query's part, once a report
        branches.push(format!(
            "SELECT s.* FROM {spans} AS s JOIN (SELECT st_process, st_thread, st_from, st_to, \
             unnest(range(st_from >> 30, (st_to >> 30) + 1)) AS st_bucket FROM (VALUES {values}) AS v(st_process, st_thread, st_from, st_to)) AS st \
             ON CAST(s.process AS BIGINT) = st.st_process AND CAST(s.thread AS BIGINT) = st.st_thread AND (CAST(s.start_ns AS BIGINT) >> 30) = st.st_bucket \
             WHERE s.dur_ns < {floor} AND s.start_ns >= st.st_from AND s.start_ns < st.st_to"
        ));
    }
    if !crossed.jobs.is_empty() {
        // alloc: a query's part, once a report
        let mut values = String::with_capacity(32 * crossed.jobs.len());
        for (i, (process, job)) in crossed.jobs.iter().enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            _ = write!(values, "{sep}({process}::BIGINT, {job}::UBIGINT)");
        }
        // alloc: a query's part, once a report
        branches.push(format!(
            "SELECT s.* FROM {spans} AS s JOIN (VALUES {values}) AS j(j_process, j_job) \
             ON CAST(s.process AS BIGINT) = j.j_process AND CAST(s.job AS UBIGINT) = j.j_job \
             WHERE s.name = '{JOB_SPAN}' AND s.dur_ns < {floor}"
        ));
    }
    // A span both branches hold (a lane's `job` span that began in its run) comes twice; the
    // reader keeps one ([`modelled`]).
    // alloc: a query, once a report
    format!("({})", branches.join(" UNION ALL "))
}

/// The second pass of [`modelled`]: the spans of `crossed` under `floor`, read from the least
/// floor that keeps them within [`PATH_BUDGET`], and that floor (`floor` itself when it reads
/// nothing more).
fn path_spans(
    source: &Source<'_>,
    spans: &str,
    build: u64,
    floor: i64,
    crossed: &Crossed,
) -> Result<(Vec<SpanIn>, i64), TimeError> {
    let from = crossed_sql(spans, floor, crossed);
    let counted = query(
        source,
        &cut_sql(&from),
        "counting the spans of the critical path's lanes under the floor by duration",
    )?;
    let (kept, _, total) = by_floor(&buckets(&counted)?);
    // Each of these spans ended (the unended were all read in the first pass), so the last floor
    // keeps none and this always finds one.
    let last = FLOORS_NS.len() - 1;
    let at = floor_within(&kept, PATH_BUDGET, PATH_BUDGET).ok_or(TimeError::TooManySpans {
        build,
        spans: total,
        read: kept[last],
        floor_ns: FLOORS_NS[last],
        budget: PATH_BUDGET,
    })?;
    let path_floor = FLOORS_NS[at];
    if path_floor >= floor {
        // alloc: none
        return Ok((Vec::new(), floor));
    }
    // alloc: a query, once a report
    let sql = format!("SELECT {SPAN_COLUMNS} FROM {from} WHERE {}", kept_at(path_floor));
    Ok((
        spans_of(&query(
            source,
            &sql,
            "reading the spans of the critical path's lanes under the floor",
        )?)?,
        path_floor,
    ))
}

/// The process whose rows are of `build`, `None` when no row is. Rows of one build from more
/// than one process are a [`TimeError::ManyProcesses`] naming each (rule 21): daemons that each
/// numbered a build so in one history, whose workers and windows read as one build would be
/// summed (selfbuild4, 2026-11-03: four daemons' build 1 read as one of 72 workers and 2296 s).
fn build_process(source: &Source<'_>, build: u64) -> Result<Option<u32>, TimeError> {
    let client = history::CLIENT_PROCESS;
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(process AS BIGINT) AS process, CAST(count(*) AS UBIGINT) AS rows, CAST(min(from_ns) AS BIGINT) AS from_ns, \
         CAST(max(to_ns) AS BIGINT) AS to_ns FROM (\
         SELECT process, start_ns AS from_ns, start_ns + dur_ns AS to_ns FROM spans WHERE build_id = {build} \
         AND process_name IS DISTINCT FROM '{client}' \
         UNION ALL SELECT process, t_ns, t_ns FROM switches WHERE build_id = {build}) GROUP BY 1 ORDER BY 1"
    );
    let batches = query(source, &sql, "reading which processes wrote the build's rows")?;
    // The rows its client sent are of the build too, from the client's process, and name it
    // (`history::CLIENT_PROCESS`): left out above, as the build's daemon is the one asked for.
    // alloc: the build's processes, once a report
    let mut processes = Vec::new();
    for batch in &batches {
        const T: &str = "spans";
        let (process, rows) = (ints(batch, T, "process")?, uints(batch, T, "rows")?);
        let (from, to) = (ints(batch, T, "from_ns")?, ints(batch, T, "to_ns")?);
        for row in 0..batch.num_rows() {
            processes.push(super::OfProcess {
                process: process.value(row) as u32,
                rows: rows.value(row),
                from_ns: from.value(row),
                to_ns: to.value(row),
            });
        }
    }
    match &processes[..] {
        [] => Ok(None),
        [one] => Ok(Some(one.process)),
        _ => Err(TimeError::ManyProcesses { build, processes }),
    }
}

/// The columns schema version 2 added, and typed span columns added since (appended, so they
/// kept the version), as typed nulls: a history of older files has none of them, and a query
/// naming a column no file has fails to bind. `switches.cpu_ns` came after (2026-11-04).
const SWITCHES_SINCE_2: &str = "NULL::UBIGINT AS by_job, NULL::BIGINT AS since_ns, NULL::UBIGINT AS began_as, NULL::BIGINT AS queued_ns, NULL::BIGINT AS cpu_ns";
/// And `spans.folded_ns`, which version 3 added: a file of an older version folded nothing
/// ([`crate::schema::VERSION`]), so its null reads as 0.
const SPANS_SINCE_2: &str =
    "NULL::UBIGINT AS job, NULL::UBIGINT AS creator, NULL::BIGINT AS folded_ns";

/// `table`'s rows of `build`, and its rows of no build that `window` holds (a condition on the
/// row's time), with the `added` columns null where its files have none.
fn of_build(table: &str, build: u64, added: &str, window: Option<String>) -> String {
    let untagged = window.map_or(String::new(), |it| format!(" OR (build_id IS NULL AND {it})"));
    // alloc: a query, once a table a report
    format!(
        "(SELECT * FROM {table} WHERE build_id = {build}{untagged} UNION ALL BY NAME SELECT {added} WHERE false)"
    )
}

pub(crate) fn array<'a>(
    batch: &'a RecordBatch,
    table: &'static str,
    column: &'static str,
) -> Result<&'a dyn Array, TimeError> {
    batch.column_by_name(column).map(|it| it.as_ref()).ok_or(TimeError::BadColumn {
        table,
        column,
        why: "is not among the rows DuckDB answered",
    })
}

pub(crate) fn ints<'a>(
    batch: &'a RecordBatch,
    table: &'static str,
    column: &'static str,
) -> Result<&'a PrimitiveArray<Int64Type>, TimeError> {
    array(batch, table, column)?.as_primitive_opt::<Int64Type>().ok_or(TimeError::BadColumn {
        table,
        column,
        why: "is not a BIGINT",
    })
}

pub(crate) fn uints<'a>(
    batch: &'a RecordBatch,
    table: &'static str,
    column: &'static str,
) -> Result<&'a PrimitiveArray<UInt64Type>, TimeError> {
    array(batch, table, column)?.as_primitive_opt::<UInt64Type>().ok_or(TimeError::BadColumn {
        table,
        column,
        why: "is not a UBIGINT",
    })
}

pub(crate) fn texts<'a>(
    batch: &'a RecordBatch,
    table: &'static str,
    column: &'static str,
) -> Result<&'a StringArray, TimeError> {
    array(batch, table, column)?.as_string_opt::<i32>().ok_or(TimeError::BadColumn {
        table,
        column,
        why: "is not a VARCHAR",
    })
}

pub(crate) fn get<'a, A: Array, T>(
    array: &'a A,
    row: usize,
    value: impl FnOnce(&'a A, usize) -> T,
) -> Option<T> {
    (!array.is_null(row)).then(|| value(array, row))
}

/// A column of a batch that must not be null, as `u32`.
fn u32_at(
    array: &PrimitiveArray<Int64Type>,
    row: usize,
    table: &'static str,
    column: &'static str,
) -> Result<u32, TimeError> {
    get(array, row, |it, row| it.value(row))
        .and_then(|it| u32::try_from(it).ok())
        .ok_or(TimeError::BadColumn { table, column, why: "is null or past u32" })
}

fn switches(batches: &[RecordBatch]) -> Result<Vec<SwitchIn>, TimeError> {
    const T: &str = "switches";
    // alloc: the build's switches, once a report
    let mut out = Vec::with_capacity(batches.iter().map(RecordBatch::num_rows).sum());
    for batch in batches {
        let (process, thread, t_ns, event) = (
            ints(batch, T, "process")?,
            ints(batch, T, "thread")?,
            ints(batch, T, "t_ns")?,
            texts(batch, T, "event")?,
        );
        let (job, fiber, core) =
            (uints(batch, T, "job")?, ints(batch, T, "fiber")?, ints(batch, T, "core")?);
        let (wait_kind, wait_value) =
            (texts(batch, T, "wait_kind")?, uints(batch, T, "wait_value")?);
        let (by_job, since_ns, began_as) =
            (uints(batch, T, "by_job")?, ints(batch, T, "since_ns")?, uints(batch, T, "began_as")?);
        let (queued_ns, cpu_ns) = (ints(batch, T, "queued_ns")?, ints(batch, T, "cpu_ns")?);
        for row in 0..batch.num_rows() {
            let name = get(event, row, |it, row| it.value(row)).ok_or(TimeError::BadColumn {
                table: T,
                column: "event",
                why: "is null",
            })?;
            let wait = get(wait_kind, row, |it, row| it.value(row))
                .map(|kind| (kind, get(wait_value, row, |it, row| it.value(row))));
            out.push(SwitchIn {
                process: u32_at(process, row, T, "process")?,
                thread: u32_at(thread, row, T, "thread")?,
                t_ns: get(t_ns, row, |it, row| it.value(row)).ok_or(TimeError::BadColumn {
                    table: T,
                    column: "t_ns",
                    why: "is null",
                })?,
                event: Event::of(name, wait)?,
                job: get(job, row, |it, row| it.value(row)).ok_or(TimeError::BadColumn {
                    table: T,
                    column: "job",
                    why: "is null",
                })?,
                fiber: u32_at(fiber, row, T, "fiber")?,
                core: u32_at(core, row, T, "core")?,
                by: get(by_job, row, |it, row| it.value(row)),
                since_ns: get(since_ns, row, |it, row| it.value(row)),
                began_as: get(began_as, row, |it, row| it.value(row)),
                queued_ns: get(queued_ns, row, |it, row| it.value(row)),
                cpu_ns: get(cpu_ns, row, |it, row| it.value(row)),
            });
        }
    }
    Ok(out)
}

fn spans_of(batches: &[RecordBatch]) -> Result<Vec<SpanIn>, TimeError> {
    const T: &str = "spans";
    // alloc: the build's spans, once a report
    let mut out = Vec::with_capacity(batches.iter().map(RecordBatch::num_rows).sum());
    let text = |it: &StringArray, row: usize| get(it, row, |it, row| it.value(row).to_owned());
    for batch in batches {
        let (process, thread, span_id, parent) = (
            ints(batch, T, "process")?,
            ints(batch, T, "thread")?,
            uints(batch, T, "span_id")?,
            uints(batch, T, "parent")?,
        );
        let (depth, name, krate, function) = (
            ints(batch, T, "depth")?,
            texts(batch, T, "name")?,
            texts(batch, T, "krate")?,
            texts(batch, T, "function")?,
        );
        let (unit, awaits, owner, of_job) = (
            texts(batch, T, "unit")?,
            texts(batch, T, "awaits")?,
            uints(batch, T, "owner")?,
            uints(batch, T, "job")?,
        );
        let creator = uints(batch, T, "creator")?;
        let (start_ns, dur_ns, self_cpu) = (
            ints(batch, T, "start_ns")?,
            ints(batch, T, "dur_ns")?,
            ints(batch, T, "self_cpu_ns")?,
        );
        let folded = ints(batch, T, "folded_ns")?;
        let null = |column: &'static str| TimeError::BadColumn { table: T, column, why: "is null" };
        for row in 0..batch.num_rows() {
            out.push(SpanIn {
                process: u32_at(process, row, T, "process")?,
                thread: u32_at(thread, row, T, "thread")?,
                span_id: get(span_id, row, |it, row| it.value(row)).ok_or(null("span_id"))?,
                parent: get(parent, row, |it, row| it.value(row)),
                depth: get(depth, row, |it, row| it.value(row))
                    .and_then(|it| u16::try_from(it).ok())
                    .ok_or(null("depth"))?,
                name: text(name, row).ok_or(null("name"))?,
                krate: text(krate, row),
                function: text(function, row),
                unit: text(unit, row),
                awaits: text(awaits, row),
                owner: get(owner, row, |it, row| it.value(row)),
                job: get(of_job, row, |it, row| it.value(row)),
                creator: get(creator, row, |it, row| it.value(row)),
                start_ns: get(start_ns, row, |it, row| it.value(row)).ok_or(null("start_ns"))?,
                dur_ns: get(dur_ns, row, |it, row| it.value(row)).ok_or(null("dur_ns"))?,
                self_cpu_ns: get(self_cpu, row, |it, row| it.value(row)),
                folded_ns: get(folded, row, |it, row| it.value(row)).unwrap_or(0),
            });
        }
    }
    Ok(out)
}

/// The events of `batches`, each with its fields from `fields` (ordered by process, event and
/// ordinal). An event of a file older than schema version 5 has no id (0) and no fields.
fn events(batches: &[RecordBatch], fields: &[RecordBatch]) -> Result<Vec<EventIn>, TimeError> {
    const T: &str = "events";
    // alloc: the build's events, once a report
    let mut out = Vec::with_capacity(batches.iter().map(RecordBatch::num_rows).sum());
    let text = |it: &StringArray, row: usize| get(it, row, |it, row| it.value(row).to_owned());
    let mut fields_of = event_fields(fields)?;
    for batch in batches {
        let (process, thread, t_ns, job) = (
            ints(batch, T, "process")?,
            ints(batch, T, "thread")?,
            ints(batch, T, "t_ns")?,
            uints(batch, T, "job")?,
        );
        let (level, target, name, message) = (
            texts(batch, T, "level")?,
            texts(batch, T, "target")?,
            texts(batch, T, "name")?,
            texts(batch, T, "message")?,
        );
        let (span_id, build_id, event_id) = (
            uints(batch, T, "span_id")?,
            uints(batch, T, "build_id")?,
            uints(batch, T, "event_id")?,
        );
        let null = |column: &'static str| TimeError::BadColumn { table: T, column, why: "is null" };
        for row in 0..batch.num_rows() {
            let process = u32_at(process, row, T, "process")?;
            let event_id = get(event_id, row, |it, row| it.value(row));
            out.push(EventIn {
                process,
                thread: u32_at(thread, row, T, "thread")?,
                t_ns: get(t_ns, row, |it, row| it.value(row)).ok_or(null("t_ns"))?,
                build: get(build_id, row, |it, row| it.value(row)),
                job: get(job, row, |it, row| it.value(row)),
                level: text(level, row).ok_or(null("level"))?,
                target: text(target, row).ok_or(null("target"))?,
                name: text(name, row).ok_or(null("name"))?,
                message: text(message, row),
                span: get(span_id, row, |it, row| it.value(row)),
                event_id: event_id.unwrap_or(0),
                fields: event_id
                    .and_then(|id| fields_of.remove(&(process, id)))
                    .unwrap_or_default(),
            });
        }
    }
    Ok(out)
}

/// The rows of `event_fields` in `batches`, by process and event, each event's in order.
fn event_fields(
    batches: &[RecordBatch],
) -> Result<
    blaze_hash::HashMap<(u32, u64), Vec<(String, crate::logs::Value)>, std::alloc::Global>,
    TimeError,
> {
    use crate::logs::Value;
    const T: &str = "event_fields";
    // alloc: the build's event fields, once a report
    let mut out = blaze_hash::HashMap::new_in(std::alloc::Global);
    for batch in batches {
        let (process, event_id, key, kind) = (
            ints(batch, T, "process")?,
            uints(batch, T, "event_id")?,
            texts(batch, T, "key")?,
            texts(batch, T, "kind")?,
        );
        let (value_i64, value_u64, value_str) = (
            ints(batch, T, "value_i64")?,
            uints(batch, T, "value_u64")?,
            texts(batch, T, "value_str")?,
        );
        let value_f64 = array(batch, T, "value_f64")?.as_primitive_opt::<Float64Type>().ok_or(
            TimeError::BadColumn { table: T, column: "value_f64", why: "is not a DOUBLE" },
        )?;
        let value_bool =
            array(batch, T, "value_bool")?.as_boolean_opt().ok_or(TimeError::BadColumn {
                table: T,
                column: "value_bool",
                why: "is not a BOOLEAN",
            })?;
        let null = |column: &'static str| TimeError::BadColumn { table: T, column, why: "is null" };
        for row in 0..batch.num_rows() {
            let process = u32_at(process, row, T, "process")?;
            let id = get(event_id, row, |it, row| it.value(row)).ok_or(null("event_id"))?;
            let value = match get(kind, row, |it, row| it.value(row)).ok_or(null("kind"))? {
                "i64" => Value::I64(
                    get(value_i64, row, |it, row| it.value(row)).ok_or(null("value_i64"))?,
                ),
                "u64" => Value::U64(
                    get(value_u64, row, |it, row| it.value(row)).ok_or(null("value_u64"))?,
                ),
                "f64" => Value::F64(
                    get(value_f64, row, |it, row| it.value(row)).ok_or(null("value_f64"))?,
                ),
                "bool" => Value::Bool(
                    get(value_bool, row, |it, row| it.value(row)).ok_or(null("value_bool"))?,
                ),
                "str" => Value::Str(get(value_str, row, |it, row| it.value(row).to_owned())),
                _ => {
                    return Err(TimeError::BadColumn {
                        table: T,
                        column: "kind",
                        why: "is no kind of value: i64, u64, f64, bool or str",
                    });
                }
            };
            // alloc: a field's key, once a field a report
            let key = get(key, row, |it, row| it.value(row).to_owned()).ok_or(null("key"))?;
            out.entry((process, id)).or_insert_with(Vec::new).push((key, value));
        }
    }
    Ok(out)
}

/// The derived tables of `model` written into the history `dir`: their files.
pub fn write(dir: &Path, model: &Model, path: &CriticalPath) -> Result<Vec<PathBuf>, TimeError> {
    // alloc: the files written, once a report
    let mut written = Vec::with_capacity(4);
    // alloc: the tables' rows, once a report
    let mut rows = Vec::with_capacity(4);
    for (table, batch) in [
        (&schema::JOB_INTERVALS, job_intervals(model)),
        (&schema::SPAN_TIME, span_time(model)),
        (&schema::PATH_LINKS, path_links(model, path)),
        (&schema::CORE_BUSY, core_busy(model)),
    ] {
        let batch = batch.map_err(|error| TimeError::Arrow { table: table.name, error })?;
        // alloc: a file's name, once a table a report
        let file = dir.join(format!("{}-00000000.{}.parquet", model.build, table.name));
        written.push(file);
        rows.push((table, [batch]));
    }
    // Listed in the manifest in one commit, in place of the last report's of the build, which
    // the same names replace ([`history::write_listed`]).
    let files: Vec<(&Path, &'static Table, &[RecordBatch])> =
        // alloc: once a report
        written.iter().zip(&rows).map(|(file, (table, batch))| (file.as_path(), *table, batch.as_slice())).collect();
    history::write_listed(dir, &files).map_err(TimeError::History)?;
    Ok(written)
}

fn column(array: impl Array + 'static) -> ArrayRef {
    // alloc: a batch's column, once a table a report
    Arc::new(array)
}

fn batch(
    table: &'static Table,
    columns: Vec<ArrayRef>,
) -> Result<RecordBatch, arrow_schema::ArrowError> {
    RecordBatch::try_new(batches::schema_of(table), columns)
}

fn job_intervals(model: &Model) -> Result<RecordBatch, arrow_schema::ArrowError> {
    // alloc: once a report
    let rows: Vec<(&super::Lane, &super::Interval)> = model
        .lanes
        .iter()
        .flat_map(|lane| lane.intervals.iter().map(move |it| (lane, it)))
        .collect();
    let wait = |it: &super::Interval| it.state.wait().map(|w| &model.waits[w]);
    let running = |it: &super::Interval| match it.state {
        State::Running { core, thread, end } => Some((core, thread, end)),
        _ => None,
    };
    batch(
        &schema::JOB_INTERVALS,
        // alloc: a batch's columns, once a report
        vec![
            column(UInt64Array::from_iter_values(rows.iter().map(|_| model.build))),
            column(UInt32Array::from_iter_values(rows.iter().map(|(lane, _)| lane.id.process()))),
            column(UInt64Array::from_iter(rows.iter().map(|(lane, _)| lane.id.job()))),
            column(UInt32Array::from_iter(rows.iter().map(|(lane, _)| match lane.id {
                LaneId::Thread { thread, .. } => Some(thread),
                LaneId::Job { .. } => None,
            }))),
            column(UInt32Array::from_iter(rows.iter().map(|(lane, _)| lane.fiber))),
            column(StringArray::from_iter_values(rows.iter().map(|(_, it)| it.state.name()))),
            column(Int64Array::from_iter_values(rows.iter().map(|(_, it)| it.from_ns))),
            column(Int64Array::from_iter_values(rows.iter().map(|(_, it)| it.to_ns))),
            column(Int64Array::from_iter_values(rows.iter().map(|(_, it)| it.dur_ns()))),
            column(UInt32Array::from_iter(
                rows.iter().map(|(_, it)| running(it).and_then(|it| it.0)),
            )),
            column(UInt32Array::from_iter(rows.iter().map(|(_, it)| running(it).map(|it| it.1)))),
            column(StringArray::from_iter(
                rows.iter().map(|(_, it)| running(it).map(|it| it.2.name())),
            )),
            column(StringArray::from_iter(
                rows.iter().map(|(_, it)| wait(it).and_then(|w| w.on).map(|on| on.name())),
            )),
            column(UInt64Array::from_iter(
                rows.iter().map(|(_, it)| wait(it).and_then(|w| w.on.map(|_| w.value))),
            )),
            column(StringArray::from_iter(rows.iter().map(|(_, it)| {
                wait(it).and_then(|w| w.span).and_then(|s| model.spans[s].awaits.as_deref())
            }))),
            column(UInt64Array::from_iter(rows.iter().map(|(_, it)| {
                wait(it).and_then(|w| w.waker.lane()).and_then(|l| model.lanes[l].id.job())
            }))),
            column(StringArray::from_iter(
                rows.iter().map(|(_, it)| wait(it).map(|w| w.waker.name())),
            )),
            column(UInt64Array::from_iter(
                rows.iter().map(|(_, it)| it.inside.map(|s| model.spans[s].span_id)),
            )),
            column(StringArray::from_iter(rows.iter().map(|(_, it)| match it.state {
                State::Unrecorded { why } => Some(why.name()),
                _ => None,
            }))),
        ],
    )
}

fn span_time(model: &Model) -> Result<RecordBatch, arrow_schema::ArrowError> {
    let spans = &model.spans;
    let times = &model.span_time;
    let ns = |at: fn(&super::SpanTime) -> i64| {
        column(Int64Array::from_iter_values(times.iter().map(at)))
    };
    batch(
        &schema::SPAN_TIME,
        // alloc: a batch's columns, once a report
        vec![
            column(UInt64Array::from_iter_values(spans.iter().map(|_| model.build))),
            column(UInt32Array::from_iter_values(spans.iter().map(|it| it.process))),
            column(UInt64Array::from_iter_values(spans.iter().map(|it| it.span_id))),
            column(UInt64Array::from_iter(
                times.iter().map(|it| it.lane.and_then(|l| model.lanes[l].id.job())),
            )),
            column(UInt32Array::from_iter_values(spans.iter().map(|it| it.thread))),
            column(StringArray::from_iter_values(spans.iter().map(|it| it.name.as_str()))),
            column(StringArray::from_iter(spans.iter().map(|it| it.krate.as_deref()))),
            column(StringArray::from_iter(spans.iter().map(|it| it.function.as_deref()))),
            column(StringArray::from_iter(spans.iter().map(|it| it.unit.as_deref()))),
            column(Int64Array::from_iter_values(spans.iter().map(|it| it.start_ns))),
            column(Int64Array::from_iter_values(spans.iter().map(|it| it.dur_ns))),
            ns(|it| it.totals.running_ns),
            ns(|it| it.totals.parked_ns),
            ns(|it| it.totals.queued_ns),
            ns(|it| it.totals.parked_or_queued_ns),
            ns(|it| it.totals.unrecorded_ns),
            ns(super::SpanTime::uncovered_ns),
            ns(|it| it.own.waiting_ns()),
        ],
    )
}

fn path_links(model: &Model, path: &CriticalPath) -> Result<RecordBatch, arrow_schema::ArrowError> {
    let links = &path.links;
    let lane = |it: &path::Link| &model.lanes[it.lane];
    batch(
        &schema::PATH_LINKS,
        // alloc: a batch's columns, once a report
        vec![
            column(UInt64Array::from_iter_values(links.iter().map(|_| model.build))),
            column(UInt64Array::from_iter_values(0..links.len() as u64)),
            column(StringArray::from_iter_values(links.iter().map(|it| it.kind.name()))),
            column(UInt32Array::from_iter_values(links.iter().map(|it| lane(it).id.process()))),
            column(UInt64Array::from_iter(links.iter().map(|it| lane(it).id.job()))),
            // alloc: a cell, once a row a report
            column(StringArray::from_iter_values(links.iter().map(|it| lane(it).id.to_string()))),
            column(UInt32Array::from_iter(links.iter().map(|it| {
                match lane(it).intervals[it.interval].state {
                    State::Running { core, .. } => core,
                    _ => None,
                }
            }))),
            column(Int64Array::from_iter_values(links.iter().map(|it| it.from_ns))),
            column(Int64Array::from_iter_values(links.iter().map(|it| it.to_ns))),
            column(Int64Array::from_iter_values(links.iter().map(path::Link::dur_ns))),
            column(StringArray::from_iter_values(
                links.iter().map(|it| report::link_item(model, it)),
            )),
            column(UInt64Array::from_iter(
                links
                    .iter()
                    .map(|it| it.wakes.and_then(|w| model.lanes[model.waits[w].lane].id.job())),
            )),
            column(StringArray::from_iter(
                links.iter().map(|it| it.wakes.map(|w| model.wait_label(&model.waits[w]))),
            )),
        ],
    )
}

fn core_busy(model: &Model) -> Result<RecordBatch, arrow_schema::ArrowError> {
    let rows = report::core_busy(model, 1_000_000_000);
    batch(
        &schema::CORE_BUSY,
        // alloc: a batch's columns, once a report
        vec![
            column(UInt64Array::from_iter_values(rows.iter().map(|_| model.build))),
            column(UInt32Array::from_iter_values(rows.iter().map(|it| it.process))),
            column(UInt32Array::from_iter_values(rows.iter().map(|it| it.core))),
            column(Int64Array::from_iter_values(rows.iter().map(|it| it.t0_ns))),
            column(Int64Array::from_iter_values(rows.iter().map(|it| it.t1_ns))),
            column(Int64Array::from_iter_values(rows.iter().map(|it| it.running_ns))),
        ],
    )
}
