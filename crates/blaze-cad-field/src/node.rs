//! The graph's node forms, after canonicalization (see `graph.rs`): every parameter an integer
//! (nanometres, micro-arcseconds, counts, millionths or 2^-40 grid units), so a digest is a
//! function of integers only and one value has one key.

use crate::placement::{Axis, Placement};

/// A node's index in its graph.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId(pub(crate) u32);

impl NodeId {
    /// The index, for tables a caller keeps beside the graph.
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// A profile's index in its graph.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ProfileId(pub(crate) u32);

/// A 32-byte BLAKE3 digest.
pub type Digest = [u8; 32];

/// A primitive's shape in its own frame (all lengths in nanometres).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Shape {
    /// `z <= 0`.
    HalfSpace,
    Sphere { r: i64 },
    /// Half-extents and an edge radius (`round <= min(half)`).
    Box { half: [i64; 3], round: i64 },
    /// Along `z`, centred.
    Cylinder { r: i64, half_h: i64 },
    /// The segment from `z = -half_h` to `z = half_h`, thickened by `r`.
    Capsule { r: i64, half_h: i64 },
    /// Radius `r0` at `z = -half_h`, `r1` at `z = half_h`.
    Cone { r0: i64, r1: i64, half_h: i64 },
    /// In the `xy` plane: major radius `big`, tube radius `small`.
    Torus { big: i64, small: i64 },
    Ellipsoid { radii: [i64; 3] },
    /// `sum |x_i / r_i|^e = 1`, exponent in millionths (`>= 1`).
    Superellipsoid { radii: [i64; 3], e_ppm: i64 },
    /// Triply periodic minimal surface (TPMS) gyroid sheet: period and wall thickness (nanometres).
    Gyroid { period: i64, thickness: i64 },
}

impl Shape {
    /// The digest tag and the parameters, in order.
    pub(crate) fn key(&self) -> (u8, [i64; 4]) {
        match *self {
            Shape::HalfSpace => (1, [0; 4]),
            Shape::Sphere { r } => (2, [r, 0, 0, 0]),
            Shape::Box { half, round } => (3, [half[0], half[1], half[2], round]),
            Shape::Cylinder { r, half_h } => (4, [r, half_h, 0, 0]),
            Shape::Capsule { r, half_h } => (5, [r, half_h, 0, 0]),
            Shape::Cone { r0, r1, half_h } => (6, [r0, r1, half_h, 0]),
            Shape::Torus { big, small } => (7, [big, small, 0, 0]),
            Shape::Ellipsoid { radii } => (8, [radii[0], radii[1], radii[2], 0]),
            Shape::Superellipsoid { radii, e_ppm } => (9, [radii[0], radii[1], radii[2], e_ppm]),
            Shape::Gyroid { period, thickness } => (10, [period, thickness, 0, 0]),
        }
    }

    /// How many of [`Shape::key`]'s parameters are used.
    pub(crate) fn arity(&self) -> usize {
        match self {
            Shape::HalfSpace => 0,
            Shape::Sphere { .. } => 1,
            Shape::Cylinder { .. } | Shape::Capsule { .. } | Shape::Torus { .. } | Shape::Gyroid { .. } => 2,
            Shape::Cone { .. } | Shape::Ellipsoid { .. } => 3,
            Shape::Box { .. } | Shape::Superellipsoid { .. } => 4,
        }
    }
}

/// Which boolean.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Boolean {
    Union,
    Intersect,
    Difference,
}

impl Boolean {
    pub(crate) const fn commutes(self) -> bool {
        !matches!(self, Boolean::Difference)
    }
}

/// How a boolean's edge is formed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Edge {
    /// The plain min or max.
    Sharp,
    /// A quadratic blend of radius `r` (nanometres).
    Round { r: i64 },
    /// A 45-degree flat of size `r` (nanometres).
    Chamfer { r: i64 },
}

/// Blend continuity between consecutive stations of a guided loft.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LoftContinuity {
    /// Piecewise-linear profile blend (`w = t`).
    Linear,
    /// C1 Hermite smoothstep profile blend (`w = 3 t^2 - 2 t^3`).
    SmoothC1,
    /// C2 quintic smootherstep profile blend (`w = 10 t^3 - 15 t^4 + 6 t^5`).
    CurvatureC2,
}

impl LoftContinuity {
    pub(crate) const fn tag(self) -> u8 {
        match self {
            LoftContinuity::Linear => 0,
            LoftContinuity::SmoothC1 => 1,
            LoftContinuity::CurvatureC2 => 2,
        }
    }

    /// Maximum derivative `max_{t in [0, 1]} |dw/dt|` of the blend polynomial.
    pub(crate) const fn max_slope(self) -> f64 {
        match self {
            LoftContinuity::Linear => 1.0,
            LoftContinuity::SmoothC1 => 1.5,
            LoftContinuity::CurvatureC2 => 1.875,
        }
    }
}

/// The level of detail a compile includes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Detail {
    /// Detail nodes are left out (their operand stands): the shape a check or a coarse mesh
    /// needs, keyed by the nodes' coarse digests.
    Shape,
    /// Everything.
    Full,
}

/// A node, canonical (see `graph.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Node {
    /// No points: the identity of union.
    Empty,
    /// A shape in a rigid frame: where every rigid map ends up.
    Primitive { shape: Shape, frame: Placement },
    /// A closed-form profile extruded along `z` by `half_h` each side.
    Extrude { profile: ProfileId, half_h: i64 },
    /// A profile in the `(r, z)` half-plane, its `x` taken as distance from the `z` axis minus
    /// `offset` (a sweep along an arc of radius `offset`), revolved a whole turn.
    Revolve { profile: ProfileId, offset: i64 },
    /// Stations `[start, start + len)` of the graph's station table; `lipschitz_ppm` is the
    /// stated Lipschitz factor in millionths, rounded up.
    Loft { start: u32, len: u32, lipschitz_ppm: i64 },
    /// A rail-guided loft (`idx` into the graph's guided-loft table).
    GuidedLoft { idx: u32 },
    /// `inner` in the frame `placement`, where it cannot be folded into leaves.
    Placed { inner: NodeId, placement: Placement },
    /// `factor * inner(p / factor)`, the factor in units of 10^-12.
    Scale { inner: NodeId, factor_pp12: i64 },
    /// `inner(A^-1 (p - t)) / L`: the linear part on the 2^-40 grid, the shift in nanometres.
    Affine { inner: NodeId, key: [i64; 12], lipschitz_ppm: i64 },
    /// `inner` with `p[axis]` replaced by `|p[axis]|`: `inner` and its mirror image (the
    /// operand lies on the positive side, checked).
    MirrorFold { inner: NodeId, axis: Axis },
    /// `count` copies of `inner` along `axis`, `step` apart (nanometres); `extent` is the
    /// operand's half-width along the axis, checked below half a step.
    Repeat { inner: NodeId, axis: Axis, step: i64, count: u32, extent: i64 },
    Boolean { op: Boolean, edge: Edge, a: NodeId, b: NodeId },
    /// A selective, spatially varying rounded or chamfered boolean (`field` indexes the graph's
    /// variable-radius field table).
    VariableBoolean { op: Boolean, chamfer: bool, a: NodeId, b: NodeId, field: u32 },
    /// `inner - r`.
    Grow { inner: NodeId, r: i64 },
    /// `inner + r`.
    Erode { inner: NodeId, r: i64 },
    /// `(inner +/- r(p)) / (1 + L_r)`: spatially varying selective grow (`!erode`) or erode
    /// (`erode`), with `field` indexing the graph's variable-radius field table.
    VariableOffset { inner: NodeId, erode: bool, field: u32 },
    /// `max(inner, -inner - t)`: a wall of thickness `t` inside the surface.
    Shell { inner: NodeId, t: i64 },
    /// The `xy` plane turned by `rate * z` (micro-arcseconds per metre).
    Twist { inner: NodeId, rate: i64, lipschitz_ppm: i64 },
    /// Bent about the axis parallel to `y` at height `radius` (nanometres).
    Bend { inner: NodeId, radius: i64, lipschitz_ppm: i64 },
    /// `xy` scaled from 1 at `z0` to `top_ppm` millionths at `z1` (nanometres), linearly, `z`
    /// clamped to `[z0, z1]`.
    Taper { inner: NodeId, top_ppm: i64, z0: i64, z1: i64, lipschitz_ppm: i64 },
    /// A periodic relief of amplitude `amp` and period `period` (nanometres), only at
    /// [`Detail::Full`].
    Detail { inner: NodeId, amp: i64, period: i64 },
    /// The exact distance of `inner`'s solid: a grid the mesher solves and attaches.
    Redistance { inner: NodeId },
}

impl Node {
    /// The operands, for a walk.
    pub(crate) fn children(&self) -> ([NodeId; 2], usize) {
        let z = NodeId(0);
        match *self {
            Node::Empty
            | Node::Primitive { .. }
            | Node::Extrude { .. }
            | Node::Revolve { .. }
            | Node::Loft { .. }
            | Node::GuidedLoft { .. } => ([z, z], 0),
            Node::Boolean { a, b, .. } | Node::VariableBoolean { a, b, .. } => ([a, b], 2),
            Node::Placed { inner, .. }
            | Node::Scale { inner, .. }
            | Node::Affine { inner, .. }
            | Node::MirrorFold { inner, .. }
            | Node::Repeat { inner, .. }
            | Node::Grow { inner, .. }
            | Node::Erode { inner, .. }
            | Node::VariableOffset { inner, .. }
            | Node::Shell { inner, .. }
            | Node::Twist { inner, .. }
            | Node::Bend { inner, .. }
            | Node::Taper { inner, .. }
            | Node::Detail { inner, .. }
            | Node::Redistance { inner } => ([inner, z], 1),
        }
    }
}
