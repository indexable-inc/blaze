//! A row change: what [`crate::program::Program::place`] gives for a demand or a delta, the
//! rows that came, went or moved, each list sorted, so a change is a function of the program
//! and the delta. The linker makes its own change of it (blaze-link's `Change`): a row that
//! came is code or a value to add by its key, a row that went is one to remove by its key, and
//! a row that moved keeps its code while the span table of an item it is made from is given
//! again. The bytes, their places and the file are the linker's: nothing here lays anything
//! out, and nothing is relinked.
//!
//! **The floor.** A change lists every row whose key moved, twice (it went under the old key
//! and came under the new), every row new to or gone from the set, and every pair of a moved
//! item and a row made from it whose key did not move: nothing else, so its length is the
//! edit's, not the program's. Computing it reads the walk's output and, for a moved item, the
//! rows of that item and its inliners by range: proportional to the change too.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use crate::codec::Digest;
use crate::key::{ItemId, Node, Symbol};

/// A row, by its node and its key: an instance key for a function, a value key for a static.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Row {
    pub node: Node,
    pub key: Digest,
}

/// What the program's set of rows changed by.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RowChange {
    /// The program held no row before: every row came (a first build).
    pub whole: bool,
    /// Rows new to the set, and rows held before under another key, under their new key.
    pub came: Vec<Row, Global>,
    /// Rows gone from the set, and rows whose key moved, under their old key.
    pub went: Vec<Row, Global>,
    /// `(item, function)`: a function whose key did not move, made from text of `item`, an item
    /// that moved in its file (its own item, or one of an instance it took in).
    pub moved: Vec<(ItemId, Symbol), Global>,
}

impl RowChange {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.came.is_empty() && self.went.is_empty() && self.moved.is_empty()
    }
}
