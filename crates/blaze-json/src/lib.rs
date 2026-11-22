//! JSON (RFC 8259) as one format over [`blaze_data::Value`], in place of `serde_json`.
//!
//! - [`Reader`]: bytes to [`Event`]s, a pull state machine that allocates nothing; strings come
//!   back as [`Str`]s borrowed from the text, decoded only when asked.
//! - [`read_in`]: bytes to a [`Value`](blaze_data::Value), over the reader.
//! - [`write_into`] / [`to_string_in`]: a `Value` to text; [`Writer`]: events to text.
//! - [`from_slice`] / [`to_string`] (and their `_in` forms): bytes straight to a
//!   [`FromValue`](blaze_data::FromValue) type, a [`ToValue`](blaze_data::ToValue) type
//!   straight to text, strict by the type's rules too ([`FromJsonError`]).
//!
//! Strict, as RFC 8259 is: no trailing commas, comments, single quotes, `NaN`, leading `+` or
//! leading zeros; text must be UTF-8 (checked once, up front); a string's lone surrogate escape
//! is refused. Duplicate keys are refused by default ([`Duplicates`]). An integer is read
//! exactly into an `i64`/`u64` and one past both is refused, never rounded through a float; a
//! number with a fraction or exponent is an `f64`, and one past its range is refused. Every
//! bound on untrusted text is a parameter in [`Limits`], and passing one is a typed refusal.
//!
//! Every [`Error`] names its byte offset; [`Error::line_col`] turns it into a line and column
//! from the text. Writers are deterministic and state their key order ([`KeyOrder`]).
//!
//! The reader is a pull reader over one whole text already in memory, not a `feed`/`finish`
//! codec (D170 C5): its state is the grammar's place and a bit stack, and every caller has the
//! whole text (a file, an agent message, an index line). A text arriving in pieces would need a
//! partial token (a string or number cut mid-way) kept beside that state; it is not written
//! until a caller needs it.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use core::fmt;

mod plant;
mod read;
mod stack;
mod tree;
mod typed;
mod write;

#[cfg(test)]
mod tests;

pub use plant::JsonPlant;
pub use read::{Event, Item, Reader, Str};
pub use tree::read_in;
pub use typed::{FromJsonError, from_slice, from_slice_in, to_string, to_string_of_in, to_string_pretty};
pub use write::{
    BytesAs, KeyOrder, Style, StreamError, StreamWhat, Unwritable, WriteError, WriteOptions, Writer, to_string_in,
    write_into, write_into_planted,
};

/// What to do with a key an object already has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Duplicates {
    /// A typed error at the second key (the default: RFC 8259 leaves it undefined).
    Refuse,
    /// The later value replaces the earlier one, keeping its place.
    LastWins,
}

/// The bounds a read holds untrusted text to (D165: every cap the reader enforces is a field
/// here, taken as a value, and drawn by the simulation down to its least legal value), and the
/// one choice of what a duplicate key means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most bytes of text ([`What::TooLarge`] past it); at least 1.
    pub most_bytes: usize,
    /// The most lists and maps open at once ([`What::TooDeep`] past it); at least 1. Any limit
    /// is held: nothing recurses on depth, and the open levels cost a bit each in the reader and
    /// a slot each in [`read_in`]'s build.
    pub most_depth: usize,
    /// The most bytes of one number as written ([`What::NumberTooLong`] past it); at least 1.
    pub most_number_len: usize,
    pub duplicates: Duplicates,
}

impl Limits {
    /// What the crate ships with, for blaze's own inputs (agent requests, profiles,
    /// rustc-format diagnostics): 256 MiB, 128 deep, numbers of up to 100 bytes, duplicates
    /// refused.
    pub const SHIPPED: Limits = Limits {
        most_bytes: 256 << 20,
        most_depth: 128,
        most_number_len: 100,
        duplicates: Duplicates::Refuse,
    };
    /// The least legal value of each cap: what a simulation draws them down to.
    pub const SMALLEST: Limits =
        Limits { most_bytes: 1, most_depth: 1, most_number_len: 1, duplicates: Duplicates::Refuse };
    /// [`Limits::SHIPPED`] under its older name, which callers outside the crate still use.
    pub const DEFAULT: Limits = Limits::SHIPPED;
}

/// What a reader wanted where it found something else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expected {
    Value,
    /// A value or `]` after `[`.
    ValueOrEndList,
    /// A key or `}` after `{`.
    KeyOrEndMap,
    /// A key after `,` in a map.
    Key,
    Colon,
    CommaOrEndList,
    CommaOrEndMap,
    /// A digit in a number.
    Digit,
    /// Four hex digits after `\u`.
    Hex,
    /// The closing `"`.
    Quote,
    /// The rest of `true`, `false` or `null`.
    Literal(&'static str),
    /// Nothing more after the value.
    End,
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expected::Value => f.write_str("a value"),
            Expected::ValueOrEndList => f.write_str("a value or `]`"),
            Expected::KeyOrEndMap => f.write_str("a key or `}`"),
            Expected::Key => f.write_str("a key"),
            Expected::Colon => f.write_str("`:`"),
            Expected::CommaOrEndList => f.write_str("`,` or `]`"),
            Expected::CommaOrEndMap => f.write_str("`,` or `}`"),
            Expected::Digit => f.write_str("a digit"),
            Expected::Hex => f.write_str("four hex digits"),
            Expected::Quote => f.write_str("the closing `\"`"),
            Expected::Literal(word) => write!(f, "`{word}`"),
            Expected::End => f.write_str("the end of the text"),
        }
    }
}

/// What is wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum What {
    /// The bytes are not UTF-8 from here on.
    Utf8,
    TooLarge { len: usize, most: usize },
    /// A list or map opened past the depth limit.
    TooDeep { most: usize },
    /// The text ended where something was wanted.
    End { expected: Expected },
    /// A character where something else was wanted.
    Unexpected { found: char, expected: Expected },
    /// A control character (below U+0020) inside a string.
    Control { byte: u8 },
    /// `\` followed by a character that is no escape.
    BadEscape { found: char },
    /// A `\uD800`-`\uDFFF` escape that is not half of a pair.
    LoneSurrogate,
    /// `01`.
    LeadingZero,
    NumberTooLong { most: usize },
    /// An integer below `i64::MIN` or above `u64::MAX`.
    IntegerOutOfRange,
    /// A number whose float is infinite.
    FloatOutOfRange,
    /// A key the object already has (under [`Duplicates::Refuse`]).
    DuplicateKey,
}

impl fmt::Display for What {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            What::Utf8 => f.write_str("the text is not UTF-8"),
            What::TooLarge { len, most } => write!(f, "the text is {len} bytes, past the limit of {most}"),
            What::TooDeep { most } => write!(f, "more than {most} lists and maps are open"),
            What::End { expected } => write!(f, "the text ends where {expected} was wanted"),
            What::Unexpected { found, expected } => write!(f, "{found:?} where {expected} was wanted"),
            What::Control { byte } => write!(f, "control character 0x{byte:02x} in a string"),
            What::BadEscape { found } => write!(f, "`\\{found}` is no escape"),
            What::LoneSurrogate => f.write_str("a surrogate escape that is not half of a pair"),
            What::LeadingZero => f.write_str("a number with a leading zero"),
            What::NumberTooLong { most } => write!(f, "a number longer than {most} bytes"),
            What::IntegerOutOfRange => f.write_str("an integer outside i64 and u64"),
            What::FloatOutOfRange => f.write_str("a number too large for a float"),
            What::DuplicateKey => f.write_str("a key the object already has"),
        }
    }
}

/// A text that is no JSON, or past a limit: the byte offset and what.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    pub at: usize,
    pub what: What,
}

impl Error {
    /// The 1-based line and column (in characters) of the error in `text`.
    pub fn line_col(&self, text: &[u8]) -> (usize, usize) {
        line_col(text, self.at)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.what, self.at)
    }
}

impl core::error::Error for Error {}

/// The 1-based line and column of byte `at` in `text`: lines end at `\n`, and a column counts
/// characters (bytes that do not continue a UTF-8 sequence).
pub fn line_col(text: &[u8], at: usize) -> (usize, usize) {
    let before = &text[..at.min(text.len())];
    let line_start = before.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let line = 1 + before.iter().filter(|&&b| b == b'\n').count();
    let col = 1 + before[line_start..].iter().filter(|&&b| b & 0xC0 != 0x80).count();
    (line, col)
}
