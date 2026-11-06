//! Each live table's one long-lived Arrow IPC stream, decoded across frames, and its rows as
//! typed structs.
//!
//! The daemon (blaze-profile's `live::Encoder`) keeps one IPC stream per table for the whole
//! session, zstd-compressed with delta dictionaries: the schema and each name go once, and a
//! frame's bytes are the next whole messages. When a stream's dictionaries reach their cap, the
//! next frame with rows starts with the end-of-stream marker [`END`] and then a new stream, its
//! schema and dictionaries again. [`Decoder`] follows that: it keeps the IPC stream across
//! frames and, at [`END`] at a frame's start, ends the old stream and begins a new one.
//!
//! The IPC itself is read through [`Stream`], a type parameter the driver picks (arrow-ipc's
//! `StreamDecoder`, in [`crate::ipc`]): that decoder seeds its maps from OS randomness, which a
//! core may not reach.
//!
//! Columns are found by name, so a table that gains a column, or a generic source whose ids are
//! narrower, still reads: every id column widens to `u64` exactly, and a required column that
//! is missing, of another type, or null in a row is an error naming the message, the column
//! and the row.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use arrow_array::{
    Array, ArrayRef, Float64Array, Int64Array, PrimitiveArray, RecordBatch, StringArray,
    UInt32Array,
    cast::AsArray,
    types::{Float64Type, Int64Type, UInt8Type, UInt16Type, UInt32Type, UInt64Type},
};
use arrow_buffer::Buffer;
use arrow_schema::{ArrowError, DataType};
use blaze_protocol::now::State;

use crate::{Error, column};

/// The Arrow IPC end-of-stream marker (continuation, then a length of 0): what a frame's IPC
/// starts with when the table's stream ends there and a new one begins.
pub const END: [u8; 8] = [0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0];

/// The smallest IPC message: a continuation and a length. Each decode step that yields a batch
/// takes at least this, which bounds a frame's steps.
const MESSAGE_MIN: usize = 8;

/// One Arrow IPC stream read message by message: what the core needs of an IPC decoder. The
/// driver supplies it ([`crate::ipc`] implements it for arrow-ipc's `StreamDecoder`), so the
/// decoder's own effects (its maps' `RandomState`) are the driver's, never the core's.
pub trait Stream: Sized {
    /// A stream with nothing read yet.
    fn begin() -> Self;
    /// The next batch from the front of `buffer`, the bytes it took cut off; `None` when the
    /// bytes held no batch (a schema, a dictionary, the end-of-stream marker, or part of a
    /// message).
    fn decode(&mut self, buffer: &mut Buffer) -> Result<Option<RecordBatch>, ArrowError>;
    /// An error unless what was read so far ends at a message's end.
    fn finish(&mut self) -> Result<(), ArrowError>;
}

/// One table's long-lived IPC stream, the page's side, read through `S`.
#[derive(Debug)]
pub struct Decoder<S> {
    table: String,
    stream: Option<S>,
    /// A frame failed mid-stream: the stream's dictionaries are gone, so nothing reads until
    /// the daemon begins a new stream ([`END`]) or the page subscribes again.
    broken: bool,
    /// Streams begun so far, so an error names which.
    streams: u64,
}

impl<S: Stream> Decoder<S> {
    pub fn new(table: &str) -> Decoder<S> {
        // alloc: once a table a session
        Decoder { table: table.to_owned(), stream: None, broken: false, streams: 0 }
    }

    /// Streams begun so far: one, plus one each reset.
    pub fn streams(&self) -> u64 {
        self.streams
    }

    fn message(&self) -> String {
        // alloc: an error's text
        format!("live frame for table `{}`", self.table)
    }

    fn error(&self, why: impl std::fmt::Display) -> Error {
        // alloc: an error's text
        Error::new(self.message(), format!("the IPC (stream {})", self.streams), why.to_string())
    }

    /// The batches of one frame's IPC of this table, into `into` (cleared first). The bytes
    /// are whole messages, the frames' in order; empty bytes are a frame with no rows.
    pub fn decode(&mut self, ipc: &[u8], into: &mut Vec<RecordBatch>) -> Result<(), Error> {
        into.clear();
        let mut rest = ipc;
        if let Some(after) = rest.strip_prefix(&END) {
            match self.stream.take() {
                Some(mut old) => {
                    let mut end = Buffer::from_slice_ref(END);
                    let ended = match old.decode(&mut end) {
                        Ok(None) => old.finish().map_err(|why| why.to_string()),
                        // alloc: an error's text
                        Ok(Some(_)) => Err("a batch where the stream ends".to_owned()),
                        Err(why) => Err(why.to_string()),
                    };
                    if let Err(why) = ended {
                        self.broken = true;
                        return Err(self.error(format_args!("its end: {why}")));
                    }
                }
                // The stream that broke ends here; the new one reads again.
                None if self.broken => {}
                None => return Err(self.error("the frame ends a stream before any began")),
            }
            self.broken = false;
            rest = after;
        }
        if rest.is_empty() {
            return Ok(());
        }
        if self.broken {
            return Err(self.error(
                "an earlier frame of this stream failed, so its dictionaries are gone: nothing \
                 reads until the daemon begins a new stream or the page subscribes again",
            ));
        }
        if self.stream.is_none() {
            self.streams += 1;
        }
        let stream = self.stream.get_or_insert_with(S::begin);
        match messages(stream, rest, into) {
            Ok(()) => Ok(()),
            Err(why) => {
                self.stream = None;
                self.broken = true;
                into.clear();
                Err(self.error(why))
            }
        }
    }
}

/// `bytes`, whole messages, through `stream` into `into`, ending at a message's end.
fn messages<S: Stream>(stream: &mut S, bytes: &[u8], into: &mut Vec<RecordBatch>) -> Result<(), String> {
    // alloc: a frame's bytes, copied 8-aligned for the decoder, once a frame
    let mut buffer = Buffer::from_slice_ref(bytes);
    // Each step yields a batch, taking at least a message, or takes the rest and yields none.
    let mut fuel = bytes.len() / MESSAGE_MIN + 1;
    while !buffer.is_empty() {
        if fuel == 0 {
            // alloc: an error's text
            return Err(format!(
                "{} of {} bytes left after as many steps as the bytes hold messages",
                buffer.len(),
                bytes.len()
            ));
        }
        fuel -= 1;
        match stream.decode(&mut buffer) {
            // alloc: a frame's batches, into the caller's reused list
            Ok(Some(batch)) => into.push(batch),
            Ok(None) if buffer.is_empty() => {}
            Ok(None) => {
                // alloc: an error's text
                return Err(format!(
                    "the stream ended {} bytes before the frame did",
                    buffer.len()
                ));
            }
            Err(why) => return Err(why.to_string()),
        }
    }
    // A frame is whole messages: the next frame starts a message.
    stream.finish().map_err(|why| why.to_string())
}

/// The batches of one whole IPC stream, read through `S`: an answer's table. `message` names it
/// in errors.
pub fn whole<S: Stream>(ipc: &[u8], message: &str) -> Result<Vec<RecordBatch>, Error> {
    // alloc: an answer's batches, once an answer
    let mut batches = Vec::new();
    let mut stream = S::begin();
    messages(&mut stream, ipc, &mut batches).map_err(|why| Error::new(message, "the IPC", why))?;
    Ok(batches)
}

/// A column as read: ids of any unsigned width, `Int64`, `Float64`, and text as `Utf8` or a
/// `Dictionary(UInt32, Utf8)`.
#[derive(Clone, Copy)]
enum Col<'a> {
    U8(&'a PrimitiveArray<UInt8Type>),
    U16(&'a PrimitiveArray<UInt16Type>),
    U32(&'a PrimitiveArray<UInt32Type>),
    U64(&'a PrimitiveArray<UInt64Type>),
    I64(&'a Int64Array),
    F64(&'a Float64Array),
    Utf8(&'a StringArray),
    Dict { keys: &'a UInt32Array, values: &'a StringArray },
}

/// What a row reader wants a column to hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    Id,
    I64,
    F64,
    Text,
}

impl Want {
    fn says(self) -> &'static str {
        match self {
            Want::Id => "an unsigned integer (UInt8 to UInt64)",
            Want::I64 => "Int64",
            Want::F64 => "Float64",
            Want::Text => "Utf8 or Dictionary(UInt32, Utf8)",
        }
    }
}

fn col(array: &ArrayRef) -> Option<Col<'_>> {
    Some(match array.data_type() {
        DataType::UInt8 => Col::U8(array.as_primitive_opt::<UInt8Type>()?),
        DataType::UInt16 => Col::U16(array.as_primitive_opt::<UInt16Type>()?),
        DataType::UInt32 => Col::U32(array.as_primitive_opt::<UInt32Type>()?),
        DataType::UInt64 => Col::U64(array.as_primitive_opt::<UInt64Type>()?),
        DataType::Int64 => Col::I64(array.as_primitive_opt::<Int64Type>()?),
        DataType::Float64 => Col::F64(array.as_primitive_opt::<Float64Type>()?),
        DataType::Utf8 => Col::Utf8(array.as_string_opt::<i32>()?),
        DataType::Dictionary(key, value)
            if **key == DataType::UInt32 && **value == DataType::Utf8 =>
        {
            let dict = array.as_dictionary_opt::<UInt32Type>()?;
            Col::Dict { keys: dict.keys(), values: dict.values().as_string_opt::<i32>()? }
        }
        _ => return None,
    })
}

fn kind(col: Col<'_>) -> Want {
    match col {
        Col::U8(_) | Col::U16(_) | Col::U32(_) | Col::U64(_) => Want::Id,
        Col::I64(_) => Want::I64,
        Col::F64(_) => Want::F64,
        Col::Utf8(_) | Col::Dict { .. } => Want::Text,
    }
}

/// One column of a batch, checked to hold what its reader wants.
#[derive(Clone, Copy)]
struct Column<'a> {
    message: &'a str,
    name: &'a str,
    col: Col<'a>,
}

impl<'a> Column<'a> {
    fn wrong(&self, row: usize, why: impl std::fmt::Display) -> Error {
        // alloc: an error's text
        Error::new(self.message, column(self.name), format!("row {row}: {why}"))
    }

    fn id(&self, row: usize) -> Result<Option<u64>, Error> {
        let it = match self.col {
            Col::U8(a) => a.is_valid(row).then(|| u64::from(a.value(row))),
            Col::U16(a) => a.is_valid(row).then(|| u64::from(a.value(row))),
            Col::U32(a) => a.is_valid(row).then(|| u64::from(a.value(row))),
            Col::U64(a) => a.is_valid(row).then(|| a.value(row)),
            _ => return Err(self.wrong(row, "read as an id, and it holds no ids")),
        };
        Ok(it)
    }

    fn i64(&self, row: usize) -> Result<Option<i64>, Error> {
        match self.col {
            Col::I64(a) => Ok(a.is_valid(row).then(|| a.value(row))),
            _ => Err(self.wrong(row, "read as Int64, and it holds none")),
        }
    }

    fn f64(&self, row: usize) -> Result<Option<f64>, Error> {
        match self.col {
            Col::F64(a) => Ok(a.is_valid(row).then(|| a.value(row))),
            _ => Err(self.wrong(row, "read as Float64, and it holds none")),
        }
    }

    fn text(&self, row: usize) -> Result<Option<&'a str>, Error> {
        match self.col {
            Col::Utf8(a) => Ok(a.is_valid(row).then(|| a.value(row))),
            Col::Dict { keys, values } => {
                if !keys.is_valid(row) {
                    return Ok(None);
                }
                let key = keys.value(row) as usize;
                if key >= values.len() {
                    return Err(self.wrong(
                        row,
                        format_args!(
                            "key {key} past the dictionary's {} values",
                            values.len()
                        ),
                    ));
                }
                Ok(values.is_valid(key).then(|| values.value(key)))
            }
            _ => Err(self.wrong(row, "read as text, and it holds none")),
        }
    }
}

/// A batch's columns by name, errors naming `message`.
struct Columns<'a> {
    batch: &'a RecordBatch,
    message: &'a str,
}

impl<'a> Columns<'a> {
    /// The first of `names` the batch has, checked to hold `want`; `None` when it has none.
    fn get(&self, names: &[&'a str], want: Want) -> Result<Option<Column<'a>>, Error> {
        let schema = self.batch.schema_ref();
        let Some((name, array)) = names.iter().find_map(|name| {
            let at = schema.index_of(name).ok()?;
            Some((*name, self.batch.column(at)))
        }) else {
            return Ok(None);
        };
        match col(array) {
            Some(col) if kind(col) == want => Ok(Some(Column { message: self.message, name, col })),
            _ => Err(Error::new(
                self.message,
                column(name),
                // alloc: an error's text
                format!("is {}, not {}", array.data_type(), want.says()),
            )),
        }
    }

    /// As [`Columns::get`], the column required.
    fn need(&self, names: &[&'a str], want: Want) -> Result<Column<'a>, Error> {
        if let Some(it) = self.get(names, want)? {
            return Ok(it);
        }
        // alloc: an error's text
        let has: Vec<&str> =
            self.batch.schema_ref().fields().iter().map(|it| it.name().as_str()).collect();
        Err(Error::new(
            self.message,
            column(names[0]),
            // alloc: an error's text
            format!("missing; the table has {}", has.join(", ")),
        ))
    }
}

/// A required column's value in `row`, an error when null.
fn needed<T>(column: &Column<'_>, row: usize, value: Option<T>) -> Result<T, Error> {
    value.ok_or_else(|| column.wrong(row, "null, and the column is required"))
}

fn opt_id(column: &Option<Column<'_>>, row: usize) -> Result<Option<u64>, Error> {
    column.as_ref().map_or(Ok(None), |it| it.id(row))
}

fn opt_i64(column: &Option<Column<'_>>, row: usize) -> Result<Option<i64>, Error> {
    column.as_ref().map_or(Ok(None), |it| it.i64(row))
}

fn opt_text(column: &Option<Column<'_>>, row: usize) -> Result<Option<String>, Error> {
    // alloc: a row's text, kept in the live state
    Ok(column.as_ref().map_or(Ok(None), |it| it.text(row))?.map(str::to_owned))
}

fn need_id(column: &Column<'_>, row: usize) -> Result<u64, Error> {
    needed(column, row, column.id(row)?)
}

fn need_i64(column: &Column<'_>, row: usize) -> Result<i64, Error> {
    needed(column, row, column.i64(row)?)
}

fn need_text(column: &Column<'_>, row: usize) -> Result<String, Error> {
    // alloc: a row's text, kept in the live state
    Ok(needed(column, row, column.text(row)?)?.to_owned())
}

/// A span a thread is inside (table `open`): one row a span of each thread's chain, outermost
/// first by `depth`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRow {
    pub process: u64,
    pub thread: u64,
    pub thread_name: String,
    /// 0 for the thread's outermost span.
    pub depth: u64,
    /// The span's id and its parent's, from a source that sends them (`span_id`, `parent`).
    pub span: Option<u64>,
    pub parent: Option<u64>,
    pub name: String,
    pub target: Option<String>,
    /// When the span began, on hello's clock.
    pub start_ns: i64,
    /// What the span is of: the crate (`serde v1.0.219 (lib)`), the function, the unit.
    pub krate: Option<String>,
    pub function: Option<String>,
    pub unit: Option<String>,
    /// What the span waits for, when it waits.
    pub awaits: Option<String>,
    /// The job or build the span works for.
    pub owner: Option<u64>,
    /// The task the span runs, from a source that names tasks (`task`).
    pub task: Option<u64>,
    /// When the daemon read the thread's chain.
    pub read_ns: Option<i64>,
}

/// A span that closed (table `spans`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanRow {
    pub process: u64,
    pub thread: u64,
    pub thread_name: Option<String>,
    pub span: u64,
    pub parent: Option<u64>,
    pub depth: u64,
    pub name: String,
    pub target: Option<String>,
    pub start_ns: i64,
    pub dur_ns: i64,
    pub krate: Option<String>,
    pub function: Option<String>,
    pub unit: Option<String>,
    pub build_id: Option<u64>,
}

/// A counter's value at a time (table `counters`).
#[derive(Clone, Debug, PartialEq)]
pub struct CounterRow {
    pub process: u64,
    pub name: String,
    pub t_ns: i64,
    pub value: f64,
    pub build_id: Option<u64>,
}

/// The rows of a batch of table `open`, appended to `into`.
pub fn open_rows(batch: &RecordBatch, message: &str, into: &mut Vec<OpenRow>) -> Result<(), Error> {
    let t = Columns { batch, message };
    let process = t.need(&["process"], Want::Id)?;
    let thread = t.need(&["thread"], Want::Id)?;
    let thread_name = t.need(&["thread_name"], Want::Text)?;
    let depth = t.need(&["depth"], Want::Id)?;
    let span = t.get(&["span_id", "span"], Want::Id)?;
    let parent = t.get(&["parent"], Want::Id)?;
    let name = t.need(&["name"], Want::Text)?;
    let target = t.get(&["target"], Want::Text)?;
    let start_ns = t.need(&["start_ns"], Want::I64)?;
    let krate = t.get(&["crate"], Want::Text)?;
    let function = t.get(&["function"], Want::Text)?;
    let unit = t.get(&["unit"], Want::Text)?;
    let awaits = t.get(&["awaits"], Want::Text)?;
    let owner = t.get(&["owner"], Want::Id)?;
    let task = t.get(&["task"], Want::Id)?;
    let read_ns = t.get(&["read_ns"], Want::I64)?;
    // alloc: a frame's rows, into the caller's list
    into.reserve(batch.num_rows());
    for row in 0..batch.num_rows() {
        into.push(OpenRow {
            process: need_id(&process, row)?,
            thread: need_id(&thread, row)?,
            thread_name: need_text(&thread_name, row)?,
            depth: need_id(&depth, row)?,
            span: opt_id(&span, row)?,
            parent: opt_id(&parent, row)?,
            name: need_text(&name, row)?,
            target: opt_text(&target, row)?,
            start_ns: need_i64(&start_ns, row)?,
            krate: opt_text(&krate, row)?,
            function: opt_text(&function, row)?,
            unit: opt_text(&unit, row)?,
            awaits: opt_text(&awaits, row)?,
            owner: opt_id(&owner, row)?,
            task: opt_id(&task, row)?,
            read_ns: opt_i64(&read_ns, row)?,
        });
    }
    Ok(())
}

/// The rows of a batch of table `spans`, appended to `into`.
pub fn span_rows(batch: &RecordBatch, message: &str, into: &mut Vec<SpanRow>) -> Result<(), Error> {
    let t = Columns { batch, message };
    let process = t.need(&["process"], Want::Id)?;
    let thread = t.need(&["thread"], Want::Id)?;
    let thread_name = t.get(&["thread_name"], Want::Text)?;
    let span = t.need(&["span_id", "span"], Want::Id)?;
    let parent = t.get(&["parent"], Want::Id)?;
    let depth = t.need(&["depth"], Want::Id)?;
    let name = t.need(&["name"], Want::Text)?;
    let target = t.get(&["target"], Want::Text)?;
    let start_ns = t.need(&["start_ns"], Want::I64)?;
    let dur_ns = t.need(&["dur_ns"], Want::I64)?;
    let krate = t.get(&["crate"], Want::Text)?;
    let function = t.get(&["function"], Want::Text)?;
    let unit = t.get(&["unit"], Want::Text)?;
    let build_id = t.get(&["build_id"], Want::Id)?;
    // alloc: a frame's rows, into the caller's list
    into.reserve(batch.num_rows());
    for row in 0..batch.num_rows() {
        into.push(SpanRow {
            process: need_id(&process, row)?,
            thread: need_id(&thread, row)?,
            thread_name: opt_text(&thread_name, row)?,
            span: need_id(&span, row)?,
            parent: opt_id(&parent, row)?,
            depth: need_id(&depth, row)?,
            name: need_text(&name, row)?,
            target: opt_text(&target, row)?,
            start_ns: need_i64(&start_ns, row)?,
            dur_ns: need_i64(&dur_ns, row)?,
            krate: opt_text(&krate, row)?,
            function: opt_text(&function, row)?,
            unit: opt_text(&unit, row)?,
            build_id: opt_id(&build_id, row)?,
        });
    }
    Ok(())
}

/// The rows of a batch of table `counters`, appended to `into`.
pub fn counter_rows(
    batch: &RecordBatch,
    message: &str,
    into: &mut Vec<CounterRow>,
) -> Result<(), Error> {
    let t = Columns { batch, message };
    let process = t.need(&["process"], Want::Id)?;
    let name = t.need(&["name"], Want::Text)?;
    let t_ns = t.need(&["t_ns"], Want::I64)?;
    let value = t.need(&["value"], Want::F64)?;
    let build_id = t.get(&["build_id"], Want::Id)?;
    // alloc: a frame's rows, into the caller's list
    into.reserve(batch.num_rows());
    for row in 0..batch.num_rows() {
        into.push(CounterRow {
            process: need_id(&process, row)?,
            name: need_text(&name, row)?,
            t_ns: need_i64(&t_ns, row)?,
            value: needed(&value, row, value.f64(row)?)?,
            build_id: opt_id(&build_id, row)?,
        });
    }
    Ok(())
}

/// A task as the source says it (table `tasks`, upserted by process and task, whole rows).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRow {
    pub process: u64,
    pub task: u64,
    /// Its layer's label (the daemon's `NodeKind` name, `download`); `task` when none.
    pub kind: String,
    pub name: String,
    pub state: State,
    /// How a settled task settled, in the source's word (`built`, `cached`, `failed`).
    pub outcome: Option<String>,
    pub error: Option<String>,
    /// When it began running, or was first said; on hello's clock.
    pub start_ns: Option<i64>,
    pub end_ns: Option<i64>,
    pub progress_done: Option<u64>,
    pub progress_total: Option<u64>,
    pub progress_unit: Option<String>,
    pub waiting_on: Option<u64>,
    pub krate: Option<String>,
    pub unit: Option<String>,
}

/// A task's need (table `task_edges`): `from` cannot finish without `to`, both of `process`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeRow {
    pub process: u64,
    pub from: u64,
    pub to: u64,
}

/// A task's state from its word: one of [`State::word`]'s, or `settled`, which is the
/// `outcome`'s (`failed`, `cancelled`, else done).
fn state_of(word: &str, outcome: Option<&str>) -> Option<State> {
    match word {
        "settled" => Some(match outcome {
            Some("failed") => State::Failed,
            Some("cancelled") => State::Cancelled,
            _ => State::Done,
        }),
        _ => State::from_word(word),
    }
}

/// The rows of a batch of table `tasks`, appended to `into`.
pub fn task_rows(batch: &RecordBatch, message: &str, into: &mut Vec<TaskRow>) -> Result<(), Error> {
    let t = Columns { batch, message };
    let process = t.need(&["process"], Want::Id)?;
    let task = t.need(&["task"], Want::Id)?;
    let kind = t.get(&["kind"], Want::Text)?;
    let name = t.need(&["name"], Want::Text)?;
    let state = t.need(&["state"], Want::Text)?;
    let outcome = t.get(&["outcome"], Want::Text)?;
    let error = t.get(&["error"], Want::Text)?;
    let start_ns = t.get(&["start_ns"], Want::I64)?;
    let end_ns = t.get(&["end_ns"], Want::I64)?;
    let progress_done = t.get(&["progress_done"], Want::Id)?;
    let progress_total = t.get(&["progress_total"], Want::Id)?;
    let progress_unit = t.get(&["progress_unit"], Want::Text)?;
    let waiting_on = t.get(&["waiting_on", "waiting_on_task"], Want::Id)?;
    let krate = t.get(&["crate"], Want::Text)?;
    let unit = t.get(&["unit"], Want::Text)?;
    // alloc: a frame's rows, into the caller's list
    into.reserve(batch.num_rows());
    for row in 0..batch.num_rows() {
        let outcome = opt_text(&outcome, row)?;
        let word = needed(&state, row, state.text(row)?)?;
        let Some(state_now) = state_of(word, outcome.as_deref()) else {
            return Err(state.wrong(
                row,
                // alloc: an error's text
                format!(
                    "`{word}`, not a task's state (waiting, ready, running, done, failed, cancelled, settled)"
                ),
            ));
        };
        into.push(TaskRow {
            process: need_id(&process, row)?,
            task: need_id(&task, row)?,
            // alloc: a row's text, kept in the live state
            kind: opt_text(&kind, row)?.unwrap_or_else(|| "task".to_owned()),
            name: need_text(&name, row)?,
            state: state_now,
            outcome,
            error: opt_text(&error, row)?,
            start_ns: opt_i64(&start_ns, row)?,
            end_ns: opt_i64(&end_ns, row)?,
            progress_done: opt_id(&progress_done, row)?,
            progress_total: opt_id(&progress_total, row)?,
            progress_unit: opt_text(&progress_unit, row)?,
            waiting_on: opt_id(&waiting_on, row)?,
            krate: opt_text(&krate, row)?,
            unit: opt_text(&unit, row)?,
        });
    }
    Ok(())
}

/// The rows of a batch of table `task_edges`, appended to `into`.
pub fn edge_rows(batch: &RecordBatch, message: &str, into: &mut Vec<EdgeRow>) -> Result<(), Error> {
    let t = Columns { batch, message };
    let process = t.need(&["process"], Want::Id)?;
    let from = t.need(&["from", "task"], Want::Id)?;
    let to = t.need(&["to", "needs"], Want::Id)?;
    // alloc: a frame's rows, into the caller's list
    into.reserve(batch.num_rows());
    for row in 0..batch.num_rows() {
        into.push(EdgeRow {
            process: need_id(&process, row)?,
            from: need_id(&from, row)?,
            to: need_id(&to, row)?,
        });
    }
    Ok(())
}
