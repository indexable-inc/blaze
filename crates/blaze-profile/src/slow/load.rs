//! The driver of [`super::analyse`] over the history: the build's rows through the pinned DuckDB
//! ([`crate::sql`]), the time model and its critical path ([`crate::time`]), and the [`Facts`]
//! the model does not hold.

use arrow_array::RecordBatch;

use super::{BuildFacts, Facts, Memory, MissGroup, QueryCount, Report, SpanKind, ThreadName, analyse};
use crate::time::{
    Model, TimeError,
    load::{Source, get, ints, query, texts, uints},
    path,
};

/// The report of `build`, or of the last build that ended.
pub fn run(source: &Source<'_>, build: Option<u64>) -> Result<Report, TimeError> {
    let input = crate::time::load::load(source, build)?;
    let build = input.build;
    let window = input.build_row.map(|(start, dur)| (start, start + dur));
    let model = Model::build(input)?;
    let path = path::critical(&model);
    let facts = facts(source, build, window)?;
    Ok(analyse(&model, &path, &facts))
}

/// The rows of `build` beside its time model; `window` is its `builds` row's start and end.
pub fn facts(source: &Source<'_>, build: u64, window: Option<(i64, i64)>) -> Result<Facts, TimeError> {
    // The spans of the build, and of no build in its window (see `time::load::load`).
    let spans = match window {
        // alloc: a query, once a report
        Some((t0, t1)) => format!("(SELECT * FROM spans WHERE build_id = {build} OR (build_id IS NULL AND start_ns >= {t0} AND start_ns < {t1}))"),
        // alloc: a query, once a report
        None => format!("(SELECT * FROM spans WHERE build_id = {build})"),
    };
    Ok(Facts {
        build: build_row(source, build)?,
        threads: threads(source, &spans)?,
        span_kinds: span_kinds(source, &spans)?,
        misses: misses(source, build)?,
        query_counts: query_counts(source, build)?,
        memory: memory(source, build, window)?,
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

fn build_row(source: &Source<'_>, build: u64) -> Result<BuildFacts, TimeError> {
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(workspace AS VARCHAR) AS workspace, CAST(command AS VARCHAR) AS command, \
         CAST(outcome AS VARCHAR) AS outcome, CAST(tree AS VARCHAR) AS tree, CAST(blaze AS VARCHAR) AS blaze, \
         CAST(hits AS UBIGINT) AS hits, CAST(misses AS UBIGINT) AS misses FROM builds \
         WHERE build_id = {build} ORDER BY dur_ns DESC NULLS LAST LIMIT 1"
    );
    let batches = query(source, &sql, "reading the build's builds row")?;
    let mut it = BuildFacts { id: build, ..BuildFacts::default() };
    if let Some((batch, row)) = rows(&batches).next() {
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

fn threads(source: &Source<'_>, spans: &str) -> Result<Vec<ThreadName>, TimeError> {
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(process AS BIGINT) AS process, CAST(thread AS BIGINT) AS thread, CAST(min(thread_name) AS VARCHAR) AS name \
         FROM {spans} WHERE thread_name IS NOT NULL GROUP BY 1, 2 ORDER BY 1, 2"
    );
    let batches = query(source, &sql, "reading the build's threads' names")?;
    // alloc: once a report
    let mut out = Vec::new();
    for (batch, row) in rows(&batches) {
        const T: &str = "spans";
        let (Some(process), Some(thread), Some(name)) = (int(batch, T, "process", row)?, int(batch, T, "thread", row)?, text(batch, T, "name", row)?)
        else {
            continue;
        };
        out.push(ThreadName { process: process as u32, thread: thread as u32, name });
    }
    Ok(out)
}

fn span_kinds(source: &Source<'_>, spans: &str) -> Result<Vec<SpanKind>, TimeError> {
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(name AS VARCHAR) AS name, CAST(query_kind AS VARCHAR) AS query_kind, CAST(min(target) AS VARCHAR) AS target, \
         CAST(count(*) AS UBIGINT) AS runs, CAST(sum(dur_ns) AS BIGINT) AS dur_ns, \
         CAST(coalesce(sum(self_cpu_ns), 0) AS BIGINT) AS self_cpu_ns \
         FROM {spans} GROUP BY 1, 2 ORDER BY 1, 2 NULLS FIRST"
    );
    let batches = query(source, &sql, "reading the build's spans by name")?;
    // alloc: once a report
    let mut out = Vec::new();
    for (batch, row) in rows(&batches) {
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

fn misses(source: &Source<'_>, build: u64) -> Result<Vec<MissGroup>, TimeError> {
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(step AS VARCHAR) AS step, CAST(coalesce(cause, '') AS VARCHAR) AS cause, CAST(count(*) AS UBIGINT) AS count, \
         CAST(min(item) AS VARCHAR) AS example, CAST(mode(what) AS VARCHAR) AS what \
         FROM misses WHERE build_id = {build} GROUP BY 1, 2 ORDER BY 3 DESC, 1, 2"
    );
    let batches = query(source, &sql, "reading the build's misses")?;
    // alloc: once a report
    let mut out = Vec::new();
    for (batch, row) in rows(&batches) {
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

fn query_counts(source: &Source<'_>, build: u64) -> Result<Vec<QueryCount>, TimeError> {
    const COLUMNS: [&str; 7] = ["executed", "verified", "reran", "evicted_asked", "evicted", "refused", "dropped"];
    // alloc: a query, once a report
    let sums = COLUMNS.map(|it| format!("CAST(sum({it}) AS UBIGINT) AS {it}")).join(", ");
    // alloc: a query, once a report
    let sql = format!("SELECT CAST(query AS VARCHAR) AS query, {sums} FROM query_counts WHERE build_id = {build} GROUP BY 1 ORDER BY 1");
    let batches = query(source, &sql, "reading the build's query counts")?;
    // alloc: once a report
    let mut out = Vec::new();
    for (batch, row) in rows(&batches) {
        const T: &str = "query_counts";
        let n = |column| uint(batch, T, column, row).map(|it| it.unwrap_or(0));
        out.push(QueryCount {
            query: text(batch, T, "query", row)?.unwrap_or_default(),
            executed: n("executed")?,
            verified: n("verified")?,
            reran: n("reran")?,
            evicted_asked: n("evicted_asked")?,
            evicted: n("evicted")?,
            refused: n("refused")?,
            dropped: n("dropped")?,
        });
    }
    Ok(out)
}

fn memory(source: &Source<'_>, build: u64, window: Option<(i64, i64)>) -> Result<Memory, TimeError> {
    let Some((t0, t1)) = window else { return Ok(Memory::default()) };
    // alloc: a query, once a report
    let sql = format!(
        "SELECT \
         CAST((SELECT arg_max(value, t_ns) FROM counters WHERE name = 'live bytes' AND t_ns <= coalesce( \
             (SELECT max(started_ns + dur_ns) FROM builds WHERE started_ns + dur_ns <= {t0}), {t0})) AS BIGINT) AS live_before, \
         CAST(arg_min(value, t_ns) FILTER (WHERE name = 'live bytes') AS BIGINT) AS live_first, \
         CAST(max(value) FILTER (WHERE name = 'live bytes') AS BIGINT) AS live_peak, \
         CAST(min(value) FILTER (WHERE name = 'live bytes') AS BIGINT) AS live_least, \
         CAST(max(value) FILTER (WHERE name = 'footprint bytes') AS BIGINT) AS footprint_peak \
         FROM counters WHERE (build_id = {build} OR build_id IS NULL) AND t_ns BETWEEN {t0} AND {t1}"
    );
    let batches = query(source, &sql, "reading the daemon's memory around the build")?;
    let Some((batch, row)) = rows(&batches).next() else { return Ok(Memory::default()) };
    const T: &str = "counters";
    Ok(Memory {
        live_before: int(batch, T, "live_before", row)?,
        live_first: int(batch, T, "live_first", row)?,
        live_peak: int(batch, T, "live_peak", row)?,
        live_least: int(batch, T, "live_least", row)?,
        footprint_peak: int(batch, T, "footprint_peak", row)?,
    })
}
