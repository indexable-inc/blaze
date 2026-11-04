//! The driver of [`super::analyse`] over the history: the build's rows through the pinned DuckDB
//! ([`crate::sql`]), the time model and its critical path ([`crate::time`]), and the [`Facts`]
//! the model does not hold.

use arrow_array::RecordBatch;

use super::{
    BuildFacts, Died, DiedSpan, DiedThread, Facts, Memory, MissGroup, PanicFacts, QueryCount, Recorder, Report, SpanKind, ThreadName,
    analyse,
};
use crate::time::{
    Model, TimeError,
    load::{Source, get, ints, query, query_many, texts, uints},
    path,
};

/// The report of `build`, or of the last build that ended or whose daemon died.
pub fn run(source: &Source<'_>, build: Option<u64>) -> Result<Report, TimeError> {
    let pick = crate::time::load::pick(source, build)?;
    let crate::time::load::Rows { input, process, cut } = crate::time::load::rows(source, pick)?;
    let build = input.build;
    let window = input.build_row.map(|(start, dur)| (start, start + dur));
    let model = Model::build(input)?;
    let path = path::critical(&model);
    let mut facts = facts(source, build, window, process)?;
    facts.spans = cut;
    if let (Some(process), Some((t0, t1))) = (pick.died, window) {
        facts.build.died = Some(died(source, process, t0, t1)?);
    }
    facts.build.panics = panics(source, build, pick.died)?;
    Ok(analyse(&model, &path, &facts))
}

/// What the daemon `process` that died in the build `t0..t1` (to its last row) was running:
/// its last reading of its threads' open spans in the build ([`crate::schema::OPEN`]).
fn died(source: &Source<'_>, process: u32, t0: i64, t1: i64) -> Result<Died, TimeError> {
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(read_ns AS BIGINT) AS read_ns, CAST(thread AS BIGINT) AS thread, CAST(thread_name AS VARCHAR) AS thread_name, \
         CAST(name AS VARCHAR) AS name, CAST(function AS VARCHAR) AS function, CAST(awaits AS VARCHAR) AS awaits, \
         CAST(owner AS UBIGINT) AS owner, CAST(start_ns AS BIGINT) AS start_ns FROM open \
         WHERE process = {process} AND read_ns = (SELECT max(read_ns) FROM open WHERE process = {process} AND read_ns BETWEEN {t0} AND {t1}) \
         ORDER BY thread, depth"
    );
    let batches = query(source, &sql, "reading what the daemon that died was running")?;
    let mut it = Died { process, last_row_ns: t1 - t0, ..Died::default() };
    for (batch, row) in rows(&batches) {
        const T: &str = "open";
        let (Some(read), Some(thread)) = (int(batch, T, "read_ns", row)?, int(batch, T, "thread", row)?) else { continue };
        it.read_ns = Some(read - t0);
        let thread = u32::try_from(thread).unwrap_or(u32::MAX);
        if it.threads.last().is_none_or(|it| it.thread != thread) {
            it.threads.push(DiedThread { thread, name: text(batch, T, "thread_name", row)?, spans: Vec::new() }); // alloc: once a thread a report
        }
        let span = DiedSpan {
            name: text(batch, T, "name", row)?.unwrap_or_default(),
            function: text(batch, T, "function", row)?,
            awaits: text(batch, T, "awaits", row)?,
            owner: uint(batch, T, "owner", row)?,
            for_ns: int(batch, T, "start_ns", row)?.map_or(0, |start| t1 - start),
        };
        if let Some(last) = it.threads.last_mut() {
            last.spans.push(span);
        }
    }
    Ok(it)
}

/// How many frames of each panic's stack a report keeps.
const PANIC_FRAMES: usize = 8;

/// The panics of `build`, and the fatal signal of `died`, the daemon that died in it, if any:
/// its `panics` rows, oldest first.
fn panics(source: &Source<'_>, build: u64, died: Option<u32>) -> Result<Vec<PanicFacts>, TimeError> {
    // alloc: a condition, once a report
    let of_died = died.map_or(String::new(), |it| format!(" OR (kind = 'signal' AND process = {it})"));
    // The stack as one text, its frames a line each, outermost first.
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(kind AS VARCHAR) AS kind, CAST(caught AS BIGINT) AS caught, CAST(message AS VARCHAR) AS message, \
         CAST(location AS VARCHAR) AS location, CAST(thread_name AS VARCHAR) AS thread_name, CAST(signal AS VARCHAR) AS signal, \
         array_to_string(CAST(stack AS VARCHAR[]), chr(10)) AS stack FROM panics \
         WHERE build_id = {build}{of_died} ORDER BY t_ns, process, thread"
    );
    let batches = query(source, &sql, "reading the build's panics and its daemon's fatal signal")?;
    // alloc: once a report
    let mut out = Vec::new();
    for (batch, row) in rows(&batches) {
        const T: &str = "panics";
        // alloc: a report's lines
        let stack: Vec<String> = text(batch, T, "stack", row)?.unwrap_or_default().lines().map(str::to_owned).collect();
        out.push(PanicFacts {
            kind: text(batch, T, "kind", row)?.unwrap_or_default(),
            caught: int(batch, T, "caught", row)?.is_some_and(|it| it != 0),
            message: text(batch, T, "message", row)?,
            location: text(batch, T, "location", row)?,
            thread: text(batch, T, "thread_name", row)?,
            signal: text(batch, T, "signal", row)?,
            // alloc: a report's lines
            top: crate::panics::top_frames(&stack, PANIC_FRAMES).into_iter().map(str::to_owned).collect(),
        });
    }
    Ok(out)
}

/// The rows of `build` beside its time model; `window` is its `builds` row's start and end, and
/// `process` the process whose rows are of it ([`crate::time::load::rows`]): the rows of no
/// build in the window are taken from it alone.
pub fn facts(source: &Source<'_>, build: u64, window: Option<(i64, i64)>, process: Option<u32>) -> Result<Facts, TimeError> {
    // alloc: a condition, once a report
    let of = process.map_or(String::new(), |it| format!(" AND process = {it}"));
    // The spans of the build, and of no build in its window (see `time::load::load`).
    let spans = match window {
        // alloc: a query, once a report
        Some((t0, t1)) => format!("(SELECT * FROM spans WHERE build_id = {build} OR (build_id IS NULL AND start_ns >= {t0} AND start_ns < {t1}{of}))"),
        // alloc: a query, once a report
        None => format!("(SELECT * FROM spans WHERE build_id = {build})"),
    };
    // The sums of its spans too short for a row each, chosen alike by when the first began.
    let folded = match window {
        // alloc: a query, once a report
        Some((t0, t1)) => format!("(SELECT * FROM folded WHERE build_id = {build} OR (build_id IS NULL AND from_ns >= {t0} AND from_ns < {t1}{of}))"),
        // alloc: a query, once a report
        None => format!("(SELECT * FROM folded WHERE build_id = {build})"),
    };
    // One DuckDB for them all: the views are made once ([`crate::sql::run_all`]).
    let windowed = window.map(|(t0, t1)| (memory_sql(build, t0, t1, &of), recorder_sql(build, t0, t1, &of)));
    // alloc: the queries, once a report
    let mut sqls = vec![build_row_sql(build), threads_sql(&spans), span_kinds_sql(&spans, &folded), misses_sql(build), query_counts_sql(build)];
    if let Some((memory, recorder)) = &windowed {
        // clone: a query, once a report
        sqls.extend([memory.clone(), recorder.clone()]);
    }
    // alloc: the queries' text, once a report
    let asked: Vec<&str> = sqls.iter().map(String::as_str).collect();
    let answers = query_many(source, &asked, "reading the build's row, threads, spans by name, misses, query counts, memory and recorder")?;
    let answer = |i: usize| answers.get(i).map(Vec::as_slice).unwrap_or_default();
    Ok(Facts {
        build: build_row(answer(0), build)?,
        threads: threads(answer(1))?,
        span_kinds: span_kinds(answer(2))?,
        misses: misses(answer(3))?,
        query_counts: query_counts(answer(4))?,
        memory: if windowed.is_some() { memory(answer(5))? } else { Memory::default() },
        recorder: if windowed.is_some() { recorder(answer(6))? } else { Recorder::default() },
        spans: crate::time::SpanCut::default(),
    })
}

/// What the recorder dropped in `window`: each counter's last value by its end (and the flush
/// after it) less its last before its start, in the same process: they count from its start.
/// `of` keeps the rows to the build's process; the folded spans are the build's and its
/// process's of no build.
fn recorder_sql(build: u64, t0: i64, t1: i64, of: &str) -> String {
    // The flush after the build's end has its rows; one flush is at most `history::EVERY` on.
    let after = t1 + crate::history::EVERY.as_nanos() as i64;
    let delta = |name: &str, column: &str| {
        // alloc: a query's part, once a report
        format!(
            "CAST((SELECT arg_max(value, t_ns) FROM counters WHERE name = '{name}' AND t_ns BETWEEN {t0} AND {after}{of}) - \
             coalesce((SELECT arg_max(value, t_ns) FROM counters WHERE name = '{name}' AND t_ns < {t0} AND process = \
             (SELECT arg_max(process, t_ns) FROM counters WHERE name = '{name}' AND t_ns BETWEEN {t0} AND {after}{of})), 0) AS BIGINT) AS {column}"
        )
    };
    // alloc: a condition, once a report
    let folded = format!("(build_id = {build} OR build_id IS NULL{of}) AND to_ns BETWEEN {t0} AND {t1}");
    // alloc: a query, once a report
    format!(
        "SELECT {}, {}, {}, \
         (SELECT CAST(coalesce(sum(spans), 0) AS BIGINT) FROM folded WHERE {folded}) AS folded_spans, \
         (SELECT CAST(coalesce(sum(dur_ns), 0) AS BIGINT) FROM folded WHERE {folded}) AS folded_ns",
        delta("profile spans lost", "spans_lost"),
        delta("profile rows skipped", "rows_skipped"),
        delta("profile rows skipped behind", "rows_behind"),
    )
}

/// What [`recorder_sql`]'s answer says.
fn recorder(batches: &[RecordBatch]) -> Result<Recorder, TimeError> {
    let Some((batch, row)) = rows(batches).next() else { return Ok(Recorder::default()) };
    const T: &str = "counters";
    Ok(Recorder {
        spans_lost: int(batch, T, "spans_lost", row)?,
        rows_skipped: int(batch, T, "rows_skipped", row)?,
        rows_behind: int(batch, T, "rows_behind", row)?,
        folded_spans: int(batch, "folded", "folded_spans", row)?,
        folded_ns: int(batch, "folded", "folded_ns", row)?,
    })
}

fn rows(batches: &[RecordBatch]) -> impl Iterator<Item = (&RecordBatch, usize)> {
    batches.iter().flat_map(|batch| (0..batch.num_rows()).map(move |row| (batch, row)))
}

fn text(batch: &RecordBatch, table: &'static str, column: &'static str, row: usize) -> Result<Option<String>, TimeError> {
    // alloc: a cell, once a row a report
    Ok(get(texts(batch, table, column)?, row, |it, row| it.value(row).to_owned()))
}

fn uint(batch: &RecordBatch, table: &'static str, column: &'static str, row: usize) -> Result<Option<u64>, TimeError> {
    Ok(get(uints(batch, table, column)?, row, |it, row| it.value(row)))
}

fn int(batch: &RecordBatch, table: &'static str, column: &'static str, row: usize) -> Result<Option<i64>, TimeError> {
    Ok(get(ints(batch, table, column)?, row, |it, row| it.value(row)))
}

fn build_row_sql(build: u64) -> String {
    // alloc: a query, once a report
    format!(
        "SELECT CAST(workspace AS VARCHAR) AS workspace, CAST(command AS VARCHAR) AS command, \
         CAST(outcome AS VARCHAR) AS outcome, CAST(tree AS VARCHAR) AS tree, CAST(blaze AS VARCHAR) AS blaze, \
         CAST(hits AS UBIGINT) AS hits, CAST(misses AS UBIGINT) AS misses FROM builds \
         WHERE build_id = {build} ORDER BY dur_ns DESC NULLS LAST LIMIT 1"
    )
}

fn build_row(batches: &[RecordBatch], build: u64) -> Result<BuildFacts, TimeError> {
    let mut it = BuildFacts { id: build, ..BuildFacts::default() };
    if let Some((batch, row)) = rows(batches).next() {
        const T: &str = "builds";
        it.workspace = text(batch, T, "workspace", row)?;
        it.command = text(batch, T, "command", row)?;
        it.outcome = text(batch, T, "outcome", row)?;
        it.tree = text(batch, T, "tree", row)?;
        it.blaze = text(batch, T, "blaze", row)?;
        it.hits = uint(batch, T, "hits", row)?;
        it.misses = uint(batch, T, "misses", row)?;
    }
    Ok(it)
}

fn threads_sql(spans: &str) -> String {
    // alloc: a query, once a report
    format!(
        "SELECT CAST(process AS BIGINT) AS process, CAST(thread AS BIGINT) AS thread, CAST(min(thread_name) AS VARCHAR) AS name \
         FROM {spans} WHERE thread_name IS NOT NULL GROUP BY 1, 2 ORDER BY 1, 2"
    )
}

fn threads(batches: &[RecordBatch]) -> Result<Vec<ThreadName>, TimeError> {
    // alloc: once a report
    let mut out = Vec::new();
    for (batch, row) in rows(batches) {
        const T: &str = "spans";
        let (Some(process), Some(thread), Some(name)) = (int(batch, T, "process", row)?, int(batch, T, "thread", row)?, text(batch, T, "name", row)?)
        else {
            continue;
        };
        out.push(ThreadName { process: process as u32, thread: thread as u32, name });
    }
    Ok(out)
}

/// The spans by name and query kind: the rows of `spans` and the sums of `folded` (the spans
/// too short for a row each, [`crate::flight::FOLD_BELOW`]) together, so a name whose spans
/// were mostly folded keeps all its runs and time. A folded query's kind is its name, as
/// `batches::spans` gives a row's.
fn span_kinds_sql(spans: &str, folded: &str) -> String {
    // alloc: a query, once a report
    format!(
        "SELECT name, query_kind, CAST(min(target) AS VARCHAR) AS target, \
         CAST(sum(runs) AS UBIGINT) AS runs, CAST(sum(dur_ns) AS BIGINT) AS dur_ns, \
         CAST(coalesce(sum(self_cpu_ns), 0) AS BIGINT) AS self_cpu_ns FROM (\
         SELECT CAST(name AS VARCHAR) AS name, CAST(query_kind AS VARCHAR) AS query_kind, CAST(target AS VARCHAR) AS target, \
         CAST(1 AS UBIGINT) AS runs, dur_ns, self_cpu_ns FROM {spans} \
         UNION ALL SELECT CAST(name AS VARCHAR), CASE WHEN ends_with(CAST(name AS VARCHAR), '_query') THEN CAST(name AS VARCHAR) END, \
         CAST(target AS VARCHAR), CAST(spans AS UBIGINT), dur_ns, self_cpu_ns FROM {folded}) \
         GROUP BY 1, 2 ORDER BY 1, 2 NULLS FIRST"
    )
}

fn span_kinds(batches: &[RecordBatch]) -> Result<Vec<SpanKind>, TimeError> {
    // alloc: once a report
    let mut out = Vec::new();
    for (batch, row) in rows(batches) {
        const T: &str = "spans";
        out.push(SpanKind {
            name: text(batch, T, "name", row)?.unwrap_or_default(),
            query_kind: text(batch, T, "query_kind", row)?,
            target: text(batch, T, "target", row)?,
            runs: uint(batch, T, "runs", row)?.unwrap_or(0),
            dur_ns: int(batch, T, "dur_ns", row)?.unwrap_or(0),
            self_cpu_ns: int(batch, T, "self_cpu_ns", row)?.unwrap_or(0),
        });
    }
    Ok(out)
}

fn misses_sql(build: u64) -> String {
    // alloc: a query, once a report
    format!(
        "SELECT CAST(step AS VARCHAR) AS step, CAST(coalesce(cause, '') AS VARCHAR) AS cause, CAST(count(*) AS UBIGINT) AS count, \
         CAST(min(item) AS VARCHAR) AS example, CAST(mode(what) AS VARCHAR) AS what \
         FROM misses WHERE build_id = {build} GROUP BY 1, 2 ORDER BY 3 DESC, 1, 2"
    )
}

fn misses(batches: &[RecordBatch]) -> Result<Vec<MissGroup>, TimeError> {
    // alloc: once a report
    let mut out = Vec::new();
    for (batch, row) in rows(batches) {
        const T: &str = "misses";
        out.push(MissGroup {
            step: text(batch, T, "step", row)?.unwrap_or_default(),
            cause: text(batch, T, "cause", row)?.unwrap_or_default(),
            count: uint(batch, T, "count", row)?.unwrap_or(0),
            example: text(batch, T, "example", row)?.unwrap_or_default(),
            what: text(batch, T, "what", row)?,
        });
    }
    Ok(out)
}

const QUERY_COUNT_COLUMNS: [&str; 8] =
    ["executed", "verified", "reran", "evicted_asked", "evicted", "dropped", "reads_recorded", "reads_kept"];

fn query_counts_sql(build: u64) -> String {
    // alloc: a query, once a report
    let sums = QUERY_COUNT_COLUMNS.map(|it| format!("CAST(sum({it}) AS UBIGINT) AS {it}")).join(", ");
    // `reads_most` is a mark, not a sum: the build's most.
    // alloc: a query, once a report
    format!(
        "SELECT CAST(query AS VARCHAR) AS query, {sums}, CAST(max(reads_most) AS UBIGINT) AS reads_most \
         FROM query_counts WHERE build_id = {build} GROUP BY 1 ORDER BY 1"
    )
}

fn query_counts(batches: &[RecordBatch]) -> Result<Vec<QueryCount>, TimeError> {
    // alloc: once a report
    let mut out = Vec::new();
    for (batch, row) in rows(batches) {
        const T: &str = "query_counts";
        let n = |column| uint(batch, T, column, row).map(|it| it.unwrap_or(0));
        out.push(QueryCount {
            query: text(batch, T, "query", row)?.unwrap_or_default(),
            executed: n("executed")?,
            verified: n("verified")?,
            reran: n("reran")?,
            evicted_asked: n("evicted_asked")?,
            evicted: n("evicted")?,
            dropped: n("dropped")?,
            reads_recorded: n("reads_recorded")?,
            reads_kept: n("reads_kept")?,
            reads_most: n("reads_most")?,
        });
    }
    Ok(out)
}

fn memory_sql(build: u64, t0: i64, t1: i64, of: &str) -> String {
    // alloc: a query, once a report
    format!(
        "SELECT \
         CAST((SELECT arg_max(value, t_ns) FROM counters WHERE name = 'live bytes' AND t_ns <= coalesce( \
             (SELECT max(started_ns + dur_ns) FROM builds WHERE started_ns + dur_ns <= {t0}), {t0})) AS BIGINT) AS live_before, \
         CAST(arg_min(value, t_ns) FILTER (WHERE name = 'live bytes') AS BIGINT) AS live_first, \
         CAST(max(value) FILTER (WHERE name = 'live bytes') AS BIGINT) AS live_peak, \
         CAST(min(value) FILTER (WHERE name = 'live bytes') AS BIGINT) AS live_least, \
         CAST(max(value) FILTER (WHERE name = 'footprint bytes') AS BIGINT) AS footprint_peak \
         FROM counters WHERE (build_id = {build} OR build_id IS NULL){of} AND t_ns BETWEEN {t0} AND {t1}"
    )
}

fn memory(batches: &[RecordBatch]) -> Result<Memory, TimeError> {
    let Some((batch, row)) = rows(batches).next() else { return Ok(Memory::default()) };
    const T: &str = "counters";
    Ok(Memory {
        live_before: int(batch, T, "live_before", row)?,
        live_first: int(batch, T, "live_first", row)?,
        live_peak: int(batch, T, "live_peak", row)?,
        live_least: int(batch, T, "live_least", row)?,
        footprint_peak: int(batch, T, "footprint_peak", row)?,
    })
}
