//! The daemon's tasks as the explorer's `tasks` and `task_edges` tables
//! (docs/v2/plan/design/explore-generic.md, "The core tables"; explore-core.md).
//!
//! The plane's loop owns the plane and the explorer's sessions never touch it: what the loop's
//! live models already read (each [`Record`] of the plane's records, through the one conversion
//! [`blaze_protocol::now::Records`], in the daemon's `serve::plane::Folds`) also goes into
//! [`Tasks`], a materialized, versioned table the loop writes and each session's feed reads. A
//! session that joins late gets every row the table holds on its first tick, then the rows that
//! changed: the `tasks` table is [`Rule::Upsert`], by (process, task), whole rows.
//!
//! - A task's id is the plane's request `<< 32 |` its node, since the plane reuses node slots
//!   across builds. Downloads in flight (`blaze_fetch::downloads`) are tasks too, of request 0
//!   (no plane request is 0), their id the download's, their `progress_*` in bytes.
//! - `kind`, `crate` and `unit` are the `blaze` layer's columns and carry the Arrow field
//!   metadata `ix.layer = blaze`; the rest are the core's.
//! - `state` is the [`State`]'s word ([`State::word`]), `outcome` the plane's word for how a
//!   settled task settled (`built`, `cached`, `cached remote`, `skipped`, `failed`,
//!   `cancelled`), `error` a failed one's message.
//! - Times are the profile's clock (`blaze_profile::clock::mono_ns`, the clock of `spans` and
//!   `open`): the plane's clock moved by the offset the loop gives ([`Tasks::clock`]).
//! - A settled task leaves 60 s after its `end_ns` ([`SETTLED_NS`]), on the tick's `at_ns`; the
//!   page evicts it by the same rule, so no row says it left. Its edges go with it.
//! - `task_edges` is append: a task's edges (`from` waits on `to`, both task ids) are sent when
//!   its deps are first said or change; the page keeps the latest per `from`.

use std::{collections::BTreeMap, sync::Arc};

use arrow_array::{
    ArrayRef, RecordBatch,
    builder::{ArrayBuilder as _, Int64Builder, StringBuilder, UInt64Builder},
};
use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef};
use blaze_fetch::downloads::Shown;
use blaze_profile::live::END;
use blaze_protocol::{
    events::NodeState,
    now::{MOST_TASKS, Record, State},
};
use ix_prof::explore::{Kind, OpError, Rule, Tick, frame};
use blaze_core::lock::{Lock, LockGuard};

/// How long a settled task stays in the live set after its `end_ns`: 60 s.
pub const SETTLED_NS: i64 = 60_000_000_000;

/// The field metadata key a layer's column carries, and the `blaze` layer's value.
pub const LAYER_KEY: &str = "ix.layer";
pub const LAYER: &str = "blaze";

/// The `kind` of a download's task.
pub const DOWNLOAD: &str = "download";

/// The unit of a download's progress.
pub const BYTES: &str = "bytes";

/// The daemon's tasks, shared by the loop (which writes them as the plane says its records) and
/// every session's feed (which reads them each tick).
pub struct Tasks {
    table: Lock<Table>,
}

impl Default for Tasks {
    fn default() -> Tasks {
        Tasks { table: Lock::new("explore.tasks.table", Table::default()) }
    }
}

impl Tasks {
    pub fn new() -> Tasks {
        Tasks::default()
    }

    /// The table, for the plane's loop to say a run of records into under one lock.
    pub fn lock(&self) -> LockGuard<'_, Table> {
        self.table.lock()
    }

    /// One reading of both clocks: the plane's `plane_ns` is the profile's `mono_ns`. The loop
    /// gives it as the daemon starts and at each look; until it does, a tick that has tasks to
    /// send fails, naming it.
    pub fn clock(&self, plane_ns: u64, mono_ns: i64) {
        let offset = mono_ns.saturating_sub(i64::try_from(plane_ns).unwrap_or(i64::MAX));
        self.table.lock().offset = Some(offset);
    }
}

/// The table itself: each task by id, in id order (so every output is in that order), and the
/// version each row last changed at.
#[derive(Default)]
pub struct Table {
    rows: BTreeMap<u64, Row>,
    /// Bumped by every change; a row carries the version it last changed at.
    version: u64,
    /// The profile's clock less the plane's ([`Tasks::clock`]).
    offset: Option<i64>,
    /// Tasks past [`MOST_TASKS`] not kept, so far: the fold ignores them too.
    refused: u64,
}

/// A task as the table keeps it. Times are the plane's clock.
struct Row {
    kind: &'static str,
    name: String,
    /// The task ids it waits on.
    deps: Vec<u64>,
    state: State,
    outcome: Option<&'static str>,
    error: Option<String>,
    said_ns: u64,
    running_ns: Option<u64>,
    end_ns: Option<u64>,
    /// The version it last changed at, and its deps did.
    version: u64,
    deps_version: u64,
}

/// The id of node `node` of plane request `request`.
pub fn task_id(request: u32, node: u64) -> u64 {
    u64::from(request) << 32 | (node & u64::from(u32::MAX))
}

/// How a settled node settled, as the plane says it: its word and, for a failure, its message.
fn outcome(node: &NodeState) -> (Option<&'static str>, Option<&str>) {
    match node {
        NodeState::Waiting | NodeState::Ready | NodeState::Running => (None, None),
        NodeState::Built => (Some("built"), None),
        NodeState::Cached { remote: false } => (Some("cached"), None),
        NodeState::Cached { remote: true } => (Some("cached remote"), None),
        NodeState::Skipped { .. } => (Some("skipped"), None),
        NodeState::Failed { why } => (Some("failed"), Some(why)),
        NodeState::Cancelled => (Some("cancelled"), None),
    }
}

impl Table {
    /// Tasks past [`MOST_TASKS`] not kept, so far.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// One record of plane request `request`, said at `at_ns` of the plane's clock. A new task
    /// past [`MOST_TASKS`] is not kept (counted in `refused`), and a move of a task not kept is
    /// ignored, as the fold does.
    pub fn said(&mut self, request: u32, at_ns: u64, record: Record<'_>) {
        match record {
            Record::Task { id, kind, name, deps } => {
                let id = task_id(request, id);
                let version = self.version + 1;
                let widened = deps.iter().map(|&dep| task_id(request, dep));
                if let Some(row) = self.rows.get_mut(&id) {
                    if !row.deps.iter().copied().eq(widened.clone()) {
                        row.deps.clear();
                        // alloc: a task's deps, as the plane says them
                        row.deps.extend(widened);
                        row.deps_version = version;
                    }
                    row.kind = kind;
                    if row.name != name {
                        row.name.clear();
                        row.name.push_str(name);
                    }
                    row.version = version;
                } else if self.rows.len() >= MOST_TASKS {
                    self.refused = self.refused.saturating_add(1);
                    return;
                } else {
                    let row = Row {
                        kind,
                        // alloc: a task's name and deps, once a task
                        name: name.to_owned(),
                        deps: widened.collect(),
                        state: State::Waiting,
                        outcome: None,
                        error: None,
                        said_ns: at_ns,
                        running_ns: None,
                        end_ns: None,
                        version,
                        deps_version: version,
                    };
                    self.rows.insert(id, row);
                }
                self.version = version;
            }
            Record::Moved { id, state, node } => {
                let Some(row) = self.rows.get_mut(&task_id(request, id)) else { return };
                let was = row.state;
                row.state = state;
                let (word, why) = outcome(node);
                row.outcome = word;
                row.error = why.map(str::to_owned); // alloc: a failure's message, once
                match state {
                    State::Running if was != State::Running => row.running_ns = Some(at_ns),
                    State::Done | State::Failed | State::Cancelled if was.unfinished() => {
                        row.end_ns = Some(at_ns)
                    }
                    _ if state.unfinished() => row.end_ns = None,
                    _ => {}
                }
                self.version += 1;
                row.version = self.version;
            }
        }
    }

    /// Drops every task settled [`SETTLED_NS`] before `at_ns` (the profile's clock).
    fn evict(&mut self, at_ns: i64) {
        let Some(offset) = self.offset else { return };
        self.rows.retain(|_, row| match row.end_ns {
            Some(end) => profile_ns(end, offset).saturating_add(SETTLED_NS) > at_ns,
            None => true,
        });
    }
}

/// A plane time on the profile's clock.
fn profile_ns(plane_ns: u64, offset: i64) -> i64 {
    i64::try_from(plane_ns).unwrap_or(i64::MAX).saturating_add(offset)
}

/// The `tasks` table's schema: the core's columns, and the `blaze` layer's (`kind`, `crate`,
/// `unit`) marked by their metadata.
pub fn tasks_schema() -> Schema {
    let layer = |name: &str, nullable: bool| {
        Field::new(name, DataType::Utf8, nullable)
            // alloc: a schema's metadata, once a session
            .with_metadata(std::collections::HashMap::from([(LAYER_KEY.to_owned(), LAYER.to_owned())]))
    };
    Schema::new(vec![
        Field::new("process", DataType::UInt64, false),
        Field::new("task", DataType::UInt64, false),
        layer("kind", false),
        Field::new("name", DataType::Utf8, false),
        Field::new("state", DataType::Utf8, false),
        Field::new("outcome", DataType::Utf8, true),
        Field::new("error", DataType::Utf8, true),
        Field::new("start_ns", DataType::Int64, false),
        Field::new("end_ns", DataType::Int64, true),
        Field::new("progress_done", DataType::UInt64, true),
        Field::new("progress_total", DataType::UInt64, true),
        Field::new("progress_unit", DataType::Utf8, true),
        Field::new("waiting_on", DataType::UInt64, true),
        layer("crate", true),
        layer("unit", true),
    ])
}

/// The `task_edges` table's schema: `from` waits on `to`.
pub fn edges_schema() -> Schema {
    Schema::new(vec![
        Field::new("process", DataType::UInt64, false),
        Field::new("from", DataType::UInt64, false),
        Field::new("to", DataType::UInt64, false),
    ])
}

/// One table's long-lived IPC stream for a session: the schema once, then a batch a frame; on
/// an error the stream ends and the next batch starts a new one after [`END`].
struct Stream {
    schema: SchemaRef,
    encoder: Option<arrow_ipc::writer::StreamEncoder>,
    begun: bool,
}

impl Stream {
    fn new(schema: Schema) -> Stream {
        Stream { schema: Arc::new(schema), encoder: None, begun: false }
    }

    /// `batch`'s bytes of the stream into `into` (cleared first); nothing for no rows.
    fn encode(&mut self, batch: &RecordBatch, into: &mut Vec<u8>) -> Result<(), ArrowError> {
        into.clear();
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let encoder = match self.encoder.take() {
            Some(it) => it,
            None => {
                if self.begun {
                    into.extend_from_slice(&END);
                }
                arrow_ipc::writer::StreamEncoder::try_new_with_options(
                    &self.schema,
                    frame::options()?,
                )
                .inspect_err(|_| into.clear())?
            }
        };
        let encoder = self.encoder.insert(encoder);
        match encoder.encode(batch) {
            Ok(buffers) => {
                for buffer in buffers {
                    into.extend_from_slice(buffer.as_slice());
                }
                self.begun = true;
                Ok(())
            }
            Err(why) => {
                self.encoder = None;
                into.clear();
                Err(why)
            }
        }
    }
}

/// A download as a session last sent it.
struct Sent {
    what: String,
    start_ns: i64,
    done: u64,
    total: Option<u64>,
    /// When it left the registry; it leaves the session's set [`SETTLED_NS`] after.
    end_ns: Option<i64>,
}

/// One session's feed of the tasks: the table's version it has, its two streams and the
/// downloads it was sent.
pub struct Feed {
    tasks: Arc<Tasks>,
    seen: u64,
    process: u64,
    rows: Stream,
    edges: Stream,
    downloads: BTreeMap<u64, Sent>,
    builder: Builder,
    from: UInt64Builder,
    to: UInt64Builder,
}

/// The `tasks` columns being built for one tick.
#[derive(Default)]
struct Builder {
    task: UInt64Builder,
    kind: StringBuilder,
    name: StringBuilder,
    state: StringBuilder,
    outcome: StringBuilder,
    error: StringBuilder,
    start_ns: Int64Builder,
    end_ns: Int64Builder,
    progress_done: UInt64Builder,
    progress_total: UInt64Builder,
    progress_unit: StringBuilder,
    waiting_on: UInt64Builder,
    krate: StringBuilder,
    unit: StringBuilder,
}

/// One `tasks` row as it is appended.
struct Out<'a> {
    task: u64,
    kind: &'a str,
    name: &'a str,
    state: State,
    outcome: Option<&'a str>,
    error: Option<&'a str>,
    start_ns: i64,
    end_ns: Option<i64>,
    progress: Option<(u64, Option<u64>)>,
}

impl Builder {
    fn push(&mut self, row: &Out<'_>) {
        self.task.append_value(row.task);
        self.kind.append_value(row.kind);
        self.name.append_value(row.name);
        self.state.append_value(row.state.word());
        self.outcome.append_option(row.outcome);
        self.error.append_option(row.error);
        self.start_ns.append_value(row.start_ns);
        self.end_ns.append_option(row.end_ns);
        self.progress_done.append_option(row.progress.map(|(done, _)| done));
        self.progress_total.append_option(row.progress.and_then(|(_, total)| total));
        self.progress_unit.append_option(row.progress.map(|_| BYTES));
        self.waiting_on.append_null();
        self.krate.append_null();
        self.unit.append_null();
    }

    fn finish(&mut self, schema: &SchemaRef, process: u64) -> Result<RecordBatch, ArrowError> {
        let rows = self.task.len();
        // alloc: a batch's list of columns, once a tick with rows
        let columns: Vec<ArrayRef> = vec![
            Arc::new(arrow_array::UInt64Array::from_value(process, rows)),
            Arc::new(self.task.finish()),
            Arc::new(self.kind.finish()),
            Arc::new(self.name.finish()),
            Arc::new(self.state.finish()),
            Arc::new(self.outcome.finish()),
            Arc::new(self.error.finish()),
            Arc::new(self.start_ns.finish()),
            Arc::new(self.end_ns.finish()),
            Arc::new(self.progress_done.finish()),
            Arc::new(self.progress_total.finish()),
            Arc::new(self.progress_unit.finish()),
            Arc::new(self.waiting_on.finish()),
            Arc::new(self.krate.finish()),
            Arc::new(self.unit.finish()),
        ];
        // clone: an `Arc` of the schema
        RecordBatch::try_new(schema.clone(), columns)
    }
}

impl Feed {
    /// A session's feed of `tasks`, from nothing: its first tick sends every row.
    pub fn new(tasks: Arc<Tasks>) -> Feed {
        Feed {
            tasks,
            seen: 0,
            process: u64::from(std::process::id()),
            rows: Stream::new(tasks_schema()),
            edges: Stream::new(edges_schema()),
            downloads: BTreeMap::new(),
            builder: Builder::default(),
            from: UInt64Builder::new(),
            to: UInt64Builder::new(),
        }
    }

    /// This tick's `tasks` (every row changed since the last, and the downloads in flight or
    /// just gone) and `task_edges` (every changed task's edges), into `into`, at `at_ns` of the
    /// profile's clock, with `downloads` as the registry shows them now.
    pub fn tick(&mut self, at_ns: i64, downloads: &[Shown], into: &mut Tick) -> Result<(), OpError> {
        let seen = {
            let mut table = self.tasks.lock();
            table.evict(at_ns);
            let changed = table.rows.iter().filter(|(_, row)| row.version > self.seen);
            let mut changed = changed.peekable();
            if changed.peek().is_some() {
                let Some(offset) = table.offset else {
                    return Err(OpError::new(
                        Kind::Encode,
                        "the daemon's tasks: the plane's clock was never read against the profile's (`blaze_explore::Tasks::clock`), so no task's time can be said",
                    ));
                };
                for (&id, row) in changed {
                    let start = row.running_ns.unwrap_or(row.said_ns);
                    self.builder.push(&Out {
                        task: id,
                        kind: row.kind,
                        name: &row.name,
                        state: row.state,
                        outcome: row.outcome,
                        error: row.error.as_deref(),
                        start_ns: profile_ns(start, offset),
                        end_ns: row.end_ns.map(|end| profile_ns(end, offset)),
                        progress: None,
                    });
                    if row.deps_version > self.seen {
                        for &dep in &row.deps {
                            self.from.append_value(id);
                            self.to.append_value(dep);
                        }
                    }
                }
            }
            table.version
        };
        self.downloads(at_ns, downloads);
        let process = self.process;
        let tasks = self.builder.finish(&self.rows.schema, process);
        let tasks = tasks.map_err(|why| encode_error("tasks", &why))?;
        let edges = {
            let rows = self.from.len();
            // alloc: a batch's list of columns, once a tick
            let columns: Vec<ArrayRef> = vec![
                Arc::new(arrow_array::UInt64Array::from_value(process, rows)),
                Arc::new(self.from.finish()),
                Arc::new(self.to.finish()),
            ];
            // clone: an `Arc` of the schema
            RecordBatch::try_new(self.edges.schema.clone(), columns)
                .map_err(|why| encode_error("task_edges", &why))?
        };
        for (name, rule, stream, batch) in [
            ("tasks", Rule::Upsert, &mut self.rows, &tasks),
            ("task_edges", Rule::Append, &mut self.edges, &edges),
        ] {
            let table = into.table(name, rule);
            stream.encode(batch, &mut table.ipc).map_err(|why| encode_error(name, &why))?;
            table.rows = batch.num_rows();
        }
        self.seen = seen;
        Ok(())
    }

    /// The downloads' rows: each one in flight whose bytes moved (or that is new), and each
    /// that left the registry since the last tick, settled; one settled [`SETTLED_NS`] ago
    /// leaves the session's set.
    fn downloads(&mut self, at_ns: i64, shown: &[Shown]) {
        let elapsed = |it: &Shown| i64::try_from(it.line.elapsed.as_nanos()).unwrap_or(i64::MAX);
        for it in shown {
            let id = task_id(0, it.id);
            let (done, total) = (it.line.done, it.line.total);
            let sent = self.downloads.entry(id).or_insert_with(|| Sent {
                // alloc: a download's name, once a download a session
                what: it.what.clone(),
                start_ns: at_ns.saturating_sub(elapsed(it)),
                done: u64::MAX,
                total: None,
                end_ns: None,
            });
            if (sent.done, sent.total) == (done, total) {
                continue;
            }
            (sent.done, sent.total) = (done, total);
            self.builder.push(&Out {
                task: id,
                kind: DOWNLOAD,
                name: &sent.what,
                state: State::Running,
                outcome: None,
                error: None,
                start_ns: sent.start_ns,
                end_ns: None,
                progress: Some((done, total)),
            });
        }
        let builder = &mut self.builder;
        self.downloads.retain(|&id, sent| {
            if let Some(end) = sent.end_ns {
                return end.saturating_add(SETTLED_NS) > at_ns;
            }
            if shown.iter().any(|it| task_id(0, it.id) == id) {
                return true;
            }
            sent.end_ns = Some(at_ns);
            builder.push(&Out {
                task: id,
                kind: DOWNLOAD,
                name: &sent.what,
                state: State::Done,
                outcome: None,
                error: None,
                start_ns: sent.start_ns,
                end_ns: sent.end_ns,
                progress: Some((sent.done, sent.total)),
            });
            true
        });
    }
}

fn encode_error(table: &str, why: &ArrowError) -> OpError {
    OpError::new(Kind::Encode, format!("the live `{table}` table: {why}"))
}
