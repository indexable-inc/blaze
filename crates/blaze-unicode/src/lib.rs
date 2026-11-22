//! The Unicode properties blaze reads, and only those: `XID_Start` and `XID_Continue` (the
//! lexer's identifiers, Rust's rule), `Emoji` (the lexer's recovery of an identifier with an
//! emoji), the general categories `Po` (the formatter's string breaks) and `Nd` (`\d` of
//! blaze-script's matcher), display width (diagnostics, the screen, the formatter), legacy
//! grapheme clusters (the formatter's string breaks) and canonical decomposition, NFD (case- and
//! normalization-insensitive path names in blaze-fetch).
//!
//! The tables are generated data (`src/tables.rs`): sorted lists of disjoint inclusive code point
//! ranges, read by binary search after an ASCII fast path. The generator is
//! `scripts/src/bin/unicode_tables.rs`, run over the Unicode Character Database of
//! [`UNICODE_VERSION`]; nothing here is edited by hand.
//!
//! No dependency, and nothing allocates: [`nfd_chars`] gives NFD's characters for the caller to
//! put where it keeps text. `alloc` only for the deprecated [`nfd`], which writes into a
//! global-heap `String`, while its one caller (blaze-fetch) moves.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core, blaze::std_use, blaze::implicit_allocator, blaze::process_static)]
#![forbid(unsafe_code)]

// Only the deprecated [`nfd`]'s `String`: it goes with its last caller, and this line with it.
extern crate alloc;

// The tests' `String`, `Vec` and `format!`.
#[cfg(test)]
extern crate std;

mod grapheme;
mod nfd;
mod tables;
mod width;

#[cfg(test)]
mod tests;

pub use grapheme::{Graphemes, graphemes};
pub use nfd::{Nfd, combining_class, nfd, nfd_chars};
pub use width::{char_width, str_width};

/// The Unicode version of the identifier and emoji tables, the lexer's: rustc's pinned
/// nightly reads `XID_Start`, `XID_Continue` and `Emoji` of this version.
pub const UNICODE_VERSION: (u8, u8, u8) = tables::UNICODE_VERSION;

/// Whether `c` is in `set`, a sorted list of disjoint inclusive ranges.
fn in_set(set: &[(u32, u32)], c: char) -> bool {
    let cp = u32::from(c);
    // The first range whose last code point is at or past `cp`.
    let at = set.partition_point(|&(_, last)| last < cp);
    set.get(at).is_some_and(|&(first, _)| first <= cp)
}

/// The value of `c` in `table`, a sorted list of disjoint inclusive ranges with a value each,
/// or `default` outside every range.
fn value_in(table: &[(u32, u32, u8)], c: char, default: u8) -> u8 {
    let cp = u32::from(c);
    let at = table.partition_point(|&(_, last, _)| last < cp);
    match table.get(at) {
        Some(&(first, _, value)) if first <= cp => value,
        _ => default,
    }
}

/// `XID_Start`: a character that may begin an identifier. Rust's identifiers begin with one of
/// these or `_` (which is not one).
pub fn is_xid_start(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic();
    }
    in_set(tables::XID_START, c)
}

/// `XID_Continue`: a character that may follow the first of an identifier (`_` is one).
pub fn is_xid_continue(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric() || c == '_';
    }
    in_set(tables::XID_CONTINUE, c)
}

/// `Emoji=Yes` (UTS #51): ASCII's `#`, `*` and the digits are among them.
pub fn is_emoji(c: char) -> bool {
    in_set(tables::EMOJI, c)
}

/// `General_Category=Other_Punctuation` (`Po`).
pub fn is_other_punctuation(c: char) -> bool {
    in_set(tables::OTHER_PUNCTUATION, c)
}

/// `General_Category=Decimal_Number` (`Nd`): the ASCII digits and the other scripts' digits.
pub fn is_decimal_number(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    in_set(tables::DECIMAL_NUMBER, c)
}
