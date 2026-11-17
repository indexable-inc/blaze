//! Lightweight column encodings read in place (docs/v2/plan/design/columns.md): a column of
//! integers or of strings is encoded once into bytes a record holds as a `Slice<u8>`, and is
//! then read where it lies, one value at a time or a block at a time, with no copy of the
//! column out first. That is what rule 51 (a record is written once and read in place) asks
//! of compression; block compressors (zstd, DEFLATE) decompress-and-copy and stay off this path.
//!
//! | module | what |
//! |---|---|
//! | [`bits`] | bit-packing: `n` values of `w` bits end to end, least significant bit first; `get(i)` is one unaligned load, a shift and a mask; eight values unpack by one 16-byte load and a byte shuffle (`swizzle_dyn`) a part |
//! | [`block`] | the schemes of one block of 1,024 integers: frame of reference, frame against a line, run-length, delta, delta of delta; each block's scheme and parameters in a fixed 24-byte directory entry, its body only packed values |
//! | [`ints`] | an integer column: blocks behind a directory, or a dictionary (distinct values in first-use order) over a column of codes; the chooser |
//! | [`ef`] | a sorted list of distinct `u64` ids below a bound, in Elias-Fano form, read by value: `next_geq`, `contains`, a leapfrog intersection, about `2 + log2(u / n)` bits an id |
//! | [`ids`] | a column of fixed-width ids (32-byte `ContentId`s): each distinct id once in a table, rows as bit-packed codes |
//! | [`fsst`] | FSST's symbol table: up to 255 symbols of 1 to 8 bytes, an escape for any other byte |
//! | [`strs`] | a string column: offsets and bytes, FSST codes, or a dictionary of whole strings; the chooser |
//! | [`front`] | a sorted set of distinct strings, front-coded in buckets of 16: `get` reads one bucket, `find` searches the buckets' first strings in place, a string's index is its rank |
//!
//! Every column's bytes are little-endian, unaligned (a `Slice<u8>` has no alignment), and a
//! pure function of the values and the policy (rule 25): the chooser breaks every tie by a
//! fixed order and reads nothing else. A column is read through a view made once by `open`,
//! which checks every header and bound a read indexes by (rule 74): a read through the view
//! checks nothing again and cannot leave the column's bytes. Values inside packed data that
//! index something (an offset, a dictionary code) are clamped where they index; their meaning
//! is held by the record's digest, checked once at fetch, not by `open`.

pub mod bits;
pub mod block;
pub mod ef;
pub mod front;
pub mod fsst;
pub mod ids;
pub mod ints;
mod plant;
pub mod strs;
#[cfg(test)]
mod tests;

use core::fmt;

use blaze_own::{AVec, AllocFailure};

pub use block::{GetCost, Scheme};
pub use ids::{IdView, encode_ids};
pub use ints::{DictUse, IntChoice, IntPolicy, IntView, encode_ints, ints_size};
pub use strs::{StrChoice, StrPolicy, StrScheme, StrView, encode_strs};

/// Why a column was not written or not opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncError {
    /// Room for the column or a scratch buffer could not be had.
    Alloc(AllocFailure),
    /// A column of more than `u32::MAX` values or bytes: `what` has `len`.
    TooLong { what: &'static str, len: usize },
    /// The bytes are not a column: `what` is wrong at byte `at` of the column.
    Corrupt { what: &'static str, at: usize },
    /// A caller's buffer of `have` bytes is too short for the string `index`, which may need
    /// `need`.
    Buffer { index: usize, need: usize, have: usize },
}

impl From<AllocFailure> for EncError {
    fn from(failure: AllocFailure) -> EncError {
        EncError::Alloc(failure)
    }
}

impl fmt::Display for EncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncError::Alloc(failure) => write!(f, "blaze_columns::enc: {failure}"),
            EncError::TooLong { what, len } => {
                write!(f, "blaze_columns::enc: {what} has {len}, more than a column's u32 holds")
            }
            EncError::Corrupt { what, at } => {
                write!(f, "blaze_columns::enc: not a column: {what} at byte {at}")
            }
            EncError::Buffer { index, need, have } => write!(
                f,
                "blaze_columns::enc: string {index} may need {need} bytes of buffer, the caller gave {have}"
            ),
        }
    }
}

impl core::error::Error for EncError {}

/// Appends `bytes`; refused with nothing written.
pub(crate) fn put(out: &mut AVec<'_, u8>, bytes: &[u8]) -> Result<(), EncError> {
    Ok(out.try_extend_from_slice(bytes)?)
}

/// `len` as the `u32` a header holds.
pub(crate) fn len32(len: usize, what: &'static str) -> Result<u32, EncError> {
    blaze_int::len32(len).map_err(|_| EncError::TooLong { what, len })
}

/// Writes `value` over the four bytes at `at`, which a header reserved.
pub(crate) fn patch32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// The `u32` at `at`, or why not.
pub(crate) fn read32(bytes: &[u8], at: usize, what: &'static str) -> Result<u32, EncError> {
    match bytes.get(at..).and_then(<[u8]>::first_chunk::<4>) {
        Some(b) => Ok(u32::from_le_bytes(*b)),
        None => Err(EncError::Corrupt { what, at }),
    }
}

/// The `u64` at `at`, or why not.
pub(crate) fn read64(bytes: &[u8], at: usize, what: &'static str) -> Result<u64, EncError> {
    match bytes.get(at..).and_then(<[u8]>::first_chunk::<8>) {
        Some(b) => Ok(u64::from_le_bytes(*b)),
        None => Err(EncError::Corrupt { what, at }),
    }
}

/// The byte at `at`, or why not.
pub(crate) fn read8(bytes: &[u8], at: usize, what: &'static str) -> Result<u8, EncError> {
    bytes.get(at).copied().ok_or(EncError::Corrupt { what, at })
}
