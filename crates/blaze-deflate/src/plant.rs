//! The bugs a control can plant in the codecs (D165, rule 78: a strategy is a type parameter at
//! one seam). Each machine that holds a plant check is generic over `P: Planted<DeflatePlant>`
//! ([`PlantedInflate`](crate::PlantedInflate), [`PlantedInflateInto`](crate::PlantedInflateInto),
//! [`PlantedDeflate`](crate::PlantedDeflate), [`PlantedGzipDecoder`](crate::PlantedGzipDecoder));
//! the names shipped code uses ([`Inflate`](crate::Inflate) and the rest) are those machines with
//! `NoPlant`, whose every `has` is `false`, so each check compiles away. The tests' controls
//! (tests.rs) pass a value.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeflatePlant {
    /// The code as it is.
    None,
    /// The inflater never holds its output to the ratio: a bomb gets through.
    RatioUnchecked,
    /// The deflater sends every match one byte farther back than it found it.
    DistanceOffByOne,
    /// The gzip reader takes any CRC-32 in a trailer.
    GzipCrcUnchecked,
    /// The fast loop copies every match from one byte farther back than its distance says.
    FastDistanceOffByOne,
}

impl Plants for DeflatePlant {}
