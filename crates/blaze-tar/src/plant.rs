//! The bugs a control can plant in the reader and the writer (D165, rule 78: a strategy is a
//! type parameter at one seam). [`PlantedReader`](crate::PlantedReader) and
//! [`PlantedWriter`](crate::PlantedWriter) are generic over `P: Planted<TarPlant>`; the names
//! shipped code uses ([`Reader`](crate::Reader), [`Writer`](crate::Writer)) are those with
//! `NoPlant`, whose every `has` is `false`, so each check compiles away. The tests' controls
//! (tests.rs) pass a value.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TarPlant {
    /// The code as it is.
    None,
    /// A `..` component is taken as a name.
    ParentUnchecked,
    /// A path through an earlier symbolic link is taken.
    ThroughLinkUnchecked,
    /// The writer's size field is one less than the entry's size.
    SizeShort,
}

impl Plants for TarPlant {}
