//! The live feed: what `blaze debug live` and the explorer's live stream show. Each subscriber
//! has a [`Feed`] of its own on the daemon, which every [`EVERY`] takes what the rings got
//! since its last frame ([`flight::read`]) and the spans each thread is inside now, as one
//! [`Frame`]. A subscriber slower than the writers skips what was written over, counted in
//! [`Frame::skipped`]; its memory stays one frame and its streams' dictionaries, whatever it
//! falls behind.
//!
//! Each table (`spans`, `counters`, `open`) is one long-lived Arrow IPC stream per subscriber
//! ([`Encoder`]): its schema and each dictionary's first values cross once, at the first frame
//! with rows, and each later frame with rows carries only that frame's record batch, preceded
//! by a delta of the dictionary values it names for the first time, so a span's name, target,
//! crate and thread cross once. A frame's bytes of a table are empty exactly when it has no
//! rows, and each frame's bytes are whole IPC messages: a client must take every non-empty
//! frame of a table, in order. A stream's dictionaries are bounded ([`Cap`]): once they hold
//! more, the next frame with rows ends the stream (the IPC end-of-stream marker, [`END`]) and
//! starts a new one, schema and fresh dictionaries, in the same bytes. Same rows in, same bytes
//! out.
//!
//! The client keeps a [`Decoder`] per table, which follows its stream across frames and
//! resets, and folds the batches into [`Top`], which renders the top-like view: running spans per
//! thread, the longest running, rates, the memo's hit rate and a warning when nothing closes
//! while threads are inside spans.

use std::{fmt::Write as _, sync::Arc, time::Duration};

use arrow_array::{
    Array, ArrayRef, DictionaryArray, RecordBatch, UInt32Array, builder::StringBuilder,
    cast::AsArray, types::UInt32Type,
};
use arrow_buffer::Buffer;
use arrow_ipc::{
    CompressionType,
    reader::{StreamDecoder, StreamReader},
    writer::{DictionaryHandling, IpcWriteOptions, StreamEncoder, StreamWriter},
};
use arrow_schema::{ArrowError, DataType, SchemaRef};
use blaze_hash::HashMap;
use std::alloc::Global;

use crate::{
    batches::{self, Process},
    flight::{self, Cursor, Plot, Read},
    schema::{self, Table},
};

/// How often a subscriber is sent a frame.
pub const EVERY: Duration = Duration::from_millis(250);

/// How long nothing may close while threads are inside spans before [`Top`] warns: well
/// under the stall watchdog's bound ([`crate::Watchdog::BOUND`]), so the warning comes first.
pub const QUIET_WARN: Duration = Duration::from_secs(10);

/// The Arrow IPC end-of-stream marker (continuation, then a length of 0): what a frame's bytes
/// of a table start with when its stream ends there and a new one begins.
pub const END: [u8; 8] = [0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0];

/// One frame of the feed: each table the next bytes of its long-lived Arrow IPC stream
/// ([`Encoder`]), empty when it has no rows.
#[derive(Default, Debug)]
pub struct Frame {
    /// When it was read, on the schema's clock.
    pub at_ns: i64,
    /// Rows written over before this subscriber read them, since its last frame.
    pub skipped: u64,
    /// Spans not recorded since the process began ([`flight::Read::lost`]).
    pub lost: u64,
    pub spans: Vec<u8>,
    pub counters: Vec<u8>,
    pub open: Vec<u8>,
    /// Rows of each table, so a reader can skip an empty one without decoding it. A table's
    /// bytes are empty exactly when its rows are 0.
    pub spans_rows: usize,
    pub counters_rows: usize,
    pub open_rows: usize,
}

/// A subscriber's place in the rings, and its stream of each table.
pub struct Feed {
    cursor: Cursor,
    read: Read,
    spans: Encoder,
    counters: Encoder,
    open: Encoder,
}

impl Default for Feed {
    fn default() -> Feed {
        Feed::new()
    }
}

impl Feed {
    /// A feed whose first frame has everything the rings still hold.
    pub fn new() -> Feed {
        Feed {
            cursor: Cursor::default(),
            read: Read::default(),
            spans: Encoder::new(&schema::SPANS),
            counters: Encoder::new(&schema::COUNTERS),
            open: Encoder::new(&schema::OPEN),
        }
    }

    /// The next frame, into `frame`, its buffers reused. On an error the frame's tables are
    /// empty, and a stream the error left mid-way ends at the next frame with rows.
    pub fn next(&mut self, frame: &mut Frame) -> Result<(), ArrowError> {
        frame.spans.clear();
        frame.counters.clear();
        frame.open.clear();
        (frame.spans_rows, frame.counters_rows, frame.open_rows) = (0, 0, 0);
        self.read.clear();
        flight::read(&mut self.cursor, &mut self.read);
        let process = Process::this();
        let [spans, counters] = batches::of(&self.read, process)?;
        let open = batches::open(&flight::threads_now(), self.read.at_ns, process)?;
        frame.at_ns = self.read.at_ns;
        frame.skipped = self.read.skipped;
        frame.lost = self.read.lost;
        // A table that failed took itself back; those encoded before it are taken back too, as
        // the frame is not sent.
        if let Err(why) = self.spans.encode(&spans, &mut frame.spans) {
            return Err(failed(frame, why));
        }
        if let Err(why) = self.counters.encode(&counters, &mut frame.counters) {
            self.spans.undo();
            return Err(failed(frame, why));
        }
        if let Err(why) = self.open.encode(&open, &mut frame.open) {
            self.spans.undo();
            self.counters.undo();
            return Err(failed(frame, why));
        }
        (frame.spans_rows, frame.counters_rows, frame.open_rows) =
            (spans.num_rows(), counters.num_rows(), open.num_rows());
        Ok(())
    }
}

/// `why`, with `frame`'s tables emptied: a frame that failed sends none of them.
fn failed(frame: &mut Frame, why: ArrowError) -> ArrowError {
    frame.spans.clear();
    frame.counters.clear();
    frame.open.clear();
    why
}

/// How much a stream's dictionaries may hold, over all its dictionary columns, before the
/// next frame with rows starts a new stream. A stream may pass it by one batch's new values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap {
    /// Distinct values.
    pub values: usize,
    /// Bytes of the values' text.
    pub bytes: usize,
}

impl Cap {
    /// A subscriber's bound per table: far over what a daemon's span sites, targets, crates and
    /// threads name, and small next to the frame's rows the subscriber may hold.
    pub const LIVE: Cap = Cap { values: 1 << 16, bytes: 1 << 20 };

    fn full(&self, dicts: &[(usize, Dict)]) -> bool {
        let values: usize = dicts.iter().map(|(_, it)| it.keys_of.len()).sum();
        let bytes: usize = dicts.iter().map(|(_, it)| it.bytes).sum();
        values >= self.values || bytes >= self.bytes
    }
}

/// One table's long-lived Arrow IPC stream, the daemon's side: zstd-compressed, dictionaries
/// sent as deltas. Each dictionary column is re-keyed onto the stream's own dictionary, whose
/// values only grow (in the order rows first name them), so a value crosses once a stream.
pub struct Encoder {
    table: &'static Table,
    cap: Cap,
    stream: Option<Stream>,
    /// Whether bytes of a stream were handed out: the next stream begins with [`END`].
    begun: bool,
    /// `begun` before the last [`Encoder::encode`], for [`Encoder::undo`].
    begun_before: bool,
    /// Streams begun, a reset each after the first.
    streams: u64,
}

/// A stream under way.
struct Stream {
    schema: SchemaRef,
    encoder: StreamEncoder,
    /// Each dictionary column's index and the stream's dictionary of it.
    dicts: Vec<(usize, Dict)>,
}

/// The stream's dictionary of one column: its values in the order first named.
struct Dict {
    keys_of: HashMap<Box<str>, u32, Global>,
    values: StringBuilder,
    /// `values` as an array: the same one until a value is added, so the IPC writer sees an
    /// unchanged dictionary without comparing it.
    array: ArrayRef,
    bytes: usize,
}

/// A batch's key of a value it never named.
const UNSEEN: u32 = u32::MAX;

impl Dict {
    fn new() -> Dict {
        // alloc: a dictionary's values, once a column a stream
        let mut values = StringBuilder::new();
        // alloc: a dictionary's (empty) values, once a column a stream
        let array: ArrayRef = Arc::new(values.finish_cloned());
        Dict { keys_of: HashMap::new_in(Global), values, array, bytes: 0 }
    }

    /// `column`, a dictionary column of `table`, keyed onto this dictionary, adding the values
    /// it names first.
    fn keyed(
        &mut self,
        column: &ArrayRef,
        table: &'static Table,
        name: &str,
    ) -> Result<ArrayRef, ArrowError> {
        let wrong = || {
            // alloc: an error, once
            ArrowError::SchemaError(format!(
                "the live `{}` column `{name}` is not a dictionary of strings with u32 keys",
                table.name
            ))
        };
        let dict = column.as_dictionary_opt::<UInt32Type>().ok_or_else(wrong)?;
        let values = dict.values().as_string_opt::<i32>().ok_or_else(wrong)?;
        let before = self.keys_of.len();
        // alloc: the batch's keys to the stream's, once a column a frame
        let mut ours = vec![UNSEEN; values.len()];
        let (keys_of, builder, bytes) = (&mut self.keys_of, &mut self.values, &mut self.bytes);
        let keys: UInt32Array = dict
            .keys()
            .iter()
            .map(|key| {
                let key = key? as usize;
                if values.is_null(key) {
                    return None;
                }
                if ours[key] == UNSEEN {
                    let value = values.value(key);
                    ours[key] = match keys_of.get(value) {
                        Some(it) => *it,
                        None => {
                            let next = keys_of.len() as u32;
                            // alloc: once per distinct value a stream keeps, bounded by its `Cap`
                            keys_of.insert(value.into(), next);
                            builder.append_value(value);
                            *bytes += value.len();
                            next
                        }
                    };
                }
                Some(ours[key])
            })
            // alloc: the column's keys, once a column a frame
            .collect();
        if self.keys_of.len() > before {
            // alloc: the dictionary's values anew, once a frame that names a new one
            self.array = Arc::new(self.values.finish_cloned());
        }
        // clone: an `Arc` of the dictionary's values
        let keyed = DictionaryArray::<UInt32Type>::try_new(keys, self.array.clone())?;
        // alloc: a column, once a frame
        Ok(Arc::new(keyed))
    }
}

/// Whether `ty` holds a dictionary anywhere below its top.
fn nested_dictionary(ty: &DataType) -> bool {
    match ty {
        DataType::Dictionary(..) => true,
        DataType::List(it)
        | DataType::LargeList(it)
        | DataType::FixedSizeList(it, _)
        | DataType::Map(it, _) => nested_dictionary(it.data_type()),
        DataType::Struct(fields) => fields.iter().any(|it| nested_dictionary(it.data_type())),
        _ => false,
    }
}

impl Stream {
    fn new(table: &'static Table, schema: &SchemaRef) -> Result<Stream, ArrowError> {
        let options = IpcWriteOptions::default()
            .try_with_compression(Some(CompressionType::ZSTD))?
            .with_dictionary_handling(DictionaryHandling::Delta);
        // alloc: a stream's dictionaries, once a stream
        let mut dicts = Vec::new();
        for (at, field) in schema.fields().iter().enumerate() {
            match field.data_type() {
                DataType::Dictionary(key, value)
                    if **key == DataType::UInt32 && **value == DataType::Utf8 =>
                {
                    dicts.push((at, Dict::new()));
                }
                ty if nested_dictionary(ty) => {
                    // alloc: an error, once
                    return Err(ArrowError::SchemaError(format!(
                        "the live `{}` column `{}` is {ty}: the live stream re-keys only top-level dictionaries of strings with u32 keys",
                        table.name,
                        field.name()
                    )));
                }
                _ => {}
            }
        }
        let encoder = StreamEncoder::try_new_with_options(schema, options)?;
        // clone: an `Arc` of the table's schema
        Ok(Stream { schema: schema.clone(), encoder, dicts })
    }

    /// `batch` with each dictionary column keyed onto the stream's dictionaries, encoded into
    /// `into`.
    fn encode(
        &mut self,
        table: &'static Table,
        batch: &RecordBatch,
        into: &mut Vec<u8>,
    ) -> Result<(), ArrowError> {
        if *batch.schema_ref() != self.schema {
            // alloc: an error, once
            return Err(ArrowError::SchemaError(format!(
                "a live `{}` batch's schema is not its stream's: {:?}",
                table.name,
                batch.schema_ref()
            )));
        }
        // alloc: a batch's list of columns (each an `Arc`), once a frame
        let mut columns: Vec<ArrayRef> = batch.columns().to_vec();
        for (at, dict) in &mut self.dicts {
            columns[*at] = dict.keyed(&columns[*at], table, self.schema.field(*at).name())?;
        }
        // clone: an `Arc` of the table's schema
        let keyed = RecordBatch::try_new(self.schema.clone(), columns)?;
        // alloc: the IPC writer's buffers of a frame
        for buffer in self.encoder.encode(&keyed)? {
            into.extend_from_slice(&buffer);
        }
        Ok(())
    }
}

impl Encoder {
    /// `table`'s stream, bounded by [`Cap::LIVE`].
    pub fn new(table: &'static Table) -> Encoder {
        Encoder::with_cap(table, Cap::LIVE)
    }

    /// `table`'s stream, bounded by `cap`.
    pub fn with_cap(table: &'static Table, cap: Cap) -> Encoder {
        Encoder { table, cap, stream: None, begun: false, begun_before: false, streams: 0 }
    }

    /// Streams begun so far: one, plus one a reset.
    pub fn streams(&self) -> u64 {
        self.streams
    }

    /// The next bytes of the stream for `batch`, into `into` (cleared first): nothing when it
    /// has no rows; else, at a stream's start, [`END`] (when a stream came before) and the
    /// schema, then the dictionaries' new values and the batch. On an error `into` is empty
    /// and the stream ends, so the next batch starts a new one.
    pub fn encode(&mut self, batch: &RecordBatch, into: &mut Vec<u8>) -> Result<(), ArrowError> {
        into.clear();
        self.begun_before = self.begun;
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let cap = self.cap;
        let stream = match self.stream.take().filter(|it| !cap.full(&it.dicts)) {
            Some(it) => it,
            None => {
                if self.begun {
                    into.extend_from_slice(&END);
                }
                let stream = Stream::new(self.table, batch.schema_ref());
                let stream = stream.inspect_err(|_| into.clear())?;
                self.streams += 1;
                stream
            }
        };
        let stream = self.stream.insert(stream);
        match stream.encode(self.table, batch, into) {
            Ok(()) => {
                self.begun = true;
                Ok(())
            }
            Err(why) => {
                // What the encoder and dictionaries took of the batch never reaches the client.
                self.stream = None;
                into.clear();
                Err(why)
            }
        }
    }

    /// Takes back the last [`Encoder::encode`], whose bytes the client will not get: the
    /// stream ends, and the next batch starts a new one, after [`END`] only when the client got
    /// bytes of an earlier stream.
    fn undo(&mut self) {
        self.stream = None;
        self.begun = self.begun_before;
    }
}

/// One table's long-lived Arrow IPC stream, the client's side: each frame's bytes decoded into
/// batches, the dictionaries kept across frames, and a stream's end ([`END`]) starting a new
/// one.
pub struct Decoder {
    table: &'static Table,
    stream: Option<StreamDecoder>,
    /// Streams begun so far, so an error names which.
    streams: u64,
}

impl Decoder {
    pub fn new(table: &'static Table) -> Decoder {
        Decoder { table, stream: None, streams: 0 }
    }

    /// Streams begun so far: one, plus one a reset.
    pub fn streams(&self) -> u64 {
        self.streams
    }

    fn error(&self, what: impl std::fmt::Display) -> ArrowError {
        // alloc: an error, once
        ArrowError::IpcError(format!(
            "the live `{}` stream (stream {}): {what}",
            self.table.name, self.streams
        ))
    }

    /// The batches of a frame's bytes of this table, into `into` (cleared first). The bytes
    /// must be whole messages, the frames' in order; empty bytes are a frame with no rows.
    pub fn decode(&mut self, bytes: &[u8], into: &mut Vec<RecordBatch>) -> Result<(), ArrowError> {
        into.clear();
        let mut rest = bytes;
        if let Some(after) = rest.strip_prefix(&END) {
            let Some(mut old) = self.stream.take() else {
                return Err(self.error("a frame ends a stream before any began"));
            };
            let mut end = Buffer::from_slice_ref(END);
            let ended = old.decode(&mut end).and_then(|batch| match batch {
                None => old.finish(),
                // alloc: an error, once
                Some(_) => Err(ArrowError::IpcError("a batch where the stream ends".to_owned())),
            });
            ended.map_err(|why| self.error(format_args!("its end: {why}")))?;
            rest = after;
        }
        if rest.is_empty() {
            return Ok(());
        }
        if self.stream.is_none() {
            self.streams += 1;
        }
        let stream = self.stream.get_or_insert_with(StreamDecoder::new);
        // alloc: a frame's bytes, copied aligned for the decoder, on the client once a frame
        let mut buffer = Buffer::from_slice_ref(rest);
        let mut decoded = Ok(());
        while !buffer.is_empty() {
            match stream.decode(&mut buffer) {
                // alloc: a frame's batches, on the client, into the caller's list
                Ok(Some(batch)) => into.push(batch),
                Ok(None) if buffer.is_empty() => {}
                Ok(None) => {
                    // alloc: an error, once
                    decoded = Err(ArrowError::IpcError(
                        "the decoder took no bytes and gave no batch".to_owned(),
                    ));
                    break;
                }
                Err(why) => {
                    decoded = Err(why);
                    break;
                }
            }
        }
        // A frame is whole messages: the next frame starts a message.
        let decoded = decoded.and_then(|()| stream.finish());
        decoded.map_err(|why| self.error(why))
    }
}

/// `batch` as one whole Arrow IPC stream, zstd-compressed, into `into` (cleared first): what
/// [`decode`] reads (a client's rows, `Request::ClientRows`), not the live feed's stream
/// ([`Encoder`]).
pub fn encode(batch: &RecordBatch, into: &mut Vec<u8>) -> Result<(), ArrowError> {
    into.clear();
    let options = IpcWriteOptions::default().try_with_compression(Some(CompressionType::ZSTD))?;
    let mut writer = StreamWriter::try_new_with_options(&mut *into, batch.schema_ref(), options)?;
    writer.write(batch)?;
    writer.finish()
}

/// The batches of one whole Arrow IPC stream (an answer: `views`; a client's rows, [`encode`]),
/// not the live feed's, which is one stream across frames ([`Decoder`]).
pub fn decode(bytes: &[u8]) -> Result<Vec<RecordBatch>, ArrowError> {
    // alloc: an answer's batches, on the client, once an answer
    StreamReader::try_new(bytes, None)?.collect()
}

/// A frame the client could not read: a table that is not the schema's, or bytes that are not
/// Arrow IPC.
#[derive(Debug)]
pub enum TopError {
    Arrow(ArrowError),
    /// The batch has no column `column` of the right type for table `table`.
    Column {
        table: &'static str,
        column: &'static str,
    },
}

impl std::fmt::Display for TopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TopError::Arrow(why) => write!(f, "the live frame is not Arrow IPC: {why}"),
            TopError::Column { table, column } => {
                write!(f, "the live frame's `{table}` has no column `{column}` of its type")
            }
        }
    }
}

impl std::error::Error for TopError {}

impl From<ArrowError> for TopError {
    fn from(why: ArrowError) -> TopError {
        TopError::Arrow(why)
    }
}

fn column<'a>(
    batch: &'a RecordBatch,
    table: &'static Table,
    name: &'static str,
) -> Result<&'a ArrayRef, TopError> {
    let missing = || TopError::Column { table: table.name, column: name };
    let at = table.column(name).ok_or_else(missing)?;
    batch.columns().get(at).filter(|_| batch.schema().field(at).name() == name).ok_or_else(missing)
}

/// The strings of a dictionary column, one a row (`None` where null).
fn strings<'a>(
    batch: &'a RecordBatch,
    table: &'static Table,
    name: &'static str,
) -> Result<impl Iterator<Item = Option<&'a str>> + 'a, TopError> {
    let array = column(batch, table, name)?;
    let dict = array
        .as_dictionary_opt::<UInt32Type>()
        .ok_or(TopError::Column { table: table.name, column: name })?;
    let values = dict
        .values()
        .as_string_opt::<i32>()
        .ok_or(TopError::Column { table: table.name, column: name })?;
    let keys = dict.keys();
    Ok((0..keys.len())
        .map(move |row| keys.is_valid(row).then(|| values.value(keys.value(row) as usize))))
}

/// The strings of a string column, one a row (`None` where null).
fn utf8s<'a>(
    batch: &'a RecordBatch,
    table: &'static Table,
    name: &'static str,
) -> Result<impl Iterator<Item = Option<&'a str>> + 'a, TopError> {
    let array = column(batch, table, name)?
        .as_string_opt::<i32>()
        .ok_or(TopError::Column { table: table.name, column: name })?;
    Ok(array.iter())
}

fn primitive<'a, T: arrow_array::ArrowPrimitiveType>(
    batch: &'a RecordBatch,
    table: &'static Table,
    name: &'static str,
) -> Result<&'a arrow_array::PrimitiveArray<T>, TopError> {
    column(batch, table, name)?
        .as_primitive_opt::<T>()
        .ok_or(TopError::Column { table: table.name, column: name })
}

/// A span a thread is inside, as the last frame showed it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Running {
    name: String,
    /// What it is of: ` `function` of crate`, each part only when it has it.
    of: String,
    start_ns: i64,
}

#[derive(Default, Debug)]
struct Thread {
    name: String,
    open: Vec<Running>,
    /// Spans it closed in the last frame.
    closed: u64,
}

impl Thread {
    /// What the `i`th span it is inside is of, as the view writes it. A pool `job`'s is the type
    /// of the closure it runs, hundreds of characters of `Scope::submit_as<..>` that would fill
    /// the line ahead of the spans inside it, which name the work: it is shown only when no
    /// span is inside it.
    fn of(&self, i: usize) -> &str {
        let span = &self.open[i];
        match span.name == "job" && i + 1 < self.open.len() {
            true => "",
            false => &span.of,
        }
    }
}

/// The client's view of the feed, folded from each frame.
#[derive(Default, Debug)]
pub struct Top {
    threads: HashMap<u32, Thread, Global>,
    at_ns: i64,
    /// Spans closed a second, over the last frame.
    rate: f64,
    /// The latest values of the memo's plots.
    hits: Option<f64>,
    misses: Option<f64>,
    /// When a span last closed, while any thread was inside one.
    last_close_ns: Option<i64>,
    skipped: u64,
    lost: u64,
}

impl Top {
    /// Folds in a frame: its header's `at_ns`, `skipped` and `lost`, and its decoded tables.
    pub fn take(
        &mut self,
        at_ns: i64,
        skipped: u64,
        lost: u64,
        spans: &[RecordBatch],
        counters: &[RecordBatch],
        open: &[RecordBatch],
    ) -> Result<(), TopError> {
        let since = at_ns - self.at_ns;
        for thread in self.threads.values_mut() {
            thread.open.clear();
            thread.closed = 0;
        }
        let mut closed = 0u64;
        for batch in spans {
            let threads =
                primitive::<arrow_array::types::UInt32Type>(batch, &schema::SPANS, "thread")?;
            let names = strings(batch, &schema::SPANS, "thread_name")?;
            for (tid, name) in threads.values().iter().zip(names) {
                let thread = self.threads.entry(*tid).or_default();
                if thread.name.is_empty() {
                    // alloc: a thread's name, once a thread the view shows
                    thread.name = name.unwrap_or("?").to_owned();
                }
                thread.closed += 1;
                closed += 1;
            }
        }
        for batch in counters {
            let names = strings(batch, &schema::COUNTERS, "name")?;
            let values =
                primitive::<arrow_array::types::Float64Type>(batch, &schema::COUNTERS, "value")?;
            for (name, value) in names.zip(values.values().iter()) {
                match name {
                    Some(it) if it == Plot::MemoHits.name() => self.hits = Some(*value),
                    Some(it) if it == Plot::MemoMisses.name() => self.misses = Some(*value),
                    _ => {}
                }
            }
        }
        let mut inside = false;
        for batch in open {
            let tids = primitive::<arrow_array::types::UInt32Type>(batch, &schema::OPEN, "thread")?;
            let thread_names = strings(batch, &schema::OPEN, "thread_name")?;
            let names = strings(batch, &schema::OPEN, "name")?;
            let starts =
                primitive::<arrow_array::types::Int64Type>(batch, &schema::OPEN, "start_ns")?;
            let functions = utf8s(batch, &schema::OPEN, "function")?;
            let crates = strings(batch, &schema::OPEN, "crate")?;
            for (((((tid, thread_name), name), start_ns), function), krate) in tids
                .values()
                .iter()
                .zip(thread_names)
                .zip(names)
                .zip(starts.values().iter())
                .zip(functions)
                .zip(crates)
            {
                let thread = self.threads.entry(*tid).or_default();
                if thread.name.is_empty() {
                    // alloc: a thread's name, once a thread the view shows
                    thread.name = thread_name.unwrap_or("?").to_owned();
                }
                // alloc: the view's copy of a running span, once a frame
                let mut of = String::new();
                if let Some(function) = function {
                    _ = write!(of, " `{function}`");
                }
                if let Some(krate) = krate {
                    _ = write!(of, " of {krate}");
                }
                // alloc: the view's copy of a running span, once a frame
                thread.open.push(Running {
                    name: name.unwrap_or("?").to_owned(),
                    of,
                    start_ns: *start_ns,
                });
                inside = true;
            }
        }
        self.threads.retain(|_, it| !it.open.is_empty() || it.closed > 0);
        self.rate =
            if self.at_ns != 0 && since > 0 { closed as f64 * 1e9 / since as f64 } else { 0.0 };
        self.last_close_ns = match (inside, closed) {
            (false, _) => None,
            (true, 0) => self.last_close_ns.or(Some(at_ns)),
            (true, _) => Some(at_ns),
        };
        self.at_ns = at_ns;
        self.skipped += skipped;
        self.lost = lost;
        Ok(())
    }

    /// How long nothing has closed while threads were inside spans, when that is past
    /// [`QUIET_WARN`].
    pub fn quiet(&self) -> Option<Duration> {
        let quiet = Duration::from_nanos((self.at_ns - self.last_close_ns?).max(0) as u64);
        (quiet >= QUIET_WARN).then_some(quiet)
    }

    /// The view, `width` columns wide, into `out` (cleared first).
    pub fn render(&self, width: usize, out: &mut String) {
        out.clear();
        let secs = |ns: i64| ns.max(0) as f64 / 1e9;
        _ = write!(out, "blaze live: {:.0} spans/s", self.rate);
        match (self.hits, self.misses) {
            (Some(hits), Some(misses)) if hits + misses > 0.0 => {
                _ = write!(
                    out,
                    ", memo hit rate {:.1}% ({hits} hits, {misses} misses)",
                    hits * 100.0 / (hits + misses)
                );
            }
            _ => out.push_str(", no build ended yet"),
        }
        if self.skipped > 0 {
            _ = write!(out, ", {} rows skipped (the view fell behind)", self.skipped);
        }
        if self.lost > 0 {
            _ = write!(out, ", {} spans not recorded", self.lost);
        }
        out.push('\n');
        if let Some(quiet) = self.quiet() {
            let inside = self.threads.values().filter(|it| !it.open.is_empty()).count();
            _ = writeln!(
                out,
                "warning: no span closed for {:.1} s while {inside} threads are inside spans",
                quiet.as_secs_f64()
            );
        }
        // alloc: the view's rows, once a frame
        let mut threads: Vec<(&u32, &Thread)> = self.threads.iter().collect();
        threads
            .sort_by_key(|(tid, it)| (it.open.first().map_or(i64::MAX, |it| it.start_ns), **tid));
        _ = writeln!(out, "{:<24} {:>9} {:>6}  running", "thread", "for", "closed");
        for (tid, thread) in &threads {
            let line_start = out.len();
            let name = if thread.name.is_empty() { "?" } else { thread.name.as_str() };
            let running_for = thread.open.first().map_or(0, |it| self.at_ns - it.start_ns);
            _ = write!(
                out,
                "{:<24} {:>8.1}s {:>6}  ",
                truncated(name, 24),
                secs(running_for),
                thread.closed
            );
            for (i, span) in thread.open.iter().enumerate() {
                out.push_str(if i == 0 { "" } else { " > " });
                out.push_str(&span.name);
                out.push_str(thread.of(i));
            }
            if thread.open.is_empty() {
                _ = write!(out, "(idle, tid {tid})");
            }
            cut(out, line_start, width);
            out.push('\n');
        }
        // alloc: the view's rows, once a frame
        let mut longest: Vec<(&Running, &str, &str)> = threads
            .iter()
            .flat_map(|&(_, thread)| {
                (thread.open.iter().enumerate())
                    .map(move |(i, it)| (it, thread.of(i), thread.name.as_str()))
            })
            // alloc: the view's rows, once a frame
            .collect();
        longest.sort_by_key(|(it, ..)| it.start_ns);
        if !longest.is_empty() {
            out.push_str("longest running\n");
        }
        for (span, of, thread) in longest.iter().take(10) {
            let line_start = out.len();
            _ = write!(
                out,
                "{:>8.1}s  {}{of}  ({thread})",
                secs(self.at_ns - span.start_ns),
                span.name,
            );
            cut(out, line_start, width);
            out.push('\n');
        }
    }
}

fn truncated(text: &str, width: usize) -> &str {
    match text.char_indices().nth(width) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

/// Cuts the line of `out` from `start` to `width` characters.
fn cut(out: &mut String, start: usize, width: usize) {
    if let Some((at, _)) = out[start..].char_indices().nth(width) {
        out.truncate(start + at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flight::{CounterRow, OpenSpan, ThreadNow};

    /// `batch` through a stream of its own: what a subscriber's first frame of `table` decodes to.
    fn ipc_of(table: &'static Table, batch: &RecordBatch) -> Vec<RecordBatch> {
        let mut bytes = Vec::new();
        Encoder::new(table).encode(batch, &mut bytes).unwrap();
        let mut back = Vec::new();
        Decoder::new(table).decode(&bytes, &mut back).unwrap();
        back
    }

    /// A thread inside `build` for `for_ns`, and inside a query under it for half that.
    fn worker(for_ns: u64) -> ThreadNow {
        ThreadNow {
            tid: 41,
            name: "worker 3".to_owned(),
            open: vec![
                OpenSpan { name: "build", target: "blaze", for_ns, ..OpenSpan::default() },
                OpenSpan {
                    name: "typeck_query",
                    target: "hir_ty",
                    for_ns: for_ns / 2,
                    krate: Some("app".into()),
                    function: Some("app::main".into()),
                    ..OpenSpan::default()
                },
            ],
            deeper: 0,
        }
    }

    /// Another thread, inside `build` and a query no other thread names.
    fn other(for_ns: u64) -> ThreadNow {
        ThreadNow {
            tid: 42,
            name: "worker 4".to_owned(),
            open: vec![
                OpenSpan { name: "build", target: "blaze", for_ns, ..OpenSpan::default() },
                OpenSpan {
                    name: "infer_query",
                    target: "hir_ty",
                    for_ns: for_ns / 3,
                    krate: Some("app".into()),
                    ..OpenSpan::default()
                },
            ],
            deeper: 0,
        }
    }

    /// The strings of a dictionary column, one a row.
    fn resolved(column: &ArrayRef) -> Vec<Option<String>> {
        let dict = column.as_dictionary::<UInt32Type>();
        let values = dict.values().as_string::<i32>();
        dict.keys().iter().map(|key| key.map(|key| values.value(key as usize).to_owned())).collect()
    }

    /// `back` has `sent`'s rows: the same values in each column, dictionary columns by their
    /// strings (the stream re-keys them).
    fn same_rows(back: &RecordBatch, sent: &RecordBatch) {
        assert_eq!(back.schema(), sent.schema());
        assert_eq!(back.num_rows(), sent.num_rows());
        let schema = sent.schema();
        for ((back, sent), field) in back.columns().iter().zip(sent.columns()).zip(schema.fields())
        {
            let name = field.name();
            match sent.data_type() {
                DataType::Dictionary(..) => assert_eq!(resolved(back), resolved(sent), "{name}"),
                _ => assert_eq!(back.to_data(), sent.to_data(), "{name}"),
            }
        }
    }

    /// What a frame's bytes hold, message by message.
    #[derive(Debug, PartialEq, Eq)]
    enum Message {
        End,
        Schema,
        /// A dictionary's values: all of them, or a delta of the new ones.
        Dictionary {
            delta: bool,
            values: i64,
        },
        Batch {
            rows: i64,
        },
    }

    fn messages(mut bytes: &[u8]) -> Vec<Message> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            assert_eq!(bytes[..4], [0xFF; 4], "a message starts with the continuation marker");
            let len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
            if len == 0 {
                out.push(Message::End);
                bytes = &bytes[8..];
                continue;
            }
            let message = arrow_ipc::root_as_message(&bytes[8..8 + len]).unwrap();
            out.push(match message.header_type() {
                arrow_ipc::MessageHeader::Schema => Message::Schema,
                arrow_ipc::MessageHeader::DictionaryBatch => {
                    let dict = message.header_as_dictionary_batch().unwrap();
                    Message::Dictionary {
                        delta: dict.isDelta(),
                        values: dict.data().unwrap().length(),
                    }
                }
                arrow_ipc::MessageHeader::RecordBatch => {
                    Message::Batch { rows: message.header_as_record_batch().unwrap().length() }
                }
                other => panic!("a live stream has no {other:?} message"),
            });
            bytes = &bytes[8 + len + message.bodyLength() as usize..];
        }
        out
    }

    fn dictionaries(messages: &[Message]) -> Vec<(bool, i64)> {
        messages
            .iter()
            .filter_map(|it| match it {
                Message::Dictionary { delta, values } => Some((*delta, *values)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_stream_sends_each_name_once_and_decodes_to_its_rows() {
        let process = Process { pid: 3, name: "blaze serve", commit: None };
        let s = 1_000_000_000i64;
        let frames = [
            batches::open(&[worker(2 * s as u64)], 5 * s, process).unwrap(),
            // The same names again: no dictionary crosses.
            batches::open(&[worker(3 * s as u64)], 6 * s, process).unwrap(),
            // No rows: no bytes.
            batches::open(&[], 7 * s, process).unwrap(),
            // `worker 4` and `infer_query` are new; `build`, `blaze`, `hir_ty` and `app` are not.
            batches::open(&[other(s as u64), worker(4 * s as u64)], 8 * s, process).unwrap(),
        ];
        let mut encoder = Encoder::new(&schema::OPEN);
        let mut decoder = Decoder::new(&schema::OPEN);
        let (mut bytes, mut back) = (Vec::new(), Vec::new());
        let mut sent = Vec::new();
        for batch in &frames {
            encoder.encode(batch, &mut bytes).unwrap();
            decoder.decode(&bytes, &mut back).unwrap();
            match batch.num_rows() {
                0 => {
                    assert!(bytes.is_empty(), "a frame with no rows has no bytes");
                    assert!(back.is_empty());
                }
                _ => {
                    assert_eq!(back.len(), 1);
                    same_rows(&back[0], batch);
                }
            }
            sent.push(messages(&bytes));
        }
        assert_eq!(sent[0].first(), Some(&Message::Schema), "{:?}", sent[0]);
        assert!(dictionaries(&sent[0]).iter().all(|(delta, _)| !delta), "{:?}", sent[0]);
        // thread_name, name, target and crate: 1 + 2 + 2 + 1.
        assert_eq!(
            dictionaries(&sent[0]).iter().map(|(_, it)| it).sum::<i64>(),
            6,
            "{:?}",
            sent[0]
        );
        assert_eq!(
            sent[1],
            [Message::Batch { rows: 2 }],
            "nothing but the batch when no name is new"
        );
        assert!(sent[2].is_empty());
        assert_eq!(dictionaries(&sent[3]), [(true, 1), (true, 1)], "{:?}", sent[3]);
        assert!(
            !sent[3].contains(&Message::Schema) && !sent[3].contains(&Message::End),
            "{:?}",
            sent[3]
        );
        assert_eq!((encoder.streams(), decoder.streams()), (1, 1));

        let mut again = Encoder::new(&schema::OPEN);
        let mut first = Encoder::new(&schema::OPEN);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for batch in &frames {
            again.encode(batch, &mut a).unwrap();
            first.encode(batch, &mut b).unwrap();
            assert_eq!(a, b, "same rows, same bytes");
        }
    }

    #[test]
    fn a_full_dictionary_ends_the_stream_and_the_decoder_follows() {
        let process = Process { pid: 3, name: "blaze serve", commit: None };
        let s = 1_000_000_000i64;
        let first = batches::open(&[worker(2 * s as u64)], 5 * s, process).unwrap();
        let second = batches::open(&[other(s as u64)], 6 * s, process).unwrap();
        let third = batches::open(&[other(2 * s as u64)], 7 * s, process).unwrap();
        // The first frame's 6 values fill it.
        let mut encoder = Encoder::with_cap(&schema::OPEN, Cap { values: 6, bytes: 1 << 20 });
        let mut decoder = Decoder::new(&schema::OPEN);
        let (mut bytes, mut back) = (Vec::new(), Vec::new());

        encoder.encode(&first, &mut bytes).unwrap();
        decoder.decode(&bytes, &mut back).unwrap();
        same_rows(&back[0], &first);
        assert_eq!(messages(&bytes).first(), Some(&Message::Schema));

        encoder.encode(&second, &mut bytes).unwrap();
        assert!(bytes.starts_with(&END), "a reset starts with the end of the last stream");
        let reset = messages(&bytes);
        assert_eq!(reset[..2], [Message::End, Message::Schema], "{reset:?}");
        assert!(
            dictionaries(&reset).iter().all(|(delta, _)| !delta),
            "fresh dictionaries: {reset:?}"
        );
        // worker 4; build, infer_query; blaze, hir_ty; app.
        assert_eq!(dictionaries(&reset).iter().map(|(_, it)| it).sum::<i64>(), 6, "{reset:?}");
        decoder.decode(&bytes, &mut back).unwrap();
        assert_eq!(back.len(), 1);
        same_rows(&back[0], &second);
        assert_eq!((encoder.streams(), decoder.streams()), (2, 2));

        // Full again at 6: the next frame resets too, and the decoder follows each time.
        encoder.encode(&third, &mut bytes).unwrap();
        assert!(bytes.starts_with(&END));
        decoder.decode(&bytes, &mut back).unwrap();
        same_rows(&back[0], &third);
        assert_eq!((encoder.streams(), decoder.streams()), (3, 3));

        // A decoder that never saw a stream refuses an end, naming its table.
        let refused = Decoder::new(&schema::OPEN).decode(&bytes, &mut back).unwrap_err();
        assert!(refused.to_string().contains("`open`"), "{refused}");
    }

    #[test]
    fn a_frame_not_sent_is_taken_back() {
        let process = Process { pid: 3, name: "blaze serve", commit: None };
        let s = 1_000_000_000i64;
        let first = batches::open(&[worker(2 * s as u64)], 5 * s, process).unwrap();
        let second = batches::open(&[other(s as u64)], 6 * s, process).unwrap();
        let (mut bytes, mut back) = (Vec::new(), Vec::new());

        // The client has the first frame; the second, with its new names, never reaches it.
        let mut encoder = Encoder::new(&schema::OPEN);
        let mut decoder = Decoder::new(&schema::OPEN);
        encoder.encode(&first, &mut bytes).unwrap();
        decoder.decode(&bytes, &mut back).unwrap();
        encoder.encode(&second, &mut bytes).unwrap();
        encoder.undo();
        encoder.encode(&second, &mut bytes).unwrap();
        assert!(bytes.starts_with(&END), "the stream the client has ends");
        decoder.decode(&bytes, &mut back).unwrap();
        same_rows(&back[0], &second);

        // A first stream the client never got begins again with no end before it.
        let mut fresh = Encoder::new(&schema::OPEN);
        fresh.encode(&first, &mut bytes).unwrap();
        fresh.undo();
        fresh.encode(&first, &mut bytes).unwrap();
        assert_eq!(messages(&bytes).first(), Some(&Message::Schema));
        Decoder::new(&schema::OPEN).decode(&bytes, &mut back).unwrap();
        same_rows(&back[0], &first);
    }

    #[test]
    fn the_view_shows_running_spans_rates_and_the_hit_rate() {
        let process = Process { pid: 3, name: "blaze serve", commit: None };
        let s = 1_000_000_000i64;
        let counters = batches::counters(
            &[
                CounterRow { build: 1, plot: Plot::MemoHits, t_ns: s, value: 90, fiber: None, named: 0 },
                CounterRow { build: 1, plot: Plot::MemoMisses, t_ns: s, value: 10, fiber: None, named: 0 },
            ],
            process,
        )
        .unwrap();
        let spans = batches::spans(&[], process).unwrap();
        let open = batches::open(&[worker(2 * s as u64)], 2 * s, process).unwrap();
        let mut top = Top::default();
        top.take(
            2 * s,
            0,
            0,
            &ipc_of(&schema::SPANS, &spans),
            &ipc_of(&schema::COUNTERS, &counters),
            &ipc_of(&schema::OPEN, &open),
        )
        .unwrap();
        let mut text = String::new();
        top.render(200, &mut text);
        assert!(text.contains("memo hit rate 90.0%"), "{text}");
        assert!(
            text.contains("worker 3") && text.contains("build > typeck_query `app::main` of app"),
            "{text}"
        );
        assert!(text.contains("longest running"), "{text}");
        assert!(!text.contains("warning"), "{text}");
    }

    #[test]
    fn threads_inside_spans_with_nothing_closing_are_warned_of() {
        let process = Process { pid: 3, name: "blaze serve", commit: None };
        let s = 1_000_000_000i64;
        let spans = batches::spans(&[], process).unwrap();
        let counters = batches::counters(&[], process).unwrap();
        let mut top = Top::default();
        let mut at = s;
        while at <= 12 * s {
            let open = batches::open(&[worker((at - s / 2) as u64)], at, process).unwrap();
            top.take(at, 0, 0, &[spans.clone()], &[counters.clone()], &[open]).unwrap();
            at += EVERY.as_nanos() as i64;
        }
        assert!(top.quiet().is_some_and(|it| it >= QUIET_WARN), "{top:?}");
        let mut text = String::new();
        top.render(80, &mut text);
        assert!(text.contains("warning: no span closed"), "{text}");
        assert!(text.lines().all(|it| it.chars().count() <= 80), "{text}");
    }

    #[test]
    fn a_feed_sends_what_closed_once() {
        crate::install();
        std::thread::spawn(|| {
            let _span = tracing::info_span!(target: "blaze", "live once").entered();
        })
        .join()
        .unwrap();
        let mut feed = Feed::new();
        let mut frame = Frame::default();
        let mut decoder = Decoder::new(&schema::SPANS);
        let mut batches = Vec::new();
        let mut count = |frame: &Frame| -> usize {
            assert_eq!(
                frame.spans.is_empty(),
                frame.spans_rows == 0,
                "bytes exactly when there are rows"
            );
            decoder.decode(&frame.spans, &mut batches).unwrap();
            assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), frame.spans_rows);
            batches
                .iter()
                .map(|batch| {
                    strings(batch, &schema::SPANS, "name")
                        .unwrap()
                        .filter(|it| *it == Some("live once"))
                        .count()
                })
                .sum()
        };
        feed.next(&mut frame).unwrap();
        assert_eq!(count(&frame), 1);
        assert!(frame.spans_rows >= 1);
        feed.next(&mut frame).unwrap();
        assert_eq!(count(&frame), 0, "a span closed before the last frame is not sent again");
    }
}
