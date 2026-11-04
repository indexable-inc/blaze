//! What a build did that its spans don't say: how each image it linked was linked (`links`),
//! the crate graph it built (`crate_deps`) and what the query engine counted of each query
//! function (`query_counts`), so "patched or relinked", the crates' edges and "what did eviction
//! cost" are queries over the history like the rest. The daemon notes them with
//! [`crate::history::History::linked`], [`crate::history::History::crate_graph`] and
//! [`crate::history::History::query_counted`]; the next flush writes them. What a build
//! computed again, and why (`misses`), is noted with [`crate::history::History::missed`], the
//! parts of each such key (`key_parts`) with [`crate::history::History::keyed`], and why the
//! query engine ran each run (`provenance`) with [`crate::history::History::ran`].

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

/// A row of the schema's `key_parts` table: one part of the key of a thing a build computed
/// again (`blaze_kernel::Part`), or the key alone when it exposes no parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPartRow {
    pub build_id: u64,
    pub step: &'static str,
    pub item: String,
    /// The whole key, hex.
    pub key: String,
    pub part: Option<String>,
    /// The part's hash, hex.
    pub hash: Option<String>,
    pub names: Option<String>,
    pub beyond: Option<&'static str>,
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
pub fn key_parts(rows: &[KeyPartRow]) -> Result<RecordBatch, ArrowError> {
    let text =
        |of: fn(&KeyPartRow) -> Option<&str>| column(StringArray::from_iter(rows.iter().map(of)));
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.build_id))),
        dict(rows.iter().map(|it| Some(it.step))),
        column(StringArray::from_iter_values(rows.iter().map(|it| it.item.as_str()))),
        column(StringArray::from_iter_values(rows.iter().map(|it| it.key.as_str()))),
        text(|it| it.part.as_deref()),
        text(|it| it.hash.as_deref()),
        text(|it| it.names.as_deref()),
        text(|it| it.beyond),
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

        let parts = key_parts(&[KeyPartRow {
            build_id: 5,
            step: "unit",
            item: "app v0.1.0 (lib)".into(),
            key: "ab".into(),
            part: Some("env CC".into()),
            hash: Some("cd".into()),
            names: None,
            beyond: Some("its process variables"),
        }])
        .unwrap();
        assert_eq!(
            parts.column(schema::KEY_PARTS.column("hash").unwrap()).as_string::<i32>().value(0),
            "cd"
        );
    }
}
