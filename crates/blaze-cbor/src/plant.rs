//! The bugs a control can plant in the writer (D165, rule 78: a strategy is a type parameter at
//! one seam). [`write_into_planted`](crate::write_into_planted) takes one as
//! `plant: impl Planted<CborPlant>`; shipped code passes `NoPlant`, whose every `has` is
//! `false`, so the check compiles away. The round trip's control (tests.rs) passes a value.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants in the writer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CborPlant {
    /// The code as it is.
    None,
    /// Map keys sorted by their bytes alone, not their encodings' (length first).
    KeysBytewise,
}

impl Plants for CborPlant {}
