//! The planted bugs (D165): each a typed variant a shipped function is generic over, `None` in
//! every public entry point, and each one an oracle in the tests must catch.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// The `.glb` writer's planted bugs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GlbPlant {
    None,
    /// The first buffer view starts one byte past its 4-byte boundary.
    MisalignView,
    /// The first accessor claims one element more than its view holds.
    AccessorPastView,
    /// The first `POSITION` accessor's `max` on x is written as its `min`.
    WrongMinMax,
}

impl Plants for GlbPlant {}

/// The PNG encoder's planted bugs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PngPlant {
    None,
    /// Each chunk's CRC taken over its data alone, without its type.
    CrcWithoutType,
    /// The Paeth predictor's tie broken toward the upper-left byte before the upper one.
    PaethTie,
}

impl Plants for PngPlant {}

/// The summary's planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SummaryPlant {
    None,
    /// Parts without a mesh (groups) left out.
    SkipGroups,
}

impl Plants for SummaryPlant {}
