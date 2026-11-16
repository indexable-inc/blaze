//! A minimal Arrow IPC writer and reader (https://arrow.apache.org/docs/format/Columnar.html,
//! "Serialization and Interprocess Communication"), in place of arrow-ipc and the arrow crates
//! (docs/v2/plan/third-party.md, "The profile's file"): the live feed's streams and the
//! explorer's `.ixprof` tables, over [`blaze_columns::Batch`]es.
//!
//! # What is written
//!
//! Encapsulated messages (the continuation `FF FF FF FF`, the metadata's length, a flatbuffer
//! `Message`, its body), each 8-aligned, metadata version V5, little-endian, uncompressed:
//!
//! - a stream ([`StreamWriter`], [`stream`]): the `Schema`, then per batch a `DictionaryBatch`
//!   for each dictionary column of the values it names for the first time (`isDelta` after the
//!   stream's first), then the `RecordBatch`, its keys those of the stream's dictionaries; the
//!   end-of-stream marker [`END`] last;
//! - a file ([`file`]): `ARROW1`, the stream's messages with each dictionary whole once, [`END`],
//!   the `Footer` (the schema and each block's place), its length, `ARROW1`.
//!
//! | column | Arrow type |
//! |---|---|
//! | `Bool` | Bool |
//! | `I32`, `I64` | Int(32 or 64, signed) |
//! | `U16`, `U32`, `U64` | Int(16, 32 or 64, unsigned) |
//! | `F64` | FloatingPoint(DOUBLE) |
//! | `Text` | Utf8 |
//! | `Dict` | Utf8, dictionary-encoded with UInt32 keys |
//! | `List(x)` | List of a non-null `item` of `x`'s |
//!
//! Same batches in, same bytes out.
//!
//! # What is read ([`StreamReader`], [`read_stream`], [`read_file`])
//!
//! What this crate writes, and other writers' streams and files of the types above plus those that
//! map onto them (Int 8 and 16 signed as `I32`, UInt8 as `U16`, FloatingPoint SINGLE as `F64`,
//! dictionary keys of any integer type). Anything else (a compressed body, big-endian, another
//! type, a nested list, a null list element) is an [`IpcError`] naming the field and what it
//! is. Every length read is held to the bytes it lies in.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

use core::fmt;

use blaze_columns::ColumnError;

mod fb;
mod read;
mod write;

#[cfg(test)]
mod tests;

pub use read::{Message, Read, StreamReader, messages, read_file, read_stream};
pub use write::{StreamWriter, file, stream};

/// The end-of-stream marker: the continuation, then a metadata length of 0.
pub const END: [u8; 8] = [0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0];

/// What an Arrow IPC file begins and ends with.
pub const MAGIC: [u8; 6] = *b"ARROW1";

/// Why a stream or file could not be written or read. `field` is a field's place in the schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcError {
    /// The bytes end inside `what`, at byte `at`.
    Truncated { what: &'static str, at: usize },
    /// A flatbuffer that is not one of Arrow's tables: `what`, at byte `at` of it.
    Flatbuffer { what: &'static str, at: usize },
    /// Not an Arrow IPC file: `why`.
    NotArrow { why: &'static str },
    /// A stream whose messages come in an order the format has none of: `what`.
    Order { what: &'static str },
    /// A field, or a message, of a form this crate does not read: `what`, and its code in the
    /// format (a type's, a version's) when it has one.
    Unsupported { field: Option<usize>, what: &'static str, code: i64 },
    /// Bytes that say something impossible: `what`.
    Corrupt { field: Option<usize>, what: &'static str },
    /// A column past what the format holds (32-bit offsets): `what`.
    TooLarge { field: usize, what: &'static str },
    /// A column could not be made of what was read.
    Column { field: usize, error: ColumnError },
    /// A record batch names dictionary `id`, which no dictionary batch before it gave.
    NoDictionary { id: i64 },
    /// A batch of other fields than its stream's.
    FieldsDiffer,
    /// A stream writer that failed before: its stream must end and a new one begin.
    Failed,
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let field = |f: &mut fmt::Formatter<'_>, at: &Option<usize>| match at {
            Some(at) => write!(f, "the Arrow field {at}"),
            None => f.write_str("an Arrow message"),
        };
        match self {
            IpcError::Truncated { what, at } => write!(f, "the Arrow IPC bytes end inside {what} (at byte {at})"),
            IpcError::Flatbuffer { what, at } => write!(f, "an Arrow IPC flatbuffer: {what} (at byte {at})"),
            IpcError::NotArrow { why } => write!(f, "not an Arrow IPC file: {why}"),
            IpcError::Order { what } => write!(f, "an Arrow IPC stream: {what}"),
            IpcError::Unsupported { field: at, what, code } => {
                field(f, at)?;
                write!(f, " is {what} (code {code}), which blaze-arrow-ipc does not read")
            }
            IpcError::Corrupt { field: at, what } => {
                field(f, at)?;
                write!(f, ": {what}")
            }
            IpcError::TooLarge { field, what } => write!(f, "the Arrow field {field}: {what}"),
            IpcError::Column { field, error } => write!(f, "the Arrow field {field}: {error}"),
            IpcError::NoDictionary { id } => {
                write!(f, "an Arrow record batch names dictionary {id}, which no dictionary batch gave")
            }
            IpcError::FieldsDiffer => f.write_str("an Arrow batch of other fields than its stream's"),
            IpcError::Failed => f.write_str("an Arrow stream writer that failed before is used again"),
        }
    }
}

impl core::error::Error for IpcError {}
