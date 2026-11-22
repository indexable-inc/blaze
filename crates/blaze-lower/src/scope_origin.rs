//! Which text wrote a block scope of a body: the item's own, or one macro expansion's, and
//! where in that text its `{` is.
//!
//! The Reference, "Statements", "Item declarations" [statement.item.scope]: "Declaring an item
//! within a statement block restricts its scope to the block containing the statement." A
//! block an expansion writes is a block like any other: its items are its own. The names
//! library files such a block as a module named after the CALL that wrote it and the block's
//! place among that expansion's blocks (blaze-scope expanded_blocks.rs), so a reader of the
//! body that resolves a name written in the block must know which expansion wrote the block
//! and which block of that expansion it is. The lowering knows it when it reads the block's
//! `{`: the word it reads is one of the expansion's words ([`crate::machine`]'s `ExpWord`
//! carries the expansion's index). So it writes it into the scope's row, and no reader
//! recovers it from positions afterwards.
//!
//! A scope row ([`crate::form::Scope`]) holds this in fields its own encoding leaves empty:
//! `d` = the `{`'s offset in the text that wrote it (item-relative for the item's own text;
//! relative to the start of the expansion's text for an expansion's: rule 71), `e` = the
//! expansion's index ([`crate::View::expansion_calls`] order) or [`NONE`] for the item's own
//! text. A scope with `d` [`NONE`] is the body's outermost scope (its parameters'), which no
//! `{` opens.

use crate::form::{NONE, Node};

/// Where a block scope's `{` was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeOrigin {
    /// The body's outermost scope (its parameters' and its module's): no block opens it.
    Root,
    /// A block of the enclosing item's own text; `open` is its `{`, item-relative.
    Item { open: u32 },
    /// A block an expansion wrote: the expansion's index (the order of
    /// [`crate::View::expansion_calls`]) and its `{`'s offset in that expansion's own text
    /// ([`crate::View::expansion_text`]).
    Expansion { expansion: u32, open: u32 },
}

impl ScopeOrigin {
    /// Written into a scope's row (fields `d` and `e`, which [`crate::form::Scope::encode`]
    /// leaves [`NONE`]).
    pub(crate) fn write(self, row: &mut Node) {
        let (d, e) = match self {
            ScopeOrigin::Root => (NONE, NONE),
            ScopeOrigin::Item { open } => (open, NONE),
            ScopeOrigin::Expansion { expansion, open } => (open, expansion),
        };
        row.d = d;
        row.e = e;
    }

    /// Read from a scope's row.
    pub(crate) fn read(row: &Node) -> ScopeOrigin {
        match (row.d, row.e) {
            (NONE, _) => ScopeOrigin::Root,
            (open, NONE) => ScopeOrigin::Item { open },
            (open, expansion) => ScopeOrigin::Expansion { expansion, open },
        }
    }
}

/// Where a body macro call that was expanded stands: the text its path is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallAt {
    /// In the enclosing item's own text, starting at `start` (item-relative).
    Item { start: u32 },
    /// In the text of the earlier expansion `expansion`, at `start` of that text.
    Expansion { expansion: u32, start: u32 },
}

#[cfg(test)]
mod tests {
    use super::{NONE, Node, ScopeOrigin};

    /// Each origin is read back from the row it is written into, and an untouched row (the
    /// outermost scope's, `Node::of`) reads as the root. Prints nothing.
    #[test]
    fn a_scope_origin_round_trips_through_its_row() {
        for origin in [ScopeOrigin::Root, ScopeOrigin::Item { open: 0 }, ScopeOrigin::Item { open: 41 }, ScopeOrigin::Expansion { expansion: 0, open: 1 }, ScopeOrigin::Expansion { expansion: 7, open: 0 }] {
            let mut row = Node::of(0);
            origin.write(&mut row);
            assert_eq!(ScopeOrigin::read(&row), origin);
        }
        assert_eq!(ScopeOrigin::read(&Node::of(0)), ScopeOrigin::Root);
        assert_eq!(NONE, u32::MAX);
    }
}
