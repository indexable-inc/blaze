//! The answer to each op of the `blaze` layer (`hello` is the session's,
//! `ix_prof::explore::session`), with no transport in it: a header and at most one table.
//! Everything here may wait (DuckDB runs as a process), so the
//! server calls it on tokio's blocking pool.

use std::{fmt, path::PathBuf, sync::Arc, time::Instant};

use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use blaze_profile::{sql, views};
use ix_prof::explore::{Answer, Kind as Core, OpError, Table};

use crate::{
    tiles::{self, Lane, Range, TilesError},
    wire::{Kind, Request},
};

/// Where the history's queries run: the pinned DuckDB CLI and the history's directory.
#[derive(Clone, Debug)]
pub struct History {
    /// `nix run .#duckdb`'s DuckDB, an absolute path; `None` when the daemon was not given one,
    /// and then every op that reads the history says so.
    pub duckdb: Option<PathBuf>,
    /// The history's directory (`blaze_profile::history::dir_in` of the daemon's state).
    pub dir: PathBuf,
    /// Run before every query, so it reads each row the daemon recorded before it was asked.
    pub flush: Flush,
}

/// Has the daemon that writes the history flush it (the same flush a CLI reader asks for with
/// `blaze_api::Request::Flush`), answering the flush's number once it committed, or why it
/// failed. The daemon gives it ([`crate::Config`]); a query whose flush fails is not run.
#[derive(Clone)]
pub struct Flush(Arc<dyn Fn() -> Result<u64, String> + Send + Sync>);

impl Flush {
    pub fn new(flush: impl Fn() -> Result<u64, String> + Send + Sync + 'static) -> Flush {
        Flush(Arc::new(flush))
    }
}

impl fmt::Debug for Flush {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Flush")
    }
}

/// What every answer reads.
pub(crate) struct Context {
    pub(crate) history: History,
}

/// The answer to `request`. A message on `stop`, or its sender gone, kills a DuckDB query under
/// way, answered `cancelled`.
pub(crate) fn answer(cx: &Context, request: Request, stop: &sql::Stop) -> Result<Answer, OpError> {
    match request {
        Request::Builds {} => {
            let sql = "SELECT * FROM builds ORDER BY started_ns";
            Ok(query(cx, "builds", sql, stop)?.0)
        }
        Request::Sql { sql } => Ok(query(cx, "sql", &sql, stop)?.0),
        Request::Views {} => {
            views_table().map(|table| Answer { table: Some(table), ..Answer::new("views") })
        }
        Request::View { name, args } => {
            let view = views::find(&name).map_err(|it| view_error(&it))?;
            let sql = views::render(view, &args).map_err(|it| view_error(&it))?;
            Ok(query(cx, "view", &sql, stop)?.0.with("view", view.name))
        }
        Request::Spans { build, t0_ns, t1_ns } => {
            if t1_ns <= t0_ns {
                let message = format!("t1_ns {t1_ns} is not after t0_ns {t0_ns}");
                return Err(OpError::new(Core::BadArgs, message));
            }
            let sql = format!(
                "SELECT * FROM spans WHERE {} ORDER BY process, thread, start_ns",
                overlapping(build, t0_ns, t1_ns)
            );
            let (answer, rows) = query(cx, "spans", &sql, stop)?;
            if rows == 0 {
                known_build(cx, build, stop)?;
            }
            Ok(answer.with("build", build))
        }
        Request::Tiles { build, t0_ns, t1_ns, bucket_ns, lane } => {
            let range = Range::new(t0_ns, t1_ns, bucket_ns).map_err(|it| tiles_error(&it))?;
            let (from, of) = match lane {
                Lane::Thread => ("spans", "thread"),
                Lane::Job => ("job_spans", "job"),
            };
            let sql = format!(
                "SELECT process, {of}, depth, name, start_ns, dur_ns FROM {from} WHERE {} AND {of} IS NOT NULL \
                 ORDER BY process, {of}, depth, start_ns",
                overlapping(build, t0_ns, t1_ns)
            );
            let started = Instant::now();
            let spans = run(cx, &sql, stop)?;
            if spans.iter().all(|it| it.num_rows() == 0) {
                known_build(cx, build, stop)?;
            }
            let sql_ns = started.elapsed().as_nanos() as u64;
            let tiles = tiles::tiles_by(&spans, range, lane).map_err(|it| tiles_error(&it))?;
            let tiles_ns = started.elapsed().as_nanos() as u64 - sql_ns;
            let answer = Answer::new("tiles")
                .with("lane", lane.column())
                .with("sql", sql)
                .with("build", build)
                .with("bucket_ns", bucket_ns)
                .with("buckets", range.buckets())
                .with("spans", spans.iter().map(RecordBatch::num_rows).sum::<usize>())
                .with("sql_ns", sql_ns)
                .with("tiles_ns", tiles_ns);
            Ok(Answer { table: Table::of(vec![tiles]), ..answer })
        }
    }
}

/// The SQL condition of the spans of `build` overlapping `[t0_ns, t1_ns)`: a span that ends
/// inside or after it and starts before its end, and an instant inside it. The same rule as
/// [`tiles::tiles`]'s; each value is an integer, so nothing is quoted.
fn overlapping(build: u64, t0_ns: i64, t1_ns: i64) -> String {
    format!(
        "build_id = {build} AND start_ns < {t1_ns} \
         AND (start_ns + dur_ns > {t0_ns} OR (dur_ns <= 0 AND start_ns >= {t0_ns}))"
    )
}

/// Runs `sql` over the history.
fn run(cx: &Context, sql: &str, stop: &sql::Stop) -> Result<Vec<RecordBatch>, OpError> {
    let Some(duckdb) = &cx.history.duckdb else {
        let message = "the daemon was started with no DuckDB to query the history with: \
                       `blaze explore` passes the one `nix run .#duckdb` pins";
        return Err(OpError::new(Kind::Sql, message).with("query", sql));
    };
    (cx.history.flush.0)().map_err(|why| {
        OpError::new(
            Kind::Flush,
            format!(
                "the history was not flushed before the query, so it would miss recent rows: {why}"
            ),
        )
        .with("query", sql)
    })?;
    let query = sql::Query { duckdb, history: &cx.history.dir, scope: sql::Scope::All, sql };
    sql::run_with(&query, stop).map_err(|error| {
        let kind = match (&error, error.kind()) {
            (sql::SqlError::Stopped, _) => Core::Cancelled.name(),
            (_, "no_such_view") => Kind::NoSuchView.name(),
            (_, "bad_args") => Core::BadArgs.name(),
            _ => Kind::Sql.name(),
        };
        let mut wire = OpError::new(kind, error.to_string()).with("query", sql);
        if let sql::SqlError::Duckdb { message, .. } = &error {
            wire = wire.with("duckdb", message.as_str());
        }
        wire
    })
}

/// The answer of op `op`: the result of `sql`, the SQL in the header; and its row count.
fn query(
    cx: &Context,
    op: &'static str,
    sql: &str,
    stop: &sql::Stop,
) -> Result<(Answer, usize), OpError> {
    let batches = run(cx, sql, stop)?;
    let table = Table::of(batches);
    let rows = table.as_ref().map_or(0, Table::rows);
    Ok((Answer { table, ..Answer::new(op).with("sql", sql).with("rows", rows) }, rows))
}

/// `Ok` when the history has build `build`, else `no_such_build`.
fn known_build(cx: &Context, build: u64, stop: &sql::Stop) -> Result<(), OpError> {
    let found =
        run(cx, &format!("SELECT build_id FROM builds WHERE build_id = {build} LIMIT 1"), stop)?;
    if found.iter().any(|it| it.num_rows() > 0) {
        return Ok(());
    }
    Err(OpError::new(Kind::NoSuchBuild, format!("the history has no build {build}"))
        .with("build", build))
}

fn view_error(error: &views::ViewError) -> OpError {
    let kind = match error.kind() {
        "no_such_view" => Kind::NoSuchView.name(),
        "bad_args" => Core::BadArgs.name(),
        _ => Kind::Sql.name(),
    };
    OpError::new(kind, error.to_string())
}

fn tiles_error(error: &TilesError) -> OpError {
    let kind = match error {
        TilesError::Range(_) => Core::BadArgs.name(),
        TilesError::Column { .. } | TilesError::Value { .. } => Kind::Sql.name(),
        TilesError::Arrow(_) => Core::Encode.name(),
    };
    OpError::new(kind, error.to_string())
}

/// The `views` table: each view's name, title, SQL with `$<arg>` where its arguments go, and
/// the JSON Schema of its arguments.
fn views_table() -> Result<Table, OpError> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("title", DataType::Utf8, false),
        Field::new("sql", DataType::Utf8, false),
        Field::new("args", DataType::Utf8, false),
    ]));
    let list = views::VIEWS;
    // alloc: once a request, the views' argument schemas as text
    let args: Vec<String> = list.iter().map(|it| views::args_schema(it).to_string()).collect();
    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(list.iter().map(|it| it.name))),
        Arc::new(StringArray::from_iter_values(list.iter().map(|it| it.title))),
        Arc::new(StringArray::from_iter_values(list.iter().map(|it| it.sql))),
        Arc::new(StringArray::from(args)),
    ];
    // clone: an `Arc` of the schema
    let batch = RecordBatch::try_new(Arc::clone(&schema), columns)
        .map_err(|why| OpError::new(Core::Encode, format!("the views table: {why}")))?;
    Ok(Table { schema, batches: vec![batch] })
}

#[cfg(test)]
mod tests {
    use arrow_array::cast::AsArray;
    use serde_json::{Map, Value};

    use super::*;

    fn answer(cx: &Context, request: Request) -> Result<Answer, OpError> {
        super::answer(cx, request, &crossbeam_channel::never())
    }

    fn cx() -> Context {
        Context {
            history: History {
                duckdb: None,
                dir: PathBuf::from("/nowhere"),
                flush: Flush::new(|| Ok(0)),
            },
        }
    }

    #[test]
    fn the_views_op_lists_every_view_with_its_argument_schema() {
        let answer = answer(&cx(), Request::Views {}).unwrap();
        assert_eq!(answer.header["op"], "views");
        let table = answer.table.unwrap();
        assert_eq!(table.rows(), views::VIEWS.len());
        let batch = &table.batches[0];
        let names = batch.column(0).as_string::<i32>();
        let args = batch.column(3).as_string::<i32>();
        for (i, view) in views::VIEWS.iter().enumerate() {
            assert_eq!(names.value(i), view.name);
            let schema: Value = serde_json::from_str(args.value(i)).unwrap();
            assert_eq!(schema, views::args_schema(view));
        }
    }

    #[test]
    fn a_view_that_is_not_there_or_badly_asked_is_its_own_error() {
        let missing = answer(&cx(), Request::View { name: "nope".to_owned(), args: Map::new() });
        assert_eq!(missing.err().map(|it| it.kind), Some(Kind::NoSuchView.name()));
        let mut args = Map::new();
        args.insert("bulid".to_owned(), 3.into());
        let name = views::VIEWS[0].name.to_owned();
        let bad = answer(&cx(), Request::View { name, args });
        assert_eq!(bad.err().map(|it| it.kind), Some(Core::BadArgs.name()));
    }

    #[test]
    fn with_no_duckdb_every_history_op_says_so_and_names_the_query() {
        let error = answer(&cx(), Request::Sql { sql: "SELECT 1".to_owned() }).err().unwrap();
        assert_eq!(error.kind, Kind::Sql.name());
        assert_eq!(error.detail["query"], "SELECT 1");
        assert!(error.message.contains("nix run .#duckdb"), "{error}");
        let error = answer(&cx(), Request::Builds {}).err().unwrap();
        assert_eq!(error.kind, Kind::Sql.name());
    }

    #[test]
    fn a_backwards_range_is_bad_args_before_any_query() {
        let spans = answer(&cx(), Request::Spans { build: 1, t0_ns: 9, t1_ns: 9 });
        assert_eq!(spans.err().map(|it| it.kind), Some(Core::BadArgs.name()));
        let tiles = answer(
            &cx(),
            Request::Tiles { build: 1, t0_ns: 0, t1_ns: 10, bucket_ns: 0, lane: Lane::Thread },
        );
        assert_eq!(tiles.err().map(|it| it.kind), Some(Core::BadArgs.name()));
    }

    #[test]
    fn the_overlap_rule_is_integers_only() {
        assert_eq!(
            overlapping(4, -10, 20),
            "build_id = 4 AND start_ns < 20 AND (start_ns + dur_ns > -10 OR (dur_ns <= 0 AND start_ns >= -10))"
        );
    }
}
