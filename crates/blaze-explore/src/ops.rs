//! The answer to each op but `hello` (the session's, `server`), with no transport in it: a
//! header and at most one table. Everything here may wait (DuckDB runs as a process, Flecs
//! answers on its own thread), so the server calls it on tokio's blocking pool.

use std::{
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use blaze_profile::{sql, views};
use serde_json::{Map, Value};

use crate::{
    tiles::{self, Lane, Range, TilesError},
    wire::{Kind, Request, WireError},
};

/// Where the history's queries run: the pinned DuckDB CLI and the history's directory.
#[derive(Clone, Debug)]
pub struct History {
    /// `nix run .#duckdb`'s DuckDB, an absolute path; `None` when the daemon was not given one,
    /// and then every op that reads the history says so.
    pub duckdb: Option<PathBuf>,
    /// The history's directory (`blaze_profile::history::dir_in` of the daemon's state).
    pub dir: PathBuf,
}

/// What every answer reads.
pub(crate) struct Context {
    pub(crate) history: History,
    /// Flecs's world of the builds; `None` when the daemon keeps none.
    pub(crate) world: Option<Arc<blaze_web::Graphs>>,
}

/// A table of an answer: its schema and its batches, all of it.
pub(crate) struct Table {
    pub(crate) schema: SchemaRef,
    pub(crate) batches: Vec<RecordBatch>,
}

impl Table {
    pub(crate) fn rows(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }

    fn of(batches: Vec<RecordBatch>) -> Option<Table> {
        // clone: an `Arc` of the schema
        let schema = batches.first()?.schema();
        Some(Table { schema, batches })
    }
}

/// An answer: its header (`op` and what else the op says) and its table, if it has one.
pub(crate) struct Answer {
    pub(crate) header: Map<String, Value>,
    pub(crate) table: Option<Table>,
}

impl Answer {
    fn new(op: &'static str) -> Answer {
        let mut header = Map::new();
        // alloc: once a request, its header
        header.insert("op".to_owned(), op.into());
        Answer { header, table: None }
    }

    #[must_use]
    fn with(mut self, key: &str, value: impl Into<Value>) -> Answer {
        // alloc: once a request, its header
        self.header.insert(key.to_owned(), value.into());
        self
    }
}

/// The answer to `request`. `hello` is the session's to answer; asked here it is a bug of the
/// caller, said as `bad_args`. A message on `stop`, or its sender gone, kills a DuckDB query under
/// way, answered `cancelled`.
pub(crate) fn answer(cx: &Context, request: Request, stop: &sql::Stop) -> Result<Answer, WireError> {
    match request {
        Request::Hello { .. } => Err(WireError::new(Kind::BadArgs, "hello is answered by the session")),
        Request::Builds {} => {
            let sql = "SELECT * FROM builds ORDER BY started_ns";
            Ok(query(cx, "builds", sql, stop)?.0)
        }
        Request::Sql { sql } => Ok(query(cx, "sql", &sql, stop)?.0),
        Request::Views {} => views_table().map(|table| Answer { table: Some(table), ..Answer::new("views") }),
        Request::View { name, args } => {
            let view = views::find(&name).map_err(|it| view_error(&it))?;
            let sql = views::render(view, &args).map_err(|it| view_error(&it))?;
            Ok(query(cx, "view", &sql, stop)?.0.with("view", view.name))
        }
        Request::Spans { build, t0_ns, t1_ns } => {
            if t1_ns <= t0_ns {
                let message = format!("t1_ns {t1_ns} is not after t0_ns {t0_ns}");
                return Err(WireError::new(Kind::BadArgs, message));
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
        Request::World { path } => world(cx, &path),
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
fn run(cx: &Context, sql: &str, stop: &sql::Stop) -> Result<Vec<RecordBatch>, WireError> {
    let Some(duckdb) = &cx.history.duckdb else {
        let message = "the daemon was started with no DuckDB to query the history with: \
                       `blaze explore` passes the one `nix run .#duckdb` pins";
        return Err(WireError::new(Kind::Sql, message).with("query", sql));
    };
    let query = sql::Query { duckdb, history: &cx.history.dir, sql };
    sql::run_with(&query, stop).map_err(|error| {
        let kind = match (&error, error.kind()) {
            (sql::SqlError::Stopped, _) => Kind::Cancelled,
            (_, "no_such_view") => Kind::NoSuchView,
            (_, "bad_args") => Kind::BadArgs,
            _ => Kind::Sql,
        };
        let mut wire = WireError::new(kind, error.to_string()).with("query", sql);
        if let sql::SqlError::Duckdb { message, .. } = &error {
            wire = wire.with("duckdb", message.as_str());
        }
        wire
    })
}

/// The answer of op `op`: the result of `sql`, the SQL in the header; and its row count.
fn query(cx: &Context, op: &'static str, sql: &str, stop: &sql::Stop) -> Result<(Answer, usize), WireError> {
    let batches = run(cx, sql, stop)?;
    let table = Table::of(batches);
    let rows = table.as_ref().map_or(0, Table::rows);
    Ok((Answer { table, ..Answer::new(op).with("sql", sql).with("rows", rows) }, rows))
}

/// `Ok` when the history has build `build`, else `no_such_build`.
fn known_build(cx: &Context, build: u64, stop: &sql::Stop) -> Result<(), WireError> {
    let found = run(cx, &format!("SELECT build_id FROM builds WHERE build_id = {build} LIMIT 1"), stop)?;
    if found.iter().any(|it| it.num_rows() > 0) {
        return Ok(());
    }
    Err(WireError::new(Kind::NoSuchBuild, format!("the history has no build {build}")).with("build", build))
}

fn view_error(error: &views::ViewError) -> WireError {
    let kind = match error.kind() {
        "no_such_view" => Kind::NoSuchView,
        "bad_args" => Kind::BadArgs,
        _ => Kind::Sql,
    };
    WireError::new(kind, error.to_string())
}

fn tiles_error(error: &TilesError) -> WireError {
    let kind = match error {
        TilesError::Range(_) => Kind::BadArgs,
        TilesError::Column { .. } | TilesError::Value { .. } => Kind::Sql,
        TilesError::Arrow(_) => Kind::Encode,
    };
    WireError::new(kind, error.to_string())
}

/// The `views` table: each view's name, title, SQL with `$<arg>` where its arguments go, and
/// the JSON Schema of its arguments.
fn views_table() -> Result<Table, WireError> {
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
        .map_err(|why| WireError::new(Kind::Encode, format!("the views table: {why}")))?;
    Ok(Table { schema, batches: vec![batch] })
}

/// Flecs's REST answer to `GET <path>` over the builds' world, its JSON in the header's
/// `world`.
fn world(cx: &Context, path: &str) -> Result<Answer, WireError> {
    let Some(graphs) = &cx.world else {
        let message = "the daemon keeps no build world: blaze_web::Graphs, the Flecs world of the \
                       builds' functions, is not made by the v2 daemon (crates/blaze-cargo) or fed \
                       its builds' records";
        return Err(WireError::new(Kind::NoWorld, message).with("needs", "blaze_web::Graphs in the daemon"));
    };
    if !path.starts_with('/') {
        return Err(WireError::new(Kind::BadArgs, format!("the world's path `{path}` does not start with `/`")));
    }
    let answered = graphs.rest("GET", path).map_err(|why| WireError::new(Kind::NoWorld, why))?;
    if answered.code != 200 {
        let message = format!("Flecs answered {} to GET {path}: {}", answered.code, String::from_utf8_lossy(&answered.body));
        return Err(WireError::new(Kind::World, message).with("code", answered.code));
    }
    let world: Value = serde_json::from_slice(&answered.body).map_err(|why| {
        WireError::new(Kind::World, format!("Flecs's answer to GET {path} is not JSON ({}): {why}", answered.kind))
    })?;
    Ok(Answer::new("world").with("path", path).with("world", world))
}

#[cfg(test)]
mod tests {
    use arrow_array::cast::AsArray;

    use super::*;

    fn answer(cx: &Context, request: Request) -> Result<Answer, WireError> {
        super::answer(cx, request, &crossbeam_channel::never())
    }

    fn cx() -> Context {
        Context { history: History { duckdb: None, dir: PathBuf::from("/nowhere") }, world: None }
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
        assert_eq!(missing.err().map(|it| it.kind), Some(Kind::NoSuchView));
        let mut args = Map::new();
        args.insert("bulid".to_owned(), 3.into());
        let name = views::VIEWS[0].name.to_owned();
        let bad = answer(&cx(), Request::View { name, args });
        assert_eq!(bad.err().map(|it| it.kind), Some(Kind::BadArgs));
    }

    #[test]
    fn with_no_duckdb_every_history_op_says_so_and_names_the_query() {
        let error = answer(&cx(), Request::Sql { sql: "SELECT 1".to_owned() }).err().unwrap();
        assert_eq!(error.kind, Kind::Sql);
        assert_eq!(error.detail["query"], "SELECT 1");
        assert!(error.message.contains("nix run .#duckdb"), "{error}");
        let error = answer(&cx(), Request::Builds {}).err().unwrap();
        assert_eq!(error.kind, Kind::Sql);
    }

    #[test]
    fn a_backwards_range_is_bad_args_before_any_query() {
        let spans = answer(&cx(), Request::Spans { build: 1, t0_ns: 9, t1_ns: 9 });
        assert_eq!(spans.err().map(|it| it.kind), Some(Kind::BadArgs));
        let tiles = answer(&cx(), Request::Tiles { build: 1, t0_ns: 0, t1_ns: 10, bucket_ns: 0, lane: Lane::Thread });
        assert_eq!(tiles.err().map(|it| it.kind), Some(Kind::BadArgs));
    }

    #[test]
    fn with_no_world_the_world_op_names_what_is_missing() {
        let error = answer(&cx(), Request::World { path: "/entity/flecs".to_owned() }).err().unwrap();
        assert_eq!(error.kind, Kind::NoWorld);
        assert!(error.message.contains("blaze_web::Graphs"), "{error}");
    }

    #[test]
    fn the_overlap_rule_is_integers_only() {
        assert_eq!(
            overlapping(4, -10, 20),
            "build_id = 4 AND start_ns < 20 AND (start_ns + dur_ns > -10 OR (dur_ns <= 0 AND start_ns >= -10))"
        );
    }
}
