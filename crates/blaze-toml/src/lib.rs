//! TOML 1.0 as one format, in place of `toml` and `toml_edit`.
//!
//! - [`Document::parse_in`]: text to a [`Document`] whose nodes keep every key's and value's
//!   byte range (manifests need them: errors on the right line, and edits that keep the rest);
//!   [`Item`] reads it, and [`Document::to_value_in`] lowers it to a
//!   [`blaze_data::Value`] (a datetime as its RFC 3339 text).
//! - [`write_document`] / [`write_inline`]: a `Value` to TOML text, keys in insertion order;
//!   [`write_string`], [`write_key`] and [`write_dotted_key`] spell strings and keys as cargo
//!   does (basic, literal or multi-line by what the text holds, [`StringForm`]).
//! - [`Edit`]: set a key, put a value into an array, or remove a key or a table, keeping every
//!   other byte of the text.
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

use blaze_text::{Put, Show, Shown, Writes as _};

mod doc;
mod edit;
mod lower;
mod parse;
mod plant;
mod write;

#[cfg(test)]
mod tests;

pub use doc::{Children, Date, Datetime, Document, Item, Kind, Offset, Origin, Time};
pub use plant::TomlPlant;
pub use edit::{Edit, EditError, EditWhat};
pub use write::{
    DocumentOptions, StringForm, Unwritable, WriteError, write_document, write_dotted_key, write_inline, write_key,
    write_string, write_string_in_form,
};

/// The bounds a read holds untrusted text to: every cap the reader enforces is a field here
/// (D165), [`Limits::SHIPPED`] the values it ships with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most bytes of text; at most `u32::MAX` (a node's range is two `u32`s).
    pub most_bytes: usize,
    /// The most arrays and inline tables open at once.
    pub most_depth: usize,
    /// The most nodes (tables, arrays and values).
    pub most_nodes: usize,
    /// The most parts of one dotted key (`a.b.c` has three); at most [`MOST_KEY_PARTS`], the
    /// parts a key holds in place.
    pub most_key_parts: usize,
    /// The most characters of a float as written, underscores aside; at most
    /// [`MOST_FLOAT_LEN`], the characters a float is read from in place.
    pub most_float_len: usize,
}

impl Limits {
    /// For manifests, configs and lockfiles: 64 MiB, 64 deep, 4 million nodes, keys of 64
    /// parts, floats of 256 characters.
    pub const SHIPPED: Limits =
        Limits { most_bytes: 64 << 20, most_depth: 64, most_nodes: 4 << 20, most_key_parts: 64, most_float_len: 256 };

    /// [`Limits::SHIPPED`], by the name manifests' readers use.
    pub const CARGO: Limits = Limits::SHIPPED;
}

/// The most parts of one dotted key (`a.b.c` has three) the shipped limits allow, and the most
/// any limits can: a key's parts are held in place.
pub const MOST_KEY_PARTS: usize = Limits::SHIPPED.most_key_parts;

/// The most characters of a float as written the shipped limits allow, and the most any limits
/// can: a float is read from a buffer in place.
pub const MOST_FLOAT_LEN: usize = Limits::SHIPPED.most_float_len;

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

impl Show for Expected {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.put_str(match self {
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
        });
    }
}

/// The bridge for `{}` users: the text is [`Show`]'s.
impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
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
    /// The document's allocator refused memory for its columns: a fact of the caller's
    /// allocator, not of the text, returned so the caller fails its own job (rule 20).
    Memory(blaze_own::AllocFailure),
}

impl Show for What {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match *self {
            What::TooLarge { len, most } => out.str("the text is ").int(len).str(" bytes, past the limit of ").int(most),
            What::TooDeep { most } => out.str("more than ").int(most).str(" arrays and inline tables are open"),
            What::TooManyNodes { most } => out.str("more than ").int(most).str(" tables, arrays and values"),
            What::KeyTooLong { most } => out.str("a dotted key of more than ").int(most).str(" parts"),
            What::End { expected } => out.str("the text ends where ").show(&expected).str(" was wanted"),
            What::Unexpected { found, expected } => out.debug(&found).str(" where ").show(&expected).str(" was wanted"),
            What::Control { ch } => out.str("control character ").debug(&ch).str(" is not allowed here"),
            What::BareCarriageReturn => out.str("a carriage return not followed by a newline"),
            What::BadEscape { found } => out.str("`\\").char(found).str("` is no escape"),
            What::NotAScalar { value } => out.str("U+").hex_upper_fixed(value, 4).str(" is not a Unicode scalar value"),
            What::LeadingZero => out.str("a number with a leading zero"),
            What::Underscore => out.str("`_` must stand between two digits"),
            What::NumberTooLong { most } => out.str("a float longer than ").int(most).str(" characters"),
            What::IntegerOutOfRange => out.str("an integer outside i64"),
            What::FloatOutOfRange => out.str("a number too large for a float"),
            What::NoSuchDatetime => out.str("no such date or time"),
            What::Duplicate { first } => out.str("the key is already defined at byte ").int(first),
            What::Redefined { first } => out.str("the table is already defined at byte ").int(first),
            What::NotATable { first } => out.str("the key at byte ").int(first).str(" is not a table"),
            What::Closed { first } => out.str("the inline table or array at byte ").int(first).str(" is closed once written"),
            What::NotAnArrayOfTables { first } => out.str("the key at byte ").int(first).str(" is not an array of tables"),
            What::Memory(blaze_own::AllocFailure::Refused { what, layout }) => {
                out.str("the allocator refused ").int(layout.size()).str(" bytes for the document's `").str(what).str("`")
            }
            What::Memory(blaze_own::AllocFailure::CapacityOverflow { what, .. }) => {
                out.str("the document's `").str(what).str("` would pass the largest size an allocation may have")
            }
        };
    }
}

/// The bridge for `{}` users: the text is [`Show`]'s.
impl fmt::Display for What {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
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

    /// `line L, column C: what`, for a person: a [`Show`], and a `Display` through [`Shown`].
    pub fn display<'e>(&'e self, text: &'e str) -> Shown<impl Show + 'e> {
        Shown(blaze_text::show_with(move |out| {
            let (line, col) = self.line_col(text);
            out.str("line ").int(line).str(", column ").int(col).str(": ").show(&self.what);
        }))
    }
}

/// `what at byte N`.
impl Show for Error {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.show(&self.what).str(" at byte ").int(self.at);
    }
}

/// The bridge `core::error::Error` needs: the text is [`Show`]'s.
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
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
