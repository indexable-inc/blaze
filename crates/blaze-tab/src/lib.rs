//! Blaze Tabular Data (`blaze-tab`): an LLM-first, human-readable tabular structured data format
//! over [`blaze_data::Value`], inspired by Nushell's NUON and intentionally improving upon it.
//!
//! # Inspiration and Improvements over NUON
//!
//! Like Nushell's NUON (Nushell Object Notation), `blaze-tab` represents lists of records with a
//! once-per-table column header (`[[col1 col2]; [v1 v2] [v3 v4]]`), eliminating repeated keys across
//! rows for 40–60% fewer LLM tokens and bytes than JSON. `blaze-tab` intentionally improves upon
//! NUON in five ways:
//!
//! 1. **Lossless [`blaze_data::Value`] round-trip**: every `Value` variant round-trips with full
//!    fidelity:
//!    - `Null` -> `null`
//!    - `Bool` -> `true` | `false`
//!    - `Int` -> exact signed `i64` AND unsigned `u64` (`i64::MIN..=u64::MAX`, with optional `_`
//!      digit separators on read such as `1_000_000`; unlike NUON which only has `i64`)
//!    - `Float` -> exact `f64` including `nan`, `inf`, `-inf`, and `-0.0` (canonical bit-exact
//!      round-trip; unlike JSON and NUON)
//!    - `Bytes` -> native hex byte literal `0x[deadbeef]` (optional whitespace and `_` inside
//!      brackets on read, lowercase canonical on write; lossless unlike JSON)
//!    - `Text` -> unambiguous bare identifiers/paths when safe (ASCII starting with `[A-Za-z_]`,
//!      continuing with `[A-Za-z0-9_./:-]` for values or `[A-Za-z0-9_./-]` for map keys and column
//!      names, excluding reserved words `null`/`true`/`false`/`inf`/`nan`/`_` and never starting
//!      with `0x[`), or escaped double-quoted `"..."` strings (`\"`, `\\`, `\n`, `\r`, `\t`, `\0`,
//!      `\u{...}`), plus triple-quoted `"""..."""` and `\`-newline continuations on read
//!    - `List` -> `[a b c]` (whitespace-separated; optional commas accepted on read, omitted on
//!      write for minimal LLM tokens)
//!    - `Map` -> `{k1: v1 k2: v2}` (unquoted bare keys when safe, quoted otherwise; optional commas
//!      accepted on read, omitted on write; duplicate keys refused by default with path and offset)
//! 2. **Automatic Tabular & Sparse-Schema Compression**:
//!    - Homogeneous lists of `>= 1` non-empty maps write as compact tables:
//!      ```text
//!      [[kind file line col message];
//!        [error src/main.rs 12 5 "mismatched types"]
//!        [warning Cargo.toml 8 1 "unused dep"]]
//!      ```
//!    - Sparse / optional-column lists of `>= 2` maps with heterogeneous optional fields (which
//!      force NUON to fall back to noisy per-row `{k: v}` maps repeating every key) automatically
//!      emit a **single union-schema table** whenever average key coverage is dense enough (default
//!      `>= 50%`, with at least one shared key), using the absent-field sentinel `_` for omitted
//!      keys. Reading `_` in a table cell omits that key from the row's map, keeping `{a: 1}`
//!      distinct from `{a: 1, b: null}` with 100% round-trip fidelity.
//! 3. **Comments & Clean Ergonomics**: `#` line comments are accepted everywhere whitespace is
//!    allowed; optional commas (including trailing commas) are accepted in lists, maps, table
//!    headers, and table rows, while omitted by writers for maximal token density.
//! 4. **Zero-Recursion Iterative Parser & Writer**: both reader and writer keep their open
//!    containers on explicit allocator-backed stacks bounded by configurable depth caps
//!    ([`Limits::MOST_DEPTH`] = 128), preventing stack overflow on adversarial input, with strict
//!    UTF-8, duplicate-key/column refusal, row-arity refusal, trailing-token refusal, and typed
//!    errors ([`ReadError`], [`WriteError`]) naming byte offset, `line:col`, and path.
//! 5. **Typed [`FromValue`](blaze_data::FromValue) / [`ToValue`](blaze_data::ToValue) API**:
//!    [`read`], [`read_typed`], [`write`], [`write_pretty`], [`write_typed`], and
//!    [`write_typed_pretty`], all taking explicit [`Alloc`](blaze_own::Alloc) with zero third-party
//!    dependencies (`#![no_std]`, `#![blaze::sans_io]`).

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::String;

mod read;
mod typed;
mod write;

#[cfg(test)]
mod tests;

pub use read::read;
pub use typed::{FromTabError, read_typed, write_typed, write_typed_pretty, write_typed_with};
pub use write::{
    KeyOrder, Style, TableMode, WriteError, WriteOptions, WriteWhat, write, write_into, write_pretty, write_with,
};

/// What to do with a key a map already has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Duplicates {
    /// A typed error at the second key (the default).
    Refuse,
    /// The later value replaces the earlier one, keeping its place.
    LastWins,
}

/// Bounds a read holds untrusted text to, and the duplicate-key policy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Maximum input length in bytes ([`What::TooLarge`] past it); at least 1.
    pub most_bytes: usize,
    /// Maximum nested containers open at once ([`What::TooDeep`] past it); at least 1.
    ///
    /// A table produces a `List` of `Map`s, so entering a table row uses two levels of depth
    /// (one for the table list and one for the row map), matching the resulting `Value` depth.
    pub most_depth: usize,
    /// Maximum bytes of one numeric token as written ([`What::NumberTooLong`] past it); at least 1.
    pub most_number_len: usize,
    /// Policy for duplicate keys within a map.
    pub duplicates: Duplicates,
}

impl Limits {
    /// Default maximum nesting depth (matches `blaze-json`).
    pub const MOST_DEPTH: usize = 128;

    /// Shipped defaults: 256 MiB, 128 deep, numbers up to 100 bytes, duplicate keys refused.
    pub const SHIPPED: Limits = Limits {
        most_bytes: 256 << 20,
        most_depth: Limits::MOST_DEPTH,
        most_number_len: 100,
        duplicates: Duplicates::Refuse,
    };

    /// Least legal value of each cap.
    pub const SMALLEST: Limits = Limits {
        most_bytes: 1,
        most_depth: 1,
        most_number_len: 1,
        duplicates: Duplicates::Refuse,
    };

    /// Alias for [`Limits::SHIPPED`].
    pub const DEFAULT: Limits = Limits::SHIPPED;
}

/// What the reader expected where it found something else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expected {
    Value,
    ValueOrEndList,
    KeyOrEndMap,
    Key,
    Colon,
    SeparatorOrEndList,
    SeparatorOrEndMap,
    SeparatorOrEndHeader,
    SeparatorOrEndRow,
    SeparatorOrEndTable,
    ColumnName,
    Semicolon,
    RowOrEndTable,
    CellOrEndRow,
    Digit,
    HexDigit,
    HexEscapeBrace,
    EndBytes,
    Quote,
    TripleQuote,
    End,
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expected::Value => f.write_str("a value"),
            Expected::ValueOrEndList => f.write_str("a value or ']'"),
            Expected::KeyOrEndMap => f.write_str("a key or '}'"),
            Expected::Key => f.write_str("a key"),
            Expected::Colon => f.write_str("':'"),
            Expected::SeparatorOrEndList => f.write_str("whitespace, ',', or ']'"),
            Expected::SeparatorOrEndMap => f.write_str("whitespace, ',', or '}'"),
            Expected::SeparatorOrEndHeader => f.write_str("whitespace, ',', or ']'"),
            Expected::SeparatorOrEndRow => f.write_str("whitespace, ',', or ']'"),
            Expected::SeparatorOrEndTable => f.write_str("whitespace, ',', or ']'"),
            Expected::ColumnName => f.write_str("a column name"),
            Expected::Semicolon => f.write_str("';' after table header"),
            Expected::RowOrEndTable => f.write_str("a table row '[' or ']'"),
            Expected::CellOrEndRow => f.write_str("a cell value, '_', or ']'"),
            Expected::Digit => f.write_str("a digit"),
            Expected::HexDigit => f.write_str("a hex digit"),
            Expected::HexEscapeBrace => f.write_str("'}' to close unicode escape"),
            Expected::EndBytes => f.write_str("']' to close hex bytes"),
            Expected::Quote => f.write_str("the closing '\"'"),
            Expected::TripleQuote => f.write_str("the closing '\"\"\"'"),
            Expected::End => f.write_str("the end of the text"),
        }
    }
}

/// Specific reason a read failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum What {
    /// Input bytes are not valid UTF-8 from this byte offset.
    Utf8,
    /// Input exceeds [`Limits::most_bytes`].
    TooLarge { len: usize, most: usize },
    /// Container nesting exceeds [`Limits::most_depth`].
    TooDeep { most: usize },
    /// Input ended where a token was expected.
    End { expected: Expected },
    /// Unexpected character where something else was expected.
    Unexpected { found: char, expected: Expected },
    /// Unescaped ASCII control character (< 0x20) inside a single-line string.
    Control { byte: u8 },
    /// Unknown escape sequence inside a string.
    BadEscape { found: char },
    /// Invalid Unicode scalar value in `\u{...}` (surrogate, empty, > 6 digits, or > 0x10FFFF).
    InvalidUnicodeEscape,
    /// Unpaired UTF-16 surrogate in `\uXXXX`.
    LoneSurrogate,
    /// Integer literal with a leading zero (e.g. `01`).
    LeadingZero,
    /// Numeric token longer than [`Limits::most_number_len`].
    NumberTooLong { most: usize },
    /// Integer outside `i64::MIN..=u64::MAX`.
    IntegerOutOfRange,
    /// Finite float literal overflowed `f64` to infinity.
    FloatOutOfRange,
    /// Hex byte literal `0x[...]` has an odd number of hex digits.
    OddHexBytes,
    /// Reserved keyword used as an unquoted map key or table column name.
    ReservedKey,
    /// Absent-field sentinel `_` used outside a table row cell.
    AbsentOutsideTable,
    /// Duplicate key inside a map (under [`Duplicates::Refuse`]).
    DuplicateKey,
    /// Duplicate column name in a table header.
    DuplicateColumn,
    /// Empty table header `[[]; ...]`.
    EmptyTableHeader,
    /// Table row has a different number of cells than header columns.
    RowArity { row: usize, expected: usize, found: usize },
}

impl fmt::Display for What {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            What::Utf8 => f.write_str("the text is not valid UTF-8"),
            What::TooLarge { len, most } => write!(f, "the text is {len} bytes, past the limit of {most}"),
            What::TooDeep { most } => write!(f, "nesting exceeds limit of {most} containers"),
            What::End { expected } => write!(f, "unexpected end of text where {expected} was wanted"),
            What::Unexpected { found, expected } => write!(f, "{found:?} where {expected} was wanted"),
            What::Control { byte } => write!(f, "unescaped control character 0x{byte:02x} in string"),
            What::BadEscape { found } => write!(f, "unknown string escape '\\{found}'"),
            What::InvalidUnicodeEscape => f.write_str("invalid unicode scalar value in \\u{...} escape"),
            What::LoneSurrogate => f.write_str("unpaired surrogate in \\u escape"),
            What::LeadingZero => f.write_str("integer literal has a leading zero"),
            What::NumberTooLong { most } => write!(f, "number longer than {most} bytes"),
            What::IntegerOutOfRange => f.write_str("integer outside i64..=u64 range"),
            What::FloatOutOfRange => f.write_str("float literal overflows f64"),
            What::OddHexBytes => f.write_str("hex byte literal has an odd number of hex digits"),
            What::ReservedKey => f.write_str("reserved word cannot be an unquoted key or column name; quote it"),
            What::AbsentOutsideTable => f.write_str("absent sentinel '_' is only valid inside a table row cell"),
            What::DuplicateKey => f.write_str("duplicate key in map"),
            What::DuplicateColumn => f.write_str("duplicate column name in table header"),
            What::EmptyTableHeader => f.write_str("table header must name at least one column"),
            What::RowArity { row, expected, found } => {
                write!(f, "table row {row} has {found} cells, expected {expected}")
            }
        }
    }
}

/// A syntax, limit, or schema error while reading `blaze-tab` text.
///
/// Names the byte offset, 1-based `line:col`, structural path (empty at root), and reason.
pub struct ReadError<A: Allocator> {
    pub at: usize,
    pub line: usize,
    pub col: usize,
    pub path: String<A>,
    pub what: What,
}

impl<A: Allocator> PartialEq for ReadError<A> {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at
            && self.line == other.line
            && self.col == other.col
            && self.path.as_str() == other.path.as_str()
            && self.what == other.what
    }
}

impl<A: Allocator> Eq for ReadError<A> {}

impl<A: Allocator> fmt::Display for ReadError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.path.is_empty() {
            true => write!(f, "{} at byte {} ({}:{})", self.what, self.at, self.line, self.col),
            false => write!(f, "{} at byte {} ({}:{}) [{}]", self.what, self.at, self.line, self.col, self.path.as_str()),
        }
    }
}

impl<A: Allocator> fmt::Debug for ReadError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ReadError({self})")
    }
}

impl<A: Allocator> core::error::Error for ReadError<A> {}

/// The 1-based `(line, col)` of byte `at` in `text`: lines end at `\n`, and columns count UTF-8
/// scalar values (bytes that are not UTF-8 continuation bytes `0b10xxxxxx`).
pub fn line_col(text: &[u8], at: usize) -> (usize, usize) {
    let clamped = at.min(text.len());
    let before = text.get(..clamped).unwrap_or(text);
    let line_start = before.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let line = 1 + before.iter().filter(|&&b| b == b'\n').count();
    let tail = before.get(line_start..).unwrap_or(&[]);
    let col = 1 + tail.iter().filter(|&&b| b & 0xC0 != 0x80).count();
    (line, col)
}

/// Whether `word` is a reserved literal or sentinel (`null`, `true`, `false`, `inf`, `nan`, `_`).
pub fn is_reserved_word(word: &str) -> bool {
    matches!(word, "null" | "true" | "false" | "inf" | "nan" | "_")
}

/// Whether `b` can start a bare text token (`[A-Za-z_]`).
pub(crate) fn is_bare_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

/// Whether `b` can continue a bare map key or table column name (`[A-Za-z0-9_./-]`).
pub(crate) fn is_bare_key_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-')
}

/// Whether `b` can continue a bare text value (`[A-Za-z0-9_./:-]`).
pub(crate) fn is_bare_value_cont(b: u8) -> bool {
    is_bare_key_cont(b) || b == b':'
}

/// Whether `text` can be written unquoted in a value position (list item, map value, table cell,
/// or document root) without ambiguity.
pub fn is_bare_value(text: &str) -> bool {
    let bytes = text.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    is_bare_start(first)
        && !is_reserved_word(text)
        && !text.starts_with("0x[")
        && bytes.iter().all(|&b| is_bare_value_cont(b))
}

/// Whether `text` can be written unquoted as a map key or table column name without ambiguity.
pub fn is_bare_key(text: &str) -> bool {
    let bytes = text.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    is_bare_start(first)
        && !is_reserved_word(text)
        && !text.starts_with("0x[")
        && bytes.iter().all(|&b| is_bare_key_cont(b))
}
