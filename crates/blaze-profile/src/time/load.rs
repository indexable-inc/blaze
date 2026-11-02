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
    Array, ArrayRef, Int64Array, PrimitiveArray, RecordBatch, StringArray, UInt32Array, UInt64Array,
    cast::AsArray,
    types::{Int64Type, UInt64Type},
};

use super::{
    Event, EventIn, Input, LaneId, Model, SpanIn, State, SwitchIn, TimeError, html,
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
    /// `None`: the last build that ended.
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
    let model = Model::build(load(source, ask.build)?)?;
    let path = path::critical(&model);
    let written = write(source.history, &model, &path)?;
    // alloc: the report, once a report
    let mut out = String::new();
    report::summary(&model, &mut out);
    out.push('\n');
    report::table(&report::rows(&model, ask.by, ask.matching), ask.by, ask.top, &mut out);
    out.push('\n');
    report::path(&model, &path, ask.top, &mut out);
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
    _ = writeln!(out, "\nfor blaze debug sql, the views job_intervals, span_time, path_links and core_busy of build {} are in:", model.build);
    for it in written {
        _ = writeln!(out, "  {}", it.display());
    }
    Ok(out)
}

fn put(path: &Path, bytes: &[u8], doing: &'static str) -> Result<(), TimeError> {
    // alloc: an error
    std::fs::write(path, bytes).map_err(|error| TimeError::Io { path: path.to_owned(), doing, error })
}

pub(crate) fn query(source: &Source<'_>, sql: &str, doing: &'static str) -> Result<Vec<RecordBatch>, TimeError> {
    sql::run(&sql::Query { duckdb: source.duckdb, history: source.history, sql }).map_err(|error| TimeError::Sql { doing, error })
}

/// The rows of `build`, or of the last build that ended.
pub fn load(source: &Source<'_>, build: Option<u64>) -> Result<Input, TimeError> {
    let build = match build {
        Some(it) => it,
        None => {
            let sql = "SELECT CAST(max(build_id) AS UBIGINT) AS build FROM builds WHERE dur_ns IS NOT NULL";
            let batches = query(source, sql, "finding the last build that ended")?;
            first_u64(&batches, "builds", "build")?.ok_or(TimeError::NoBuild)?
        }
    };
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(started_ns AS BIGINT) AS started_ns, CAST(dur_ns AS BIGINT) AS dur_ns FROM builds \
         WHERE build_id = {build} AND dur_ns IS NOT NULL ORDER BY dur_ns DESC LIMIT 1"
    );
    let batches = query(source, &sql, "reading the build's builds row")?;
    let mut build_row = None;
    for batch in &batches {
        let (started, dur) = (ints(batch, "builds", "started_ns")?, ints(batch, "builds", "dur_ns")?);
        if batch.num_rows() > 0 {
            build_row = Some((started.value(0), dur.value(0)));
        }
    }
    // Rows of no build in the build's window are its too: the recorder leaves `build_id` null on
    // a job's rows once its code let go of the build (most of a cold build's switches and spans
    // on 2026-11-02), and a reader that took only the tagged rows saw a tenth of the work.
    let window = build_row.map(|(start, dur)| (start, start + dur));
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(process AS BIGINT) AS process, CAST(thread AS BIGINT) AS thread, CAST(t_ns AS BIGINT) AS t_ns, \
         CAST(event AS VARCHAR) AS event, CAST(job AS UBIGINT) AS job, CAST(fiber AS BIGINT) AS fiber, \
         CAST(core AS BIGINT) AS core, CAST(wait_kind AS VARCHAR) AS wait_kind, CAST(wait_value AS UBIGINT) AS wait_value, \
         CAST(by_job AS UBIGINT) AS by_job, CAST(since_ns AS BIGINT) AS since_ns, CAST(began_as AS UBIGINT) AS began_as \
         FROM {}",
        of_build("switches", build, SWITCHES_SINCE_2, window.map(|(t0, t1)| format!("t_ns BETWEEN {t0} AND {t1}")))
    );
    let switches = switches(&query(source, &sql, "reading the build's switches")?)?;
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(process AS BIGINT) AS process, CAST(thread AS BIGINT) AS thread, CAST(span_id AS UBIGINT) AS span_id, \
         CAST(parent AS UBIGINT) AS parent, CAST(depth AS BIGINT) AS depth, CAST(name AS VARCHAR) AS name, \
         CAST(crate AS VARCHAR) AS krate, CAST(function AS VARCHAR) AS function, CAST(unit AS VARCHAR) AS unit, \
         CAST(awaits AS VARCHAR) AS awaits, CAST(owner AS UBIGINT) AS owner, CAST(job AS UBIGINT) AS job, \
         CAST(start_ns AS BIGINT) AS start_ns, CAST(dur_ns AS BIGINT) AS dur_ns, \
         CAST(self_cpu_ns AS BIGINT) AS self_cpu_ns FROM {}",
        of_build("spans", build, SPANS_SINCE_2, window.map(|(t0, t1)| format!("start_ns >= {t0} AND start_ns < {t1}")))
    );
    let spans = spans(&query(source, &sql, "reading the build's spans")?)?;
    // alloc: a query, once a report
    let sql = format!(
        "SELECT CAST(process AS BIGINT) AS process, CAST(thread AS BIGINT) AS thread, CAST(t_ns AS BIGINT) AS t_ns, \
         CAST(job AS UBIGINT) AS job, CAST(level AS VARCHAR) AS level, CAST(target AS VARCHAR) AS target, \
         CAST(name AS VARCHAR) AS name, CAST(message AS VARCHAR) AS message, CAST(span_id AS UBIGINT) AS span_id \
         FROM events WHERE build_id = {build}{}",
        window.map_or(String::new(), |(t0, t1)| format!(" OR (build_id IS NULL AND t_ns BETWEEN {t0} AND {t1})"))
    );
    let events = events(&query(source, &sql, "reading the build's events")?)?;
    Ok(Input { build, build_row, switches, spans, events })
}

/// The columns schema version 2 added, as typed nulls: a history of version 1 files has none of
/// them, and a query naming a column no file has fails to bind.
const SWITCHES_SINCE_2: &str = "NULL::UBIGINT AS by_job, NULL::BIGINT AS since_ns, NULL::UBIGINT AS began_as";
const SPANS_SINCE_2: &str = "NULL::UBIGINT AS job";

/// `table`'s rows of `build`, and its rows of no build that `window` holds (a condition on the
/// row's time), with the `added` columns null where its files have none.
fn of_build(table: &str, build: u64, added: &str, window: Option<String>) -> String {
    let untagged = window.map_or(String::new(), |it| format!(" OR (build_id IS NULL AND {it})"));
    // alloc: a query, once a table a report
    format!("(SELECT * FROM {table} WHERE build_id = {build}{untagged} UNION ALL BY NAME SELECT {added} WHERE false)")
}

pub(crate) fn array<'a>(batch: &'a RecordBatch, table: &'static str, column: &'static str) -> Result<&'a dyn Array, TimeError> {
    batch
        .column_by_name(column)
        .map(|it| it.as_ref())
        .ok_or(TimeError::BadColumn { table, column, why: "is not among the rows DuckDB answered" })
}

pub(crate) fn ints<'a>(batch: &'a RecordBatch, table: &'static str, column: &'static str) -> Result<&'a PrimitiveArray<Int64Type>, TimeError> {
    array(batch, table, column)?.as_primitive_opt::<Int64Type>().ok_or(TimeError::BadColumn {
        table,
        column,
        why: "is not a BIGINT",
    })
}

pub(crate) fn uints<'a>(batch: &'a RecordBatch, table: &'static str, column: &'static str) -> Result<&'a PrimitiveArray<UInt64Type>, TimeError> {
    array(batch, table, column)?.as_primitive_opt::<UInt64Type>().ok_or(TimeError::BadColumn {
        table,
        column,
        why: "is not a UBIGINT",
    })
}

pub(crate) fn texts<'a>(batch: &'a RecordBatch, table: &'static str, column: &'static str) -> Result<&'a StringArray, TimeError> {
    array(batch, table, column)?.as_string_opt::<i32>().ok_or(TimeError::BadColumn {
        table,
        column,
        why: "is not a VARCHAR",
    })
}

pub(crate) fn get<'a, A: Array, T>(array: &'a A, row: usize, value: impl FnOnce(&'a A, usize) -> T) -> Option<T> {
    (!array.is_null(row)).then(|| value(array, row))
}

/// A column of a batch that must not be null, as `u32`.
fn u32_at(array: &PrimitiveArray<Int64Type>, row: usize, table: &'static str, column: &'static str) -> Result<u32, TimeError> {
    get(array, row, |it, row| it.value(row))
        .and_then(|it| u32::try_from(it).ok())
        .ok_or(TimeError::BadColumn { table, column, why: "is null or past u32" })
}

fn first_u64(batches: &[RecordBatch], table: &'static str, column: &'static str) -> Result<Option<u64>, TimeError> {
    for batch in batches {
        let it = uints(batch, table, column)?;
        if batch.num_rows() > 0 {
            return Ok(get(it, 0, |it, row| it.value(row)));
        }
    }
    Ok(None)
}

fn switches(batches: &[RecordBatch]) -> Result<Vec<SwitchIn>, TimeError> {
    const T: &str = "switches";
    // alloc: the build's switches, once a report
    let mut out = Vec::with_capacity(batches.iter().map(RecordBatch::num_rows).sum());
    for batch in batches {
        let (process, thread, t_ns, event) = (ints(batch, T, "process")?, ints(batch, T, "thread")?, ints(batch, T, "t_ns")?, texts(batch, T, "event")?);
        let (job, fiber, core) = (uints(batch, T, "job")?, ints(batch, T, "fiber")?, ints(batch, T, "core")?);
        let (wait_kind, wait_value) = (texts(batch, T, "wait_kind")?, uints(batch, T, "wait_value")?);
        let (by_job, since_ns, began_as) = (uints(batch, T, "by_job")?, ints(batch, T, "since_ns")?, uints(batch, T, "began_as")?);
        for row in 0..batch.num_rows() {
            let name = get(event, row, |it, row| it.value(row)).ok_or(TimeError::BadColumn { table: T, column: "event", why: "is null" })?;
            let wait = get(wait_kind, row, |it, row| it.value(row)).map(|kind| (kind, get(wait_value, row, |it, row| it.value(row))));
            out.push(SwitchIn {
                process: u32_at(process, row, T, "process")?,
                thread: u32_at(thread, row, T, "thread")?,
                t_ns: get(t_ns, row, |it, row| it.value(row)).ok_or(TimeError::BadColumn { table: T, column: "t_ns", why: "is null" })?,
                event: Event::of(name, wait)?,
                job: get(job, row, |it, row| it.value(row)).ok_or(TimeError::BadColumn { table: T, column: "job", why: "is null" })?,
                fiber: u32_at(fiber, row, T, "fiber")?,
                core: u32_at(core, row, T, "core")?,
                by: get(by_job, row, |it, row| it.value(row)),
                since_ns: get(since_ns, row, |it, row| it.value(row)),
                began_as: get(began_as, row, |it, row| it.value(row)),
            });
        }
    }
    Ok(out)
}

fn spans(batches: &[RecordBatch]) -> Result<Vec<SpanIn>, TimeError> {
    const T: &str = "spans";
    // alloc: the build's spans, once a report
    let mut out = Vec::with_capacity(batches.iter().map(RecordBatch::num_rows).sum());
    let text = |it: &StringArray, row: usize| get(it, row, |it, row| it.value(row).to_owned());
    for batch in batches {
        let (process, thread, span_id, parent) = (ints(batch, T, "process")?, ints(batch, T, "thread")?, uints(batch, T, "span_id")?, uints(batch, T, "parent")?);
        let (depth, name, krate, function) = (ints(batch, T, "depth")?, texts(batch, T, "name")?, texts(batch, T, "krate")?, texts(batch, T, "function")?);
        let (unit, awaits, owner, of_job) = (texts(batch, T, "unit")?, texts(batch, T, "awaits")?, uints(batch, T, "owner")?, uints(batch, T, "job")?);
        let (start_ns, dur_ns, self_cpu) = (ints(batch, T, "start_ns")?, ints(batch, T, "dur_ns")?, ints(batch, T, "self_cpu_ns")?);
        let null = |column: &'static str| TimeError::BadColumn { table: T, column, why: "is null" };
        for row in 0..batch.num_rows() {
            out.push(SpanIn {
                process: u32_at(process, row, T, "process")?,
                thread: u32_at(thread, row, T, "thread")?,
                span_id: get(span_id, row, |it, row| it.value(row)).ok_or(null("span_id"))?,
                parent: get(parent, row, |it, row| it.value(row)),
                depth: get(depth, row, |it, row| it.value(row)).and_then(|it| u16::try_from(it).ok()).ok_or(null("depth"))?,
                name: text(name, row).ok_or(null("name"))?,
                krate: text(krate, row),
                function: text(function, row),
                unit: text(unit, row),
                awaits: text(awaits, row),
                owner: get(owner, row, |it, row| it.value(row)),
                job: get(of_job, row, |it, row| it.value(row)),
                start_ns: get(start_ns, row, |it, row| it.value(row)).ok_or(null("start_ns"))?,
                dur_ns: get(dur_ns, row, |it, row| it.value(row)).ok_or(null("dur_ns"))?,
                self_cpu_ns: get(self_cpu, row, |it, row| it.value(row)),
            });
        }
    }
    Ok(out)
}

fn events(batches: &[RecordBatch]) -> Result<Vec<EventIn>, TimeError> {
    const T: &str = "events";
    // alloc: the build's events, once a report
    let mut out = Vec::with_capacity(batches.iter().map(RecordBatch::num_rows).sum());
    let text = |it: &StringArray, row: usize| get(it, row, |it, row| it.value(row).to_owned());
    for batch in batches {
        let (process, thread, t_ns, job) = (ints(batch, T, "process")?, ints(batch, T, "thread")?, ints(batch, T, "t_ns")?, uints(batch, T, "job")?);
        let (level, target, name, message) = (texts(batch, T, "level")?, texts(batch, T, "target")?, texts(batch, T, "name")?, texts(batch, T, "message")?);
        let span_id = uints(batch, T, "span_id")?;
        let null = |column: &'static str| TimeError::BadColumn { table: T, column, why: "is null" };
        for row in 0..batch.num_rows() {
            out.push(EventIn {
                process: u32_at(process, row, T, "process")?,
                thread: u32_at(thread, row, T, "thread")?,
                t_ns: get(t_ns, row, |it, row| it.value(row)).ok_or(null("t_ns"))?,
                job: get(job, row, |it, row| it.value(row)),
                level: text(level, row).ok_or(null("level"))?,
                target: text(target, row).ok_or(null("target"))?,
                name: text(name, row).ok_or(null("name"))?,
                message: text(message, row),
                span: get(span_id, row, |it, row| it.value(row)),
            });
        }
    }
    Ok(out)
}

/// The derived tables of `model` written into the history `dir`: their files.
pub fn write(dir: &Path, model: &Model, path: &CriticalPath) -> Result<Vec<PathBuf>, TimeError> {
    let mut writer = history::Writer::new();
    // alloc: the files written, once a report
    let mut written = Vec::with_capacity(4);
    for (table, batch) in [
        (&schema::JOB_INTERVALS, job_intervals(model)),
        (&schema::SPAN_TIME, span_time(model)),
        (&schema::PATH_LINKS, path_links(model, path)),
        (&schema::CORE_BUSY, core_busy(model)),
    ] {
        let batch = batch.map_err(|error| TimeError::Arrow { table: table.name, error })?;
        // alloc: a file's name, once a table a report
        let file = dir.join(format!("{}-00000000.{}.parquet", model.build, table.name));
        writer.write(&file, table, &[batch]).map_err(TimeError::History)?;
        written.push(file);
    }
    Ok(written)
}

fn column(array: impl Array + 'static) -> ArrayRef {
    // alloc: a batch's column, once a table a report
    Arc::new(array)
}

fn batch(table: &'static Table, columns: Vec<ArrayRef>) -> Result<RecordBatch, arrow_schema::ArrowError> {
    RecordBatch::try_new(batches::schema_of(table), columns)
}

fn job_intervals(model: &Model) -> Result<RecordBatch, arrow_schema::ArrowError> {
    // alloc: once a report
    let rows: Vec<(&super::Lane, &super::Interval)> =
        model.lanes.iter().flat_map(|lane| lane.intervals.iter().map(move |it| (lane, it))).collect();
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
            column(UInt32Array::from_iter(rows.iter().map(|(_, it)| running(it).and_then(|it| it.0)))),
            column(UInt32Array::from_iter(rows.iter().map(|(_, it)| running(it).map(|it| it.1)))),
            column(StringArray::from_iter(rows.iter().map(|(_, it)| running(it).map(|it| it.2.name())))),
            column(StringArray::from_iter(rows.iter().map(|(_, it)| wait(it).and_then(|w| w.on).map(|on| on.name())))),
            column(UInt64Array::from_iter(rows.iter().map(|(_, it)| wait(it).and_then(|w| w.on.map(|_| w.value))))),
            column(StringArray::from_iter(
                rows.iter().map(|(_, it)| wait(it).and_then(|w| w.span).and_then(|s| model.spans[s].awaits.as_deref())),
            )),
            column(UInt64Array::from_iter(
                rows.iter().map(|(_, it)| wait(it).and_then(|w| w.waker.lane()).and_then(|l| model.lanes[l].id.job())),
            )),
            column(StringArray::from_iter(rows.iter().map(|(_, it)| wait(it).map(|w| w.waker.name())))),
            column(UInt64Array::from_iter(rows.iter().map(|(_, it)| it.inside.map(|s| model.spans[s].span_id)))),
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
    let ns = |at: fn(&super::SpanTime) -> i64| column(Int64Array::from_iter_values(times.iter().map(at)));
    batch(
        &schema::SPAN_TIME,
        // alloc: a batch's columns, once a report
        vec![
            column(UInt64Array::from_iter_values(spans.iter().map(|_| model.build))),
            column(UInt32Array::from_iter_values(spans.iter().map(|it| it.process))),
            column(UInt64Array::from_iter_values(spans.iter().map(|it| it.span_id))),
            column(UInt64Array::from_iter(times.iter().map(|it| it.lane.and_then(|l| model.lanes[l].id.job())))),
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
            column(UInt32Array::from_iter(links.iter().map(|it| match lane(it).intervals[it.interval].state {
                State::Running { core, .. } => core,
                _ => None,
            }))),
            column(Int64Array::from_iter_values(links.iter().map(|it| it.from_ns))),
            column(Int64Array::from_iter_values(links.iter().map(|it| it.to_ns))),
            column(Int64Array::from_iter_values(links.iter().map(path::Link::dur_ns))),
            column(StringArray::from_iter_values(links.iter().map(|it| report::link_item(model, it)))),
            column(UInt64Array::from_iter(links.iter().map(|it| it.wakes.and_then(|w| model.lanes[model.waits[w].lane].id.job())))),
            column(StringArray::from_iter(links.iter().map(|it| it.wakes.map(|w| model.wait_label(&model.waits[w]))))),
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
