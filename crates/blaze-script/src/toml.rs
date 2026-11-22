//! TOML for the scripts, the one home: blaze-toml's reader, a document lowered to blaze-data's
//! value (tables in the order the text names their keys, a datetime as its RFC 3339 text), and
//! the document itself for a script whose errors name a key's line ([`read`], [`Node`]). Each
//! is named here with the allocator, `Global`, so a script names none.

use std::alloc::Global;

pub use blaze_toml::{Error, Kind, Limits, What};

/// A TOML value, lowered.
pub type Value = blaze_data::Value<Global>;
/// A TOML table, lowered.
pub type Table = blaze_data::Map<Global>;
/// A TOML document as read: every key and value with its place in the text.
pub type Doc<'t> = blaze_toml::Document<'t, Global>;
/// A table, array or value of a [`Doc`].
pub type Node<'d, 't> = blaze_toml::Item<'d, 't, Global>;

/// `text` read as TOML, bounded by [`Limits::CARGO`]. A refusal is shown with
/// `error.display(text)`: its line, column and what.
pub fn read(text: &str) -> Result<Doc<'_>, Error> {
    blaze_toml::Document::parse_in(text, Limits::CARGO, Global)
}

/// `text` read as TOML and lowered to its root table.
pub fn parse(text: &str) -> Result<Table, Error> {
    Ok(read(text)?.to_map_in(Global))
}
