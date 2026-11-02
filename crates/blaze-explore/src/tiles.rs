//! What the trace view draws zoomed out: the spans of `[t0_ns, t1_ns)` per (process, thread,
//! depth, bucket), a bucket `bucket_ns` wide (about one pixel), as the schema's `tiles` table:
//! how many spans overlap the bucket, how much of it is inside any of them (`busy_ns`, the
//! union, so it never exceeds the bucket), and the longest one's name. The last bucket ends at
//! `t1_ns` and may be narrower.
//!
//! One pass after a sort by (process, thread, depth, start): within a track the spans come in
//! order of their start, so the buckets before a span's first are final once it is reached, and
//! only the buckets a span covers are open at once. The union is the sweep's high-water mark:
//! a span adds only the part of it past the end of every span before it on its track. Cost:
//! the sort (skipped when the rows come sorted, as the `tiles` op's SQL asks for) and one visit
//! per (span, bucket it covers), so the work is the input plus the output.

use std::{collections::VecDeque, sync::Arc};

use arrow_array::{
    Array, ArrayRef, DictionaryArray, Int64Array, LargeStringArray, RecordBatch, StringArray,
    StringViewArray, UInt16Array, UInt32Array, UInt64Array,
    cast::AsArray,
    types::{Int32Type, Int64Type, UInt16Type, UInt32Type, UInt64Type},
};
use arrow_schema::{ArrowError, DataType};
use blaze_hash::HashMap;
use blaze_profile::{batches::schema_of, schema};

/// The most buckets one request may ask for: eight 8K screens across.
pub const MAX_BUCKETS: u64 = 1 << 16;

/// The time range and the bucket width of a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    t0_ns: i64,
    t1_ns: i64,
    bucket_ns: i64,
}

impl Range {
    pub fn new(t0_ns: i64, t1_ns: i64, bucket_ns: i64) -> Result<Range, TilesError> {
        if t1_ns <= t0_ns {
            return Err(TilesError::Range(format!("t1_ns {t1_ns} is not after t0_ns {t0_ns}")));
        }
        if bucket_ns <= 0 {
            return Err(TilesError::Range(format!("bucket_ns {bucket_ns} is not positive")));
        }
        let range = Range { t0_ns, t1_ns, bucket_ns };
        let buckets = range.buckets();
        if buckets > MAX_BUCKETS {
            return Err(TilesError::Range(format!(
                "{buckets} buckets of {bucket_ns} ns in [{t0_ns}, {t1_ns}): at most {MAX_BUCKETS}"
            )));
        }
        Ok(range)
    }

    pub fn t0_ns(&self) -> i64 {
        self.t0_ns
    }

    pub fn t1_ns(&self) -> i64 {
        self.t1_ns
    }

    pub fn bucket_ns(&self) -> i64 {
        self.bucket_ns
    }

    pub fn buckets(&self) -> u64 {
        let width = i128::from(self.t1_ns) - i128::from(self.t0_ns);
        let bucket = i128::from(self.bucket_ns);
        ((width + bucket - 1) / bucket) as u64
    }

    /// The bucket of `at`, in `[t0_ns, t1_ns)`.
    fn bucket_of(&self, at: i64) -> u64 {
        ((i128::from(at) - i128::from(self.t0_ns)) / i128::from(self.bucket_ns)) as u64
    }

    fn start(&self, bucket: u64) -> i64 {
        (i128::from(self.t0_ns) + i128::from(bucket) * i128::from(self.bucket_ns)) as i64
    }

    fn end(&self, bucket: u64) -> i64 {
        let end = i128::from(self.t0_ns) + (i128::from(bucket) + 1) * i128::from(self.bucket_ns);
        end.min(i128::from(self.t1_ns)) as i64
    }
}

/// Why spans could not be made tiles.
#[derive(Debug)]
pub enum TilesError {
    Range(String),
    /// A batch has no column `column`, or not of a type it can be read as.
    Column { column: &'static str, found: Option<DataType> },
    /// A column holds a null or a value out of its schema's range.
    Value { column: &'static str, row: usize },
    Arrow(ArrowError),
}

impl std::fmt::Display for TilesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TilesError::Range(why) => write!(f, "the tiles' range: {why}"),
            TilesError::Column { column, found: Some(ty) } => {
                write!(f, "the spans' column `{column}` is {ty}, not an integer or string column tiles read")
            }
            TilesError::Column { column, found: None } => write!(f, "the spans have no column `{column}`"),
            TilesError::Value { column, row } => {
                write!(f, "the spans' `{column}` at row {row} is null or out of the schema's range")
            }
            TilesError::Arrow(why) => write!(f, "the tiles batch: {why}"),
        }
    }
}

impl std::error::Error for TilesError {}

/// Whose lane a tile is of: a thread's (`tiles`) or a fiber job's (`job_tiles`, from the
/// `job_spans` view, whose `job` column takes the place of `thread`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde_derive::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    #[default]
    Thread,
    Job,
}

impl Lane {
    /// The spans' column that names the lane, and the output's.
    pub const fn column(self) -> &'static str {
        match self {
            Lane::Thread => "thread",
            Lane::Job => "job",
        }
    }

    pub const fn table(self) -> &'static schema::Table {
        match self {
            Lane::Thread => &schema::TILES,
            Lane::Job => &schema::JOB_TILES,
        }
    }
}

/// A span of the range, clipped to it, and its track: (process, thread or job).
#[derive(Clone, Copy, Debug)]
struct Span {
    group: (u32, u64),
    depth: u16,
    s: i64,
    e: i64,
    /// Its whole length, unclipped: what "longest" compares.
    dur: i64,
    name: u32,
}

/// A bucket being summed.
#[derive(Clone, Copy, Debug)]
struct Acc {
    count: u32,
    busy: i64,
    longest: i64,
    name: u32,
}

const EMPTY: Acc = Acc { count: 0, busy: 0, longest: -1, name: 0 };

/// An integer column, read as `i64` whatever its width.
enum Ints<'a> {
    U16(&'a [u16]),
    U32(&'a [u32]),
    I32(&'a [i32]),
    I64(&'a [i64]),
    U64(&'a [u64]),
}

impl Ints<'_> {
    #[inline]
    fn get(&self, row: usize) -> Option<i64> {
        match self {
            Ints::U16(it) => Some(i64::from(it[row])),
            Ints::U32(it) => Some(i64::from(it[row])),
            Ints::I32(it) => Some(i64::from(it[row])),
            Ints::I64(it) => Some(it[row]),
            Ints::U64(it) => i64::try_from(it[row]).ok(),
        }
    }
}

fn column<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a ArrayRef, TilesError> {
    let column = batch.column_by_name(name).ok_or(TilesError::Column { column: name, found: None })?;
    if column.null_count() > 0 {
        let row = (0..column.len()).find(|&it| column.is_null(it)).unwrap_or(0);
        return Err(TilesError::Value { column: name, row });
    }
    Ok(column)
}

fn ints<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<Ints<'a>, TilesError> {
    let array = column(batch, name)?;
    Ok(match array.data_type() {
        DataType::UInt16 => Ints::U16(&array.as_primitive::<UInt16Type>().values()[..]),
        DataType::UInt32 => Ints::U32(&array.as_primitive::<UInt32Type>().values()[..]),
        DataType::Int32 => Ints::I32(&array.as_primitive::<Int32Type>().values()[..]),
        DataType::Int64 => Ints::I64(&array.as_primitive::<Int64Type>().values()[..]),
        DataType::UInt64 => Ints::U64(&array.as_primitive::<UInt64Type>().values()[..]),
        other => {
            // clone: an error path
            return Err(TilesError::Column { column: name, found: Some(other.clone()) });
        }
    })
}

/// The `name` column: the schema's dictionary, or plain strings as DuckDB gives them back.
enum Names<'a> {
    DictU32(&'a [u32], &'a StringArray),
    DictI32(&'a [i32], &'a StringArray),
    Utf8(&'a StringArray),
    Large(&'a LargeStringArray),
    View(&'a StringViewArray),
}

fn names<'a>(batch: &'a RecordBatch) -> Result<Names<'a>, TilesError> {
    let array = column(batch, "name")?;
    // clone: an error path
    let wrong = || TilesError::Column { column: "name", found: Some(array.data_type().clone()) };
    Ok(match array.data_type() {
        DataType::Dictionary(key, _) if **key == DataType::UInt32 => {
            let dict = array.as_dictionary::<UInt32Type>();
            Names::DictU32(&dict.keys().values()[..], dict.values().as_string_opt::<i32>().ok_or_else(wrong)?)
        }
        DataType::Dictionary(key, _) if **key == DataType::Int32 => {
            let dict = array.as_dictionary::<Int32Type>();
            Names::DictI32(&dict.keys().values()[..], dict.values().as_string_opt::<i32>().ok_or_else(wrong)?)
        }
        DataType::Utf8 => Names::Utf8(array.as_string::<i32>()),
        DataType::LargeUtf8 => Names::Large(array.as_string::<i64>()),
        DataType::Utf8View => Names::View(array.as_string_view()),
        _ => return Err(wrong()),
    })
}

/// The names of the output, each once: their number is their key in the output's dictionary.
#[derive(Default)]
struct NameTable<'a> {
    ids: HashMap<&'a str, u32>,
    names: Vec<&'a str>,
}

impl<'a> NameTable<'a> {
    fn id(&mut self, name: &'a str) -> u32 {
        let next = self.names.len() as u32;
        let names = &mut self.names;
        *self.ids.entry(name).or_insert_with(|| {
            names.push(name);
            next
        })
    }
}

/// The columns of the output.
#[derive(Default)]
struct Out {
    process: Vec<u32>,
    lane: Vec<u64>,
    depth: Vec<u16>,
    t0: Vec<i64>,
    t1: Vec<i64>,
    count: Vec<u32>,
    busy: Vec<i64>,
    name: Vec<u32>,
}

impl Out {
    fn push(&mut self, range: Range, group: (u32, u64), depth: u16, bucket: u64, acc: Acc) {
        if acc.count == 0 {
            return;
        }
        self.process.push(group.0);
        self.lane.push(group.1);
        self.depth.push(depth);
        self.t0.push(range.start(bucket));
        self.t1.push(range.end(bucket));
        self.count.push(acc.count);
        self.busy.push(acc.busy);
        self.name.push(acc.name);
    }
}

/// The `tiles` of the spans in `spans` (batches with the schema's `spans` columns `process`,
/// `thread`, `depth`, `name`, `start_ns` and `dur_ns`; others are not read) over `range`.
pub fn tiles(spans: &[RecordBatch], range: Range) -> Result<RecordBatch, TilesError> {
    tiles_by(spans, range, Lane::Thread)
}

/// [`tiles`] with a lane per `lane`: for [`Lane::Job`], the spans have `job` in place of
/// `thread` and the output is `job_tiles`.
pub fn tiles_by(spans: &[RecordBatch], range: Range, lane: Lane) -> Result<RecordBatch, TilesError> {
    let mut table = NameTable::default();
    let rows: usize = spans.iter().map(RecordBatch::num_rows).sum();
    // alloc: once a request, the spans of its range
    let mut kept: Vec<Span> = Vec::with_capacity(rows);
    // alloc: once a request, grown to the largest dictionary of a batch
    let mut keyed: Vec<u32> = Vec::new();
    for batch in spans {
        let (process, lanes, depth) = (ints(batch, "process")?, ints(batch, lane.column())?, ints(batch, "depth")?);
        let (start, dur, names) = (ints(batch, "start_ns")?, ints(batch, "dur_ns")?, names(batch)?);
        keyed.clear();
        if let Names::DictU32(_, values) | Names::DictI32(_, values) = &names {
            keyed.resize(values.len(), u32::MAX);
        }
        for row in 0..batch.num_rows() {
            let at = |column: &'static str| TilesError::Value { column, row };
            let start = start.get(row).ok_or_else(|| at("start_ns"))?;
            let dur = dur.get(row).ok_or_else(|| at("dur_ns"))?.max(0);
            let end = start.saturating_add(dur);
            let inside = start < range.t1_ns && (end > range.t0_ns || (dur == 0 && start >= range.t0_ns));
            if !inside {
                continue;
            }
            let process = process.get(row).and_then(|it| u32::try_from(it).ok()).ok_or_else(|| at("process"))?;
            let of = lanes.get(row).and_then(|it| match lane {
                Lane::Thread => u32::try_from(it).ok().map(u64::from),
                Lane::Job => u64::try_from(it).ok(),
            });
            let of = of.ok_or_else(|| at(lane.column()))?;
            let depth = depth.get(row).and_then(|it| u16::try_from(it).ok()).ok_or_else(|| at("depth"))?;
            let name = match &names {
                Names::DictU32(keys, values) => dict_name(&mut table, &mut keyed, values, keys[row] as usize),
                Names::DictI32(keys, values) => {
                    let key = usize::try_from(keys[row]).map_err(|_| at("name"))?;
                    dict_name(&mut table, &mut keyed, values, key)
                }
                Names::Utf8(it) => Some(table.id(it.value(row))),
                Names::Large(it) => Some(table.id(it.value(row))),
                Names::View(it) => Some(table.id(it.value(row))),
            }
            .ok_or_else(|| at("name"))?;
            kept.push(Span {
                group: (process, of),
                depth,
                s: start.max(range.t0_ns),
                e: end.min(range.t1_ns),
                dur,
                name,
            });
        }
    }
    let order = |it: &Span| (it.group, it.depth, it.s);
    if !kept.is_sorted_by_key(order) {
        kept.sort_unstable_by_key(order);
    }
    let mut out = Out::default();
    // alloc: once a request, the buckets one span covers at most
    let mut open: VecDeque<Acc> = VecDeque::new();
    let mut at = 0;
    while at < kept.len() {
        let track = (kept[at].group, kept[at].depth);
        let len = kept[at..].iter().position(|it| (it.group, it.depth) != track).unwrap_or(kept.len() - at);
        sweep(&kept[at..at + len], range, &mut open, &mut out);
        at += len;
    }
    let values = StringArray::from(table.names);
    let name = DictionaryArray::<UInt32Type>::try_new(UInt32Array::from(out.name), Arc::new(values))
        .map_err(TilesError::Arrow)?;
    let lanes: ArrayRef = match lane {
        // alloc: a column of the answer, once a request
        Lane::Thread => Arc::new(UInt32Array::from_iter_values(out.lane.iter().map(|it| *it as u32))),
        // alloc: a column of the answer, once a request
        Lane::Job => Arc::new(UInt64Array::from(out.lane)),
    };
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt32Array::from(out.process)),
        lanes,
        Arc::new(UInt16Array::from(out.depth)),
        Arc::new(Int64Array::from(out.t0)),
        Arc::new(Int64Array::from(out.t1)),
        Arc::new(UInt32Array::from(out.count)),
        Arc::new(Int64Array::from(out.busy)),
        Arc::new(name),
    ];
    RecordBatch::try_new(schema_of(lane.table()), columns).map_err(TilesError::Arrow)
}

/// The output's number of the name at `key` of a batch's dictionary `values`, looked up once a
/// key a batch.
#[inline]
fn dict_name<'a>(table: &mut NameTable<'a>, keyed: &mut [u32], values: &'a StringArray, key: usize) -> Option<u32> {
    let slot = keyed.get_mut(key)?;
    if *slot == u32::MAX {
        if values.is_null(key) {
            return None;
        }
        *slot = table.id(values.value(key));
    }
    Some(*slot)
}

/// The buckets of one track, its spans in order of their start.
fn sweep(track: &[Span], range: Range, open: &mut VecDeque<Acc>, out: &mut Out) {
    let (group, depth) = (track[0].group, track[0].depth);
    open.clear();
    // The bucket of `open[0]`.
    let mut first = 0u64;
    // The end of the union of the spans so far.
    let mut covered = i64::MIN;
    for span in track {
        let from_bucket = range.bucket_of(span.s);
        let to_bucket = if span.e > span.s { range.bucket_of(span.e - 1) } else { from_bucket };
        while first < from_bucket {
            let Some(acc) = open.pop_front() else { break };
            out.push(range, group, depth, first, acc);
            first += 1;
        }
        if open.is_empty() {
            first = from_bucket;
        }
        while first + (open.len() as u64) <= to_bucket {
            open.push_back(EMPTY);
        }
        let new_from = span.s.max(covered);
        for bucket in from_bucket..=to_bucket {
            let acc = &mut open[(bucket - first) as usize];
            acc.count = acc.count.saturating_add(1);
            if span.dur > acc.longest {
                acc.longest = span.dur;
                acc.name = span.name;
            }
            let (lo, hi) = (range.start(bucket).max(new_from), range.end(bucket).min(span.e));
            if hi > lo {
                acc.busy += hi - lo;
            }
        }
        covered = covered.max(span.e);
    }
    while let Some(acc) = open.pop_front() {
        out.push(range, group, depth, first, acc);
        first += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use arrow_array::cast::AsArray;

    use super::*;

    /// A batch of `(process, thread, depth, name, start_ns, dur_ns)`, names as plain strings.
    fn batch(rows: &[(u32, u32, u16, &str, i64, i64)]) -> RecordBatch {
        RecordBatch::try_from_iter([
            ("process", Arc::new(UInt32Array::from_iter_values(rows.iter().map(|it| it.0))) as ArrayRef),
            ("thread", Arc::new(UInt32Array::from_iter_values(rows.iter().map(|it| it.1)))),
            ("depth", Arc::new(UInt16Array::from_iter_values(rows.iter().map(|it| it.2)))),
            ("name", Arc::new(StringArray::from_iter_values(rows.iter().map(|it| it.3)))),
            ("start_ns", Arc::new(Int64Array::from_iter_values(rows.iter().map(|it| it.4)))),
            ("dur_ns", Arc::new(Int64Array::from_iter_values(rows.iter().map(|it| it.5)))),
        ])
        .unwrap()
    }

    /// The tiles as rows: (process, thread, depth, t0, t1, count, busy, name).
    fn rows(tiles: &RecordBatch) -> Vec<(u32, u32, u16, i64, i64, u32, i64, String)> {
        let u32s = |at: usize| tiles.column(at).as_primitive::<UInt32Type>().values().to_vec();
        let i64s = |at: usize| tiles.column(at).as_primitive::<Int64Type>().values().to_vec();
        let depth = tiles.column(2).as_primitive::<UInt16Type>().values().to_vec();
        let names = tiles.column(7).as_dictionary::<UInt32Type>();
        let values = names.values().as_string::<i32>();
        let (process, thread, t0, t1, count, busy) = (u32s(0), u32s(1), i64s(3), i64s(4), u32s(5), i64s(6));
        (0..tiles.num_rows())
            .map(|i| {
                let name = values.value(names.keys().value(i) as usize).to_owned();
                (process[i], thread[i], depth[i], t0[i], t1[i], count[i], busy[i], name)
            })
            .collect()
    }

    #[test]
    fn spans_become_buckets_with_the_union_busy_and_the_longest_named() {
        let spans = batch(&[
            (7, 1, 0, "a", 0, 100),
            (7, 1, 1, "b", 10, 10),
            (7, 1, 1, "c", 15, 25),
            // Outside the range: left out.
            (7, 1, 1, "late", 60, 5),
            (7, 2, 0, "x", -30, 35),
        ]);
        let tiles = tiles(&[spans], Range::new(0, 50, 10).unwrap()).unwrap();
        assert_eq!(tiles.schema().metadata()["blaze.table"], "tiles");
        let s = |n: &str| n.to_owned();
        assert_eq!(
            rows(&tiles),
            vec![
                (7, 1, 0, 0, 10, 1, 10, s("a")),
                (7, 1, 0, 10, 20, 1, 10, s("a")),
                (7, 1, 0, 20, 30, 1, 10, s("a")),
                (7, 1, 0, 30, 40, 1, 10, s("a")),
                (7, 1, 0, 40, 50, 1, 10, s("a")),
                (7, 1, 1, 10, 20, 2, 10, s("c")),
                (7, 1, 1, 20, 30, 1, 10, s("c")),
                (7, 1, 1, 30, 40, 1, 10, s("c")),
                (7, 2, 0, 0, 10, 1, 5, s("x")),
            ]
        );
    }

    #[test]
    fn the_last_bucket_ends_at_the_range_and_an_instant_counts_where_it_is() {
        let spans = batch(&[(1, 1, 0, "long", 0, 1000), (1, 1, 1, "instant", 22, 0), (1, 1, 1, "at end", 25, 0)]);
        let range = Range::new(0, 25, 10).unwrap();
        assert_eq!(range.buckets(), 3);
        let got = rows(&tiles(&[spans], range).unwrap());
        assert_eq!(got[2], (1, 1, 0, 20, 25, 1, 5, "long".to_owned()));
        assert_eq!(got[3], (1, 1, 1, 20, 25, 1, 0, "instant".to_owned()));
        assert_eq!(got.len(), 4, "an instant at t1_ns is outside the range");
    }

    #[test]
    fn a_range_that_is_empty_backwards_or_too_fine_is_refused() {
        assert!(matches!(Range::new(5, 5, 1), Err(TilesError::Range(_))));
        assert!(matches!(Range::new(5, 1, 1), Err(TilesError::Range(_))));
        assert!(matches!(Range::new(0, 10, 0), Err(TilesError::Range(_))));
        assert!(matches!(Range::new(0, 1 << 20, 1), Err(TilesError::Range(_))));
        assert!(Range::new(i64::MIN, i64::MAX, i64::MAX).is_ok());
        assert_eq!(Range::new(0, MAX_BUCKETS as i64, 1).unwrap().buckets(), MAX_BUCKETS);
    }

    #[test]
    fn names_read_from_a_dictionary_or_strings_and_other_integer_widths() {
        let range = Range::new(0, 100, 50).unwrap();
        let names = DictionaryArray::<UInt32Type>::try_new(
            UInt32Array::from(vec![1, 0]),
            Arc::new(StringArray::from(vec!["first", "second"])),
        )
        .unwrap();
        let dict = RecordBatch::try_from_iter([
            ("process", Arc::new(Int64Array::from(vec![3, 3])) as ArrayRef),
            ("thread", Arc::new(Int64Array::from(vec![4, 4]))),
            ("depth", Arc::new(arrow_array::Int32Array::from(vec![0, 0]))),
            ("name", Arc::new(names)),
            ("start_ns", Arc::new(Int64Array::from(vec![0, 60]))),
            ("dur_ns", Arc::new(Int64Array::from(vec![10, 30]))),
        ])
        .unwrap();
        let plain = batch(&[(3, 4, 0, "second", 20, 20)]);
        let got = rows(&tiles(&[dict, plain], range).unwrap());
        assert_eq!(got, vec![(3, 4, 0, 0, 50, 2, 30, "second".to_owned()), (3, 4, 0, 50, 100, 1, 30, "first".to_owned())]);
        let tiles = tiles(&[batch(&[(1, 1, 0, "a", 0, 1)]), batch(&[(1, 1, 0, "a", 2, 1)])], range).unwrap();
        assert_eq!(tiles.column(7).as_dictionary::<UInt32Type>().values().len(), 1, "a name is in the output once");
    }

    #[test]
    fn a_missing_or_mistyped_column_is_named() {
        let range = Range::new(0, 10, 1).unwrap();
        let no_depth = batch(&[(1, 1, 0, "a", 0, 1)]).project(&[0, 1, 3, 4, 5]).unwrap();
        assert!(matches!(tiles(&[no_depth], range), Err(TilesError::Column { column: "depth", found: None })));
        let floats = RecordBatch::try_from_iter([
            ("process", Arc::new(arrow_array::Float64Array::from(vec![1.0])) as ArrayRef),
            ("thread", Arc::new(UInt32Array::from(vec![1]))),
        ])
        .unwrap();
        assert!(matches!(
            tiles(&[floats], range),
            Err(TilesError::Column { column: "process", found: Some(DataType::Float64) })
        ));
        let huge = batch(&[(1, 1, 0, "a", 0, 1)]);
        let mut columns = huge.columns().to_vec();
        columns[2] = Arc::new(arrow_array::Int32Array::from(vec![70_000]));
        let huge = RecordBatch::try_from_iter(
            ["process", "thread", "depth", "name", "start_ns", "dur_ns"].into_iter().zip(columns),
        )
        .unwrap();
        assert!(matches!(tiles(&[huge], range), Err(TilesError::Value { column: "depth", row: 0 })));
    }

    /// Random tracks of overlapping, nested, instant and out-of-range spans, against a count of
    /// each bucket by brute force: the counts, the union and the longest name agree, busy never
    /// exceeds its bucket, and the counts add up to the buckets each span covers.
    #[test]
    fn tiles_agree_with_brute_force_on_random_spans() {
        let mut rng = blaze_rng::SplitMix64::new(0x7113_5eed);
        for case in 0..300 {
            let t0 = rng.range(0..2_000) as i64 - 1_000;
            let width = rng.range(1..5_000) as i64;
            let bucket = rng.range(1..(width as u64 / 2 + 2)) as i64;
            let Ok(range) = Range::new(t0, t0 + width, bucket) else { continue };
            let n = rng.below(60) as usize;
            // alloc: test input
            let names: Vec<String> = (0..n).map(|it| format!("s{it}")).collect();
            let spans: Vec<(u32, u32, u16, &str, i64, i64)> = (0..n)
                .map(|i| {
                    let start = t0 - 500 + rng.below(width as u64 + 1_000) as i64;
                    let dur = if rng.one_in(6) { 0 } else { rng.below(width as u64) as i64 };
                    (1 + rng.below(2) as u32, 10 + rng.below(2) as u32, rng.below(3) as u16, names[i].as_str(), start, dur)
                })
                .collect();
            let half = spans.len() / 2;
            let got = tiles(&[batch(&spans[..half]), batch(&spans[half..])], range).unwrap();
            let got = rows(&got);
            let dur_of: BTreeMap<&str, i64> = spans.iter().map(|it| (it.3, it.5)).collect();
            // The brute force: every bucket of every track, every span tested against it.
            let mut want = BTreeMap::new();
            let mut covered_buckets = 0u64;
            for b in 0..range.buckets() {
                let (bt0, bt1) = (range.start(b), range.end(b));
                for &(p, t, d, _, start, dur) in &spans {
                    let (s, e) = (start.max(t0), (start + dur).min(t0 + width));
                    let inside = start < t0 + width && (start + dur > t0 || (dur == 0 && start >= t0));
                    if inside && (if e > s { s < bt1 && e > bt0 } else { (bt0..bt1).contains(&s) }) {
                        covered_buckets += 1;
                        let entry = want.entry((p, t, d, bt0)).or_insert((bt1, 0u32, Vec::new(), -1i64));
                        entry.1 += 1;
                        if e > s {
                            entry.2.push((s.max(bt0), e.min(bt1)));
                        }
                        entry.3 = entry.3.max(dur);
                    }
                }
            }
            assert_eq!(got.len(), want.len(), "case {case}: one row a bucket with a span");
            let mut counted = 0u64;
            for (p, t, d, bt0, bt1, count, busy, name) in &got {
                let (want_t1, want_count, intervals, longest) = &want[&(*p, *t, *d, *bt0)];
                let mut intervals = intervals.clone();
                intervals.sort_unstable();
                let (mut union, mut until) = (0, i64::MIN);
                for (lo, hi) in intervals {
                    let lo = lo.max(until);
                    if hi > lo {
                        union += hi - lo;
                    }
                    until = until.max(hi);
                }
                assert_eq!((bt1, count, busy), (want_t1, want_count, &union), "case {case}");
                assert!(*busy >= 0 && *busy <= bt1 - bt0, "case {case}: busy within the bucket");
                assert_eq!(dur_of[name.as_str()], *longest, "case {case}: named for a longest span");
                counted += u64::from(*count);
            }
            assert_eq!(counted, covered_buckets, "case {case}: the counts add up");
        }
    }
}
