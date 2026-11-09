//! The flight recorder's rows ([`flight::Read`]) as Arrow record batches of the schema's tables:
//! what the Parquet history writes, the live feed sends as IPC, and the explorer reads. Names
//! the ring keeps as numbers (sites and [`flight::Sym`]s) are resolved here, once per distinct
//! number a batch, into each dictionary column's values.

use std::{
    sync::{Arc, OnceLock},
    time::Instant,
};

use arrow_array::{
    Array, ArrayRef, BooleanArray, DictionaryArray, Float64Array, Int64Array, RecordBatch,
    StringArray, UInt16Array, UInt32Array, UInt64Array,
    builder::{ListBuilder, StringDictionaryBuilder, UInt64Builder},
    types::UInt32Type,
};
use arrow_schema::{ArrowError, DataType, SchemaRef};
use blaze_hash::HashMap;
use std::alloc::Global;

use crate::{
    flight::{
        self, CounterRow, EventRow, FieldRow, FieldValue, FoldRow, OpenSpan, Plot, Read,
        SpanRow, SwitchRow, ThreadNow, WaitedOn,
    },
    sampler::Sample,
    schema::{self, Table},
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

/// The Arrow schema of `table`, made once a process.
pub fn schema_of(table: &'static Table) -> SchemaRef {
    const ALL: usize = schema::TABLES.len() + schema::RESULTS.len();
    static SCHEMAS: [OnceLock<SchemaRef>; ALL] = [const { OnceLock::new() }; ALL];
    let at = schema::TABLES
        .iter()
        .chain(&schema::RESULTS)
        .position(|it| it.name == table.name)
        .unwrap_or_else(|| unreachable!("{} is a table of the schema", table.name));
    let it = &SCHEMAS[at];
    // clone: an `Arc` of a schema made once
    it.get_or_init(|| Arc::new(crate::arrow::schema(table))).clone()
}

/// A column of a batch.
fn column(array: impl Array + 'static) -> ArrayRef {
    // alloc: a column, once a batch, never per row
    Arc::new(array)
}

/// A dictionary column being built: each distinct number given a key the first time, its
/// string looked up then.
struct Dict {
    keys_of: HashMap<u64, u32, Global>,
    values: Vec<std::sync::Arc<str>>,
}

impl Dict {
    fn new() -> Dict {
        // alloc: a dictionary's values, once a column a batch
        Dict { keys_of: HashMap::new_in(Global), values: Vec::new() }
    }

    fn key<T: Into<std::sync::Arc<str>>>(&mut self, number: u64, text: impl FnOnce() -> T) -> u32 {
        let next = self.values.len() as u32;
        let values = &mut self.values;
        *self.keys_of.entry(number).or_insert_with(|| {
            // alloc: once per distinct value a column a batch (a refcount for a name's)
            values.push(text().into());
            next
        })
    }

    fn finish(self, keys: UInt32Array) -> Result<ArrayRef, ArrowError> {
        let values = column(StringArray::from_iter_values(self.values.iter().map(|it| &**it)));
        Ok(column(DictionaryArray::<UInt32Type>::try_new(keys, values)?))
    }
}

/// A dictionary column of `values`, keyed as they come.
fn dict<'a>(values: impl Iterator<Item = &'a str>) -> ArrayRef {
    column(values.collect::<DictionaryArray<UInt32Type>>())
}

/// A dictionary column of one value, `rows` times; null throughout when `value` is `None`.
fn constant(value: Option<&str>, rows: usize) -> Result<ArrayRef, ArrowError> {
    let values = column(StringArray::from_iter_values(value));
    let keys = match value {
        Some(_) => UInt32Array::from_value(0, rows),
        None => UInt32Array::new_null(rows),
    };
    Ok(column(DictionaryArray::<UInt32Type>::try_new(keys, values)?))
}

fn build_id(build: u32) -> Option<u64> {
    (build != 0).then_some(u64::from(build))
}

/// The `spans` rows of `spans`, all from `process`.
pub fn spans(spans: &[SpanRow], process: Process<'_>) -> Result<RecordBatch, ArrowError> {
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
        thread_keys.push(threads.key(u64::from(row.thread_name), || {
            flight::name(row.thread_name).unwrap_or_else(|| "?".into())
        }));
        name_keys.push(names.key(u64::from(row.site), || name));
        target_keys.push(targets.key(u64::from(row.site), || meta.map_or("?", |it| it.target)));
        // A rust-analyzer query's span is named for its query.
        kind_keys.push(name.ends_with("_query").then(|| kinds.key(u64::from(row.site), || name)));
    }
    // alloc: a batch's list of columns, once a read
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(schema::SPANS.columns.len());
    columns.extend([
        column(UInt64Array::from_iter(spans.iter().map(|it| build_id(it.build)))),
        constant(process.commit, rows)?,
        column(UInt32Array::from_value(process.pid, rows)),
        constant(Some(process.name), rows)?,
        column(UInt32Array::from_iter_values(spans.iter().map(|it| it.thread))),
        threads.finish(UInt32Array::from(thread_keys))?,
        column(UInt64Array::from_iter_values(spans.iter().map(|it| it.span_id))),
        column(UInt64Array::from_iter(
            spans.iter().map(|it| (it.parent != 0).then_some(it.parent)),
        )),
        column(UInt16Array::from_iter_values(spans.iter().map(|it| it.depth))),
        names.finish(UInt32Array::from(name_keys))?,
        targets.finish(UInt32Array::from(target_keys))?,
        column(Int64Array::from_iter_values(spans.iter().map(|it| it.start_ns))),
        column(Int64Array::from_iter_values(spans.iter().map(|it| it.dur_ns))),
        column(Int64Array::from_iter(spans.iter().map(|it| it.self_cpu_ns.map(|it| it as i64)))),
        column(UInt64Array::from_iter(spans.iter().map(|it| it.alloc.map(|it| it.bytes)))),
        column(UInt64Array::from_iter(spans.iter().map(|it| it.alloc.map(|it| it.count)))),
        kinds.finish(UInt32Array::from(kind_keys))?,
    ]);
    let (generic, typed) = span::CELLS.split_at(span::GENERIC);
    for cells in generic {
        columns.push(span_column(spans, cells)?);
    }
    columns.extend([
        column(UInt64Array::from_iter(spans.iter().map(|it| it.copied.map(|it| it.copies)))),
        column(UInt64Array::from_iter(spans.iter().map(|it| it.copied.map(|it| it.bytes)))),
        // Null where it folded none: most spans.
        column(UInt64Array::from_iter(
            spans.iter().map(|it| (it.folded_spans != 0).then_some(it.folded_spans)),
        )),
        column(Int64Array::from_iter(
            spans.iter().map(|it| (it.folded_spans != 0).then_some(it.folded_ns)),
        )),
    ]);
    for cells in typed {
        columns.push(span_column(spans, cells)?);
    }
    RecordBatch::try_new(schema_of(&schema::SPANS), columns)
}

/// The column of `spans` a span kind's field fills ([`span::CELLS`]), as its schema types it.
fn span_column(spans: &[SpanRow], cells: &span::Cells) -> Result<ArrayRef, ArrowError> {
    let words = spans.iter().map(|it| it.said().get(cells.at));
    Ok(match cells.column.ty {
        schema::Type::U64 => column(UInt64Array::from_iter(words.map(cells.number))),
        schema::Type::Utf8 => {
            // alloc: a batch's column, once a read
            let texts: Vec<Option<std::sync::Arc<str>>> = words.map(cells.text).collect();
            column(StringArray::from_iter(texts.iter().map(|it| it.as_deref())))
        }
        schema::Type::Dict => {
            let mut values = Dict::new();
            let keys: UInt32Array = words
                .map(|word| {
                    (word != 0).then(|| {
                        values.key(word, || (cells.text)(word).unwrap_or_else(|| "?".into()))
                    })
                })
                .collect();
            values.finish(keys)?
        }
        other => {
            // alloc: an error, once
            return Err(ArrowError::SchemaError(format!(
                "the spans column `{}` is {other:?}, which no span field fills",
                cells.column.name
            )));
        }
    })
}

/// The `counters` rows of `counters`, all from `process`.
pub fn counters(counters: &[CounterRow], process: Process<'_>) -> Result<RecordBatch, ArrowError> {
    let rows = counters.len();
    let mut names = Dict::new();
    let name_keys = UInt32Array::from_iter_values(counters.iter().map(|it| match it.plot {
        // A named counter's own name: `?` when it was let go before this read.
        Plot::Named => names.key(1 << 32 | u64::from(it.named), || {
            flight::name(it.named).unwrap_or_else(|| "?".into())
        }),
        plot => names.key(plot as u64, || plot.name()),
    }));
    // alloc: a batch's list of columns, once a read
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter(counters.iter().map(|it| build_id(it.build)))),
        column(UInt32Array::from_value(process.pid, rows)),
        names.finish(name_keys)?,
        column(Int64Array::from_iter_values(counters.iter().map(|it| it.t_ns))),
        column(Float64Array::from_iter_values(counters.iter().map(|it| it.value as f64))),
        column(UInt32Array::from_iter(counters.iter().map(|it| it.fiber.map(|(fiber, _)| fiber)))),
        column(UInt64Array::from_iter(counters.iter().map(|it| it.fiber.map(|(_, job)| job)))),
    ];
    RecordBatch::try_new(schema_of(&schema::COUNTERS), columns)
}

/// The `open` rows of `threads`, read at `at_ns`, all from `process`.
pub fn open(
    threads: &[ThreadNow],
    at_ns: i64,
    process: Process<'_>,
) -> Result<RecordBatch, ArrowError> {
    let rows = threads.iter().map(|it| it.open.len()).sum();
    let spans = || {
        threads.iter().flat_map(|thread| thread.open.iter().enumerate().map(move |it| (thread, it)))
    };
    // alloc: a batch's list of columns, once a read of the threads
    let columns: Vec<ArrayRef> = vec![
        column(UInt32Array::from_value(process.pid, rows)),
        column(UInt32Array::from_iter_values(spans().map(|(thread, _)| thread.tid as u32))),
        dict(spans().map(|(thread, _)| thread.name.as_str())),
        column(UInt16Array::from_iter_values(spans().map(|(_, (depth, _))| depth as u16))),
        dict(spans().map(|(_, (_, span))| span.name)),
        dict(spans().map(|(_, (_, span))| span.target)),
        column(Int64Array::from_iter_values(
            spans().map(|(_, (_, span))| at_ns - span.for_ns as i64),
        )),
        column(
            spans()
                .map(|(_, (_, span))| span.krate.as_deref())
                .collect::<DictionaryArray<UInt32Type>>(),
        ),
        column(StringArray::from_iter(spans().map(|(_, (_, span))| span.function.as_deref()))),
        column(StringArray::from_iter(spans().map(|(_, (_, span))| span.unit.as_deref()))),
        column(StringArray::from_iter(spans().map(|(_, (_, span))| span.awaits.as_deref()))),
        column(UInt64Array::from_iter(spans().map(|(_, (_, span))| span.owner))),
        column(Int64Array::from_value(at_ns, rows)),
        column(UInt64Array::from_iter(threads.iter().flat_map(|thread| nearest_tasks(&thread.open)))),
    ];
    RecordBatch::try_new(schema_of(&schema::OPEN), columns)
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

/// The `samples` rows of `samples`, each frame named by `frames` (the frames of every address
/// in them, by the address each is looked up at: [`crate::symbols::frames`]), all from
/// `process`. `at` is one reading of both clocks: an `Instant` and the schema's nanoseconds then
/// ([`crate::clock::mono_ns`]).
pub(crate) fn samples(
    samples: &[Sample],
    frames: &HashMap<u64, Vec<String>, Global>,
    at: (Instant, i64),
    process: Process<'_>,
) -> Result<RecordBatch, ArrowError> {
    let rows = samples.len();
    let schema = schema_of(&schema::SAMPLES);
    let list_field = |name: &str| {
        let at = schema::SAMPLES.column(name).unwrap_or_else(|| unreachable!("samples has {name}"));
        let DataType::List(item) = schema.field(at).data_type() else {
            unreachable!("{name} is a list (arrow.rs)")
        };
        // clone: an `Arc` of the schema's field, once a batch
        item.clone()
    };
    let mut stacks = ListBuilder::new(StringDictionaryBuilder::<UInt32Type>::new())
        .with_field(list_field("stack"));
    let mut pcs = ListBuilder::new(UInt64Builder::new()).with_field(list_field("stack_pcs"));
    for sample in samples {
        let stack = &sample.pcs[..usize::from(sample.depth)];
        // Outermost first; the sampler keeps the innermost first.
        for (i, pc) in stack.iter().enumerate().rev() {
            // Every address has its frames ([`crate::symbols::frames`]): `?` only for a caller
            // that named fewer.
            let named = frames.get(&crate::symbols::lookup(i, *pc)).map_or(&[][..], Vec::as_slice);
            for name in named {
                stacks.values().append_value(name);
            }
            if named.is_empty() {
                stacks.values().append_value("?");
            }
            pcs.values().append_value(*pc);
        }
        stacks.append(true);
        pcs.append(true);
    }
    let t_ns = |it: &Sample| at.1 - at.0.saturating_duration_since(it.at).as_nanos() as i64;
    // alloc: a batch's list of columns, once a read
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter(samples.iter().map(|it| build_id(it.build)))),
        column(UInt32Array::from_value(process.pid, rows)),
        column(UInt32Array::from_iter_values(samples.iter().map(|it| it.tid as u32))),
        column(Int64Array::from_iter_values(samples.iter().map(t_ns))),
        column(stacks.finish()),
        column(UInt64Array::from_iter(samples.iter().map(|it| (it.span != 0).then_some(it.span)))),
        dict(samples.iter().map(|it| if it.on_cpu { "on" } else { "off" })),
        column(pcs.finish()),
        column(UInt64Array::from_iter(samples.iter().map(|it| (it.job != 0).then_some(it.job)))),
        column(UInt32Array::from_iter(samples.iter().map(|it| (it.fiber != 0).then_some(it.fiber)))),
    ];
    RecordBatch::try_new(schema, columns)
}

/// The `alloc_samples` rows of `samples` (the heap sampler's, [`crate::heap`]), each frame
/// named by `frames` ([`crate::symbols::frames`]) and each row's call site the innermost frame
/// outside the allocator ([`crate::heap::site_of`]), all from `process`. `at` is one reading
/// of both clocks, as for [`samples`].
pub(crate) fn alloc_samples(
    samples: &[crate::heap::HeapSample],
    frames: &HashMap<u64, Vec<String>, Global>,
    at: (Instant, i64),
    process: Process<'_>,
) -> Result<RecordBatch, ArrowError> {
    let rows = samples.len();
    let schema = schema_of(&schema::ALLOC_SAMPLES);
    let list_field = |name: &str| {
        let at = schema::ALLOC_SAMPLES
            .column(name)
            .unwrap_or_else(|| unreachable!("alloc_samples has {name}"));
        let DataType::List(item) = schema.field(at).data_type() else {
            unreachable!("{name} is a list (arrow.rs)")
        };
        // clone: an `Arc` of the schema's field, once a batch
        item.clone()
    };
    let mut stacks = ListBuilder::new(StringDictionaryBuilder::<UInt32Type>::new())
        .with_field(list_field("stack"));
    let mut pcs = ListBuilder::new(UInt64Builder::new()).with_field(list_field("stack_pcs"));
    for sample in samples {
        let stack = &sample.pcs[..usize::from(sample.depth)];
        // Outermost first; the sampler keeps the innermost first.
        for (i, pc) in stack.iter().enumerate().rev() {
            let named = frames.get(&crate::symbols::lookup(i, *pc)).map_or(&[][..], Vec::as_slice);
            for name in named {
                stacks.values().append_value(name);
            }
            if named.is_empty() {
                stacks.values().append_value("?");
            }
            pcs.values().append_value(*pc);
        }
        stacks.append(true);
        pcs.append(true);
    }
    let t_ns = |it: &crate::heap::HeapSample| {
        at.1 - at.0.saturating_duration_since(it.at).as_nanos() as i64
    };
    let tags: DictionaryArray<UInt32Type> =
        samples.iter().map(|it| it.tag.map(blaze_mem::Tag::name)).collect();
    // alloc: a batch's list of columns, once a read
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter(samples.iter().map(|it| build_id(it.build)))),
        column(UInt32Array::from_value(process.pid, rows)),
        column(UInt32Array::from_iter_values(samples.iter().map(|it| it.tid as u32))),
        column(Int64Array::from_iter_values(samples.iter().map(t_ns))),
        column(UInt64Array::from_iter_values(samples.iter().map(|it| it.weight))),
        column(stacks.finish()),
        column(UInt64Array::from_iter(samples.iter().map(|it| (it.span != 0).then_some(it.span)))),
        column(pcs.finish()),
        column(tags),
        column(UInt64Array::from_iter_values(samples.iter().map(|it| it.bytes))),
        column(StringArray::from_iter(
            samples
                .iter()
                .map(|it| crate::heap::site_of(&it.pcs[..usize::from(it.depth)], frames)),
        )),
    ];
    RecordBatch::try_new(schema, columns)
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
) -> Result<RecordBatch, ArrowError> {
    let rows = images.len();
    // alloc: a batch's list of columns, once a flush with new images
    let columns: Vec<ArrayRef> = vec![
        column(UInt32Array::from_value(process.pid, rows)),
        constant(boot, rows)?,
        column(Int64Array::from_value(seen_ns, rows)),
        column(StringArray::from_iter_values(images.iter().map(|it| it.path.as_str()))),
        column(StringArray::from_iter(images.iter().map(|it| it.build_id.as_deref()))),
        column(StringArray::from_iter(images.iter().map(|it| it.blake3.as_deref().ok()))),
        column(StringArray::from_iter(
            images.iter().map(|it| it.blake3.as_ref().err().map(String::as_str)),
        )),
        column(UInt64Array::from_iter_values(images.iter().map(|it| it.base))),
        column(UInt64Array::from_iter_values(images.iter().map(|it| it.size))),
        column(UInt64Array::from_iter_values(images.iter().map(|it| it.slide))),
        dict(images.iter().map(|it| it.names)),
    ];
    RecordBatch::try_new(schema_of(&schema::IMAGES), columns)
}

/// The `switches` rows of `switches`, all from `process`.
pub fn switches(switches: &[SwitchRow], process: Process<'_>) -> Result<RecordBatch, ArrowError> {
    let rows = switches.len();
    let (mut threads, mut events, mut kinds) = (Dict::new(), Dict::new(), Dict::new());
    // alloc: a batch's keys, once a read
    let (mut thread_keys, mut event_keys, mut kind_keys) =
        (Vec::with_capacity(rows), Vec::with_capacity(rows), Vec::with_capacity(rows));
    for row in switches {
        thread_keys.push(threads.key(u64::from(row.thread_name), || {
            flight::name(row.thread_name).unwrap_or_else(|| "?".into())
        }));
        event_keys.push(events.key(row.event as u64, || row.event.name()));
        kind_keys.push(row.waited.map(|(on, _)| kinds.key(on as u64, || on.name())));
    }
    let value = |row: &SwitchRow| {
        row.waited.filter(|(on, _)| *on != WaitedOn::Moved).map(|(_, value)| value)
    };
    // alloc: a batch's list of columns, once a read
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter(switches.iter().map(|it| build_id(it.build)))),
        column(UInt32Array::from_value(process.pid, rows)),
        column(UInt32Array::from_iter_values(switches.iter().map(|it| it.thread))),
        threads.finish(UInt32Array::from(thread_keys))?,
        column(Int64Array::from_iter_values(switches.iter().map(|it| it.t_ns))),
        events.finish(UInt32Array::from(event_keys))?,
        column(UInt64Array::from_iter_values(switches.iter().map(|it| it.job))),
        column(UInt32Array::from_iter_values(switches.iter().map(|it| it.fiber))),
        column(UInt32Array::from_iter_values(switches.iter().map(|it| it.core))),
        kinds.finish(UInt32Array::from(kind_keys))?,
        column(UInt64Array::from_iter(switches.iter().map(value))),
        column(UInt64Array::from_iter(switches.iter().map(|it| it.by))),
        column(Int64Array::from_iter(switches.iter().map(|it| it.since_ns))),
        column(UInt64Array::from_iter(switches.iter().map(|it| it.began_as))),
        column(Int64Array::from_iter(switches.iter().map(|it| it.queued_ns))),
        column(Int64Array::from_iter(switches.iter().map(|it| it.cpu_ns.map(|it| it as i64)))),
    ];
    RecordBatch::try_new(schema_of(&schema::SWITCHES), columns)
}

/// The `events` rows of `events`, all from `process`.
pub fn events(events: &[EventRow], process: Process<'_>) -> Result<RecordBatch, ArrowError> {
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
        thread_keys.push(threads.key(u64::from(row.thread_name), || {
            flight::name(row.thread_name).unwrap_or_else(|| "?".into())
        }));
        level_keys.push(levels.key(u64::from(row.site), || meta.map_or("?", |it| it.level)));
        target_keys.push(targets.key(u64::from(row.site), || meta.map_or("?", |it| it.target)));
        name_keys.push(names.key(u64::from(row.site), || meta.map_or("?", |it| it.name)));
    }
    // alloc: a batch's messages, once a read
    let messages: Vec<Option<std::sync::Arc<str>>> =
        events.iter().map(|it| flight::name(it.message)).collect();
    // alloc: a batch's list of columns, once a read
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter(events.iter().map(|it| build_id(it.build)))),
        column(UInt32Array::from_value(process.pid, rows)),
        column(UInt32Array::from_iter_values(events.iter().map(|it| it.thread))),
        threads.finish(UInt32Array::from(thread_keys))?,
        column(Int64Array::from_iter_values(events.iter().map(|it| it.t_ns))),
        column(UInt64Array::from_iter(events.iter().map(|it| (it.job != 0).then_some(it.job)))),
        levels.finish(UInt32Array::from(level_keys))?,
        targets.finish(UInt32Array::from(target_keys))?,
        names.finish(UInt32Array::from(name_keys))?,
        column(StringArray::from_iter(messages.iter().map(|it| it.as_deref()))),
        column(UInt64Array::from_iter(events.iter().map(|it| (it.span != 0).then_some(it.span)))),
        column(UInt64Array::from_iter_values(events.iter().map(|it| it.event_id))),
    ];
    RecordBatch::try_new(schema_of(&schema::EVENTS), columns)
}

/// The `event_fields` rows of `fields`, all from `process`: a field's key is its callsite's
/// name for it, looked up once a batch for each site and place.
pub fn event_fields(fields: &[FieldRow], process: Process<'_>) -> Result<RecordBatch, ArrowError> {
    let rows = fields.len();
    let (mut keys, mut kinds) = (Dict::new(), Dict::new());
    // alloc: a batch's keys, once a read
    let (mut key_keys, mut kind_keys) = (Vec::with_capacity(rows), Vec::with_capacity(rows));
    for row in fields {
        key_keys.push(keys.key(u64::from(row.site) << 8 | u64::from(row.index), || {
            flight::meta(row.site)
                .and_then(|it| it.fields().iter().nth(usize::from(row.index)))
                .map_or("?", |it| it.name())
        }));
        kind_keys.push(kinds.key(row.value.kind(), || row.value.kind_name()));
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
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter(fields.iter().map(|it| build_id(it.build)))),
        column(UInt32Array::from_value(process.pid, rows)),
        column(UInt64Array::from_iter_values(fields.iter().map(|it| it.event_id))),
        column(UInt16Array::from_iter_values(fields.iter().map(|it| it.ordinal))),
        keys.finish(UInt32Array::from(key_keys))?,
        kinds.finish(UInt32Array::from(kind_keys))?,
        column(Int64Array::from_iter(fields.iter().map(|it| match it.value {
            FieldValue::I64(it) => Some(it),
            _ => None,
        }))),
        column(UInt64Array::from_iter(fields.iter().map(|it| match it.value {
            FieldValue::U64(it) => Some(it),
            _ => None,
        }))),
        column(Float64Array::from_iter(fields.iter().map(|it| match it.value {
            FieldValue::F64(it) => Some(it),
            _ => None,
        }))),
        column(BooleanArray::from_iter(fields.iter().map(|it| match it.value {
            FieldValue::Bool(it) => Some(it),
            _ => None,
        }))),
        column(StringArray::from_iter(texts.iter().map(|it| it.as_deref()))),
    ];
    RecordBatch::try_new(schema_of(&schema::EVENT_FIELDS), columns)
}

/// The `folded` rows of a read: its sums of short spans.
pub fn folded(folds: &[FoldRow], process: Process<'_>) -> Result<RecordBatch, ArrowError> {
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
        thread_keys.push(threads.key(u64::from(row.thread_name), || {
            flight::name(row.thread_name).unwrap_or_else(|| "?".into())
        }));
        name_keys.push(names.key(u64::from(row.site), || meta.map_or("?", |it| it.name)));
        target_keys.push(targets.key(u64::from(row.site), || meta.map_or("?", |it| it.target)));
        let parent = (row.parent_site != 0).then(|| flight::site_text(row.parent_site));
        parent_keys.push(parent.map(|meta| {
            parents.key(u64::from(row.parent_site), || meta.map_or("?", |it| it.name))
        }));
        parent_target_keys.push(parent.map(|meta| {
            parent_targets.key(u64::from(row.parent_site), || meta.map_or("?", |it| it.target))
        }));
    }
    // alloc: a batch's list of columns, once a read
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter(folds.iter().map(|it| build_id(it.build)))),
        column(UInt32Array::from_value(process.pid, rows)),
        column(UInt32Array::from_iter_values(folds.iter().map(|it| it.thread))),
        threads.finish(UInt32Array::from(thread_keys))?,
        names.finish(UInt32Array::from(name_keys))?,
        targets.finish(UInt32Array::from(target_keys))?,
        column(UInt32Array::from_iter_values(folds.iter().map(|it| u32::from(it.root)))),
        column(Int64Array::from_iter_values(folds.iter().map(|it| it.from_ns))),
        column(Int64Array::from_iter_values(folds.iter().map(|it| it.to_ns))),
        column(UInt64Array::from_iter_values(folds.iter().map(|it| it.spans))),
        column(Int64Array::from_iter_values(folds.iter().map(|it| it.dur_ns))),
        column(Int64Array::from_iter_values(folds.iter().map(|it| it.max_dur_ns))),
        column(Int64Array::from_iter(folds.iter().map(|it| it.self_cpu_ns.map(|it| it as i64)))),
        column(UInt64Array::from_iter(folds.iter().map(|it| it.alloc.map(|it| it.bytes)))),
        column(UInt64Array::from_iter(folds.iter().map(|it| it.alloc.map(|it| it.count)))),
        parents.finish(UInt32Array::from(parent_keys))?,
        parent_targets.finish(UInt32Array::from(parent_target_keys))?,
    ];
    RecordBatch::try_new(schema_of(&schema::FOLDED), columns)
}

/// Both tables of a read: `spans` then `counters`.
pub fn of(read: &Read, process: Process<'_>) -> Result<[RecordBatch; 2], ArrowError> {
    Ok([spans(&read.spans, process)?, counters(&read.counters, process)?])
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use arrow_array::{Array, cast::AsArray};

    use super::*;

    #[test]
    fn a_read_is_a_batch_of_each_table_in_the_schema() {
        crate::install();
        std::thread::spawn(|| {
            let _build = flight::building(9);
            let _span = tracing::info_span!(target: "blaze", "batched", krate = "app").entered();
            flight::plot(flight::Plot::Queued, 4);
        })
        .join()
        .unwrap();
        let mut read = Read::default();
        flight::window(Duration::from_secs(60), &mut read);
        let process = Process { pid: 7, name: "blaze test", commit: Some("abc") };
        let [spans, counters] = of(&read, process).unwrap();
        assert_eq!(spans.schema().metadata()["blaze.table"], "spans");
        assert_eq!(spans.num_rows(), read.spans.len());
        assert_eq!(counters.num_rows(), read.counters.len());
        let at = read
            .spans
            .iter()
            .position(|it| flight::meta(it.site).map(|it| it.name()) == Some("batched"))
            .expect("the span was read");
        let column = |name: &str| spans.column(schema::SPANS.column(name).unwrap());
        let dict = |name: &str, row: usize| {
            let it = column(name).as_dictionary::<UInt32Type>();
            let values = it.values().as_string::<i32>();
            it.keys().is_valid(row).then(|| values.value(it.keys().value(row) as usize).to_owned())
        };
        assert_eq!(dict("name", at).as_deref(), Some("batched"));
        assert_eq!(dict("crate", at).as_deref(), Some("app"));
        assert_eq!(dict("commit", at).as_deref(), Some("abc"));
        assert_eq!(dict("query_kind", at), None);
        assert_eq!(
            column("build_id").as_primitive::<arrow_array::types::UInt64Type>().value(at),
            9
        );
        let plotted = counters.column(schema::COUNTERS.column("value").unwrap());
        assert!(plotted.as_primitive::<arrow_array::types::Float64Type>().values().contains(&4.0));
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
        let plot = batch.column(schema::COUNTERS.column("name").unwrap()).as_dictionary::<UInt32Type>();
        let names = plot.values().as_string::<i32>();
        let values = batch.column(schema::COUNTERS.column("value").unwrap());
        let values = values.as_primitive::<arrow_array::types::Float64Type>();
        let mut ours: Vec<f64> = (0..batch.num_rows())
            .filter(|it| names.value(plot.keys().value(*it) as usize) == "owner bytes: batches test")
            .map(|it| values.value(it))
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
        use bytes::Bytes;
        use parquet::arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder};

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
                        let _span = tracing::info_span!(
                            target: "blaze",
                            "names round trip",
                            krate = krate.as_str(),
                            function = function.as_str()
                        )
                        .entered();
                        let item = format!("round trip item {t}");
                        tracing::info!(target: "blaze", item = item.as_str(), "round trip event {t}");
                    })
                    .unwrap();
            }
            // Opened here, named and closed on another thread: its names go in as links.
            let span = tracing::info_span!(
                target: "blaze",
                "names round trip moved",
                krate = "round_trip_opened_here",
                function = tracing::field::Empty
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
        let parquet = |batch: RecordBatch| {
            // alloc: a test's file
            let mut bytes = Vec::new();
            let mut writer = ArrowWriter::try_new(&mut bytes, batch.schema(), None).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
            let reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
                .unwrap()
                .build()
                .unwrap();
            let mut batches = reader.map(Result::unwrap);
            let back = batches.next().unwrap();
            assert!(batches.next().is_none(), "a test's rows are one batch");
            assert_eq!(back.num_rows(), batch.num_rows());
            back
        };
        let text = |batch: &RecordBatch, table: &Table, name: &str, row: usize| {
            let column = batch.column(table.column(name).unwrap());
            match column.data_type() {
                arrow_schema::DataType::Dictionary(..) => {
                    let it = column.as_dictionary::<UInt32Type>();
                    let values = it.values().as_string::<i32>();
                    it.keys()
                        .is_valid(row)
                        .then(|| values.value(it.keys().value(row) as usize).to_owned())
                }
                _ => {
                    let it = column.as_string::<i32>();
                    it.is_valid(row).then(|| it.value(row).to_owned())
                }
            }
        };
        let spans = parquet(spans(&read.spans, process).unwrap());
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
        let events = parquet(super::events(&read.events, process).unwrap());
        let mut messages: Vec<String> = (0..events.num_rows())
            .filter_map(|it| text(&events, &schema::EVENTS, "message", it))
            .filter(|it| it.starts_with("round trip event "))
            .collect();
        messages.sort();
        let mut want: Vec<String> = (0..THREADS).map(|t| format!("round trip event {t}")).collect();
        want.sort();
        assert_eq!(messages, want, "each thread's event message");
        let fields = parquet(event_fields(&read.fields, process).unwrap());
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
        let frames: HashMap<u64, Vec<String>, Global> = [
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
        let column = |name: &str| batch.column(schema::SAMPLES.column(name).unwrap());
        let stack = column("stack").as_list::<i32>().value(0);
        let named = stack.as_dictionary::<UInt32Type>();
        let values = named.values().as_string::<i32>();
        let named: Vec<&str> =
            named.keys().values().iter().map(|it| values.value(*it as usize)).collect();
        assert_eq!(named, ["main", "build", "typeck", "typeck::inlined"]);
        let raw = column("stack_pcs").as_list::<i32>().value(0);
        assert_eq!(
            raw.as_primitive::<arrow_array::types::UInt64Type>().values().as_ref(),
            [0x31, 0x21, 0x10]
        );
        assert_eq!(column("span_id").as_primitive::<arrow_array::types::UInt64Type>().value(0), 41);
        assert_eq!(column("build_id").as_primitive::<arrow_array::types::UInt64Type>().value(0), 9);
        let cpu = column("cpu").as_dictionary::<UInt32Type>();
        assert_eq!(cpu.values().as_string::<i32>().value(cpu.keys().value(0) as usize), "off");
        let t_ns = column("t_ns");
        assert_eq!(t_ns.as_primitive::<arrow_array::types::Int64Type>().value(0), 1_000);
        assert_eq!(
            column("job").as_primitive::<arrow_array::types::UInt64Type>().value(0),
            5 << 32 | 3
        );
        assert_eq!(column("fiber").as_primitive::<arrow_array::types::UInt32Type>().value(0), 2);
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
        let batch = samples(&[sample], &HashMap::new_in(Global), (now, 1_000), process).unwrap();
        assert!(batch.column(schema::SAMPLES.column("span_id").unwrap()).is_null(0));
        assert!(batch.column(schema::SAMPLES.column("build_id").unwrap()).is_null(0));
        assert!(batch.column(schema::SAMPLES.column("job").unwrap()).is_null(0));
        assert!(batch.column(schema::SAMPLES.column("fiber").unwrap()).is_null(0));
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
        let column = |name: &str| batch.column(schema::SPANS.column(name).unwrap());
        let u64s = |name: &str| {
            let it = column(name).as_primitive::<arrow_array::types::UInt64Type>();
            [0, 1].map(|row| it.is_valid(row).then(|| it.value(row)))
        };
        let cpu = column("self_cpu_ns").as_primitive::<arrow_array::types::Int64Type>();
        assert_eq!([0, 1].map(|row| cpu.is_valid(row).then(|| cpu.value(row))), [Some(700), None]);
        assert_eq!(u64s("steps"), [Some(4_200_000), None]);
        assert_eq!(u64s("peak_bytes"), [Some(48 << 20), None]);
        let outcome = column("outcome").as_dictionary::<UInt32Type>();
        let values = outcome.values().as_string::<i32>();
        assert_eq!(values.value(outcome.keys().value(0) as usize), "ok");
        assert!(!outcome.keys().is_valid(1));
    }

    #[test]
    fn an_empty_read_is_empty_batches() {
        let [spans, counters] = of(&Read::default(), Process::this()).unwrap();
        assert_eq!((spans.num_rows(), counters.num_rows()), (0, 0));
    }
}
