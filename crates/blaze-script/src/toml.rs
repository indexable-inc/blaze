//! TOML for the scripts, the one home: blaze-toml's reader, and a document lowered to
//! blaze-data's [`Value`] (tables in the order the text names their keys, a datetime as its
//! RFC 3339 text). A script that needs a key's line reads the [`Document`] and its [`Item`]s.

use std::alloc::Global;

pub use blaze_data::{Map, Value};
pub use blaze_toml::{Document, Error, Item, Limits};

/// A TOML table, lowered.
pub type Table = Map<Global>;

/// `text` read as TOML, bounded by [`Limits::CARGO`], and lowered to its root table. A refusal
/// is shown with `error.display(text)`: its line, column and what.
pub fn parse(text: &str) -> Result<Table, Error> {
    let doc = Document::parse_in(text, Limits::CARGO, Global)?;
    match doc.to_value_in(Global) {
        Value::Map(table) => Ok(table),
        _ => unreachable!("a TOML document's root is a table"),
    }
}
