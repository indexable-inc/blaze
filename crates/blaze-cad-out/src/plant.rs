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

/// The binary STL writer's planted bugs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StlPlant {
    None,
    /// The 80-byte header starts with `"solid "`, breaking ASCII/binary STL auto-detectors.
    SolidHeader,
    /// The 4-byte triangle count under-reports the number of facets by one.
    WrongTriCount,
    /// The first facet's stored normal is negated (opposing its right-hand winding).
    FlipNormal,
    /// The first facet's vertices 1 and 2 are swapped (breaking outward winding).
    FlipWinding,
    /// The first zero float emitted is written as `-0.0` instead of canonical `+0.0`.
    NegativeZero,
}

impl Plants for StlPlant {}

/// The 2D vector SVG cross-section writer's planted bugs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SvgPlant {
    None,
    /// Saddle cases 5 and 10 invert the cell-centre asymptotic decider.
    WrongSaddle,
    /// Edge crossings use cell midpoints (`t = 0.5`) instead of linear zero-crossing interpolation.
    WrongInterp,
    /// Closed contour subpaths omit the closing `Z` command.
    DropZClose,
    /// Contour Y coordinates are not negated from Y-up into SVG Y-down.
    NoYFlip,
    /// The filled cross-section silhouette omits `fill-rule="evenodd"`.
    OmitEvenOdd,
}

impl Plants for SvgPlant {}
