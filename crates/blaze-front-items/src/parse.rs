//! `parse(file, edition)`: a pure function from a file's text, its edition and its crate's
//! syntax extensions to the tree, as the record the tree crate already stores
//! (`syntax::image`: the lexer's and parser's errors and `FlatTree`'s image, under
//! `syntax::image::TREE_VERSION`).
//!
//! Nothing here is new but the step's identity: the record is `image::write`'s bytes for a
//! full parse (every body parsed: `items(file)` takes the items out of bodies, so an outline
//! would be parsed in full by the next step anyway). The record does not hold the text; a
//! reader gives the text it was made of ([`tree`]), which the same world pins.
//!
//! Cost against the floor: the floor for any edit of a file is "reparse the file", which is
//! this; a comment edit changes the record (no cutoff here: any byte moves it).

#![blaze::sans_io]

use alloc::{format, string::String, sync::Arc, vec::Vec};

use blaze_bytes::Bytes;
use syntax::{Edition, Extensions, Parse, SourceFile, Text, image};

/// How a file is read: its edition and its crate's syntax extensions.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Reading {
    pub edition: Edition,
    pub extensions: Extensions,
}

/// The tree of `text` read as `reading` says, as its record.
pub fn parse(text: &str, reading: Reading) -> Vec<u8> {
    let parsed = SourceFile::parse_shared_with(text, reading.edition, reading.extensions);
    // alloc: the step's value, kept by the kernel
    let mut out = Vec::new();
    let record = image::write(&parsed, &mut out);
    debug_assert_eq!(record.start, 0, "a record written to an empty buffer starts it");
    out.truncate(record.end);
    out
}

/// The parse `record` holds, which [`parse`] made of `text` read as `reading` says. A record
/// of another parser version or other extensions, or of a text of another length, is refused
/// saying which: never parsed again in its place. A full parse's record does not name its
/// edition (`syntax::image`'s word for it is 0), so the edition is the step's identity's to
/// hold, and it does ([`crate::steps`]).
pub fn tree(record: &[u8], text: &str, reading: Reading) -> Result<Parse<SourceFile>, String> {
    // alloc: the tree is read in place from a counted copy of the record
    let bytes = Bytes::from(Arc::<[u8]>::from(record));
    // alloc: the tree shares a counted copy of the text
    let text = Text::from(text);
    image::read(&bytes, 0, &text, reading.edition, false, reading.extensions).map_err(|error| {
        // alloc: error path
        format!("a tree record of {} bytes: {} (at byte {})", record.len(), error.what, error.at)
    })
}
