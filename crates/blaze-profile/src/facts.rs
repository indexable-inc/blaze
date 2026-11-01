//! What a build did that its spans don't say: how each image it linked was linked (`links`) and
//! the crate graph it built (`crate_deps`), so "patched or relinked" and the crates' edges are
//! queries over the history like the rest. The daemon notes them with [`crate::history::History::linked`]
//! and [`crate::history::History::crate_graph`]; the next flush writes them.

use arrow_array::{ArrayRef, DictionaryArray, Int64Array, RecordBatch, StringArray, UInt64Array, types::UInt32Type};
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
            LinkRow { build_id: 3, image: "app (test)".into(), krate: None, outcome: LinkOutcome::Placed, dur_ns: None },
        ])
        .unwrap();
        let outcome = links.column(3).as_dictionary::<UInt32Type>();
        let names = outcome.values().as_string::<i32>();
        let said: Vec<&str> = outcome.keys().iter().map(|it| names.value(it.unwrap() as usize)).collect();
        assert_eq!(said, ["patched", "placed"]);
        assert!(links.column(2).is_null(1) && links.column(4).is_null(1));

        let deps = crate_deps(&[CrateDepRow { build_id: 3, krate: "app".into(), dep: "cc".into(), kind: DepKind::Build }])
            .unwrap();
        let kind = deps.column(3).as_dictionary::<UInt32Type>();
        assert_eq!(kind.values().as_string::<i32>().value(kind.keys().value(0) as usize), "build");
    }
}
