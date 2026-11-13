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

use arrow_array::{
    Array, DictionaryArray, Float64Array, Int64Array, RecordBatch, StringArray, UInt16Array,
    UInt32Array, UInt64Array,
    cast::AsArray,
    types::{Float64Type, Int64Type, UInt16Type, UInt32Type, UInt64Type},
};
use parquet::arrow::{ProjectionMask, arrow_reader::ParquetRecordBatchReaderBuilder};

use crate::{
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
        error: parquet::errors::ParquetError,
    },
    Arrow {
        path: PathBuf,
        error: arrow_schema::ArrowError,
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
        error: arrow_schema::ArrowError,
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
            TableError::Arrow { path, error } => {
                write!(f, "the profile history: reading the rows of {}: {error}", path.display())
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
    pub batch: RecordBatch,
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
        Cells::of(self.batch.column(at).as_ref())
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
        schema::readable(entry.version).map_err(|why| TableError::Version {
            path: path.clone(), // clone: an error
            version: entry.version,
            why,
        })?;
        let opened = fs::File::open(&path)
            .map_err(|error| TableError::Io { path: path.clone(), error })?; // clone: an error
        let parquet = |error| TableError::Parquet { path: path.clone(), error }; // clone: an error
        let builder = ParquetRecordBatchReaderBuilder::try_new(opened).map_err(parquet)?;
        let fields = builder.schema().fields();
        // alloc: a file's projection, once a file
        let roots: Vec<usize> = columns
            .iter()
            .filter_map(|name| fields.iter().position(|it| it.name() == name))
            .collect();
        let mask = ProjectionMask::roots(builder.parquet_schema(), roots);
        let reader = builder.with_projection(mask).build().map_err(parquet)?;
        for batch in reader {
            let batch = batch.map_err(|error| TableError::Arrow { path: path.clone(), error })?; // clone: an error
            let schema = batch.schema();
            // alloc: a batch's column map, once a batch
            let at = columns.iter().map(|name| schema.index_of(name).ok()).collect();
            out.push(Rows { path: path.clone(), batch, at }); // clone: a path, once a batch
        }
    }
    Ok(out)
}

/// A column's cells, of each type the history writes ([`schema::Type`]); `Missing` for a
/// column the file has none of, every cell null.
pub enum Cells<'a> {
    Missing,
    U16(&'a UInt16Array),
    U32(&'a UInt32Array),
    U64(&'a UInt64Array),
    I64(&'a Int64Array),
    F64(&'a Float64Array),
    Text(&'a StringArray),
    Dict(&'a DictionaryArray<UInt32Type>, &'a StringArray),
}

impl<'a> Cells<'a> {
    fn of(array: &'a dyn Array) -> Option<Cells<'a>> {
        if let Some(it) = array.as_primitive_opt::<UInt16Type>() {
            return Some(Cells::U16(it));
        }
        if let Some(it) = array.as_primitive_opt::<UInt32Type>() {
            return Some(Cells::U32(it));
        }
        if let Some(it) = array.as_primitive_opt::<UInt64Type>() {
            return Some(Cells::U64(it));
        }
        if let Some(it) = array.as_primitive_opt::<Int64Type>() {
            return Some(Cells::I64(it));
        }
        if let Some(it) = array.as_primitive_opt::<Float64Type>() {
            return Some(Cells::F64(it));
        }
        if let Some(it) = array.as_string_opt::<i32>() {
            return Some(Cells::Text(it));
        }
        let dict = array.as_dictionary_opt::<UInt32Type>()?;
        Some(Cells::Dict(dict, dict.values().as_string_opt::<i32>()?))
    }

    fn null(&self, row: usize) -> bool {
        match self {
            Cells::Missing => true,
            Cells::U16(it) => it.is_null(row),
            Cells::U32(it) => it.is_null(row),
            Cells::U64(it) => it.is_null(row),
            Cells::I64(it) => it.is_null(row),
            Cells::F64(it) => it.is_null(row),
            Cells::Text(it) => it.is_null(row),
            Cells::Dict(it, _) => it.is_null(row),
        }
    }

    /// An integer cell; `None` null, or of a text or float column.
    pub fn int(&self, row: usize) -> Option<i128> {
        if self.null(row) {
            return None;
        }
        match self {
            Cells::U16(it) => Some(it.value(row).into()),
            Cells::U32(it) => Some(it.value(row).into()),
            Cells::U64(it) => Some(it.value(row).into()),
            Cells::I64(it) => Some(it.value(row).into()),
            _ => None,
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
        if self.null(row) {
            return None;
        }
        match self {
            Cells::F64(it) => Some(it.value(row)),
            // As DuckDB casts an integer to DOUBLE: the nearest double.
            _ => self.int(row).map(|it| it as f64),
        }
    }

    /// A text cell, of a plain or a dictionary column.
    pub fn text(&self, row: usize) -> Option<&'a str> {
        if self.null(row) {
            return None;
        }
        match self {
            Cells::Text(it) => Some(it.value(row)),
            Cells::Dict(dict, values) => {
                let key = usize::try_from(dict.keys().value(row)).ok()?;
                (key < values.len()).then(|| values.value(key))
            }
            _ => None,
        }
    }
}
