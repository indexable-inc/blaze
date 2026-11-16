//! TOML 1.0 as one format, in place of `toml` and `toml_edit`.
//!
//! - [`Document::parse_in`]: text to a [`Document`] whose nodes keep every key's and value's
//!   byte range (manifests need them: errors on the right line, and edits that keep the rest);
//!   [`Item`] reads it, and [`Document::to_value_in`] lowers it to a
//!   [`blaze_data::Value`] (a datetime as its RFC 3339 text).
//! - [`write_document`] / [`write_inline`]: a `Value` to TOML text, keys in insertion order.
//! - [`Edit`]: set a key, or put a value into an array, keeping every other byte of the text.
//!
//! The reader is the whole of TOML 1.0: tables, arrays of tables and inline tables, dotted
//! keys, the four string forms, integers in every radix, floats with `inf` and `nan`, booleans,
//! and the four datetime forms, checked for real days and times. Which later lines may add to
//! a table follows TOML's rules ([`Origin`]); a breach names both places. Every bound on
//! untrusted text is a parameter in [`Limits`], and passing one is a typed refusal. Every
//! [`Error`] names a byte offset; [`Error::line_col`] turns it into a line and column.
//!
//! The rules on redefining tables are the ones makepad/relax's TOML reader states (its
//! `Origin` and header-versus-dotted navigation, libs/toml_parser/src/toml.rs); the code here is
//! written for this document's columns.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

use core::fmt;

mod doc;
mod edit;
mod lower;
mod parse;
mod write;

#[cfg(test)]
mod plant;
#[cfg(test)]
mod tests;

/// The planted bug the edit control asks for, compiled away outside tests.
#[cfg(not(test))]
mod plant {
    #[inline(always)]
    pub(crate) fn body_end(end: usize) -> usize {
        end
    }
}

pub use doc::{Children, Date, Datetime, Document, Item, Kind, Offset, Origin, Time};
pub use edit::{Edit, EditError, EditWhat};
pub use write::{DocumentOptions, Unwritable, WriteError, write_document, write_inline, write_key};

/// The bounds a read holds untrusted text to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most bytes of text; at most `u32::MAX`.
    pub most_bytes: usize,
    /// The most arrays and inline tables open at once.
    pub most_depth: usize,
    /// The most nodes (tables, arrays and values).
    pub most_nodes: usize,
}

impl Limits {
    /// For manifests, configs and lockfiles: 64 MiB, 64 deep, 4 million nodes.
    pub const CARGO: Limits = Limits { most_bytes: 64 << 20, most_depth: 64, most_nodes: 4 << 20 };
}

/// The most parts of one dotted key (`a.b.c` has three).
pub const MOST_KEY_PARTS: usize = 64;

/// What a reader wanted where it found something else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expected {
    Key,
    /// `=` after a key.
    Equals,
    Value,
    /// The end of the line, after a key/value or a header (a comment may come first).
    EndOfLine,
    /// `]` closing `[a`.
    CloseHeader,
    /// `]]` closing `[[a`.
    CloseArrayHeader,
    /// `,` or `]` in an array.
    CommaOrEndArray,
    /// `,` or `}` in an inline table.
    CommaOrEndInline,
    Digit,
    Hex,
    /// The closing quote(s) of a string.
    Quote,
    /// A part of a date or time (`-`, `:`, two digits).
    DatetimePart,
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Expected::Key => "a key",
            Expected::Equals => "`=`",
            Expected::Value => "a value",
            Expected::EndOfLine => "the end of the line",
            Expected::CloseHeader => "`]`",
            Expected::CloseArrayHeader => "`]]`",
            Expected::CommaOrEndArray => "`,` or `]`",
            Expected::CommaOrEndInline => "`,` or `}`",
            Expected::Digit => "a digit",
            Expected::Hex => "hex digits",
            Expected::Quote => "the closing quote",
            Expected::DatetimePart => "the rest of a date or time",
        })
    }
}

/// What is wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum What {
    TooLarge { len: usize, most: usize },
    TooDeep { most: usize },
    TooManyNodes { most: usize },
    KeyTooLong { most: usize },
    End { expected: Expected },
    Unexpected { found: char, expected: Expected },
    /// A control character where TOML allows none (strings, comments).
    Control { ch: char },
    /// A `\r` not followed by `\n`.
    BareCarriageReturn,
    BadEscape { found: char },
    /// `\u`/`\U` naming no Unicode scalar value.
    NotAScalar { value: u32 },
    LeadingZero,
    /// `_` not between two digits.
    Underscore,
    /// A float of more than this many characters (underscores aside).
    NumberTooLong { most: usize },
    IntegerOutOfRange,
    FloatOutOfRange,
    /// A date or time that does not exist (`2026-02-30`, `24:00:00`).
    NoSuchDatetime,
    /// A key the table already has; `first` is where it was.
    Duplicate { first: usize },
    /// A table defined twice; `first` is where.
    Redefined { first: usize },
    /// A key path through something that is not a table; `first` is where that was made.
    NotATable { first: usize },
    /// Adding to an inline table or a `[ ]` array, which are closed once written.
    Closed { first: usize },
    /// `[[a]]` where `a` is not an array of tables.
    NotAnArrayOfTables { first: usize },
}

impl fmt::Display for What {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            What::TooLarge { len, most } => write!(f, "the text is {len} bytes, past the limit of {most}"),
            What::TooDeep { most } => write!(f, "more than {most} arrays and inline tables are open"),
            What::TooManyNodes { most } => write!(f, "more than {most} tables, arrays and values"),
            What::KeyTooLong { most } => write!(f, "a dotted key of more than {most} parts"),
            What::End { expected } => write!(f, "the text ends where {expected} was wanted"),
            What::Unexpected { found, expected } => write!(f, "{found:?} where {expected} was wanted"),
            What::Control { ch } => write!(f, "control character {ch:?} is not allowed here"),
            What::BareCarriageReturn => f.write_str("a carriage return not followed by a newline"),
            What::BadEscape { found } => write!(f, "`\\{found}` is no escape"),
            What::NotAScalar { value } => write!(f, "U+{value:04X} is not a Unicode scalar value"),
            What::LeadingZero => f.write_str("a number with a leading zero"),
            What::Underscore => f.write_str("`_` must stand between two digits"),
            What::NumberTooLong { most } => write!(f, "a float longer than {most} characters"),
            What::IntegerOutOfRange => f.write_str("an integer outside i64"),
            What::FloatOutOfRange => f.write_str("a number too large for a float"),
            What::NoSuchDatetime => f.write_str("no such date or time"),
            What::Duplicate { first } => write!(f, "the key is already defined at byte {first}"),
            What::Redefined { first } => write!(f, "the table is already defined at byte {first}"),
            What::NotATable { first } => write!(f, "the key at byte {first} is not a table"),
            What::Closed { first } => {
                write!(f, "the inline table or array at byte {first} is closed once written")
            }
            What::NotAnArrayOfTables { first } => {
                write!(f, "the key at byte {first} is not an array of tables")
            }
        }
    }
}

/// A text that is no TOML, or past a limit: the byte offset and what.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    pub at: usize,
    pub what: What,
}

impl Error {
    /// The 1-based line and column (in characters) of the error in `text`.
    pub fn line_col(&self, text: &str) -> (usize, usize) {
        line_col(text, self.at)
    }

    /// `line L, column C: what`, for a person.
    pub fn display<'e>(&'e self, text: &'e str) -> impl fmt::Display + 'e {
        struct Shown<'e>(&'e Error, &'e str);
        impl fmt::Display for Shown<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let (line, col) = self.0.line_col(self.1);
                write!(f, "line {line}, column {col}: {}", self.0.what)
            }
        }
        Shown(self, text)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.what, self.at)
    }
}

impl core::error::Error for Error {}

/// The 1-based line and column of byte `at` in `text`: lines end at `\n`, a column counts
/// characters. (The same rule as blaze-json's; one home for both is a text crate to come.)
pub fn line_col(text: &str, at: usize) -> (usize, usize) {
    let before = &text.as_bytes()[..at.min(text.len())];
    let line_start = before.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let line = 1 + before.iter().filter(|&&b| b == b'\n').count();
    let col = 1 + before[line_start..].iter().filter(|&&b| b & 0xC0 != 0x80).count();
    (line, col)
}
