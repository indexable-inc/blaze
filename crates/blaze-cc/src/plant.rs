//! The bugs a control can plant (D165, rule 78: a strategy is a type parameter at one seam).
//! Shipped code passes `NoPlant`, whose every `has` is `false`; the controls in tests.rs pass
//! these values and their oracles must catch each one.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug planted in the job machine (job.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobPlant {
    /// The code as it is.
    None,
    /// A lookup that finds nothing is answered but left out of the read list.
    ForgetsMisses,
    /// A `..` segment of a guest path is dropped instead of taking the segment before it away.
    DropsDotDot,
}

impl Plants for JobPlant {}

/// A bug planted in the declaration codec (decls.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeclsPlant {
    /// The code as it is.
    None,
    /// A line split between two feeds loses the part fed first.
    LineRestarts,
}

impl Plants for DeclsPlant {}
