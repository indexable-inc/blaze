//! A minimal Parquet writer and reader (https://parquet.apache.org/docs/file-format/), in place
//! of the parquet and arrow crates (docs/v2/plan/third-party.md, "The profile's file", option 2):
//! what blaze-profile writes its history in, so every query of the debugging skill runs on the
//! files as before (`read_parquet(..., union_by_name = true)`).
//!
//! # What is written ([`Writer`])
//!
//! A file of [`blaze_columns::Batch`]es of one set of fields: `PAR1`, row groups of at most
//! [`Options::row_group_rows`] rows, the footer, its length, `PAR1`. Each column chunk is data
//! pages v1 of at most about [`Options::page_bytes`] of values, cut at a row, each encoded as its
//! chunk's first page says is smallest ([`Encoding`]: PLAIN, a dictionary, DELTA_BINARY_PACKED,
//! DELTA_BYTE_ARRAY, DELTA_LENGTH_BYTE_ARRAY; RLE booleans) and compressed as
//! [`Options::compression`] says ([`Compression`]). A nullable column's definition levels, and a
//! list column's repetition levels, are the RLE/bit-packed hybrid (each length-prefixed, as v1
//! has them). The types:
//!
//! | column | physical | annotation | DuckDB reads |
//! |---|---|---|---|
//! | `Bool` | BOOLEAN | | BOOLEAN |
//! | `I32` | INT32 | | INTEGER |
//! | `I64` | INT64 | | BIGINT |
//! | `U16` | INT32 | INTEGER(16, unsigned), UINT_16 | USMALLINT |
//! | `U32` | INT32 | INTEGER(32, unsigned), UINT_32 | UINTEGER |
//! | `U64` | INT64 | INTEGER(64, unsigned), UINT_64 | UBIGINT |
//! | `F64` | DOUBLE | | DOUBLE |
//! | `Text`, `Dict` | BYTE_ARRAY | STRING, UTF8 | VARCHAR |
//! | `List(x)` | a 3-level LIST of `x`'s, its elements required | LIST | `x[]` |
//!
//! Each chunk has statistics (min and max by the type's order, `min_value`/`max_value`, and the
//! null count; no min or max for text past [`STAT_BYTES`], none for a list) and the footer says
//! the type-defined order for every leaf, so DuckDB prunes row groups by them. The key-value
//! metadata is the caller's (`blaze.schema`, `blaze.table`, `blaze.clock`). Same batches in,
//! same bytes out: nothing of the machine or the clock is written.
//!
//! # Which codec
//!
//! Measured by `tests/measure.rs` (release, Apple M5 Max, 2026-11-15) on 174 old profile
//! files of one build session (2 981 361 rows of 31 tables; arrow-rs wrote them in 28.60 MB,
//! dictionary and delta encodings under zstd). Speeds are of the 588 MB the tables are as
//! PLAIN, uncompressed; read is a full scan into batches:
//!
//! | pages | bytes | write | read |
//! |---|---|---|---|
//! | PLAIN, none | 588.06 MB | 1475 MB/s | 1768 MB/s |
//! | encoded, none | 150.65 MB | 701 MB/s | 1728 MB/s |
//! | encoded, LZ4_RAW | 30.72 MB | 641 MB/s | 1097 MB/s |
//! | encoded, zstd 1 | 24.88 MB | 553 MB/s | 1106 MB/s |
//! | encoded, zstd 3 | 24.67 MB | 545 MB/s | 1109 MB/s |
//!
//! A build's files are encoded only ([`Compression::HOT`]): LZ4_RAW saves 80% of the bytes but
//! costs 9% of the write (7% to 11% over three runs), past the 5% a build's own files may
//! spend. A merged or cold file is zstd 3 ([`Compression::COLD`], a merge recompresses its
//! inputs' pages), under arrow-rs's 28.60 MB. 105 MB of the 150.65 is one column, `spans.function`:
//! 214 MB of text, 77 663 distinct values up to 64 KB each (Debug prints of macro call ids),
//! which no dictionary or delta encoding shrinks and zstd takes to 2.5 MB; without it the
//! encoded files are 45.5 MB.
//!
//! # What is read ([`File`])
//!
//! The files this crate writes and DuckDB's uncompressed answers (`COPY ... (FORMAT parquet,
//! COMPRESSION uncompressed)`): flat columns and 3-level lists of the types above (and DuckDB's
//! others that map onto them: TINYINT and SMALLINT as `I32`, UTINYINT as `U16`, FLOAT as `F64`,
//! DECIMAL as `F64`, DATE as `I32`, TIMESTAMP as `I64`, HUGEINT, which DuckDB writes as
//! DOUBLE), data pages v1 and v2, every encoding and codec the writer writes (and
//! PLAIN_DICTIONARY). Anything else (another codec or encoding, a nested group, a null list
//! element) is a
//! [`ParquetError`] naming the column and what it is, never a guess. Every length read is held
//! to the bytes it lies in.
//!
//! # Merges ([`Concat`])
//!
//! Files of one schema as one file, each row group's column chunks copied as their bytes are,
//! with only their offsets moved in the new footer.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

use core::fmt;

use blaze_columns::ColumnError;
use blaze_own::FixedString;

mod compress;
mod concat;
mod delta;
mod levels;
/// The LZ4 block codec, public for the transfer measurement (blaze-squeeze's example), which
/// compares it per record with zstd and blaze's column encodings.
pub mod lz4;
pub mod meta;
mod read;
mod thrift;
mod write;

#[cfg(test)]
mod tests;

pub use compress::Compression;
pub use concat::{Concat, Failed, Input};
pub use read::{File, IntStats};
pub use write::{Encoding, Options, STAT_BYTES, Writer, write};

/// The four bytes a Parquet file begins and ends with.
pub const MAGIC: [u8; 4] = *b"PAR1";

/// A column's name in an error, cut at 64 bytes.
pub type ColumnName = FixedString<64>;

/// `name`, cut at a character's boundary within 64 bytes.
pub fn column_name(name: &str) -> ColumnName {
    let mut out = ColumnName::new();
    for char in name.chars() {
        if out.try_push(char).is_err() {
            break;
        }
    }
    out
}

/// Why a file could not be written or read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParquetError {
    /// The bytes end inside `what`, at byte `at` of what was being read.
    Truncated { what: &'static str, at: usize },
    /// The footer or a page header is not Thrift of Parquet's structs: `what`, at byte `at`.
    Thrift { what: &'static str, at: usize },
    /// Not a Parquet file: `why`.
    NotParquet { why: &'static str },
    /// A batch's column, or a column asked for, that does not fit: `why`.
    Field { column: ColumnName, why: &'static str },
    /// A column of a form this crate does not read: `what`, and its code in the format (a
    /// codec's, an encoding's, a type's) when it has one.
    Unsupported { column: ColumnName, what: &'static str, code: i64 },
    /// A column whose bytes say something impossible: `what`.
    Corrupt { column: ColumnName, what: &'static str },
    /// A column past what the format or this reader holds: `what`.
    TooLarge { column: ColumnName, what: &'static str },
    /// A column could not be made of what was read.
    Column { column: ColumnName, error: ColumnError },
    /// A merge's input of another schema than its first.
    SchemaDiffers,
}

impl fmt::Display for ParquetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParquetError::Truncated { what, at } => write!(f, "the Parquet bytes end inside {what} (at byte {at})"),
            ParquetError::Thrift { what, at } => write!(f, "the Parquet footer or a page header: {what} (at byte {at})"),
            ParquetError::NotParquet { why } => write!(f, "not a Parquet file: {why}"),
            ParquetError::Field { column, why } => write!(f, "the Parquet column `{column}`: {why}"),
            ParquetError::Unsupported { column, what, code } => {
                write!(f, "the Parquet column `{column}` is {what} (code {code}), which blaze-parquet does not read")
            }
            ParquetError::Corrupt { column, what } => write!(f, "the Parquet column `{column}`: {what}"),
            ParquetError::TooLarge { column, what } => write!(f, "the Parquet column `{column}`: {what}"),
            ParquetError::Column { column, error } => write!(f, "the Parquet column `{column}`: {error}"),
            ParquetError::SchemaDiffers => f.write_str("a Parquet file of another schema than the merge's first"),
        }
    }
}

impl core::error::Error for ParquetError {}
