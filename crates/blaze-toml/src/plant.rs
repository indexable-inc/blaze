//! The bugs a control can plant in the reader (D165, rule 78: a strategy is a type parameter at
//! one seam). [`Document::parse_in_planted`](crate::Document::parse_in_planted) takes one as
//! `plant: impl Planted<TomlPlant>`; [`Document::parse_in`](crate::Document::parse_in) passes
//! `NoPlant`, whose every `has` is `false`, so each check compiles away. The controls
//! (tests.rs) pass a value.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TomlPlant {
    /// The reader as it is.
    None,
    /// A section's body end is kept one byte short, before its last newline: an edit that
    /// appends to the section then lands inside its last line.
    BodyEndShort,
}

impl Plants for TomlPlant {}
