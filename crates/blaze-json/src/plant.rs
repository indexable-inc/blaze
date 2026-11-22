//! The bugs a control can plant in the writer (D165, rule 78: a strategy is a type parameter at
//! one seam). [`write_into_planted`](crate::write_into_planted) takes one as
//! `plant: impl Planted<JsonPlant>`; shipped code passes `NoPlant`, whose every `has` is
//! `false`, so the check compiles away. The round trip's control (tests.rs) passes a value.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants in the writer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JsonPlant {
    /// The code as it is.
    None,
    /// A `"` inside a string is written bare.
    QuoteUnescaped,
}

impl Plants for JsonPlant {}
