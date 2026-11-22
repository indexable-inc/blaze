//! The bugs a control can plant in the encoder (D165, rule 78: a strategy is a type parameter at
//! one seam). [`Engine::encode_into_planted`](crate::Engine::encode_into_planted) takes one as
//! `plant: impl Planted<Base64Plant>`; shipped code passes `NoPlant`, whose every `has` is
//! `false`, so the check compiles away. The round trip's control (tests.rs) passes a value.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants in the encoder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Base64Plant {
    /// The code as it is.
    None,
    /// A last quantum of two bytes sets a bit past the second byte in its third symbol.
    TailSetsBit,
}

impl Plants for Base64Plant {}
