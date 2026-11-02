//! The flight recorder's rows ([`flight::Read`]) as Arrow record batches of the schema's tables:
//! what the Parquet history writes, the live feed sends as IPC, and the explorer reads. Names
//! the ring keeps as numbers (sites and [`flight::Sym`]s) are resolved here, once per distinct
//! number a batch, into each dictionary column's values.

use std::{
    sync::{Arc, OnceLock},
    time::Instant,
};

use arrow_array::{
    builder::{ListBuilder, StringDictionaryBuilder},
    Array, ArrayRef, DictionaryArray, Float64Array, Int64Array, RecordBatch, StringArray, UInt16Array,
    UInt32Array, UInt64Array, types::UInt32Type,
};
use arrow_schema::{ArrowError, DataType, SchemaRef};
use blaze_hash::HashMap;

use crate::{
    flight::{self, CounterRow, Read, SpanRow, SwitchRow, ThreadNow, WaitedOn},
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
    keys_of: HashMap<u64, u32>,
    values: Vec<std::sync::Arc<str>>,
}

impl Dict {
    fn new() -> Dict {
        // alloc: a dictionary's values, once a column a batch
        Dict { keys_of: HashMap::default(), values: Vec::new() }
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
    let (mut threads, mut names, mut targets, mut kinds) = (Dict::new(), Dict::new(), Dict::new(), Dict::new());
    // alloc: a batch's key columns, once a read, never per span written
    let (mut thread_keys, mut name_keys, mut target_keys, mut kind_keys) =
        (Vec::with_capacity(rows), Vec::with_capacity(rows), Vec::with_capacity(rows), Vec::with_capacity(rows));
    for row in spans {
        let meta = flight::meta(row.site);
        let name = meta.map_or("?", |it| it.name());
        thread_keys.push(threads.key(u64::from(row.thread_name), || {
            flight::name(row.thread_name).unwrap_or_else(|| "?".into())
        }));
        name_keys.push(names.key(u64::from(row.site), || name));
        target_keys.push(targets.key(u64::from(row.site), || meta.map_or("?", |it| it.target())));
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
        column(UInt64Array::from_iter(spans.iter().map(|it| (it.parent != 0).then_some(it.parent)))),
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
                    (word != 0).then(|| values.key(word, || (cells.text)(word).unwrap_or_else(|| "?".into())))
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
    let name_keys =
        UInt32Array::from_iter_values(counters.iter().map(|it| names.key(it.plot as u64, || it.plot.name())));
    let mut kinds = Dict::new();
    let kind_keys = UInt32Array::from_iter(counters.iter().map(|it| {
        (it.query_kind != 0).then(|| {
            kinds.key(u64::from(it.query_kind), || flight::name(it.query_kind).unwrap_or_else(|| "?".into()))
        })
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
        kinds.finish(kind_keys)?,
    ];
    RecordBatch::try_new(schema_of(&schema::COUNTERS), columns)
}

/// The `open` rows of `threads`, read at `at_ns`, all from `process`.
pub fn open(threads: &[ThreadNow], at_ns: i64, process: Process<'_>) -> Result<RecordBatch, ArrowError> {
    let rows = threads.iter().map(|it| it.open.len()).sum();
    let spans = || threads.iter().flat_map(|thread| thread.open.iter().enumerate().map(move |it| (thread, it)));
    // alloc: a batch's list of columns, once a read of the threads
    let columns: Vec<ArrayRef> = vec![
        column(UInt32Array::from_value(process.pid, rows)),
        column(UInt32Array::from_iter_values(spans().map(|(thread, _)| thread.tid as u32))),
        dict(spans().map(|(thread, _)| thread.name.as_str())),
        column(UInt16Array::from_iter_values(spans().map(|(_, (depth, _))| depth as u16))),
        dict(spans().map(|(_, (_, span))| span.name)),
        dict(spans().map(|(_, (_, span))| span.target)),
        column(Int64Array::from_iter_values(spans().map(|(_, (_, span))| at_ns - span.for_ns as i64))),
        column(spans().map(|(_, (_, span))| span.krate.as_deref()).collect::<DictionaryArray<UInt32Type>>()),
        column(StringArray::from_iter(spans().map(|(_, (_, span))| span.function.as_deref()))),
        column(StringArray::from_iter(spans().map(|(_, (_, span))| span.unit.as_deref()))),
        column(StringArray::from_iter(spans().map(|(_, (_, span))| span.awaits.as_deref()))),
        column(UInt64Array::from_iter(spans().map(|(_, (_, span))| span.owner))),
    ];
    RecordBatch::try_new(schema_of(&schema::OPEN), columns)
}

/// The `samples` rows of `samples`, each frame named by `names` (which has every address in
/// them), all from `process`. `at` is one reading of both clocks: an `Instant` and the schema's
/// nanoseconds then ([`crate::clock::mono_ns`]).
pub(crate) fn samples(
    samples: &[Sample],
    names: &HashMap<u64, String>,
    at: (Instant, i64),
    process: Process<'_>,
) -> Result<RecordBatch, ArrowError> {
    let rows = samples.len();
    let schema = schema_of(&schema::SAMPLES);
    let stack = schema::SAMPLES.column("stack").unwrap_or_else(|| unreachable!("samples has a stack"));
    let DataType::List(frame) = schema.field(stack).data_type() else {
        unreachable!("a stack is a list (arrow.rs)")
    };
    // clone: an `Arc` of the schema's field, once a batch
    let mut stacks = ListBuilder::new(StringDictionaryBuilder::<UInt32Type>::new()).with_field(frame.clone());
    for sample in samples {
        // Outermost first; the sampler keeps the innermost first.
        for pc in sample.pcs[..usize::from(sample.depth)].iter().rev() {
            stacks.values().append_value(names.get(pc).map_or("?", String::as_str));
        }
        stacks.append(true);
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
    ];
    RecordBatch::try_new(schema, columns)
}

/// The `switches` rows of `switches`, all from `process`.
pub fn switches(switches: &[SwitchRow], process: Process<'_>) -> Result<RecordBatch, ArrowError> {
    let rows = switches.len();
    let (mut threads, mut events, mut kinds) = (Dict::new(), Dict::new(), Dict::new());
    // alloc: a batch's keys, once a read
    let (mut thread_keys, mut event_keys, mut kind_keys) =
        (Vec::with_capacity(rows), Vec::with_capacity(rows), Vec::with_capacity(rows));
    for row in switches {
        thread_keys.push(threads.key(u64::from(row.thread_name), || flight::name(row.thread_name).unwrap_or_else(|| "?".into())));
        event_keys.push(events.key(row.event as u64, || row.event.name()));
        kind_keys.push(row.waited.map(|(on, _)| kinds.key(on as u64, || on.name())));
    }
    let value = |row: &SwitchRow| row.waited.filter(|(on, _)| *on != WaitedOn::Moved).map(|(_, value)| value);
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
    ];
    RecordBatch::try_new(schema_of(&schema::SWITCHES), columns)
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
        assert_eq!(column("build_id").as_primitive::<arrow_array::types::UInt64Type>().value(at), 9);
        let plotted = counters.column(schema::COUNTERS.column("value").unwrap());
        assert!(plotted.as_primitive::<arrow_array::types::Float64Type>().values().contains(&4.0));
    }

    #[test]
    fn a_samples_stack_is_outermost_first_and_named() {
        let now = Instant::now();
        let mut pcs = [0; crate::sampler::DEPTH];
        pcs[..3].copy_from_slice(&[0x30, 0x20, 0x10]);
        let sample = Sample { at: now, tid: 12, depth: 3, pcs, span: 0, build: 0, on_cpu: true };
        let names: HashMap<u64, String> = [(0x10, "main"), (0x20, "build"), (0x30, "typeck")]
            .into_iter()
            .map(|(pc, it)| (pc, it.to_owned()))
            .collect();
        let process = Process { pid: 7, name: "blaze serve", commit: None };
        let batch = samples(&[sample], &names, (now, 1_000), process).unwrap();
        assert_eq!(batch.num_rows(), 1);
        let stack = batch.column(schema::SAMPLES.column("stack").unwrap()).as_list::<i32>().value(0);
        let frames = stack.as_dictionary::<UInt32Type>();
        let values = frames.values().as_string::<i32>();
        let named: Vec<&str> = frames.keys().values().iter().map(|it| values.value(*it as usize)).collect();
        assert_eq!(named, ["main", "build", "typeck"]);
        let t_ns = batch.column(schema::SAMPLES.column("t_ns").unwrap());
        assert_eq!(t_ns.as_primitive::<arrow_array::types::Int64Type>().value(0), 1_000);
    }

    #[test]
    fn a_runs_cpu_steps_peak_and_outcome_are_its_columns_and_null_where_unsaid() {
        use crate::span::{Fields as _, MacroExpanded, Outcome, Said, col};
        let mut typed = Said::EMPTY;
        MacroExpanded { steps: Some(4_200_000), peak_bytes: Some(48 << 20), outcome: Outcome::Ok }.write(&mut typed);
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
            function: flight::intern("serde_derive::Serialize"),
            unit: 0,
            self_cpu_ns: Some(700),
            typed: typed.typed,
        };
        assert_eq!(said.get::<col::Outcome>(), Some(Outcome::Ok));
        let unsaid = SpanRow { span_id: 12, self_cpu_ns: None, typed: Said::EMPTY.typed, ..said };
        let batch = spans(&[said, unsaid], Process { pid: 7, name: "blaze serve", commit: None }).unwrap();
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
