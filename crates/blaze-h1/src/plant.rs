//! The bugs a control can plant in the response parser (D165, rule 78: a strategy is a type
//! parameter at one seam). [`Response::feed_planted`](crate::Response::feed_planted) takes one as
//! `plant: impl Planted<H1Plant>`; shipped code passes `NoPlant` (through `Response::feed`), whose
//! every `has` is `false`, so the check compiles away. The split oracle's control (tests.rs)
//! passes a value.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants in the response parser.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum H1Plant {
    /// The code as it is.
    None,
    /// A chunk-size line split between two feeds loses the digits of the first.
    SizeLineRestarts,
}

impl Plants for H1Plant {}
