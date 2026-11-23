//! The history's tables read without DuckDB: the files of a table its manifest lists (every
//! one, or those that can hold one build's rows: [`Scope`]), each read through the Parquet
//! reader blaze links, only the columns asked for. What a fixed question of the profile reads
//! (`blaze explain memory`, [`crate::memory`]) comes from here and is aggregated in Rust; the
//! SQL views stay `blaze debug sql`'s, and each native answer's test holds it to its view.
//!
//! The rules DuckDB's prelude (`sql/views.sql`) has, kept here: the manifest is the one list of
//! files (never a directory listing, which held a merge's output beside its inputs); a file of
//! a schema version this blaze can't read is an error naming it; a column a file lacks (a file
//! older than the column) reads as null, as `union_by_name` has it; a column the table does not
//! have is an error naming it, never a column of nulls.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use std::alloc::Global;

use blaze_parquet::ParquetError;

use crate::{
    columns::{Batch, BatchError, Column},
    history::{HistoryError, Pin, manifest::Scope},
    schema::{self, Table},
};

#[derive(Debug)]
pub enum TableError {
    /// The history could not be pinned and its manifest read ([`Pin`]).
    History(HistoryError),
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
    Parquet {
        path: PathBuf,
        error: ParquetError,
    },
    /// A file of a schema version this blaze can't read ([`schema::readable`]).
    Version {
        path: PathBuf,
        version: u32,
        why: &'static str,
    },
    /// A column asked of a table that has none of the name: a bug in the reader.
    NoColumn {
        table: &'static str,
        column: &'static str,
    },
    /// A file's column of another type than its table's.
    Type {
        path: PathBuf,
        column: &'static str,
    },
    /// An answer's rows could not be made a batch: a bug in the reader that made them.
    Answer {
        view: &'static str,
        error: BatchError,
    },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::History(error) => write!(f, "pinning the profile history to read it: {error}"),
            TableError::Io { path, error } => {
                write!(f, "the profile history: opening {}: {error}", path.display())
            }
            TableError::Parquet { path, error } => {
                write!(f, "the profile history: reading {}: {error}", path.display())
            }
            TableError::Version { path, version, why } => write!(
                f,
                "the profile history: {} is of schema version {version}, which this blaze can't read: {why}",
                path.display()
            ),
            TableError::NoColumn { table, column } => {
                write!(f, "the profile table `{table}` has no column `{column}` (a bug in its reader)")
            }
            TableError::Type { path, column } => write!(
                f,
                "the profile history: {}'s column `{column}` is not of the type its table has",
                path.display()
            ),
            TableError::Answer { view, error } => {
                write!(f, "the answer of `{view}` read without DuckDB: {error}")
            }
        }
    }
}

impl std::error::Error for TableError {}

/// One file's rows of the columns asked for, each named by its place in the ask.
pub struct Rows {
    pub path: PathBuf,
    pub batch: Batch,
    /// For each column asked, its index in `batch`; `None` where the file has none of it.
    at: Vec<Option<usize>>,
}

impl Rows {
    pub fn len(&self) -> usize {
        self.batch.num_rows()
    }

    pub fn is_empty(&self) -> bool {
        self.batch.num_rows() == 0
    }

    /// The `i`th column asked for, as cells.
    pub fn cells(&self, i: usize, column: &'static str) -> Result<Cells<'_>, TableError> {
        let Some(at) = self.at.get(i).copied().flatten() else { return Ok(Cells::Missing) };
        Cells::of(self.batch.column(at))
            .ok_or_else(|| TableError::Type { path: self.path.clone(), column }) // clone: an error
    }
}

/// `columns` of `table`'s files that a reader of `scope` reads, of the history `pin` holds at
/// `dir`, one [`Rows`] a batch, in the manifest's order.
pub fn read(
    dir: &Path,
    pin: &Pin,
    table: &'static Table,
    scope: Scope,
    columns: &[&'static str],
) -> Result<Vec<Rows>, TableError> {
    for &column in columns {
        if table.column(column).is_none() {
            return Err(TableError::NoColumn { table: table.name, column });
        }
    }
    // alloc: the batches read, once a question; what outlives it is the answer made of them
    let mut out = Vec::new();
    for (file, entry) in pin.manifest().scoped(table.name, scope) {
        let path = dir.join(file);
        if let Err(why) = schema::readable(entry.version) {
            return Err(TableError::Version { path, version: entry.version, why });
        }
        // alloc: a file's bytes, once a question
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => return Err(TableError::Io { path, error }),
        };
        let file = match blaze_parquet::File::open(&bytes, Global) {
            Ok(file) => file,
            Err(error) => return Err(TableError::Parquet { path, error }),
        };
        // alloc: a file's projection, once a file
        let roots: Vec<usize> = columns.iter().filter_map(|name| file.column(name)).collect();
        let batches = match file.read(&roots) {
            Ok(batches) => batches,
            Err(error) => return Err(TableError::Parquet { path, error }),
        };
        for batch in batches {
            // alloc: a batch's column map, once a batch
            let at = columns.iter().map(|name| batch.index_of(name)).collect();
            out.push(Rows { path: path.clone(), batch, at }); // clone: a path, once a batch
        }
    }
    Ok(out)
}

/// A column's cells, of each type the history writes ([`schema::Type`]); `Missing` for a
/// column the file has none of, every cell null.
pub enum Cells<'a> {
    Missing,
    /// A column of one value a row: a list column is of no type a fixed question reads.
    Column(&'a Column),
}

impl<'a> Cells<'a> {
    fn of(column: &'a Column) -> Option<Cells<'a>> {
        column.lists().is_none().then_some(Cells::Column(column))
    }

    /// An integer cell; `None` null, or of a text or float column.
    pub fn int(&self, row: usize) -> Option<i128> {
        match self {
            Cells::Missing => None,
            Cells::Column(it) => it.int(row),
        }
    }

    pub fn u64(&self, row: usize) -> Option<u64> {
        self.int(row).and_then(|it| u64::try_from(it).ok())
    }

    pub fn i64(&self, row: usize) -> Option<i64> {
        self.int(row).and_then(|it| i64::try_from(it).ok())
    }

    /// A number cell as a double, as DuckDB casts one for arithmetic with a double.
    pub fn f64(&self, row: usize) -> Option<f64> {
        match self {
            Cells::Missing => None,
            // An integer as DuckDB casts one to DOUBLE: the nearest double.
            Cells::Column(it) => it.float(row),
        }
    }

    /// A text cell, of a plain or a dictionary column.
    pub fn text(&self, row: usize) -> Option<&'a str> {
        match self {
            Cells::Missing => None,
            Cells::Column(it) => it.text(row),
        }
    }
}
