//! The planted bugs (D165): each a typed variant a shipped function is generic over, `None` in
//! every public entry point, and each one an invariant oracle in the tests must catch.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// Simplification's planted bugs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecimatePlant {
    None,
    /// The feature rule skipped: a collapse may move a crease corner or cross a crease, border or
    /// boundary.
    CrossCrease,
    /// Ties between equal costs broken by the order candidates were pushed (the input's triangle
    /// order) instead of by the edge's ids.
    TieByInsertion,
    /// Every collapse maps region onto fan barycentrically, never vertically (simplify's ERROR
    /// BOUND): sliding a vertex within a flat face then costs its whole slide, and a flat face
    /// cannot coarsen beyond the tolerance's ball.
    MovedOnly,
    /// Every map looks through every live triangle's pieces for the region's, not through the
    /// region's slots (simplify's COST): the corners read grow as the square of the input.
    MeasuresEverything,
}

impl Plants for DecimatePlant {}

/// Clustering's planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClusterPlant {
    None,
    /// The vertex cap checked one too loose: a cluster may get one vertex past it.
    OverCap,
}

impl Plants for ClusterPlant {}

/// Quickhull's planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HullPlant {
    None,
    /// The first point found outside the starting simplex is left out of every outside set.
    SkipVertex,
}

impl Plants for HullPlant {}

/// The mesher's planted bugs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContourPlant {
    None,
    /// A piece's [`RegionKey`](crate::RegionKey) is made without its leaf size (the tolerance's
    /// grid): a piece meshed at one tolerance is reused at another.
    KeyWithoutTolerance,
    /// The residual test is gone: every cell the surface may cross splits down to the leaf (the
    /// uniform walk, `A / leaf²` cells for a surface of area `A`).
    NoResidualTest,
}

impl Plants for ContourPlant {}

/// The redistance solver's planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RedistancePlant {
    None,
    /// Every grid value is the unsigned distance: the inside reads as outside.
    Unsigned,
}

impl Plants for RedistancePlant {}

/// Quantization's planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QuantPlant {
    None,
    /// Positions rounded toward zero instead of to nearest.
    TowardZero,
}

impl Plants for QuantPlant {}

/// UV unwrapping and tangent generation's planted bugs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UvPlant {
    None,
    /// Places all charts at `(0, 0)` without packing, caught by the non-overlap oracle.
    OverlapCharts,
    /// Outputs raw un-orthogonalized tangent and wrong handedness, caught by the tangent-frame
    /// oracle.
    SkipTangentOrtho,
}

impl Plants for UvPlant {}
