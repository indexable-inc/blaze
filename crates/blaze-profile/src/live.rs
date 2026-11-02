//! The live feed: what `blaze debug live` and the explorer's live stream show. Each subscriber
//! has a [`Feed`] of its own on the daemon, which every [`EVERY`] takes what the rings got
//! since its last frame ([`flight::read`]) and the spans each thread is inside now, as one
//! [`Frame`] of Arrow IPC streams. A subscriber slower than the writers skips what was written
//! over, counted in [`Frame::skipped`]; its memory stays one frame, whatever it falls behind.
//!
//! The client decodes a frame ([`decode`]) and folds it into [`Top`], which renders the
//! top-like view: running spans per thread, the longest running, rates, the memo's hit rate
//! and a warning when nothing closes while threads are inside spans.

use std::{fmt::Write as _, time::Duration};

use arrow_array::{Array, ArrayRef, RecordBatch, cast::AsArray, types::UInt32Type};
use arrow_ipc::{
    CompressionType,
    reader::StreamReader,
    writer::{IpcWriteOptions, StreamWriter},
};
use arrow_schema::ArrowError;
use blaze_hash::HashMap;

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

/// One frame of the feed: each table one Arrow IPC stream (schema, a batch, end of stream).
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
    /// Rows of each table, so a reader can skip an empty one without decoding it.
    pub spans_rows: usize,
    pub counters_rows: usize,
    pub open_rows: usize,
}

/// A subscriber's place in the rings.
pub struct Feed {
    cursor: Cursor,
    read: Read,
}

impl Default for Feed {
    fn default() -> Feed {
        Feed::new()
    }
}

impl Feed {
    /// A feed whose first frame has everything the rings still hold.
    pub fn new() -> Feed {
        Feed { cursor: Cursor::default(), read: Read::default() }
    }

    /// The next frame, into `frame`, its buffers reused.
    pub fn next(&mut self, frame: &mut Frame) -> Result<(), ArrowError> {
        self.read.clear();
        flight::read(&mut self.cursor, &mut self.read);
        let process = Process::this();
        let [spans, counters] = batches::of(&self.read, process)?;
        let open = batches::open(&flight::threads_now(), self.read.at_ns, process)?;
        frame.at_ns = self.read.at_ns;
        frame.skipped = self.read.skipped;
        frame.lost = self.read.lost;
        (frame.spans_rows, frame.counters_rows, frame.open_rows) =
            (spans.num_rows(), counters.num_rows(), open.num_rows());
        ipc(&spans, &mut frame.spans)?;
        ipc(&counters, &mut frame.counters)?;
        ipc(&open, &mut frame.open)
    }
}

/// `batch` as one Arrow IPC stream, zstd-compressed, into `into` (cleared first).
pub fn ipc(batch: &RecordBatch, into: &mut Vec<u8>) -> Result<(), ArrowError> {
    into.clear();
    let options = IpcWriteOptions::default().try_with_compression(Some(CompressionType::ZSTD))?;
    let mut writer = StreamWriter::try_new_with_options(&mut *into, batch.schema_ref(), options)?;
    writer.write(batch)?;
    writer.finish()
}

/// The batches of one Arrow IPC stream.
pub fn decode(bytes: &[u8]) -> Result<Vec<RecordBatch>, ArrowError> {
    // alloc: a frame's batches, on the client, once a frame
    StreamReader::try_new(bytes, None)?.collect()
}

/// A frame the client could not read: a table that is not the schema's, or bytes that are not
/// Arrow IPC.
#[derive(Debug)]
pub enum TopError {
    Arrow(ArrowError),
    /// The batch has no column `column` of the right type for table `table`.
    Column { table: &'static str, column: &'static str },
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

fn column<'a>(batch: &'a RecordBatch, table: &'static Table, name: &'static str) -> Result<&'a ArrayRef, TopError> {
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
    Ok((0..keys.len()).map(move |row| keys.is_valid(row).then(|| values.value(keys.value(row) as usize))))
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

/// The client's view of the feed, folded from each frame.
#[derive(Default, Debug)]
pub struct Top {
    threads: HashMap<u32, Thread>,
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
            let threads = primitive::<arrow_array::types::UInt32Type>(batch, &schema::SPANS, "thread")?;
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
            let values = primitive::<arrow_array::types::Float64Type>(batch, &schema::COUNTERS, "value")?;
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
            let starts = primitive::<arrow_array::types::Int64Type>(batch, &schema::OPEN, "start_ns")?;
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
                thread.open.push(Running { name: name.unwrap_or("?").to_owned(), of, start_ns: *start_ns });
                inside = true;
            }
        }
        self.threads.retain(|_, it| !it.open.is_empty() || it.closed > 0);
        self.rate = if self.at_ns != 0 && since > 0 { closed as f64 * 1e9 / since as f64 } else { 0.0 };
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
        threads.sort_by_key(|(tid, it)| (it.open.first().map_or(i64::MAX, |it| it.start_ns), **tid));
        _ = writeln!(out, "{:<24} {:>9} {:>6}  running", "thread", "for", "closed");
        for (tid, thread) in &threads {
            let line_start = out.len();
            let name = if thread.name.is_empty() { "?" } else { thread.name.as_str() };
            let running_for = thread.open.first().map_or(0, |it| self.at_ns - it.start_ns);
            _ = write!(out, "{:<24} {:>8.1}s {:>6}  ", truncated(name, 24), secs(running_for), thread.closed);
            for (i, span) in thread.open.iter().enumerate() {
                out.push_str(if i == 0 { "" } else { " > " });
                out.push_str(&span.name);
                out.push_str(&span.of);
            }
            if thread.open.is_empty() {
                _ = write!(out, "(idle, tid {tid})");
            }
            cut(out, line_start, width);
            out.push('\n');
        }
        // alloc: the view's rows, once a frame
        let mut longest: Vec<(&Running, &str)> = threads
            .iter()
            .flat_map(|&(_, thread)| thread.open.iter().map(move |it| (it, thread.name.as_str())))
            // alloc: the view's rows, once a frame
            .collect();
        longest.sort_by_key(|(it, _)| it.start_ns);
        if !longest.is_empty() {
            out.push_str("longest running\n");
        }
        for (span, thread) in longest.iter().take(10) {
            let line_start = out.len();
            _ = write!(out, "{:>8.1}s  {}{}  ({thread})", secs(self.at_ns - span.start_ns), span.name, span.of);
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

    fn ipc_of(batch: &RecordBatch) -> Vec<RecordBatch> {
        let mut bytes = Vec::new();
        ipc(batch, &mut bytes).unwrap();
        decode(&bytes).unwrap()
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

    #[test]
    fn a_frame_round_trips_through_ipc() {
        let process = Process { pid: 3, name: "blaze serve", commit: None };
        let open = batches::open(&[worker(2_000_000_000)], 5_000_000_000, process).unwrap();
        let back = ipc_of(&open);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0], open);
    }

    #[test]
    fn the_view_shows_running_spans_rates_and_the_hit_rate() {
        let process = Process { pid: 3, name: "blaze serve", commit: None };
        let s = 1_000_000_000i64;
        let counters = batches::counters(
            &[
                CounterRow { build: 1, plot: Plot::MemoHits, t_ns: s, value: 90, fiber: None, query_kind: 0 },
                CounterRow { build: 1, plot: Plot::MemoMisses, t_ns: s, value: 10, fiber: None, query_kind: 0 },
            ],
            process,
        )
        .unwrap();
        let spans = batches::spans(&[], process).unwrap();
        let open = batches::open(&[worker(2 * s as u64)], 2 * s, process).unwrap();
        let mut top = Top::default();
        top.take(2 * s, 0, 0, &ipc_of(&spans), &ipc_of(&counters), &ipc_of(&open)).unwrap();
        let mut text = String::new();
        top.render(200, &mut text);
        assert!(text.contains("memo hit rate 90.0%"), "{text}");
        assert!(text.contains("worker 3") && text.contains("build > typeck_query `app::main` of app"), "{text}");
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
        let count = |frame: &Frame| -> usize {
            let batches = decode(&frame.spans).unwrap();
            batches
                .iter()
                .map(|batch| {
                    strings(batch, &schema::SPANS, "name").unwrap().filter(|it| *it == Some("live once")).count()
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
