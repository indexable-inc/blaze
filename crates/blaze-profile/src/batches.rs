//! The flight recorder's rows ([`flight::Read`]) as batches of the schema's tables
//! ([`crate::columns`]): what the Parquet history writes, the live feed sends as Arrow IPC, and
//! the explorer reads. Names the ring keeps as numbers (sites and [`flight::Sym`]s) are resolved
//! here, once per distinct number a batch, into each dictionary column's values.

use std::{alloc::Global, sync::OnceLock, time::Instant};

use blaze_columns::{ColumnError, Texts, Values};
use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::Vec as Buf;

pub use crate::columns::{Batch, BatchError};
use crate::{
    columns::{self, Column},
    flight::{
        self, CounterRow, EventRow, FieldRow, FieldValue, FoldRow, OpenSpan, Plot, Read,
        SpanRow, SwitchRow, ThreadNow, WaitedOn,
    },
    sampler::Sample,
    schema,
    span,
};

/// What every row of a batch from one process shares.
#[derive(Clone, Copy, Debug)]
pub struct Process<'a> {
    pub pid: u32,
    /// What the process is: `blaze serve`, `blaze build`, a build script's crate.
    pub name: &'a str,
    /// The workspace's commit, when the process builds one checkout.
    pub commit: Option<&'a str>,
}

impl Process<'static> {
    /// This process, with no commit.
    pub fn this() -> Process<'static> {
        static NAME: OnceLock<String> = OnceLock::new();
        Process { pid: std::process::id(), name: NAME.get_or_init(crate::command), commit: None }
    }
}

/// A column of `values`, none null.
fn values<T: blaze_columns::Native>(values: impl IntoIterator<Item = T>) -> Column {
    // alloc: a column, once a batch, never per row
    Column::values(values, Global)
}

/// A column of `values`, null where `None`.
fn options<T: blaze_columns::Native>(values: impl IntoIterator<Item = Option<T>>) -> Column {
    // alloc: a column, once a batch, never per row
    Column::options(values, Global)
}

/// A dictionary column being built: each distinct number given a key the first time, its
/// string looked up then.
struct Dict {
    keys_of: Map<u64, u32, FixedState, Global>,
    values: Texts<Global>,
}

impl Dict {
    fn new() -> Dict {
        Dict { keys_of: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }), values: Texts::new_in(Global) }
    }

    fn key<T: AsRef<str>>(&mut self, number: u64, text: impl FnOnce() -> T) -> u32 {
        let next = u32::try_from(self.values.len()).unwrap_or(u32::MAX);
        let values = &mut self.values;
        *self.keys_of.entry(number).or_insert_with(|| {
            // alloc: once per distinct value a column a batch, into its one text buffer
            values.push(text().as_ref());
            next
        })
    }

    fn finish(self, keys: impl IntoIterator<Item = Option<u32>>) -> Result<Column, ColumnError> {
        Column::dict(keys, self.values, Global)
    }
}

/// A dictionary column being built over text: each distinct text given a key the first time.
struct TextDict<'t> {
    keys_of: Map<&'t str, u32, FixedState, Global>,
    values: Texts<Global>,
}

impl<'t> TextDict<'t> {
    fn new() -> TextDict<'t> {
        TextDict { keys_of: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }), values: Texts::new_in(Global) }
    }

    fn key(&mut self, text: &'t str) -> u32 {
        let next = u32::try_from(self.values.len()).unwrap_or(u32::MAX);
        let values = &mut self.values;
        *self.keys_of.entry(text).or_insert_with(|| {
            // alloc: once per distinct value a column a batch, into its one text buffer
            values.push(text);
            next
        })
    }
}

/// A dictionary column of `values`, keyed as they come, null where `None`.
fn dict<'a>(values: impl Iterator<Item = Option<&'a str>>) -> Result<Column, ColumnError> {
    let mut dict = TextDict::new();
    // alloc: a batch's keys, once a column
    let keys: std::vec::Vec<Option<u32>> = values.map(|it| it.map(|it| dict.key(it))).collect();
    Column::dict(keys, dict.values, Global)
}

/// A dictionary column of one value, `rows` times; null throughout when `value` is `None`.
fn constant(value: Option<&str>, rows: usize) -> Result<Column, ColumnError> {
    let mut values = Texts::new_in(Global);
    if let Some(value) = value {
        values.push(value);
    }
    Column::dict(std::iter::repeat_n(value.map(|_| 0), rows), values, Global)
}

fn build_id(build: u32) -> Option<u64> {
    (build != 0).then_some(u64::from(build))
}

/// A list's end, as list offsets hold it.
fn offset(elements: usize) -> Result<u32, ColumnError> {
    u32::try_from(elements).map_err(|_| ColumnError::Offsets { why: "pass u32::MAX elements" })
}

/// The `spans` rows of `spans`, all from `process`.
pub fn spans(spans: &[SpanRow], process: Process<'_>) -> Result<Batch, BatchError> {
    let err = BatchError::of(&schema::SPANS);
    let rows = spans.len();
    let (mut threads, mut names, mut targets, mut kinds) =
        (Dict::new(), Dict::new(), Dict::new(), Dict::new());
    // alloc: a batch's key columns, once a read, never per span written
    let (mut thread_keys, mut name_keys, mut target_keys, mut kind_keys) = (
        Vec::with_capacity(rows),
        Vec::with_capacity(rows),
        Vec::with_capacity(rows),
        Vec::with_capacity(rows),
    );
    for row in spans {
        // A site of this process, or of a dead one whose rows a postmortem read back.
        let meta = flight::site_text(row.site);
        let name = meta.map_or("?", |it| it.name);
        thread_keys.push(Some(threads.key(u64::from(row.thread_name), || {
            flight::name(row.thread_name).unwrap_or_else(|| "?".into())
        })));
        name_keys.push(Some(names.key(u64::from(row.site), || name)));
        target_keys.push(Some(targets.key(u64::from(row.site), || meta.map_or("?", |it| it.target))));
        // A rust-analyzer query's span is named for its query.
        kind_keys.push(name.ends_with("_query").then(|| kinds.key(u64::from(row.site), || name)));
    }
    // alloc: a batch's list of columns, once a read
    let mut columns: Vec<Column> = Vec::with_capacity(schema::SPANS.columns.len());
    columns.extend([
        options(spans.iter().map(|it| build_id(it.build))),
        constant(process.commit, rows).map_err(err)?,
        Column::repeat(process.pid, rows, Global),
        constant(Some(process.name), rows).map_err(err)?,
        values(spans.iter().map(|it| it.thread)),
        threads.finish(thread_keys).map_err(err)?,
        values(spans.iter().map(|it| it.span_id)),
        options(spans.iter().map(|it| (it.parent != 0).then_some(it.parent))),
        values(spans.iter().map(|it| it.depth)),
        names.finish(name_keys).map_err(err)?,
        targets.finish(target_keys).map_err(err)?,
        values(spans.iter().map(|it| it.start_ns)),
        values(spans.iter().map(|it| it.dur_ns)),
        options(spans.iter().map(|it| it.self_cpu_ns.map(|it| it as i64))),
        options(spans.iter().map(|it| it.alloc.map(|it| it.bytes))),
        options(spans.iter().map(|it| it.alloc.map(|it| it.count))),
        kinds.finish(kind_keys).map_err(err)?,
    ]);
    let (generic, typed) = span::CELLS.split_at(span::GENERIC);
    for cells in generic {
        columns.push(span_column(spans, cells)?);
    }
    columns.extend([
        options(spans.iter().map(|it| it.copied.map(|it| it.copies))),
        options(spans.iter().map(|it| it.copied.map(|it| it.bytes))),
        // Null where it folded none: most spans.
        options(spans.iter().map(|it| (it.folded_spans != 0).then_some(it.folded_spans))),
        options(spans.iter().map(|it| (it.folded_spans != 0).then_some(it.folded_ns))),
    ]);
    for cells in typed {
        columns.push(span_column(spans, cells)?);
    }
    columns::batch(&schema::SPANS, columns)
}

/// A text column of `texts`, its bytes allocated once at their exact size. Until 2026-11-12
/// the column was collected with no size given, so its bytes grew by doubling: on a cold build
/// of tempo the largest site of the `profile` tag's heap churn, 22.25 GiB of 135.31 in blocks
/// of 6.3 MB on average (`alloc_samples`, site `GenericByteBuilder::append_value::<&str>`,
/// under `in_row_batches::<SpanRow, ..>`; int-1112 d45e70ddc).
fn texts_column(texts: &[Option<std::sync::Arc<str>>]) -> Column {
    // alloc: a batch's column, once a read, at its size
    Column::opt_texts(texts, Global)
}

/// The column of `spans` a span kind's field fills ([`span::CELLS`]), as its schema types it.
fn span_column(spans: &[SpanRow], cells: &span::Cells) -> Result<Column, BatchError> {
    let words = spans.iter().map(|it| it.said().get(cells.at));
    Ok(match cells.column.ty {
        schema::Type::U64 => options(words.map(cells.number)),
        schema::Type::Utf8 => {
            // alloc: a batch's texts, once a read
            let texts: Vec<Option<std::sync::Arc<str>>> = words.map(cells.text).collect();
            texts_column(&texts)
        }
        schema::Type::Dict => {
            let mut values = Dict::new();
            // alloc: a batch's keys, once a column a read
            let keys: Vec<Option<u32>> = words
                .map(|word| {
                    (word != 0).then(|| {
                        values.key(word, || (cells.text)(word).unwrap_or_else(|| "?".into()))
                    })
                })
                .collect();
            values.finish(keys).map_err(BatchError::of(&schema::SPANS))?
        }
        other => {
            return Err(BatchError {
                table: schema::SPANS.name,
                error: ColumnError::Type {
                    at: schema::SPANS.column(cells.column.name).unwrap_or(usize::MAX),
                    field: columns::column_type(cells.column.ty),
                    column: columns::column_type(other),
                },
            });
        }
    })
}

/// The `counters` rows of `counters`, all from `process`.
pub fn counters(counters: &[CounterRow], process: Process<'_>) -> Result<Batch, BatchError> {
    let err = BatchError::of(&schema::COUNTERS);
    let rows = counters.len();
    let mut names = Dict::new();
    // alloc: a batch's keys, once a read
    let name_keys: Vec<Option<u32>> = counters
        .iter()
        .map(|it| {
            Some(match it.plot {
                // A named counter's own name: `?` when it was let go before this read.
                Plot::Named => names.key(1 << 32 | u64::from(it.named), || {
                    flight::name(it.named).unwrap_or_else(|| "?".into())
                }),
                plot => names.key(plot as u64, || plot.name()),
            })
        })
        .collect();
    // alloc: a batch's list of columns, once a read
    let columns: Vec<Column> = vec![
        options(counters.iter().map(|it| build_id(it.build))),
        Column::repeat(process.pid, rows, Global),
        names.finish(name_keys).map_err(err)?,
        values(counters.iter().map(|it| it.t_ns)),
        values(counters.iter().map(|it| it.value as f64)),
        options(counters.iter().map(|it| it.fiber.map(|(fiber, _)| fiber))),
        options(counters.iter().map(|it| it.fiber.map(|(_, job)| job))),
    ];
    columns::batch(&schema::COUNTERS, columns)
}

/// The `open` rows of `threads`, read at `at_ns`, all from `process`.
pub fn open(threads: &[ThreadNow], at_ns: i64, process: Process<'_>) -> Result<Batch, BatchError> {
    let err = BatchError::of(&schema::OPEN);
    let rows = threads.iter().map(|it| it.open.len()).sum();
    let spans = || {
        threads.iter().flat_map(|thread| thread.open.iter().enumerate().map(move |it| (thread, it)))
    };
    // alloc: a batch's texts, once a read of the threads, at their size
    let texts = |text: fn(&OpenSpan) -> Option<&str>| -> Vec<Option<&str>> {
        spans().map(|(_, (_, span))| text(span)).collect()
    };
    // alloc: a batch's list of columns, once a read of the threads
    let columns: Vec<Column> = vec![
        Column::repeat(process.pid, rows, Global),
        values(spans().map(|(thread, _)| thread.tid as u32)),
        dict(spans().map(|(thread, _)| Some(thread.name.as_str()))).map_err(err)?,
        values(spans().map(|(_, (depth, _))| depth as u16)),
        dict(spans().map(|(_, (_, span))| Some(span.name))).map_err(err)?,
        dict(spans().map(|(_, (_, span))| Some(span.target))).map_err(err)?,
        values(spans().map(|(_, (_, span))| at_ns - span.for_ns as i64)),
        dict(spans().map(|(_, (_, span))| span.krate.as_deref())).map_err(err)?,
        Column::opt_texts(&texts(|span| span.function.as_deref()), Global),
        Column::opt_texts(&texts(|span| span.unit.as_deref()), Global),
        Column::opt_texts(&texts(|span| span.awaits.as_deref()), Global),
        options(spans().map(|(_, (_, span))| span.owner)),
        Column::repeat(at_ns, rows, Global),
        options(threads.iter().flat_map(|thread| nearest_tasks(&thread.open))),
    ];
    columns::batch(&schema::OPEN, columns)
}

/// Each of a thread's open spans' `task` (outermost first): the nearest task among the span
/// and its ancestors, a daemon `task` span's plane request `<< 32 |` its node. A span of no
/// build (`build` 0) carries none.
fn nearest_tasks(open: &[OpenSpan]) -> impl Iterator<Item = Option<u64>> + '_ {
    open.iter().scan(None, |nearest, span| {
        if let (build @ 1.., Some(node)) = (span.build, span.node) {
            *nearest = Some(u64::from(build) << 32 | (node & u64::from(u32::MAX)));
        }
        Some(*nearest)
    })
}

/// The `stack` (each frame's names, outermost first) and `stack_pcs` (the raw addresses,
/// outermost first) columns of stacks given innermost first, each frame named by `frames`
/// ([`crate::symbols::frames`]).
fn stack_columns<'s>(
    stacks: impl Iterator<Item = &'s [u64]>,
    frames: &Map<u64, Vec<String>, FixedState, Global>,
) -> Result<[Column; 2], ColumnError> {
    let mut names = TextDict::new();
    // alloc: a batch's two list columns, once a read
    let (mut name_ends, mut keys) = (Buf::from_elem_in(0u32, 1, Global), Buf::new_in(Global));
    // alloc: a batch's two list columns, once a read
    let (mut pc_ends, mut pcs) = (Buf::from_elem_in(0u32, 1, Global), Buf::new_in(Global));
    for stack in stacks {
        // Outermost first; the sampler keeps the innermost first.
        for (i, pc) in stack.iter().enumerate().rev() {
            // Every address has its frames ([`crate::symbols::frames`]): `?` only for a caller
            // that named fewer.
            let named = frames.get(&crate::symbols::lookup(i, *pc)).map_or(&[][..], Vec::as_slice);
            for name in named {
                keys.push(names.key(name));
            }
            if named.is_empty() {
                keys.push(names.key("?"));
            }
            pcs.push(*pc);
        }
        name_ends.push(offset(keys.len())?);
        pc_ends.push(offset(pcs.len())?);
    }
    Ok([
        Column::list(name_ends, None, Values::Dict { keys, values: names.values })?,
        Column::list(pc_ends, None, Values::U64(pcs))?,
    ])
}

/// The `samples` rows of `samples`, each frame named by `frames` (the frames of every address
/// in them, by the address each is looked up at: [`crate::symbols::frames`]), all from
/// `process`. `at` is one reading of both clocks: an `Instant` and the schema's nanoseconds then
/// ([`crate::clock::mono_ns`]).
pub(crate) fn samples(
    samples: &[Sample],
    frames: &Map<u64, Vec<String>, FixedState, Global>,
    at: (Instant, i64),
    process: Process<'_>,
) -> Result<Batch, BatchError> {
    let err = BatchError::of(&schema::SAMPLES);
    let rows = samples.len();
    let [stacks, pcs] =
        stack_columns(samples.iter().map(|it| &it.pcs[..usize::from(it.depth)]), frames)
            .map_err(err)?;
    let t_ns = |it: &Sample| at.1 - at.0.saturating_duration_since(it.at).as_nanos() as i64;
    // alloc: a batch's list of columns, once a read
    let columns: Vec<Column> = vec![
        options(samples.iter().map(|it| build_id(it.build))),
        Column::repeat(process.pid, rows, Global),
        values(samples.iter().map(|it| it.tid as u32)),
        values(samples.iter().map(t_ns)),
        stacks,
        options(samples.iter().map(|it| (it.span != 0).then_some(it.span))),
        dict(samples.iter().map(|it| Some(if it.on_cpu { "on" } else { "off" }))).map_err(err)?,
        pcs,
        options(samples.iter().map(|it| (it.job != 0).then_some(it.job))),
        options(samples.iter().map(|it| (it.fiber != 0).then_some(it.fiber))),
    ];
    columns::batch(&schema::SAMPLES, columns)
}

/// The `alloc_samples` rows of `samples` (the heap sampler's, [`crate::heap`]), each frame
/// named by `frames` ([`crate::symbols::frames`]) and each row's call site the innermost frame
/// outside the allocator ([`crate::heap::site_of`]), all from `process`. `at` is one reading
/// of both clocks, as for [`samples`].
pub(crate) fn alloc_samples(
    samples: &[crate::heap::HeapSample],
    frames: &Map<u64, Vec<String>, FixedState, Global>,
    at: (Instant, i64),
    process: Process<'_>,
) -> Result<Batch, BatchError> {
    heap_samples(&schema::ALLOC_SAMPLES, None, samples, frames, at, process)
}

/// The `held_samples` rows of one reading, at `read_ns`, of the samples still held
/// ([`crate::heap::held`]): [`alloc_samples`]' columns of each, then the reading's time.
pub(crate) fn held_samples(
    samples: &[crate::heap::HeapSample],
    frames: &Map<u64, Vec<String>, FixedState, Global>,
    at: (Instant, i64),
    read_ns: i64,
    process: Process<'_>,
) -> Result<Batch, BatchError> {
    heap_samples(&schema::HELD_SAMPLES, Some(read_ns), samples, frames, at, process)
}

/// The rows of `table`, `alloc_samples` or `held_samples`, which has `read_ns` as its last
/// column: one column list for both, so the two tables' shared columns cannot differ.
fn heap_samples(
    table: &'static schema::Table,
    read_ns: Option<i64>,
    samples: &[crate::heap::HeapSample],
    frames: &Map<u64, Vec<String>, FixedState, Global>,
    at: (Instant, i64),
    process: Process<'_>,
) -> Result<Batch, BatchError> {
    let err = BatchError::of(table);
    let rows = samples.len();
    let [stacks, pcs] =
        stack_columns(samples.iter().map(|it| &it.pcs[..usize::from(it.depth)]), frames)
            .map_err(err)?;
    let t_ns = |it: &crate::heap::HeapSample| {
        at.1 - at.0.saturating_duration_since(it.at).as_nanos() as i64
    };
    // alloc: a batch's sites, once a read
    let sites: Vec<Option<_>> = samples
        .iter()
        .map(|it| crate::heap::site_of(&it.pcs[..usize::from(it.depth)], frames))
        .collect();
    // alloc: a batch's list of columns, once a read
    let mut columns: Vec<Column> = vec![
        options(samples.iter().map(|it| build_id(it.build))),
        Column::repeat(process.pid, rows, Global),
        values(samples.iter().map(|it| it.tid as u32)),
        values(samples.iter().map(t_ns)),
        values(samples.iter().map(|it| it.weight)),
        stacks,
        options(samples.iter().map(|it| (it.span != 0).then_some(it.span))),
        pcs,
        dict(samples.iter().map(|it| it.tag.map(blaze_mem::Tag::name))).map_err(err)?,
        values(samples.iter().map(|it| it.bytes)),
        Column::opt_texts(&sites, Global),
    ];
    if let Some(read_ns) = read_ns {
        columns.push(Column::repeat(read_ns, rows, Global)); // alloc: one column more
    }
    columns::batch(table, columns)
}

/// A code image a flush's `stack_pcs` addresses are in ([`crate::symbols::ImagesWritten`]), or
/// a recovered crash's ([`crate::crash::Crash::image`]).
#[derive(Debug)]
pub(crate) struct ImageRow {
    pub(crate) path: String,
    pub(crate) build_id: Option<String>,
    /// The BLAKE3 of its file, hex, or why it was not hashed.
    pub(crate) blake3: Result<String, String>,
    pub(crate) base: u64,
    pub(crate) size: u64,
    pub(crate) slide: u64,
    /// How its frames were named: dwarf, symbols, exports or none.
    pub(crate) names: &'static str,
}

/// The `images` rows of `images`, all from `process` in the machine's boot `boot`, written at
/// `seen_ns`.
pub(crate) fn images(
    images: &[ImageRow],
    boot: Option<&str>,
    seen_ns: i64,
    process: Process<'_>,
) -> Result<Batch, BatchError> {
    let err = BatchError::of(&schema::IMAGES);
    let rows = images.len();
    // alloc: a batch's texts, once a flush with new images
    let texts = |text: fn(&ImageRow) -> Option<&str>| -> Vec<Option<&str>> {
        images.iter().map(text).collect()
    };
    // alloc: a batch's list of columns, once a flush with new images
    let columns: Vec<Column> = vec![
        Column::repeat(process.pid, rows, Global),
        constant(boot, rows).map_err(err)?,
        Column::repeat(seen_ns, rows, Global),
        Column::texts_of(images.iter().map(|it| it.path.as_str()), Global),
        Column::opt_texts(&texts(|it| it.build_id.as_deref()), Global),
        Column::opt_texts(&texts(|it| it.blake3.as_deref().ok()), Global),
        Column::opt_texts(&texts(|it| it.blake3.as_ref().err().map(String::as_str)), Global),
        values(images.iter().map(|it| it.base)),
        values(images.iter().map(|it| it.size)),
        values(images.iter().map(|it| it.slide)),
        dict(images.iter().map(|it| Some(it.names))).map_err(err)?,
    ];
    columns::batch(&schema::IMAGES, columns)
}

/// The `switches` rows of `switches`, all from `process`.
pub fn switches(switches: &[SwitchRow], process: Process<'_>) -> Result<Batch, BatchError> {
    let err = BatchError::of(&schema::SWITCHES);
    let rows = switches.len();
    let (mut threads, mut events, mut kinds) = (Dict::new(), Dict::new(), Dict::new());
    // alloc: a batch's keys, once a read
    let (mut thread_keys, mut event_keys, mut kind_keys) =
        (Vec::with_capacity(rows), Vec::with_capacity(rows), Vec::with_capacity(rows));
    for row in switches {
        thread_keys.push(Some(threads.key(u64::from(row.thread_name), || {
            flight::name(row.thread_name).unwrap_or_else(|| "?".into())
        })));
        event_keys.push(Some(events.key(row.event as u64, || row.event.name())));
        kind_keys.push(row.waited.map(|(on, _)| kinds.key(on as u64, || on.name())));
    }
    let value = |row: &SwitchRow| {
        row.waited.filter(|(on, _)| *on != WaitedOn::Moved).map(|(_, value)| value)
    };
    // alloc: a batch's list of columns, once a read
    let columns: Vec<Column> = vec![
        options(switches.iter().map(|it| build_id(it.build))),
        Column::repeat(process.pid, rows, Global),
        values(switches.iter().map(|it| it.thread)),
        threads.finish(thread_keys).map_err(err)?,
        values(switches.iter().map(|it| it.t_ns)),
        events.finish(event_keys).map_err(err)?,
        values(switches.iter().map(|it| it.job)),
        values(switches.iter().map(|it| it.fiber)),
        values(switches.iter().map(|it| it.core)),
        kinds.finish(kind_keys).map_err(err)?,
        options(switches.iter().map(value)),
        options(switches.iter().map(|it| it.by)),
        options(switches.iter().map(|it| it.since_ns)),
        options(switches.iter().map(|it| it.began_as)),
        options(switches.iter().map(|it| it.queued_ns)),
        options(switches.iter().map(|it| it.cpu_ns.map(|it| it as i64))),
    ];
    columns::batch(&schema::SWITCHES, columns)
}

/// The `events` rows of `events`, all from `process`.
pub fn events(events: &[EventRow], process: Process<'_>) -> Result<Batch, BatchError> {
    let err = BatchError::of(&schema::EVENTS);
    let rows = events.len();
    let (mut threads, mut levels, mut targets, mut names) =
        (Dict::new(), Dict::new(), Dict::new(), Dict::new());
    // alloc: a batch's keys, once a read
    let (mut thread_keys, mut level_keys, mut target_keys, mut name_keys) = (
        Vec::with_capacity(rows),
        Vec::with_capacity(rows),
        Vec::with_capacity(rows),
        Vec::with_capacity(rows),
    );
    for row in events {
        let meta = flight::site_text(row.site);
        thread_keys.push(Some(threads.key(u64::from(row.thread_name), || {
            flight::name(row.thread_name).unwrap_or_else(|| "?".into())
        })));
        level_keys.push(Some(levels.key(u64::from(row.site), || meta.map_or("?", |it| it.level))));
        target_keys.push(Some(targets.key(u64::from(row.site), || meta.map_or("?", |it| it.target))));
        name_keys.push(Some(names.key(u64::from(row.site), || meta.map_or("?", |it| it.name))));
    }
    // alloc: a batch's messages, once a read
    let messages: Vec<Option<std::sync::Arc<str>>> =
        events.iter().map(|it| flight::name(it.message)).collect();
    // alloc: a batch's list of columns, once a read
    let columns: Vec<Column> = vec![
        options(events.iter().map(|it| build_id(it.build))),
        Column::repeat(process.pid, rows, Global),
        values(events.iter().map(|it| it.thread)),
        threads.finish(thread_keys).map_err(err)?,
        values(events.iter().map(|it| it.t_ns)),
        options(events.iter().map(|it| (it.job != 0).then_some(it.job))),
        levels.finish(level_keys).map_err(err)?,
        targets.finish(target_keys).map_err(err)?,
        names.finish(name_keys).map_err(err)?,
        texts_column(&messages),
        options(events.iter().map(|it| (it.span != 0).then_some(it.span))),
        values(events.iter().map(|it| it.event_id)),
    ];
    columns::batch(&schema::EVENTS, columns)
}

/// The `event_fields` rows of `fields`, all from `process`: a field's key is its callsite's
/// name for it, looked up once a batch for each site and place.
pub fn event_fields(fields: &[FieldRow], process: Process<'_>) -> Result<Batch, BatchError> {
    let err = BatchError::of(&schema::EVENT_FIELDS);
    let rows = fields.len();
    let (mut keys, mut kinds) = (Dict::new(), Dict::new());
    // alloc: a batch's keys, once a read
    let (mut key_keys, mut kind_keys) = (Vec::with_capacity(rows), Vec::with_capacity(rows));
    for row in fields {
        key_keys.push(Some(keys.key(u64::from(row.site) << 8 | u64::from(row.index), || {
            flight::meta(row.site)
                .and_then(|it| it.fields().iter().nth(usize::from(row.index)))
                .map_or("?", |it| it.name())
        })));
        kind_keys.push(Some(kinds.key(row.value.kind(), || row.value.kind_name())));
    }
    // alloc: a batch's texts, once a read
    let texts: Vec<Option<std::sync::Arc<str>>> = fields
        .iter()
        .map(|it| match it.value {
            FieldValue::Str(sym) => flight::name(sym),
            _ => None,
        })
        .collect();
    // alloc: a batch's list of columns, once a read
    let columns: Vec<Column> = vec![
        options(fields.iter().map(|it| build_id(it.build))),
        Column::repeat(process.pid, rows, Global),
        values(fields.iter().map(|it| it.event_id)),
        values(fields.iter().map(|it| it.ordinal)),
        keys.finish(key_keys).map_err(err)?,
        kinds.finish(kind_keys).map_err(err)?,
        options(fields.iter().map(|it| match it.value {
            FieldValue::I64(it) => Some(it),
            _ => None,
        })),
        options(fields.iter().map(|it| match it.value {
            FieldValue::U64(it) => Some(it),
            _ => None,
        })),
        options(fields.iter().map(|it| match it.value {
            FieldValue::F64(it) => Some(it),
            _ => None,
        })),
        options(fields.iter().map(|it| match it.value {
            FieldValue::Bool(it) => Some(it),
            _ => None,
        })),
        texts_column(&texts),
    ];
    columns::batch(&schema::EVENT_FIELDS, columns)
}

/// The `folded` rows of a read: its sums of short spans.
pub fn folded(folds: &[FoldRow], process: Process<'_>) -> Result<Batch, BatchError> {
    let err = BatchError::of(&schema::FOLDED);
    let rows = folds.len();
    let (mut threads, mut names, mut targets) = (Dict::new(), Dict::new(), Dict::new());
    let (mut parents, mut parent_targets) = (Dict::new(), Dict::new());
    // alloc: a batch's keys, once a read
    let (mut thread_keys, mut name_keys, mut target_keys) =
        (Vec::with_capacity(rows), Vec::with_capacity(rows), Vec::with_capacity(rows));
    // alloc: a batch's keys, once a read
    let (mut parent_keys, mut parent_target_keys) =
        (Vec::with_capacity(rows), Vec::with_capacity(rows));
    for row in folds {
        let meta = flight::site_text(row.site);
        thread_keys.push(Some(threads.key(u64::from(row.thread_name), || {
            flight::name(row.thread_name).unwrap_or_else(|| "?".into())
        })));
        name_keys.push(Some(names.key(u64::from(row.site), || meta.map_or("?", |it| it.name))));
        target_keys.push(Some(targets.key(u64::from(row.site), || meta.map_or("?", |it| it.target))));
        let parent = (row.parent_site != 0).then(|| flight::site_text(row.parent_site));
        parent_keys.push(parent.map(|meta| {
            parents.key(u64::from(row.parent_site), || meta.map_or("?", |it| it.name))
        }));
        parent_target_keys.push(parent.map(|meta| {
            parent_targets.key(u64::from(row.parent_site), || meta.map_or("?", |it| it.target))
        }));
    }
    // alloc: a batch's list of columns, once a read
    let columns: Vec<Column> = vec![
        options(folds.iter().map(|it| build_id(it.build))),
        Column::repeat(process.pid, rows, Global),
        values(folds.iter().map(|it| it.thread)),
        threads.finish(thread_keys).map_err(err)?,
        names.finish(name_keys).map_err(err)?,
        targets.finish(target_keys).map_err(err)?,
        values(folds.iter().map(|it| u32::from(it.root))),
        values(folds.iter().map(|it| it.from_ns)),
        values(folds.iter().map(|it| it.to_ns)),
        values(folds.iter().map(|it| it.spans)),
        values(folds.iter().map(|it| it.dur_ns)),
        values(folds.iter().map(|it| it.max_dur_ns)),
        options(folds.iter().map(|it| it.self_cpu_ns.map(|it| it as i64))),
        options(folds.iter().map(|it| it.alloc.map(|it| it.bytes))),
        options(folds.iter().map(|it| it.alloc.map(|it| it.count))),
        parents.finish(parent_keys).map_err(err)?,
        parent_targets.finish(parent_target_keys).map_err(err)?,
    ];
    columns::batch(&schema::FOLDED, columns)
}

/// Both tables of a read: `spans` then `counters`.
pub fn of(read: &Read, process: Process<'_>) -> Result<[Batch; 2], BatchError> {
    Ok([spans(&read.spans, process)?, counters(&read.counters, process)?])
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::schema::Table;

    /// Row `row` of column `name` of `batch`, a batch of `table`.
    fn column<'b>(batch: &'b Batch, table: &Table, name: &str) -> &'b Column {
        batch.column(table.column(name).unwrap())
    }

    #[test]
    fn a_read_is_a_batch_of_each_table_in_the_schema() {
        crate::install();
        std::thread::spawn(|| {
            let _build = flight::building(9);
            let _span = blaze_trace::info_span!(target: "blaze", "batched", krate = "app").entered();
            flight::plot(flight::Plot::Queued, 4);
        })
        .join()
        .unwrap();
        let mut read = Read::default();
        flight::window(Duration::from_secs(60), &mut read);
        let process = Process { pid: 7, name: "blaze test", commit: Some("abc") };
        let [spans, counters] = of(&read, process).unwrap();
        assert_eq!(spans.fields(), columns::fields(&schema::SPANS));
        assert_eq!(spans.num_rows(), read.spans.len());
        assert_eq!(counters.num_rows(), read.counters.len());
        let at = read
            .spans
            .iter()
            .position(|it| flight::meta(it.site).map(|it| it.name()) == Some("batched"))
            .expect("the span was read");
        let text = |name: &str| column(&spans, &schema::SPANS, name).text(at);
        assert_eq!(text("name"), Some("batched"));
        assert_eq!(text("crate"), Some("app"));
        assert_eq!(text("commit"), Some("abc"));
        assert_eq!(text("query_kind"), None);
        assert_eq!(column(&spans, &schema::SPANS, "build_id").int(at), Some(9));
        let plotted = column(&counters, &schema::COUNTERS, "value").f64s().unwrap();
        assert!(plotted.values().contains(&4.0));
    }

    /// A named counter is a `counters` row whose `name` is its own, kept once a process.
    #[test]
    fn a_named_counter_is_a_counters_row_of_its_name() {
        crate::install();
        std::thread::spawn(|| {
            flight::plot_named("owner bytes: batches test", 41);
            flight::plot_named("owner bytes: batches test", 42);
        })
        .join()
        .unwrap();
        let mut read = Read::default();
        flight::window(Duration::from_secs(600), &mut read);
        let process = Process { pid: 7, name: "blaze test", commit: None };
        let batch = counters(&read.counters, process).unwrap();
        let names = column(&batch, &schema::COUNTERS, "name");
        let values = column(&batch, &schema::COUNTERS, "value");
        let mut ours: Vec<f64> = (0..batch.num_rows())
            .filter(|it| names.text(*it) == Some("owner bytes: batches test"))
            .filter_map(|it| values.float(it))
            .collect();
        ours.sort_by(f64::total_cmp);
        assert_eq!(ours, [41.0, 42.0]);
    }

    /// Names given on many threads at once, each thread in its own table and ring, written as
    /// the spans, events and event fields parquet and read back: every dynamic name is the text
    /// recorded, a span opened on one thread and closed on another included
    /// (docs/v2/plan/design/profile-names.md#proof).
    #[test]
    fn names_given_on_many_threads_read_back_from_parquet_as_recorded() {
        const THREADS: usize = 6;
        crate::install();
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                std::thread::Builder::new()
                    .name(format!("names round trip {t}"))
                    .spawn_scoped(scope, move || {
                        let _build = flight::building(31);
                        let krate = format!("round_trip_crate_{t}");
                        let function = format!("round_trip::{}::f", "segment".repeat(t * 3));
                        let _span = blaze_trace::info_span!(
                            target: "blaze",
                            "names round trip",
                            krate = krate.as_str(),
                            function = function.as_str()
                        )
                        .entered();
                        let item = format!("round trip item {t}");
                        blaze_trace::info!(target: "blaze", item = item.as_str(), "round trip event {t}");
                    })
                    .unwrap();
            }
            // Opened here, named and closed on another thread: its names go in as links.
            let span = blaze_trace::info_span!(
                target: "blaze",
                "names round trip moved",
                krate = "round_trip_opened_here",
                function = blaze_trace::field::Empty
            );
            std::thread::Builder::new()
                .name("names round trip closer".to_owned())
                .spawn_scoped(scope, move || {
                    span.record("function", "round_trip::closed_elsewhere");
                    drop(span);
                })
                .unwrap()
                .join()
                .unwrap();
        });
        // A span closed a moment ago may wait for its thread's CPU rate before a read takes it:
        // read again, a bounded number of times, until every one is in.
        let ours = |read: &Read| {
            read.spans
                .iter()
                .filter(|it| {
                    flight::meta(it.site)
                        .is_some_and(|it| it.name().starts_with("names round trip"))
                })
                .count()
        };
        let mut read = Read::default();
        for _ in 0..100 {
            read = Read::default();
            flight::window(Duration::from_secs(600), &mut read);
            if ours(&read) > THREADS {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let process = Process { pid: 7, name: "blaze test", commit: None };
        let parquet = |table: &'static Table, batch: Batch| {
            let bytes = blaze_parquet::write(
                columns::fields(table),
                &[batch],
                &columns::metadata(table),
                blaze_parquet::Options::DEFAULT,
                Global,
            )
            .unwrap();
            let file = blaze_parquet::File::open(&bytes, Global).unwrap();
            // Read back as one batch: the window holds every row the process's rings do, the
            // tests' beside this one included, fewer than a row group's.
            let mut batches = file.read_all().unwrap().into_iter();
            let back = batches.next().unwrap_or_else(|| columns::batch(table, columns::fields(table).iter().map(|it| Column::nulls(it.ty, 0, Global))).unwrap());
            assert!(batches.next().is_none(), "the rows written are read back as one batch");
            back
        };
        let text = |batch: &Batch, table: &Table, name: &str, row: usize| {
            column(batch, table, name).text(row).map(str::to_owned)
        };
        let spans = parquet(&schema::SPANS, spans(&read.spans, process).unwrap());
        let rows = |name: &str| -> Vec<(Option<String>, Option<String>)> {
            (0..spans.num_rows())
                .filter(|it| text(&spans, &schema::SPANS, "name", *it).as_deref() == Some(name))
                .map(|it| {
                    (
                        text(&spans, &schema::SPANS, "crate", it),
                        text(&spans, &schema::SPANS, "function", it),
                    )
                })
                .collect()
        };
        let mut got = rows("names round trip");
        got.sort();
        let mut want: Vec<_> = (0..THREADS)
            .map(|t| {
                (
                    Some(format!("round_trip_crate_{t}")),
                    Some(format!("round_trip::{}::f", "segment".repeat(t * 3))),
                )
            })
            .collect();
        want.sort();
        assert_eq!(got, want, "each thread's span, its names as it gave them");
        assert_eq!(
            rows("names round trip moved"),
            [(
                Some("round_trip_opened_here".to_owned()),
                Some("round_trip::closed_elsewhere".to_owned())
            )],
            "a span opened on one thread and closed on another"
        );
        let events = parquet(&schema::EVENTS, super::events(&read.events, process).unwrap());
        let mut messages: Vec<String> = (0..events.num_rows())
            .filter_map(|it| text(&events, &schema::EVENTS, "message", it))
            .filter(|it| it.starts_with("round trip event "))
            .collect();
        messages.sort();
        let mut want: Vec<String> = (0..THREADS).map(|t| format!("round trip event {t}")).collect();
        want.sort();
        assert_eq!(messages, want, "each thread's event message");
        let fields = parquet(&schema::EVENT_FIELDS, event_fields(&read.fields, process).unwrap());
        let mut items: Vec<String> = (0..fields.num_rows())
            .filter_map(|it| text(&fields, &schema::EVENT_FIELDS, "value_str", it))
            .filter(|it| it.starts_with("round trip item "))
            .collect();
        items.sort();
        let mut want: Vec<String> = (0..THREADS).map(|t| format!("round trip item {t}")).collect();
        want.sort();
        assert_eq!(items, want, "each thread's event field");
    }

    /// A sample's row: its stack named outermost first, each address by the frames it is looked
    /// up at (a return address at the call before it, an inlined function's frames together),
    /// its raw addresses outermost first in `stack_pcs`, and the span and build it was taken in.
    #[test]
    fn a_sample_row_carries_its_stack_addresses_span_and_build() {
        let now = Instant::now();
        let mut pcs = [0; crate::sampler::DEPTH];
        // Innermost first: 0x10 is where the thread was, 0x21 and 0x31 return addresses.
        pcs[..3].copy_from_slice(&[0x10, 0x21, 0x31]);
        let sample = Sample {
            at: now,
            tid: 12,
            depth: 3,
            pcs,
            span: 41,
            build: 9,
            on_cpu: false,
            job: 5 << 32 | 3,
            fiber: 2,
        };
        let frames: Map<u64, Vec<String>, FixedState, Global> = [
            (0x10, &["typeck", "typeck::inlined"][..]),
            (0x20, &["build"][..]),
            (0x30, &["main"][..]),
        ]
        .into_iter()
        .map(|(pc, it)| (pc, it.iter().map(|it| (*it).to_owned()).collect()))
        .collect();
        let process = Process { pid: 7, name: "blaze serve", commit: None };
        let batch = samples(&[sample], &frames, (now, 1_000), process).unwrap();
        assert_eq!(batch.num_rows(), 1);
        let column = |name: &str| column(&batch, &schema::SAMPLES, name);
        let named: Vec<&str> = column("stack").lists().unwrap().texts(0).unwrap().collect();
        assert_eq!(named, ["main", "build", "typeck", "typeck::inlined"]);
        let raw = column("stack_pcs").lists().unwrap().slice::<u64>(0).unwrap().to_vec();
        assert_eq!(raw, [0x31, 0x21, 0x10]);
        assert_eq!(column("span_id").int(0), Some(41));
        assert_eq!(column("build_id").int(0), Some(9));
        assert_eq!(column("cpu").text(0), Some("off"));
        assert_eq!(column("t_ns").int(0), Some(1_000));
        assert_eq!(column("job").int(0), Some(5 << 32 | 3));
        assert_eq!(column("fiber").int(0), Some(2));
    }

    /// A sample outside any span and build has those columns null, not 0.
    #[test]
    fn a_sample_outside_any_span_has_no_span_or_build() {
        let now = Instant::now();
        let sample = Sample {
            at: now,
            tid: 1,
            depth: 0,
            pcs: [0; crate::sampler::DEPTH],
            span: 0,
            build: 0,
            on_cpu: true,
            job: 0,
            fiber: 0,
        };
        let process = Process { pid: 7, name: "blaze serve", commit: None };
        let batch = samples(&[sample], &Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }), (now, 1_000), process).unwrap();
        for name in ["span_id", "build_id", "job", "fiber"] {
            assert!(column(&batch, &schema::SAMPLES, name).is_null(0), "{name}");
        }
    }

    #[test]
    fn a_runs_cpu_steps_peak_and_outcome_are_its_columns_and_null_where_unsaid() {
        use crate::span::{Fields as _, MacroExpanded, Outcome, Said, col};
        let mut typed = Said::EMPTY;
        MacroExpanded { steps: Some(4_200_000), peak_bytes: Some(48 << 20), outcome: Outcome::Ok }
            .write(&mut typed);
        let said = SpanRow {
            build: 1,
            thread: 3,
            thread_name: 0,
            span_id: 11,
            parent: 0,
            depth: 0,
            site: 0,
            start_ns: 0,
            dur_ns: 900,
            alloc: None,
            copied: None,
            krate: 0,
            function: flight::read_sym("serde_derive::Serialize"),
            unit: 0,
            self_cpu_ns: Some(700),
            typed: typed.typed,
            folded_spans: 0,
            folded_ns: 0,
        };
        assert_eq!(said.get::<col::Outcome>(), Some(Outcome::Ok));
        let unsaid = SpanRow { span_id: 12, self_cpu_ns: None, typed: Said::EMPTY.typed, ..said };
        let batch =
            spans(&[said, unsaid], Process { pid: 7, name: "blaze serve", commit: None }).unwrap();
        let ints = |name: &str| [0, 1].map(|row| column(&batch, &schema::SPANS, name).int(row));
        assert_eq!(ints("self_cpu_ns"), [Some(700), None]);
        assert_eq!(ints("steps"), [Some(4_200_000), None]);
        assert_eq!(ints("peak_bytes"), [Some(48 << 20), None]);
        let outcome = column(&batch, &schema::SPANS, "outcome");
        assert_eq!((outcome.text(0), outcome.text(1)), (Some("ok"), None));
    }

    #[test]
    fn an_empty_read_is_empty_batches() {
        let [spans, counters] = of(&Read::default(), Process::this()).unwrap();
        assert_eq!((spans.num_rows(), counters.num_rows()), (0, 0));
    }
}
