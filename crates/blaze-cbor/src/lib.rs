//! blaze's deterministic CBOR (rule 33; RFC 8949, section 4.2.1), in one home.
//!
//! The encoding rules, each refused by every reader here when broken:
//!
//! - every head (a number, a length) in its shortest form;
//! - every length definite: no indefinite strings, arrays or maps, no break;
//! - every float in the shortest of half, single and double precision that holds it exactly,
//!   and every NaN as the one quiet NaN `f9 7e00`;
//! - map keys in the bytewise order of their encodings, none twice (for text keys, the shorter
//!   key first, then bytewise: the head holds the length).
//!
//! | module | what |
//! |---|---|
//! | [`write`] | a head of each kind appended to a `Vec` in the caller's allocator; [`Head`] for any byte sink |
//! | [`Reader`] | the field reader records written as arrays in a fixed order are read with: one call a field, a cap on every length |
//! | [`Decoder`] / [`Encoder`] | the streaming event form, as blaze-json's: bytes to [`Event`]s and back, bounded by [`Limits`] |
//! | [`read_in`] / [`to_vec_in`] / [`write_into`] | [`blaze_data::Value`] read and written |
//!
//! How `Value` is said: null `f6`, bools `f4`/`f5`, an integer as major 0 (non-negative) or 1
//! (negative) exactly, a float by the rule above, text as major 3, bytes as major 2, a list as
//! major 4, a map as major 5 with text keys in the order above. Reading back, a key that is not
//! text, a tag, `undefined`, another simple value, and a negative integer below `i64::MIN` have
//! no `Value` and are refused.
//!
//! Every refusal is an [`Error`]: the byte offset and why, the reason one of the constants in
//! [`why`] (a caller's own cap names its own reason, as [`Reader`]'s methods take it).
//!
//! Moved here from blaze-pack's `cbor` and blaze-watch's copy of it (rule 30): blaze-pack
//! re-exports this crate under its old path; blaze-watch names it as its `cbor`.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

use core::fmt;

mod event;
mod reader;
mod value;
pub mod write;

#[cfg(test)]
mod plant;
#[cfg(test)]
mod tests;

/// The planted bug the round trip's control asks for, compiled away outside tests.
#[cfg(not(test))]
mod plant {
    #[inline(always)]
    pub(crate) fn length_first() -> bool {
        true
    }
}

pub use event::{Decoder, Encoder, Event, Item, StreamError, StreamWhat};
pub use reader::Reader;
pub use value::{read_in, to_vec_in, write_into};
pub use write::{Head, array, boolean, bytes, bytes_head, float, head_len, int, map, negative, null, text, uint};

/// The major types (RFC 8949, section 3.1).
pub mod major {
    pub const UINT: u8 = 0;
    pub const NEGATIVE: u8 = 1;
    pub const BYTES: u8 = 2;
    pub const TEXT: u8 = 3;
    pub const ARRAY: u8 = 4;
    pub const MAP: u8 = 5;
    pub const TAG: u8 = 6;
    pub const SIMPLE: u8 = 7;
}

/// The reasons this crate refuses bytes, by name: what [`Error::why`] holds.
pub mod why {
    pub const END_AT_VALUE: &str = "the input ends where a value starts";
    pub const END_IN_VALUE: &str = "the input ends before the value does";
    pub const INDEFINITE: &str = "a length that is indefinite or reserved";
    pub const NOT_SHORTEST: &str = "a number not in its shortest form";
    pub const FLOAT_NOT_SHORTEST: &str = "a float not in its shortest exact form";
    pub const NAN_NOT_CANONICAL: &str = "a NaN other than `f9 7e00`";
    pub const BYTES_AFTER: &str = "bytes after the value's end";
    pub const BYTES_PAST_INPUT: &str = "a byte string longer than what is left of the input";
    pub const TEXT_PAST_INPUT: &str = "a text string longer than what is left of the input";
    pub const ARRAY_PAST_INPUT: &str = "an array of more items than bytes are left of the input";
    pub const MAP_PAST_INPUT: &str = "a map of more entries than bytes are left of the input";
    pub const NOT_UTF8: &str = "a text string that is not UTF-8";
    pub const NOT_UINT: &str = "not an unsigned integer";
    pub const NOT_BOOL: &str = "not a boolean";
    pub const NOT_BYTES: &str = "not a byte string";
    pub const NOT_TEXT: &str = "not a text string";
    pub const NOT_ARRAY: &str = "not an array";
    pub const FIXED_LONGER: &str = "a byte string longer than its field's";
    pub const FIXED_LENGTH: &str = "a byte string of another length than its field's";
    pub const RECORD_LENGTH: &str = "an array of another length than its record's";
    pub const KEY_NOT_TEXT: &str = "a map key that is not a text string";
    pub const KEY_ORDER: &str = "map keys not in the bytewise order of their encodings";
    pub const KEY_TWICE: &str = "a map key twice";
    pub const TAG: &str = "a tag, which no value here has";
    pub const SIMPLE: &str = "a simple value other than false, true and null";
    pub const NEGATIVE_PAST_I64: &str = "a negative integer below i64::MIN";
    pub const TOO_LARGE: &str = "an input past the limit on bytes";
    pub const TOO_DEEP: &str = "arrays and maps nested past the limit";
}

/// Why a read stopped: where, and what was wrong there (one of [`why`], or a caller's cap).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    pub at: usize,
    pub why: &'static str,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CBOR at byte {}: {}", self.at, self.why)
    }
}

impl core::error::Error for Error {}

/// The bounds a read of untrusted bytes holds to. Each item takes at least a byte, so a
/// length is never trusted past what is left of the input either.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    pub most_bytes: usize,
    /// Arrays and maps open at once.
    pub most_depth: usize,
}

impl Limits {
    /// 256 MiB, 128 deep: an agent request or answer.
    pub const DEFAULT: Limits = Limits { most_bytes: 256 << 20, most_depth: 128 };
}

pub(crate) fn wide(n: usize) -> u64 {
    match u64::try_from(n) {
        Ok(n) => n,
        Err(_) => unreachable!("blaze's targets have a `usize` of 64 bits or fewer"),
    }
}
