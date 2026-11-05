//! What a build did that its spans don't say: how each image it linked was linked (`links`),
//! the crate graph it built (`crate_deps`) and what the query engine counted of each query
//! function (`query_counts`), so "patched or relinked", the crates' edges and "what did eviction
//! cost" are queries over the history like the rest. The daemon notes them with
//! [`crate::history::History::linked`], [`crate::history::History::crate_graph`] and
//! [`crate::history::History::query_counted`]; the next flush writes them. What a build
//! computed again, and why (`misses`), is noted with [`crate::history::History::missed`], the
//! parts of each such key (`key_parts`) with [`crate::history::History::keyed`], and why the
//! query engine ran each run (`provenance`) with [`crate::history::History::ran`].
//!
//! Every child process a driver reaped (`processes`) comes through [`observe_children`]: the
//! driver's reap hands it to [`reaped`] on whatever thread reaped it, which queues it in
//! [`CHILDREN`] for the next flush to take ([`take_processes`]). The tests a build ran
//! (`test_runs`) come with its end, as its links do ([`crate::history::History::tests_ran`]).

use arrow_array::{
    ArrayRef, DictionaryArray, Int64Array, RecordBatch, StringArray, UInt32Array, UInt64Array,
    types::UInt32Type,
};
use arrow_schema::ArrowError;

use crate::{batches, schema};

/// How an image was linked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The image kept from the last link was patched in place.
    Patched,
    /// The image was laid out whole: a first link, or one the kept image could not take.
    Placed,
}

impl LinkOutcome {
    pub const fn name(self) -> &'static str {
        match self {
            LinkOutcome::Patched => "patched",
            LinkOutcome::Placed => "placed",
        }
    }
}

/// A row of the schema's `links` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRow {
    pub build_id: u64,
    /// The image: the target as cargo names it, `blaze (bin)`.
    pub image: String,
    /// The package it is of.
    pub krate: Option<String>,
    pub outcome: LinkOutcome,
    pub dur_ns: Option<i64>,
}

/// Which of a package's dependency tables an edge is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepKind {
    Normal,
    Build,
    Dev,
}

impl DepKind {
    pub const fn name(self) -> &'static str {
        match self {
            DepKind::Normal => "normal",
            DepKind::Build => "build",
            DepKind::Dev => "dev",
        }
    }
}

/// A row of the schema's `crate_deps` table: `krate` depends on `dep`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrateDepRow {
    pub build_id: u64,
    pub krate: String,
    pub dep: String,
    pub kind: DepKind,
}

/// A row of the schema's `query_counts` table: what the query engine counted of one query
/// function by the end of a build (`blaze_query::db::Counts`, field for field).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryCountRow {
    pub build_id: u64,
    pub query: &'static str,
    pub executed: u64,
    pub verified: u64,
    pub reran: u64,
    pub evicted_asked: u64,
    pub evicted: u64,
    pub dropped: u64,
    pub reads_recorded: u64,
    pub reads_kept: u64,
    pub reads_most: u64,
    /// Of an input: the times a setter wrote one of its fields.
    pub written: u64,
    /// Of `executed`, by cause: first runs, a read changed, an untracked read, a cycle.
    pub ran_new: u64,
    pub ran_changed: u64,
    pub ran_untracked: u64,
    pub ran_cycle: u64,
}

/// What a `provenance` row says happened ([`ProvenanceRow::outcome`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// The memo ran.
    Ran,
    /// The memo's run made a read the engine can't see.
    UntrackedRead,
}

impl RunOutcome {
    pub const fn name(self) -> &'static str {
        match self {
            RunOutcome::Ran => "ran",
            RunOutcome::UntrackedRead => "untracked read",
        }
    }
}

/// A row of the schema's `provenance` table: one entry of the query engine's run log
/// (`blaze_query::db::LoggedRun`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvenanceRow {
    pub build_id: u64,
    /// The memo, `` `query`(Id(n)) ``.
    pub query_key: String,
    pub kind: &'static str,
    pub outcome: RunOutcome,
    /// For a run: changed, evicted, untracked or cycle.
    pub cause: Option<&'static str>,
    pub view: u32,
    /// For changed: the read that changed, named as `query_key` names a memo.
    pub cause_key: Option<String>,
    /// What `cause_key` is: input, tracked or function.
    pub cause_kind: Option<&'static str>,
    /// For an untracked read: its name.
    pub what: Option<&'static str>,
}

/// What the schema's `key_parts` table holds of one thing a build computed again: a row each
/// part of its key (`blaze_kernel::Part`), or one with no part when the key exposes none. Kept a
/// miss at a time, its item and key once rather than once a part, and made rows only as a
/// flush's batch is ([`key_parts`]): a cold `blaze test --workspace` computed 1.79M dev
/// templates, 12.7M parts, each part row with its own copy of its item and key, 9 to 15 GiB of
/// the daemon's heap at the build's end (#1726).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyedMiss {
    pub build_id: u64,
    pub step: &'static str,
    pub item: String,
    /// The whole key, hex.
    pub key: String,
    /// Its key's parts, each a row.
    pub parts: Vec<KeyPart>,
    pub beyond: Option<&'static str>,
}

/// One part of a [`KeyedMiss`]'s key: a `key_parts` row's own columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPart {
    pub name: String,
    /// The part's hash, hex.
    pub hash: Option<String>,
    pub names: Option<String>,
}

/// A row of the schema's `misses` table: one thing a build computed again, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissRow {
    pub build_id: u64,
    pub step: String,
    pub item: String,
    /// The cause's name, as the table's `cause` column lists them.
    pub cause: &'static str,
    pub what: Option<String>,
    pub names: Option<String>,
    pub was: Option<String>,
    pub now: Option<String>,
    pub changed: Option<u32>,
    pub of: Option<u32>,
    pub since: Option<u64>,
}

/// A row of the schema's `processes` table: one child process blaze spawned and reaped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessRow {
    /// The build it worked for, 0 for none.
    pub build_id: u64,
    pub pid: u32,
    /// `build_script`, `test`, `run`, `proc_macro_host`, `linker`, `wrapper`, or `unlabelled`.
    pub kind: &'static str,
    pub unit: Option<String>,
    pub program: String,
    /// The span it was spawned inside, 0 for none.
    pub parent_span: u64,
    /// On the schema's clock ([`crate::clock::mono_ns`]).
    pub spawned_ns: i64,
    pub exec_ns: Option<i64>,
    pub exited_ns: i64,
    pub sandbox_ns: Option<i64>,
    pub user_ns: i64,
    pub system_ns: i64,
    pub peak_rss: Option<u64>,
    pub minor_faults: Option<u64>,
    pub major_faults: Option<u64>,
    pub voluntary_switches: Option<u64>,
    pub involuntary_switches: Option<u64>,
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// How a test ended, as `test_runs.outcome` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestOutcome {
    Passed,
    Failed,
    Ignored,
    TimedOut,
    /// A signal ended it, or it could not be run or its result had.
    Crashed,
}

impl TestOutcome {
    pub const fn name(self) -> &'static str {
        match self {
            TestOutcome::Passed => "passed",
            TestOutcome::Failed => "failed",
            TestOutcome::Ignored => "ignored",
            TestOutcome::TimedOut => "timed_out",
            TestOutcome::Crashed => "crashed",
        }
    }
}

/// A row of the schema's `test_runs` table: one test the test queue finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestRunRow {
    pub build_id: u64,
    pub test: String,
    pub krate: Option<String>,
    pub binary: String,
    pub outcome: TestOutcome,
    pub dur_ns: i64,
    /// Attempts after the first.
    pub retries: u32,
    /// When the queue took its outcome, on the schema's clock.
    pub ended_ns: i64,
}

/// The children reaped since the last flush took them: pushed by whichever thread reaped one,
/// taken by the history's flush. Unbounded as a queue, bounded in practice by the children a
/// flush interval reaps; a process with no history never installs the observer that fills it.
static CHILDREN: crossbeam_queue::SegQueue<ProcessRow> = crossbeam_queue::SegQueue::new();

/// Has every child a driver reaps from now on queued as a `processes` row, each label naming
/// the build this thread works for and the span it is inside ([`crate::flight::current_build`],
/// [`crate::flight::giving_span`]). Once a process; called by [`crate::install`].
pub fn observe_children() {
    blaze_core::reaped::observe(context, reaped);
}

fn context() -> (u64, u64) {
    (u64::from(crate::flight::current_build()), crate::flight::giving_span())
}

/// A child reaped, as its row, queued for the next flush.
pub fn reaped(child: &blaze_core::reaped::Reaped<'_>) {
    CHILDREN.push(process_row(child));
}

/// The row of `child`, its instants on the schema's clock.
pub fn process_row(child: &blaze_core::reaped::Reaped<'_>) -> ProcessRow {
    let ns = |it: std::time::Duration| i64::try_from(it.as_nanos()).unwrap_or(i64::MAX);
    let (kind, unit, build_id, parent_span) = match &child.label {
        // clone: the unit's name, once per reaped child
        Some(label) => (label.kind, label.unit.clone(), label.build, label.span),
        None => ("unlabelled", None, 0, 0),
    };
    ProcessRow {
        build_id,
        pid: child.pid,
        kind,
        unit,
        program: child.program.display().to_string(), // alloc: the program's path, once per reaped child
        parent_span,
        spawned_ns: crate::clock::mono_of(child.spawned),
        exec_ns: Some(crate::clock::mono_of(child.executed)),
        exited_ns: crate::clock::mono_of(child.exited),
        sandbox_ns: child.sandbox.map(ns),
        user_ns: ns(child.user),
        system_ns: ns(child.system),
        peak_rss: Some(child.peak_rss),
        minor_faults: Some(child.minor_faults),
        major_faults: Some(child.major_faults),
        voluntary_switches: Some(child.voluntary_switches),
        involuntary_switches: Some(child.involuntary_switches),
        code: child.code,
        signal: child.signal,
    }
}

/// Every child queued since the last take, oldest first, into `into`.
pub fn take_processes(into: &mut Vec<ProcessRow>) {
    while let Some(row) = CHILDREN.pop() {
        into.push(row);
    }
}

/// The `processes` rows of `rows`, all reaped by the process `pid`.
pub fn processes(rows: &[ProcessRow], pid: u32) -> Result<RecordBatch, ArrowError> {
    let int =
        |of: fn(&ProcessRow) -> Option<i64>| column(Int64Array::from_iter(rows.iter().map(of)));
    let count =
        |of: fn(&ProcessRow) -> Option<u64>| column(UInt64Array::from_iter(rows.iter().map(of)));
    let word = |it: Option<i32>| it.map(|it| it as u32);
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        count(|it| (it.build_id != 0).then_some(it.build_id)),
        column(UInt32Array::from_value(pid, rows.len())),
        column(UInt32Array::from_iter_values(rows.iter().map(|it| it.pid))),
        dict(rows.iter().map(|it| Some(it.kind))),
        column(StringArray::from_iter(rows.iter().map(|it| it.unit.as_deref()))),
        column(StringArray::from_iter_values(rows.iter().map(|it| it.program.as_str()))),
        count(|it| (it.parent_span != 0).then_some(it.parent_span)),
        column(Int64Array::from_iter_values(rows.iter().map(|it| it.spawned_ns))),
        int(|it| it.exec_ns),
        column(Int64Array::from_iter_values(rows.iter().map(|it| it.exited_ns))),
        int(|it| it.sandbox_ns),
        column(Int64Array::from_iter_values(rows.iter().map(|it| it.user_ns))),
        column(Int64Array::from_iter_values(rows.iter().map(|it| it.system_ns))),
        count(|it| it.peak_rss),
        count(|it| it.minor_faults),
        count(|it| it.major_faults),
        count(|it| it.voluntary_switches),
        count(|it| it.involuntary_switches),
        column(UInt32Array::from_iter(rows.iter().map(|it| word(it.code)))),
        column(UInt32Array::from_iter(rows.iter().map(|it| word(it.signal)))),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::PROCESSES), columns)
}

/// The `test_runs` rows of `rows`.
pub fn test_runs(rows: &[TestRunRow]) -> Result<RecordBatch, ArrowError> {
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.build_id))),
        column(StringArray::from_iter_values(rows.iter().map(|it| it.test.as_str()))),
        dict(rows.iter().map(|it| it.krate.as_deref())),
        column(StringArray::from_iter_values(rows.iter().map(|it| it.binary.as_str()))),
        dict(rows.iter().map(|it| Some(it.outcome.name()))),
        column(Int64Array::from_iter_values(rows.iter().map(|it| it.dur_ns))),
        column(UInt32Array::from_iter_values(rows.iter().map(|it| it.retries))),
        column(Int64Array::from_iter_values(rows.iter().map(|it| it.ended_ns))),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::TEST_RUNS), columns)
}

/// The `links` rows of `rows`.
pub fn links(rows: &[LinkRow]) -> Result<RecordBatch, ArrowError> {
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.build_id))),
        column(StringArray::from_iter_values(rows.iter().map(|it| it.image.as_str()))),
        dict(rows.iter().map(|it| it.krate.as_deref())),
        dict(rows.iter().map(|it| Some(it.outcome.name()))),
        column(Int64Array::from_iter(rows.iter().map(|it| it.dur_ns))),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::LINKS), columns)
}

/// The `crate_deps` rows of `rows`.
pub fn crate_deps(rows: &[CrateDepRow]) -> Result<RecordBatch, ArrowError> {
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.build_id))),
        dict(rows.iter().map(|it| Some(it.krate.as_str()))),
        dict(rows.iter().map(|it| Some(it.dep.as_str()))),
        dict(rows.iter().map(|it| Some(it.kind.name()))),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::CRATE_DEPS), columns)
}

/// The `query_counts` rows of `rows`.
pub fn query_counts(rows: &[QueryCountRow]) -> Result<RecordBatch, ArrowError> {
    let count =
        |of: fn(&QueryCountRow) -> u64| column(UInt64Array::from_iter_values(rows.iter().map(of)));
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        count(|it| it.build_id),
        dict(rows.iter().map(|it| Some(it.query))),
        count(|it| it.executed),
        count(|it| it.verified),
        count(|it| it.reran),
        count(|it| it.evicted_asked),
        count(|it| it.evicted),
        count(|it| it.dropped),
        count(|it| it.reads_recorded),
        count(|it| it.reads_kept),
        count(|it| it.reads_most),
        count(|it| it.written),
        count(|it| it.ran_new),
        count(|it| it.ran_changed),
        count(|it| it.ran_untracked),
        count(|it| it.ran_cycle),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::QUERY_COUNTS), columns)
}

/// The `provenance` rows of `rows`.
pub fn provenance(rows: &[ProvenanceRow]) -> Result<RecordBatch, ArrowError> {
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.build_id))),
        column(StringArray::from_iter_values(rows.iter().map(|it| it.query_key.as_str()))),
        dict(rows.iter().map(|it| Some(it.kind))),
        dict(rows.iter().map(|it| Some(it.outcome.name()))),
        column(StringArray::from_iter(rows.iter().map(|it| it.cause))),
        column(UInt32Array::from_iter_values(rows.iter().map(|it| it.view))),
        column(StringArray::from_iter(rows.iter().map(|it| it.cause_key.as_deref()))),
        dict(rows.iter().map(|it| it.cause_kind)),
        column(StringArray::from_iter(rows.iter().map(|it| it.what))),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::PROVENANCE), columns)
}

/// The `key_parts` rows of `rows`.
pub fn key_parts(misses: &[KeyedMiss]) -> Result<RecordBatch, ArrowError> {
    // Each miss's rows: its parts, or one with none when its key exposes none.
    let rows = || {
        misses.iter().flat_map(|miss| {
            let none = miss.parts.is_empty().then_some(None);
            none.into_iter().chain(miss.parts.iter().map(Some)).map(move |part| (miss, part))
        })
    };
    let text = |of: fn(Option<&KeyPart>) -> Option<&str>| {
        column(StringArray::from_iter(rows().map(|(_, part)| of(part))))
    };
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter_values(rows().map(|(miss, _)| miss.build_id))),
        dict(rows().map(|(miss, _)| Some(miss.step))),
        column(StringArray::from_iter_values(rows().map(|(miss, _)| miss.item.as_str()))),
        column(StringArray::from_iter_values(rows().map(|(miss, _)| miss.key.as_str()))),
        text(|part| part.map(|it| it.name.as_str())),
        text(|part| part.and_then(|it| it.hash.as_deref())),
        text(|part| part.and_then(|it| it.names.as_deref())),
        column(StringArray::from_iter(rows().map(|(miss, _)| miss.beyond))),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::KEY_PARTS), columns)
}

/// The `misses` rows of `rows`.
pub fn misses(rows: &[MissRow]) -> Result<RecordBatch, ArrowError> {
    let text =
        |of: fn(&MissRow) -> Option<&str>| column(StringArray::from_iter(rows.iter().map(of)));
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.build_id))),
        dict(rows.iter().map(|it| Some(it.step.as_str()))),
        column(StringArray::from_iter_values(rows.iter().map(|it| it.item.as_str()))),
        dict(rows.iter().map(|it| Some(it.cause))),
        text(|it| it.what.as_deref()),
        text(|it| it.names.as_deref()),
        text(|it| it.was.as_deref()),
        text(|it| it.now.as_deref()),
        column(UInt32Array::from_iter(rows.iter().map(|it| it.changed))),
        column(UInt32Array::from_iter(rows.iter().map(|it| it.of))),
        column(UInt64Array::from_iter(rows.iter().map(|it| it.since))),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::MISSES), columns)
}

/// The most text one batch of a fact table holds. Arrow's `Utf8` columns address their bytes
/// with `i32` offsets, so one column past 2 GiB is an "offset overflow"; a cold workspace
/// build's `key_parts` passed that in one build (#1726). A quarter of a GiB a batch keeps every
/// column far below it, whatever share of a row's text one column holds.
pub const BATCH_TEXT_BYTES: usize = 256 << 20;

/// `rows` as consecutive runs, in order, each holding at most `most` bytes of text by
/// `text_bytes` (a row larger than `most` alone in its run): every row in exactly one run.
pub fn runs<'a, T>(
    rows: &'a [T],
    text_bytes: impl Fn(&T) -> usize + 'a,
    most: usize,
) -> impl Iterator<Item = &'a [T]> + 'a {
    let mut rest = rows;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut bytes = 0usize;
        let mut end = 0;
        while let Some(row) = rest.get(end) {
            let next = bytes.saturating_add(text_bytes(row));
            if end > 0 && next > most {
                break;
            }
            bytes = next;
            end += 1;
        }
        let (run, after) = rest.split_at(end);
        rest = after;
        Some(run)
    })
}

/// `rows` as batches of `batch`, split by [`runs`] at `most` bytes of text (the flush passes
/// [`BATCH_TEXT_BYTES`]); no rows is one empty batch, so every table still has its part.
pub fn in_batches<T>(
    rows: &[T],
    text_bytes: impl Fn(&T) -> usize,
    most: usize,
    batch: impl Fn(&[T]) -> Result<RecordBatch, ArrowError>,
) -> Result<Vec<RecordBatch>, ArrowError> {
    if rows.is_empty() {
        return Ok(vec![batch(rows)?]); // alloc: a flush's parts, once a flush
    }
    // alloc: a flush's parts, once a flush
    runs(rows, text_bytes, most).map(batch).collect()
}

fn text_len(text: Option<&str>) -> usize {
    text.map_or(0, str::len)
}

impl ProvenanceRow {
    /// The bytes of text this row puts in its batch's string columns.
    pub fn text_bytes(&self) -> usize {
        self.query_key.len()
            + self.kind.len()
            + text_len(self.cause)
            + text_len(self.cause_key.as_deref())
            + text_len(self.cause_kind)
            + text_len(self.what)
    }
}

impl KeyedMiss {
    /// The bytes of text its rows put in their batch's string columns.
    pub fn text_bytes(&self) -> usize {
        let rows = self.parts.len().max(1);
        let each = self.step.len() + self.item.len() + self.key.len() + text_len(self.beyond);
        let parts: usize = self
            .parts
            .iter()
            .map(|it| it.name.len() + text_len(it.hash.as_deref()) + text_len(it.names.as_deref()))
            .sum();
        rows * each + parts
    }
}

impl MissRow {
    /// The bytes of text this row puts in its batch's string columns.
    pub fn text_bytes(&self) -> usize {
        self.step.len()
            + self.item.len()
            + self.cause.len()
            + text_len(self.what.as_deref())
            + text_len(self.names.as_deref())
            + text_len(self.was.as_deref())
            + text_len(self.now.as_deref())
    }
}

fn column(array: impl arrow_array::Array + 'static) -> ArrayRef {
    // alloc: a batch's column, once a flush
    std::sync::Arc::new(array)
}

fn dict<'a>(values: impl Iterator<Item = Option<&'a str>>) -> ArrayRef {
    column(values.collect::<DictionaryArray<UInt32Type>>())
}

#[cfg(test)]
mod tests {
    use arrow_array::{Array, cast::AsArray};

    use super::*;

    /// Every row lands in exactly one run, in order, and a run holds more than `most` bytes of
    /// text only when it is a single row: what keeps one flush's `key_parts` under the 2 GiB a
    /// `Utf8` column holds (#1726).
    #[hegel::test(test_cases = 300)]
    fn runs_keep_every_row_once_in_order_under_the_cap(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let sizes: Vec<usize> =
            tc.draw(gs::vecs(gs::integers::<usize>().max_value(1 << 12)).max_size(200));
        let most = tc.draw(gs::integers::<usize>().max_value(1 << 13));
        let rows: Vec<(usize, usize)> = sizes.iter().copied().enumerate().collect();
        let mut seen = Vec::new();
        for run in runs(&rows, |it| it.1, most) {
            assert!(!run.is_empty(), "a run holds a row");
            let bytes: usize = run.iter().map(|it| it.1).sum();
            assert!(run.len() == 1 || bytes <= most, "{bytes} bytes in {} rows over {most}", run.len());
            seen.extend(run.iter().map(|it| it.0));
        }
        assert_eq!(seen, (0..rows.len()).collect::<Vec<_>>());
    }

    #[test]
    fn a_flush_of_more_text_than_one_batch_holds_is_split_and_keeps_every_row() {
        let row = KeyedMiss {
            build_id: 7,
            step: "codegen",
            item: "x".repeat(1000),
            key: "ab".into(),
            parts: Vec::new(),
            beyond: None,
        };
        let most = 10 * row.text_bytes();
        let rows = vec![row; 25];
        let batches = in_batches(&rows, KeyedMiss::text_bytes, most, key_parts).unwrap();
        assert_eq!(batches.iter().map(RecordBatch::num_rows).collect::<Vec<_>>(), [10, 10, 5]);
        assert_eq!(in_batches(&[], KeyedMiss::text_bytes, most, key_parts).unwrap().len(), 1);
    }

    #[test]
    fn a_links_row_says_patched_or_placed_and_a_crate_edge_its_kind() {
        let links = links(&[
            LinkRow {
                build_id: 3,
                image: "blaze (bin)".into(),
                krate: Some("blaze".into()),
                outcome: LinkOutcome::Patched,
                dur_ns: Some(40_000_000),
            },
            LinkRow {
                build_id: 3,
                image: "app (test)".into(),
                krate: None,
                outcome: LinkOutcome::Placed,
                dur_ns: None,
            },
        ])
        .unwrap();
        let outcome = links.column(3).as_dictionary::<UInt32Type>();
        let names = outcome.values().as_string::<i32>();
        let said: Vec<&str> =
            outcome.keys().iter().map(|it| names.value(it.unwrap() as usize)).collect();
        assert_eq!(said, ["patched", "placed"]);
        assert!(links.column(2).is_null(1) && links.column(4).is_null(1));

        let deps = crate_deps(&[CrateDepRow {
            build_id: 3,
            krate: "app".into(),
            dep: "cc".into(),
            kind: DepKind::Build,
        }])
        .unwrap();
        let kind = deps.column(3).as_dictionary::<UInt32Type>();
        assert_eq!(kind.values().as_string::<i32>().value(kind.keys().value(0) as usize), "build");
    }

    #[test]
    fn a_provenance_row_names_its_cause_and_a_key_part_row_its_hash() {
        let changed = ProvenanceRow {
            build_id: 5,
            query_key: "`infer_query`(Id(7))".into(),
            kind: "infer_query",
            outcome: RunOutcome::Ran,
            cause: Some("changed"),
            view: 3,
            cause_key: Some("`FileText.text`(Id(2))".into()),
            cause_kind: Some("input"),
            what: None,
        };
        let read = ProvenanceRow {
            outcome: RunOutcome::UntrackedRead,
            cause: None,
            cause_key: None,
            cause_kind: None,
            what: Some("the proc macro's output"),
            ..changed.clone()
        };
        let batch = provenance(&[changed, read]).unwrap();
        let at = |name: &str| schema::PROVENANCE.column(name).unwrap();
        assert_eq!(
            batch.column(at("cause_key")).as_string::<i32>().value(0),
            "`FileText.text`(Id(2))"
        );
        assert!(batch.column(at("cause")).is_null(1) && batch.column(at("cause_key")).is_null(1));
        assert_eq!(batch.column(at("what")).as_string::<i32>().value(1), "the proc macro's output");

        let part = |name: &str, hash: &str| KeyPart {
            name: name.into(),
            hash: Some(hash.into()),
            names: None,
        };
        let parts = key_parts(&[
            KeyedMiss {
                build_id: 5,
                step: "unit",
                item: "app v0.1.0 (lib)".into(),
                key: "ab".into(),
                parts: vec![part("env CC", "cd"), part("env LD", "ef")],
                beyond: Some("its process variables"),
            },
            KeyedMiss {
                build_id: 5,
                step: "unit",
                item: "lib v0.1.0 (lib)".into(),
                key: "01".into(),
                parts: Vec::new(),
                beyond: None,
            },
        ])
        .unwrap();
        assert_eq!(
            parts.column(schema::KEY_PARTS.column("hash").unwrap()).as_string::<i32>().value(0),
            "cd"
        );
        // A row a part, and one with no part for the key that exposes none, each with its
        // miss's item.
        let text = |name: &str| parts.column(schema::KEY_PARTS.column(name).unwrap()).as_string::<i32>();
        assert_eq!(parts.num_rows(), 3);
        assert_eq!(text("part").value(1), "env LD");
        assert_eq!(text("item").value(1), "app v0.1.0 (lib)");
        assert!(text("part").is_null(2) && text("hash").is_null(2));
        assert_eq!(text("item").value(2), "lib v0.1.0 (lib)");
        assert_eq!(text("beyond").value(1), "its process variables");
        assert!(text("beyond").is_null(2));
    }
}
