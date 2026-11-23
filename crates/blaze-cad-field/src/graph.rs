//! THE GRAPH: an arena of canonical nodes in a handed allocator, hash-consed by BLAKE3 digest.
//!
//! # Digests are the incremental mechanism
//!
//! A node's digest is BLAKE3 (derived-key mode, context `blaze-cad-field 2026-11-20 node`) of
//! its kind, its integer parameters and its operands' digests. Building a node costs one hash of
//! a few dozen bytes; a node already present (same digest) is returned, not added. So changing
//! one leaf parameter makes one new leaf and one new node per ancestor: the count of new
//! digests is the path depth, never the graph size (tested in `tests.rs`).
//!
//! # Canonical form before hashing
//!
//! Every builder returns a canonical node, so one shape has one digest however it was written:
//!
//! - A rigid map is a [`Placement`] on the grid. Placing a primitive folds the placement into
//!   its frame (`Primitive { shape, frame }`: no transform node in the common case); placing a
//!   placed node composes the two; the identity placement is dropped.
//! - PLACEMENT IS HOISTED, NOT PUSHED: a boolean, offset, shell, redistance, detail or scale whose
//!   operands all share one placement is built on the unplaced operands and placed once above
//!   ([`Graph::split_placement`] reads it back). Operands with different placements keep their
//!   own; nothing is rewritten into the leaves, so moving one part re-keys the part's
//!   placement node and its ancestors only, never the part's geometry.
//! - Commutative operands are ordered by digest.
//! - Identities go: union with empty, an offset of zero, a scale of one, a repeat of one copy,
//!   `a union a`. Offsets compose (`grow(grow(a, r), s) = grow(a, r + s)`), scales multiply,
//!   placements compose.
//! - Parameters are integers before they are hashed: lengths as whole nanometres, angles as
//!   whole micro-arcseconds (blaze-units), counts as integers, ratios as millionths (or 10^-12
//!   for a scale), a placement's linear part on the 2^-40 grid. The f64 an interpreter uses is
//!   one fixed conversion of that integer.
//!
//! # Uniform scale
//!
//! A uniform scale keeps distances up to its factor, so it is not a placement and is not hoisted
//! as one: it is a node of its own, `Scale { inner, factor }`, with the factor keyed in units of
//! 10^-12. A placement under a scale is hoisted above it with its shift multiplied by the factor
//! (rounded to the nanometre: the only rounding a hoist makes); two scales multiply.
//!
//! # Bounds before intervals
//!
//! Every node carries a conservative box in its own frame, built from its operands' (exact for
//! primitives, union for a union, grown for offsets and blends, the image box for a map). A
//! deformation's field is guarded by its box (`BoxBound`, see `lower.rs`: outside the grown
//! box the box's own distance stands), and the inspections start from the root's box. A
//! union operand is NOT guarded by its box: for a field at least a bound, `max(f, box bound)`
//! is `f` wherever `f` is exact and no tighter elsewhere, so the guard would add instructions
//! and no pruning; a far part leaves a cell's tape by its own interval (tested in `tests.rs`).
//!
//! Names are kept in a side table, never hashed: a node rebuilt with the same shape is the same
//! node, and keeps its first name.

use core::marker::PhantomData;

use blaze_blake3::Hasher;
use blaze_hash::{FixedState, Map, MapOpts};
use blaze_math::{Aabb3, Affine3, Mat3, Vec3};
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{AVec, Alloc, Vec};
use blaze_units::{Angle, Length, Plain};

use crate::Limits;
use crate::error::{CadError, Name, NodeKind, Value};
use crate::level::{AtLeastInside, AtLeastOutside, Bound, Ex, Exact, Inside, Join, Level, Lipschitz, Outside};
use crate::node::{Boolean, Detail, Digest, Edge, LoftContinuity, Node, NodeId, ProfileId, Shape};
use crate::placement::{ANGLE_UNIT, Axis, Placement};
use crate::plant::CadPlant;
use crate::profile::{self, Closed, Point2, Profile, SEG_WIDTH};

/// A solid whose field is at level `K` (see `level.rs`): a node id with its exactness in its
/// type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Solid<K: Ex> {
    id: NodeId,
    level: PhantomData<K>,
}

impl<K: Ex> Solid<K> {
    fn new(id: NodeId) -> Solid<K> {
        Solid { id, level: PhantomData }
    }

    /// The node.
    pub const fn id(self) -> NodeId {
        self.id
    }

    /// The same solid at a weaker level.
    pub fn weaken<J: Ex>(self) -> Solid<J>
    where
        K: crate::level::Weakens<J>,
    {
        Solid::new(self.id)
    }
}

/// A profile whose field is at level `K`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Prof<K: Ex> {
    id: ProfileId,
    level: PhantomData<K>,
}

impl<K: Ex> Prof<K> {
    /// The profile.
    pub const fn id(self) -> ProfileId {
        self.id
    }
}

/// What the graph keeps per node.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rec {
    pub node: Node,
    pub full: Digest,
    pub coarse: Digest,
    pub level: Level,
    pub aabb: Aabb3,
}

/// What the graph keeps per profile.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProfileRec {
    pub digest: Digest,
    pub level: Level,
    /// `[lo_x, lo_y, hi_x, hi_y]`, metres.
    pub bounds: [f64; 4],
    pub closed: Option<Closed>,
    /// Rows `[start, start + len)` of the segment table (`SEG_WIDTH` values a row).
    pub segs: (u32, u32),
}

/// A solved distance grid for a redistance node (the mesher makes it): values at the points
/// `origin + step * (i, j, k)`, `i` fastest, in metres.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GridRec {
    pub origin: [f64; 3],
    pub step: f64,
    pub dims: [u32; 3],
    /// The band the grid covers beyond the solid: outside the grid's box the field is the box's
    /// bound plus this.
    pub band: f64,
    pub start: u32,
}

/// One quantized cubic spline span on `[z0_nm, z1_nm]` (coefficients in units of 10^-12).
/// Evaluated for `u = (z - z0) / (z1 - z0) in [0, 1]` as `c0 + u * (c1 + u * (c2 + u * c3))`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct SplineSpan {
    pub z0_nm: i64,
    pub z1_nm: i64,
    pub c_pp12: [i64; 4],
}

/// What the graph keeps per [`Node::GuidedLoft`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct GuidedLoftRec {
    pub stations_start: u32,
    pub stations_len: u32,
    /// `(start, len)` in `Graph::spline_spans` for `[sx, sy, cx, cy, twist]`.
    /// `len == 0` denotes the constant identity (`1.0` for `sx`/`sy`, `0.0` for `cx`/`cy`/`twist`).
    pub splines: [(u32, u32); 5],
    /// Minimum of `sx(z)` and `sy(z)` over the loft z-range in units of `10^-12` (strictly > 0).
    pub s_min_pp12: [i64; 2],
    pub continuity: LoftContinuity,
    pub cap_round: i64,
    pub lipschitz_ppm: i64,
    /// Whether any spline varies with `z` (requiring the grown-AABB box guard).
    pub guarded: bool,
}

/// Canonical quantized [`RadiusProfile`] stored in `Graph::var_fields`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum CanonProfile {
    Constant { r_nm: i64 },
    Linear { from_nm: [i64; 3], to_nm: [i64; 3], r0_nm: i64, r1_nm: i64 },
    Curve { origin_nm: [i64; 3], axis_pp12: [i64; 3], start: u32, len: u32 },
}

/// Canonical quantized [`EdgeSelect`] stored in `Graph::var_fields`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum CanonSelect {
    All,
    Box { lo_nm: [i64; 3], hi_nm: [i64; 3], fade_nm: i64 },
    Ball { center_nm: [i64; 3], radius_nm: i64, fade_nm: i64 },
    Capsule { a_nm: [i64; 3], b_nm: [i64; 3], radius_nm: i64, fade_nm: i64 },
}

/// What the graph keeps per variable-radius field (`Node::VariableBoolean`, `Node::VariableOffset`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct VarFieldRec {
    pub profile: CanonProfile,
    pub select: CanonSelect,
    pub r_max_nm: i64,
    pub l_r_ppm: i64,
}

/// The graph's construction arguments (rule 83).
#[blaze::args]
#[derive(Clone, Copy)]
pub struct GraphOpts<'a> {
    /// Where the graph's tables live.
    pub alloc: Alloc<'a>,
    /// Its bounds.
    pub limits: Limits = Limits::SHIPPED,
}

macro_rules! opts {
    ($($(#[$m:meta])* $name:ident { $($(#[$fm:meta])* $f:ident : $t:ty $(= $d:expr)?),* $(,)? })*) => {$(
        $(#[$m])*
        #[blaze::args]
        #[derive(Clone, Copy, Debug)]
        pub struct $name<'n> {
            $($(#[$fm])* pub $f: $t $(= $d)?,)*
            /// The author's name for the node, printed in errors and kept beside the graph
            /// (never hashed).
            pub name: Option<&'n str> = None,
        }
    )*};
}

opts! {
    /// A half-space `z <= 0` in its frame.
    HalfSpaceOpts {}
    /// A sphere about the origin.
    SphereOpts { radius: Length }
    /// A box about the origin with optionally rounded edges.
    BoxOpts { size: [Length; 3], round: Length = Length::ZERO }
    /// A cylinder along `z`, centred.
    CylinderOpts { radius: Length, height: Length }
    /// A capsule along `z`: `length` between the centres of its end caps.
    CapsuleOpts { radius: Length, length: Length }
    /// A cone or frustum along `z`: `bottom` radius at `-height/2`, `top` at `+height/2`.
    ConeOpts { bottom: Length, top: Length, height: Length }
    /// A torus in the `xy` plane.
    TorusOpts { major: Length, minor: Length }
    /// An ellipsoid with these semi-axes.
    EllipsoidOpts { radii: [Length; 3] }
    /// `sum |x_i / r_i|^e = 1` (`exponent >= 1`).
    SuperellipsoidOpts { radii: [Length; 3], exponent: f64 }
    /// A triply periodic minimal surface (TPMS) gyroid sheet of spatial `period` and nominal wall `thickness`.
    GyroidOpts { period: Length, thickness: Length }
    /// A profile: every form but the superellipse (which is a bound).
    ProfileOpts { profile: Profile<'n> }
    /// Extrude along `z`, centred.
    ExtrudeOpts { height: Length }
    /// Revolve about `z` a whole turn; the profile's `x` is the distance from the axis.
    RevolveOpts {}
    /// Revolve about `z` through `angle` from the `+x` axis counterclockwise.
    RevolvePartOpts { angle: Angle }
    /// Sweep along an arc of radius `radius` about `z`, from `+x` through `angle`.
    SweepArcOpts { radius: Length, angle: Angle }
    /// Sweep along the polyline through `points` (mitred joints of at most a quarter turn).
    SweepPolylineOpts { points: &'n [[Length; 3]] }
    /// Uniform scale about the origin.
    ScaleOpts { factor: f64 }
    /// A general affine map `p -> linear p + shift` (a bound).
    AffineOpts { linear: Mat3, shift: [Length; 3] }
    /// A copy mirrored across the plane through the origin normal to `axis`.
    MirrorOpts { axis: Axis }
    /// `count` copies along `axis`, `step` apart, the first where the operand is.
    RepeatOpts { axis: Axis, step: Length, count: u32 }
    /// `count` copies turned about `z`, evenly.
    CircularOpts { count: u32 }
    /// A rounded (`round`) or chamfered (`chamfer`) boolean of size `radius`.
    BlendOpts { radius: Length }
    /// Grow or erode by `by`.
    OffsetOpts { by: Length }
    /// A wall of `thickness` inside the surface.
    ShellOpts { thickness: Length }
    /// Round every edge by `radius` (erode, redistance, grow).
    RoundOpts { radius: Length }
    /// Turn the `xy` plane by `per_metre` for each metre of `z`.
    TwistOpts { per_metre: Angle }
    /// Bend about the axis parallel to `y` at height `radius`.
    BendOpts { radius: Length }
    /// Scale `xy` from 1 at the operand's bottom to `top_scale` at its top.
    TaperOpts { top_scale: f64 }
    /// A relief of amplitude `amplitude` and period `period`, present only at `Detail::Full`.
    DetailOpts { amplitude: Length, period: Length }
    /// A loft through stations (each an exact profile at a height, heights increasing).
    LoftOpts { stations: &'n [Station] }
    /// A solved grid for a redistance node: values at `origin + step (i, j, k)`, `i` fastest,
    /// covering the solid grown by `band`.
    GridOpts { origin: [Length; 3], step: Length, dims: [u32; 3], values: &'n [f64], band: Length }
}

/// One loft station.
#[derive(Clone, Copy, Debug)]
pub struct Station {
    pub z: Length,
    pub profile: Prof<Exact>,
}

/// One knot of a [`GuideRail`]: height `z`, target `value`, and optional dimensionless tangent
/// `d(value)/dz` (`None` solves the C2 natural cubic spline condition at this knot).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RailPoint {
    pub z: Length,
    pub value: Length,
    pub tangent: Option<f64> = None,
}

/// A smooth 1-D guide rail along `z` for [`Graph::guided_loft`]: C2 natural cubic spline where
/// tangents are `None`, clamped cubic Hermite where tangents are `Some`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct GuideRail<'s> {
    pub points: &'s [RailPoint],
}

/// One station of a [`Graph::guided_loft`]: exact 2-D profile at height `z` with optional center
/// offset, `(sx, sy)` scale, and twist angle (interpolated by C2 natural cubic spline when the
/// corresponding guide rail is omitted).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct GuidedStation {
    pub z: Length,
    pub profile: Prof<Exact>,
    pub center: [Length; 2] = [Length::ZERO; 2],
    pub scale: [f64; 2] = [1.0, 1.0],
    pub twist: Angle = Angle::ZERO,
}

/// Options for [`Graph::guided_loft`] (rule 83).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct GuidedLoftOpts<'s> {
    pub stations: &'s [GuidedStation],
    pub side_rail: Option<GuideRail<'s>> = None,
    pub ridge_rail: Option<GuideRail<'s>> = None,
    pub center_x_rail: Option<GuideRail<'s>> = None,
    pub center_y_rail: Option<GuideRail<'s>> = None,
    pub continuity: LoftContinuity = LoftContinuity::CurvatureC2,
    pub cap_round: Length = Length::ZERO,
    pub name: Option<&'s str> = None,
}

/// One knot of a [`RadiusProfile::Curve`]: parameter `at` along the axis and target `radius`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RadiusStop {
    pub at: Length,
    pub radius: Length,
}

/// Spatially varying radius profile for variable blends and variable offsets.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RadiusProfile<'s> {
    /// Constant radius everywhere.
    Constant(Length),
    /// Linearly ramps from `r0` at `from` to `r1` at `to`, clamped outside `[0, 1]`.
    Linear { from: [Length; 3], to: [Length; 3], r0: Length, r1: Length },
    /// Smooth C1 cubic Hermite interpolation between `stops` along `(p - origin) . unit(axis)`.
    Curve { origin: [Length; 3], axis: [f64; 3], stops: &'s [RadiusStop] },
}

/// Spatial selection region for selective blends and offsets.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum EdgeSelect {
    /// Full strength everywhere (`mask = 1`).
    All,
    /// Full strength inside `[lo, hi]`, smoothstep-faded to zero across `fade` outside.
    Box { lo: [Length; 3], hi: [Length; 3], fade: Length },
    /// Full strength inside the sphere `(center, radius)`, smoothstep-faded across `fade` outside.
    Ball { center: [Length; 3], radius: Length, fade: Length },
    /// Full strength inside the capsule `(a, b, radius)`, smoothstep-faded across `fade` outside.
    Capsule { a: [Length; 3], b: [Length; 3], radius: Length, fade: Length },
}

/// Options for selective, variable-radius rounded and chamfered booleans (rule 83).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct VariableBlendOpts<'s> {
    pub profile: RadiusProfile<'s>,
    pub select: EdgeSelect = EdgeSelect::All,
    pub name: Option<&'s str> = None,
}

/// Options for selective, variable-radius grow and erode (rule 83).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct VariableOffsetOpts<'s> {
    pub profile: RadiusProfile<'s>,
    pub select: EdgeSelect = EdgeSelect::All,
    pub name: Option<&'s str> = None,
}

/// One oriented station along a 3-D surface curve for [`Graph::sweep_frames`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SweepFrame {
    pub point: [Length; 3],
    pub tangent: [f64; 3],
    pub normal: [f64; 3],
    pub width: Length,
    pub depth: Length,
}

/// Cross-section shape for [`Graph::sweep_frames`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SweepCrossSection {
    /// Circular (or stadium when `width != depth`).
    Circular,
    /// D-shaped half-round arch of `width` and `depth`.
    HalfRound,
    /// Smoothly filleted groove channel of `width` and `depth`.
    Channel,
    /// V-shaped triangular groove of `width` and `depth`.
    VGroove,
    /// Rectangular section of `width` and `depth` with corner radius `round`.
    Rectangular { round: Length },
}

/// Options for [`Graph::sweep_frames`] (rule 83).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct SweepFramesOpts<'s> {
    pub frames: &'s [SweepFrame],
    pub section: SweepCrossSection = SweepCrossSection::Channel,
    pub closed: bool = false,
    pub smooth_join: Length = Length::ZERO,
    pub name: Option<&'s str> = None,
}

/// The fold, in nanometres, of a length to metres: the one fixed conversion.
pub(crate) fn m(nm: i64) -> f64 {
    nm as f64 / 1e9
}

fn finite(b: &Aabb3) -> bool {
    let a = b.lo.to_array();
    let c = b.hi.to_array();
    a.iter().chain(c.iter()).all(|v| v.is_finite())
}

/// `p` applied to `b`, conservatively: the whole space for an unbounded box under a placement
/// that is not the identity.
pub(crate) fn place_box(p: &Placement, b: &Aabb3) -> Aabb3 {
    if p.is_identity() || b.is_empty() {
        *b
    } else if finite(b) {
        p.apply_box(b)
    } else {
        Aabb3::ALL
    }
}

/// The box of a shape in its own frame.
fn shape_box(s: &Shape) -> Aabb3 {
    let cube = |x: f64, y: f64, z: f64| Aabb3::new(Vec3::new(-x, -y, -z), Vec3::new(x, y, z));
    match *s {
        Shape::HalfSpace => Aabb3::new(Vec3::splat(f64::NEG_INFINITY), Vec3::new(f64::INFINITY, f64::INFINITY, 0.0)),
        Shape::Sphere { r } => cube(m(r), m(r), m(r)),
        Shape::Box { half, .. } => cube(m(half[0]), m(half[1]), m(half[2])),
        Shape::Cylinder { r, half_h } => cube(m(r), m(r), m(half_h)),
        Shape::Capsule { r, half_h } => cube(m(r), m(r), m(half_h + r)),
        Shape::Cone { r0, r1, half_h } => cube(m(r0.max(r1)), m(r0.max(r1)), m(half_h)),
        Shape::Torus { big, small } => cube(m(big + small), m(big + small), m(small)),
        Shape::Ellipsoid { radii } | Shape::Superellipsoid { radii, .. } => cube(m(radii[0]), m(radii[1]), m(radii[2])),
        Shape::Gyroid { .. } => Aabb3::ALL,
    }
}

/// The margin a deformation's container is grown by: a point on the grown container's face
/// is at least this far from the solid.
pub(crate) fn margin_of(b: &Aabb3) -> f64 {
    (b.longest_side() / 64.0).max(1e-6)
}

/// For a twist, bend or taper of an operand with box `inner`: the container `C` of the result
/// (a box in the node's frame the solid lies in) and the stated Lipschitz factor of the map on
/// `C` grown by its margin. `None` when the map is not defined on that box.
pub(crate) fn deform(node: &Node, inner: &Aabb3) -> Option<(Aabb3, f64)> {
    if !finite(inner) || inner.is_empty() {
        return None;
    }
    let (lo, hi) = (inner.lo, inner.hi);
    match *node {
        Node::Twist { rate, .. } => {
            let rho = [lo.x.abs(), hi.x.abs()].iter().fold(0.0f64, |a, x| a.max(*x)).hypot_core([lo.y.abs(), hi.y.abs()].iter().fold(0.0f64, |a, y| a.max(*y)));
            let c = Aabb3::new(Vec3::new(-rho, -rho, lo.z), Vec3::new(rho, rho, hi.z));
            let cg = c.grow(margin_of(&c));
            let omega = Angle::<Plain>::microarcsec(rate).as_radians().abs();
            let a = omega * (cg.hi.x * cg.hi.x + cg.hi.y * cg.hi.y).sqrt_core();
            Some((c, a * 0.5 + (1.0 + a * a * 0.25).sqrt_core()))
        }
        Node::Bend { radius, .. } => {
            let r = m(radius);
            let (rmin, rmax) = (r - hi.z, r - lo.z);
            let (tlo, thi) = (lo.x / r, hi.x / r);
            let quarter = core::f64::consts::FRAC_PI_2;
            if rmin <= 0.0 || tlo <= -quarter || thi >= quarter {
                return None;
            }
            let (s0, c0) = blaze_math::sin_cos(tlo);
            let (s1, c1) = blaze_math::sin_cos(thi);
            let xs = [rmin * s0, rmin * s1, rmax * s0, rmax * s1];
            let xlo = xs.iter().fold(f64::INFINITY, |a, x| a.min(*x));
            let xhi = xs.iter().fold(f64::NEG_INFINITY, |a, x| a.max(*x));
            let cmax = if tlo <= 0.0 && thi >= 0.0 { 1.0 } else { c0.max(c1) };
            let cmin = c0.min(c1);
            let c = Aabb3::new(Vec3::new(xlo, lo.y, r - rmax * cmax), Vec3::new(xhi, hi.y, r - rmin * cmin));
            let cg = c.grow(margin_of(&c));
            // The distance from the bend axis (x = 0, z = r) to the grown container.
            let dx = if cg.lo.x > 0.0 { cg.lo.x } else if cg.hi.x < 0.0 { -cg.hi.x } else { 0.0 };
            let dz = if cg.hi.z < r { r - cg.hi.z } else { 0.0 };
            let near = (dx * dx + dz * dz).sqrt_core();
            if near <= 0.0 {
                return None;
            }
            Some((c, (r / near).max(1.0)))
        }
        Node::Taper { top_ppm, .. } => {
            let top = top_ppm as f64 / 1e6;
            let smin = top.min(1.0);
            if smin <= 0.0 {
                return None;
            }
            let xs = [lo.x, lo.x * top, hi.x, hi.x * top];
            let ys = [lo.y, lo.y * top, hi.y, hi.y * top];
            let mn = |v: &[f64; 4]| v.iter().fold(f64::INFINITY, |a, x| a.min(*x));
            let mx = |v: &[f64; 4]| v.iter().fold(f64::NEG_INFINITY, |a, x| a.max(*x));
            let c = Aabb3::new(Vec3::new(mn(&xs), mn(&ys), lo.z), Vec3::new(mx(&xs), mx(&ys), hi.z));
            let cg = c.grow(margin_of(&c));
            let span = hi.z - lo.z;
            let k = if span > 0.0 { (top - 1.0) / span } else { 0.0 };
            let xx = cg.lo.x.abs().max(cg.hi.x.abs());
            let yy = cg.lo.y.abs().max(cg.hi.y.abs());
            let l = (1.0 + 2.0 / (smin * smin) + (xx * xx + yy * yy) * k * k / (smin * smin * smin * smin)).sqrt_core();
            Some((c, l))
        }
        _ => None,
    }
}

/// `sqrt` and `hypot` by blaze-math (deterministic), as methods for the box arithmetic above.
trait CoreSqrt {
    fn sqrt_core(self) -> f64;
    fn hypot_core(self, o: f64) -> f64;
}

impl CoreSqrt for f64 {
    fn sqrt_core(self) -> f64 {
        blaze_math::sqrt(self)
    }
    fn hypot_core(self, o: f64) -> f64 {
        blaze_math::hypot(self, o)
    }
}

/// A Lipschitz factor in millionths, rounded up (what a node keys and the compiler divides by).
fn ppm_up(l: f64) -> i64 {
    core::f64::math::ceil(l * 1e6) as i64
}

/// A digest writer: little-endian integers and operand digests.
struct H(Hasher);

impl H {
    fn new(context: &str) -> H {
        H(Hasher::new_derive_key(context))
    }
    fn u8(&mut self, v: u8) {
        self.0.update(&[v]);
    }
    fn i64(&mut self, v: i64) {
        self.0.update(&v.to_le_bytes());
    }
    fn d(&mut self, d: &Digest) {
        self.0.update(d);
    }
    fn done(&self) -> Digest {
        self.0.finalize()
    }
}

/// The context of every node digest. A change to what a digest covers changes the date.
const NODE_CONTEXT: &str = "blaze-cad-field 2026-11-20 node";
const PROFILE_CONTEXT: &str = "blaze-cad-field 2026-11-20 profile";

/// The canonical field graph (see the module).
pub struct Graph<'a, P: Planted<CadPlant> = NoPlant> {
    alloc: Alloc<'a>,
    limits: Limits,
    plant: P,
    nodes: AVec<'a, Rec>,
    by_digest: Map<Digest, NodeId, FixedState, Alloc<'a>>,
    profiles: AVec<'a, ProfileRec>,
    profile_by_digest: Map<Digest, ProfileId, FixedState, Alloc<'a>>,
    /// Path rows, `SEG_WIDTH` values each.
    segs: AVec<'a, f64>,
    /// Loft stations: height in nanometres and profile.
    stations: AVec<'a, (i64, ProfileId)>,
    /// Quantized piecewise-cubic spline spans for [`Node::GuidedLoft`].
    spline_spans: AVec<'a, SplineSpan>,
    /// Metadata records for [`Node::GuidedLoft`].
    guided_lofts: AVec<'a, GuidedLoftRec>,
    /// Quantized variable-radius fields for [`Node::VariableBoolean`] and [`Node::VariableOffset`].
    var_fields: AVec<'a, VarFieldRec>,
    /// Quantized `(at_nm, radius_nm)` stops for [`CanonProfile::Curve`].
    radius_stops: AVec<'a, (i64, i64)>,
    names: Map<NodeId, Name, FixedState, Alloc<'a>>,
    grids: Map<NodeId, GridRec, FixedState, Alloc<'a>>,
    grid_values: AVec<'a, f64>,
}

impl<'a> Graph<'a, NoPlant> {
    /// An empty graph.
    pub fn new(opts: GraphOpts<'a>) -> Graph<'a, NoPlant> {
        Graph::planted(opts, NoPlant)
    }
}

impl<'a, P: Planted<CadPlant>> Graph<'a, P> {
    /// An empty graph whose builders consult `plant` (the controls' seam; shipped code passes
    /// [`NoPlant`] through [`Graph::new`]).
    pub fn planted(opts: GraphOpts<'a>, plant: P) -> Graph<'a, P> {
        let alloc = opts.alloc;
        let map = || MapOpts { hasher: FixedState, alloc, capacity: None };
        Graph {
            alloc,
            limits: opts.limits,
            plant,
            nodes: Vec::new_in(alloc),
            by_digest: Map::new(map()),
            profiles: Vec::new_in(alloc),
            profile_by_digest: Map::new(map()),
            segs: Vec::new_in(alloc),
            stations: Vec::new_in(alloc),
            spline_spans: Vec::new_in(alloc),
            guided_lofts: Vec::new_in(alloc),
            var_fields: Vec::new_in(alloc),
            radius_stops: Vec::new_in(alloc),
            names: Map::new(map()),
            grids: Map::new(map()),
            grid_values: Vec::new_in(alloc),
        }
    }

    // ---- reading --------------------------------------------------------------------------

    /// How many nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The limits it was made with.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The plant the graph was made with: the compiler consults the same one.
    pub(crate) fn plant(&self) -> P {
        self.plant
    }

    pub(crate) fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    pub(crate) fn rec(&self, id: NodeId) -> &Rec {
        &self.nodes[id.0 as usize]
    }

    pub(crate) fn has(&self, id: NodeId) -> bool {
        (id.0 as usize) < self.nodes.len()
    }

    /// The node.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.rec(id).node
    }

    /// The digest of the node with every detail.
    pub fn digest(&self, id: NodeId) -> Digest {
        self.rec(id).full
    }

    /// The digest at `detail`: at [`Detail::Shape`] detail nodes are skipped.
    pub fn digest_at(&self, id: NodeId, detail: Detail) -> Digest {
        match detail {
            Detail::Full => self.rec(id).full,
            Detail::Shape => self.rec(id).coarse,
        }
    }

    /// The level the node's field is at.
    pub fn level(&self, id: NodeId) -> Level {
        self.rec(id).level
    }

    /// The node's conservative box, in its own frame.
    pub fn aabb(&self, id: NodeId) -> Aabb3 {
        self.rec(id).aabb
    }

    /// The author's name, if one was given.
    pub fn name(&self, id: NodeId) -> Option<&Name> {
        self.names.get(&id)
    }

    pub(crate) fn profile_rec(&self, id: ProfileId) -> &ProfileRec {
        &self.profiles[id.0 as usize]
    }

    pub(crate) fn seg_rows(&self, p: &ProfileRec) -> &[f64] {
        let (start, len) = (p.segs.0 as usize * SEG_WIDTH, p.segs.1 as usize * SEG_WIDTH);
        &self.segs[start..start + len]
    }

    pub(crate) fn station(&self, i: u32) -> (i64, ProfileId) {
        self.stations[i as usize]
    }

    pub(crate) fn guided_loft_rec(&self, idx: u32) -> &GuidedLoftRec {
        &self.guided_lofts[idx as usize]
    }

    pub(crate) fn spline_slice(&self, slice: (u32, u32)) -> &[SplineSpan] {
        let (start, len) = (slice.0 as usize, slice.1 as usize);
        &self.spline_spans[start..start + len]
    }

    pub(crate) fn var_field_rec(&self, idx: u32) -> &VarFieldRec {
        &self.var_fields[idx as usize]
    }

    pub(crate) fn radius_stops_slice(&self, start: u32, len: u32) -> &[(i64, i64)] {
        let (s, l) = (start as usize, len as usize);
        &self.radius_stops[s..s + l]
    }

    pub(crate) fn grid(&self, id: NodeId) -> Option<(&GridRec, &[f64])> {
        let g = self.grids.get(&id)?;
        let n = g.dims.iter().map(|d| *d as usize).product::<usize>();
        Some((g, &self.grid_values[g.start as usize..g.start as usize + n]))
    }

    /// The node's placement and its unplaced geometry (PLACEMENT IS NOT GEOMETRY): a placed
    /// node gives its placement and operand, a primitive its frame and the same shape at the
    /// origin, any other node the identity and itself. `rotate(part)` changes only the first.
    pub fn split_placement<K: Ex>(&mut self, s: Solid<K>) -> Result<(Placement, Solid<K>), CadError> {
        let (p, id) = self.split(s.id)?;
        Ok((p, Solid::new(id)))
    }

    /// The placement [`Graph::split`] would take off, without interning anything.
    fn placement_of(&self, id: NodeId) -> Placement {
        match self.rec(id).node {
            Node::Placed { placement, .. } => placement,
            Node::Primitive { frame, .. } if !self.plant.has(CadPlant::FrameKeptInGeometry) => frame,
            _ => Placement::IDENTITY,
        }
    }

    fn split(&mut self, id: NodeId) -> Result<(Placement, NodeId), CadError> {
        self.check(id)?;
        match self.rec(id).node {
            Node::Placed { inner, placement } => Ok((placement, inner)),
            Node::Primitive { shape, frame } if !frame.is_identity() && !self.plant.has(CadPlant::FrameKeptInGeometry) => {
                let r = self.rec(id);
                let (level, aabb) = (r.level, shape_box(&shape));
                let inner = self.intern(Node::Primitive { shape, frame: Placement::IDENTITY }, level, aabb, None)?;
                Ok((frame, inner))
            }
            _ => Ok((Placement::IDENTITY, id)),
        }
    }

    fn check(&self, id: NodeId) -> Result<(), CadError> {
        if self.has(id) { Ok(()) } else { Err(CadError::Foreign { id: id.0 }) }
    }

    // ---- interning ------------------------------------------------------------------------

    fn digests(&self, node: &Node) -> (Digest, Digest) {
        let mut f = H::new(NODE_CONTEXT);
        let mut c = H::new(NODE_CONTEXT);
        macro_rules! both {
            ($($e:tt)*) => {{ { let h = &mut f; h.$($e)*; } { let h = &mut c; h.$($e)*; } }};
        }
        let kid = |f: &mut H, c: &mut H, id: NodeId| {
            let r = &self.nodes[id.0 as usize];
            f.d(&r.full);
            c.d(&r.coarse);
        };
        let place = |f: &mut H, c: &mut H, p: &Placement| {
            for v in p.key() {
                f.i64(v);
                c.i64(v);
            }
        };
        match *node {
            Node::Empty => both!(u8(0)),
            Node::Primitive { shape, frame } => {
                let (tag, key) = shape.key();
                both!(u8(1));
                both!(u8(tag));
                let mut n = shape.arity();
                if self.plant.has(CadPlant::DigestDropsParameter) && n > 0 {
                    n -= 1;
                }
                for v in &key[..n] {
                    both!(i64(*v));
                }
                place(&mut f, &mut c, &frame);
            }
            Node::Extrude { profile, half_h } => {
                both!(u8(2));
                both!(d(&self.profile_rec(profile).digest));
                both!(i64(half_h));
            }
            Node::Revolve { profile, offset } => {
                both!(u8(3));
                both!(d(&self.profile_rec(profile).digest));
                both!(i64(offset));
            }
            Node::Loft { start, len, lipschitz_ppm } => {
                both!(u8(4));
                both!(i64(i64::from(len)));
                for i in start..start + len {
                    let (z, p) = self.stations[i as usize];
                    both!(i64(z));
                    both!(d(&self.profile_rec(p).digest));
                }
                both!(i64(lipschitz_ppm));
            }
            Node::Placed { inner, placement } => {
                both!(u8(5));
                place(&mut f, &mut c, &placement);
                kid(&mut f, &mut c, inner);
            }
            Node::Scale { inner, factor_pp12 } => {
                both!(u8(6));
                both!(i64(factor_pp12));
                kid(&mut f, &mut c, inner);
            }
            Node::Affine { inner, key, lipschitz_ppm } => {
                both!(u8(7));
                for v in key {
                    both!(i64(v));
                }
                both!(i64(lipschitz_ppm));
                kid(&mut f, &mut c, inner);
            }
            Node::MirrorFold { inner, axis } => {
                both!(u8(8));
                both!(u8(axis.index() as u8));
                kid(&mut f, &mut c, inner);
            }
            Node::Repeat { inner, axis, step, count, extent } => {
                both!(u8(9));
                both!(u8(axis.index() as u8));
                both!(i64(step));
                both!(i64(i64::from(count)));
                both!(i64(extent));
                kid(&mut f, &mut c, inner);
            }
            Node::Boolean { op, edge, a, b } => {
                both!(u8(10));
                both!(u8(op as u8));
                match edge {
                    Edge::Sharp => both!(u8(0)),
                    Edge::Round { r } => {
                        both!(u8(1));
                        both!(i64(r));
                    }
                    Edge::Chamfer { r } => {
                        both!(u8(2));
                        both!(i64(r));
                    }
                }
                kid(&mut f, &mut c, a);
                kid(&mut f, &mut c, b);
            }
            Node::Grow { inner, r } | Node::Erode { inner, r } | Node::Shell { inner, t: r } => {
                let tag = match node {
                    Node::Grow { .. } => 11,
                    Node::Erode { .. } => 12,
                    _ => 13,
                };
                both!(u8(tag));
                both!(i64(r));
                kid(&mut f, &mut c, inner);
            }
            Node::Twist { inner, rate, lipschitz_ppm } => {
                both!(u8(14));
                both!(i64(rate));
                both!(i64(lipschitz_ppm));
                kid(&mut f, &mut c, inner);
            }
            Node::Bend { inner, radius, lipschitz_ppm } => {
                both!(u8(15));
                both!(i64(radius));
                both!(i64(lipschitz_ppm));
                kid(&mut f, &mut c, inner);
            }
            Node::Taper { inner, top_ppm, z0, z1, lipschitz_ppm } => {
                both!(u8(16));
                both!(i64(top_ppm));
                both!(i64(z0));
                both!(i64(z1));
                both!(i64(lipschitz_ppm));
                kid(&mut f, &mut c, inner);
            }
            Node::Detail { inner, amp, period } => {
                // The coarse digest is the operand's: at `Detail::Shape` the relief is absent.
                f.u8(17);
                f.i64(amp);
                f.i64(period);
                let r = &self.nodes[inner.0 as usize];
                f.d(&r.full);
                return (f.done(), r.coarse);
            }
            Node::Redistance { inner } => {
                both!(u8(18));
                kid(&mut f, &mut c, inner);
            }
            Node::GuidedLoft { idx } => {
                let gl = self.guided_lofts[idx as usize];
                both!(u8(19));
                both!(i64(i64::from(gl.stations_len)));
                for i in gl.stations_start..gl.stations_start + gl.stations_len {
                    let (z, p) = self.stations[i as usize];
                    both!(i64(z));
                    both!(d(&self.profile_rec(p).digest));
                }
                for (start, len) in gl.splines {
                    both!(i64(i64::from(len)));
                    for sp in &self.spline_spans[start as usize..(start + len) as usize] {
                        both!(i64(sp.z0_nm));
                        both!(i64(sp.z1_nm));
                        for coeff in sp.c_pp12 {
                            both!(i64(coeff));
                        }
                    }
                }
                both!(i64(gl.s_min_pp12[0]));
                both!(i64(gl.s_min_pp12[1]));
                both!(u8(gl.continuity.tag()));
                both!(i64(gl.cap_round));
                both!(i64(gl.lipschitz_ppm));
                both!(u8(u8::from(gl.guarded)));
            }
            Node::VariableBoolean { op, chamfer, a, b, field } => {
                let vf = self.var_fields[field as usize];
                both!(u8(20));
                both!(u8(op as u8));
                both!(u8(u8::from(chamfer)));
                self.hash_var_field(&mut f, &mut c, &vf);
                kid(&mut f, &mut c, a);
                kid(&mut f, &mut c, b);
            }
            Node::VariableOffset { inner, erode, field } => {
                let vf = self.var_fields[field as usize];
                both!(u8(21));
                both!(u8(u8::from(erode)));
                self.hash_var_field(&mut f, &mut c, &vf);
                kid(&mut f, &mut c, inner);
            }
        }
        (f.done(), c.done())
    }

    fn hash_var_field(&self, f: &mut H, c: &mut H, vf: &VarFieldRec) {
        macro_rules! both {
            ($($e:tt)*) => {{ { let h = &mut *f; h.$($e)*; } { let h = &mut *c; h.$($e)*; } }};
        }
        match vf.profile {
            CanonProfile::Constant { r_nm } => {
                both!(u8(0));
                both!(i64(r_nm));
            }
            CanonProfile::Linear { from_nm, to_nm, r0_nm, r1_nm } => {
                both!(u8(1));
                for v in from_nm {
                    both!(i64(v));
                }
                for v in to_nm {
                    both!(i64(v));
                }
                both!(i64(r0_nm));
                both!(i64(r1_nm));
            }
            CanonProfile::Curve { origin_nm, axis_pp12, start, len } => {
                both!(u8(2));
                for v in origin_nm {
                    both!(i64(v));
                }
                for v in axis_pp12 {
                    both!(i64(v));
                }
                both!(i64(i64::from(len)));
                for &(at_nm, r_nm) in &self.radius_stops[start as usize..(start + len) as usize] {
                    both!(i64(at_nm));
                    both!(i64(r_nm));
                }
            }
        }
        match vf.select {
            CanonSelect::All => both!(u8(0)),
            CanonSelect::Box { lo_nm, hi_nm, fade_nm } => {
                both!(u8(1));
                for v in lo_nm {
                    both!(i64(v));
                }
                for v in hi_nm {
                    both!(i64(v));
                }
                both!(i64(fade_nm));
            }
            CanonSelect::Ball { center_nm, radius_nm, fade_nm } => {
                both!(u8(2));
                for v in center_nm {
                    both!(i64(v));
                }
                both!(i64(radius_nm));
                both!(i64(fade_nm));
            }
            CanonSelect::Capsule { a_nm, b_nm, radius_nm, fade_nm } => {
                both!(u8(3));
                for v in a_nm {
                    both!(i64(v));
                }
                for v in b_nm {
                    both!(i64(v));
                }
                both!(i64(radius_nm));
                both!(i64(fade_nm));
            }
        }
        both!(i64(vf.r_max_nm));
        both!(i64(vf.l_r_ppm));
    }

    fn intern(&mut self, node: Node, level: Level, aabb: Aabb3, name: Option<&str>) -> Result<NodeId, CadError> {
        let (full, coarse) = self.digests(&node);
        let found = if self.plant.has(CadPlant::NoHashConsing) { None } else { self.by_digest.get(&full) };
        if let Some(id) = found {
            let id = *id;
            if let Some(n) = name
                && self.names.get(&id).is_none()
            {
                self.names.insert(id, Name::new(n));
            }
            return Ok(id);
        }
        if self.nodes.len() as u64 >= u64::from(self.limits.most_nodes) {
            return Err(CadError::Full { what: "node", most: u64::from(self.limits.most_nodes) });
        }
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Rec { node, full, coarse, level, aabb });
        self.by_digest.insert(full, id);
        if let Some(n) = name {
            self.names.insert(id, Name::new(n));
        }
        Ok(id)
    }

    /// `inner` placed by `p`, canonical: folded into a primitive's frame, composed with a
    /// placement, dropped when the identity.
    fn wrap(&mut self, inner: NodeId, p: Placement, name: Option<&str>) -> Result<NodeId, CadError> {
        if p.is_identity() {
            return Ok(inner);
        }
        let r = *self.rec(inner);
        match r.node {
            Node::Empty => Ok(inner),
            Node::Primitive { shape, frame } => {
                let frame = p.then_after(&frame);
                let aabb = place_box(&frame, &shape_box(&shape));
                self.intern(Node::Primitive { shape, frame }, r.level, aabb, name)
            }
            Node::Placed { inner: i, placement } => {
                let q = p.then_after(&placement);
                self.wrap(i, q, name)
            }
            _ => {
                let aabb = place_box(&p, &r.aabb);
                self.intern(Node::Placed { inner, placement: p }, r.level, aabb, name)
            }
        }
    }

    // ---- primitives -----------------------------------------------------------------------

    fn positive(kind: NodeKind, name: Option<&str>, arg: &'static str, l: Length) -> Result<i64, CadError> {
        if l.as_nm() > 0 {
            Ok(l.as_nm())
        } else {
            Err(CadError::Argument { kind, name: name.map(Name::new), arg, value: Value::Length(l), accepted: "a length above zero" })
        }
    }

    fn primitive<K: Ex>(&mut self, shape: Shape, level: Level, name: Option<&str>) -> Result<Solid<K>, CadError> {
        let aabb = shape_box(&shape);
        Ok(Solid::new(self.intern(Node::Primitive { shape, frame: Placement::IDENTITY }, level, aabb, name)?))
    }

    /// The empty solid: the identity of union.
    pub fn empty(&mut self) -> Result<Solid<Exact>, CadError> {
        Ok(Solid::new(self.intern(Node::Empty, Level::Exact, Aabb3::EMPTY, None)?))
    }

    /// `z <= 0`.
    pub fn half_space(&mut self, o: HalfSpaceOpts<'_>) -> Result<Solid<Exact>, CadError> {
        self.primitive(Shape::HalfSpace, Level::Exact, o.name)
    }

    /// A sphere.
    pub fn sphere(&mut self, o: SphereOpts<'_>) -> Result<Solid<Exact>, CadError> {
        let r = Self::positive(NodeKind::Sphere, o.name, "radius", o.radius)?;
        self.primitive(Shape::Sphere { r }, Level::Exact, o.name)
    }

    /// A box (rounded when `round > 0`).
    pub fn cuboid(&mut self, o: BoxOpts<'_>) -> Result<Solid<Exact>, CadError> {
        let k = NodeKind::Box;
        let w = [
            Self::positive(k, o.name, "size x", o.size[0])?,
            Self::positive(k, o.name, "size y", o.size[1])?,
            Self::positive(k, o.name, "size z", o.size[2])?,
        ];
        let half = [w[0] / 2, w[1] / 2, w[2] / 2];
        let round = o.round.as_nm();
        let most = half[0].min(half[1]).min(half[2]);
        if round < 0 || round > most {
            return Err(CadError::Argument {
                kind: k,
                name: o.name.map(Name::new),
                arg: "round",
                value: Value::Length(o.round),
                accepted: "from zero to half the smallest side",
            });
        }
        self.primitive(Shape::Box { half, round }, Level::Exact, o.name)
    }

    /// A cylinder.
    pub fn cylinder(&mut self, o: CylinderOpts<'_>) -> Result<Solid<Exact>, CadError> {
        let r = Self::positive(NodeKind::Cylinder, o.name, "radius", o.radius)?;
        let h = Self::positive(NodeKind::Cylinder, o.name, "height", o.height)?;
        self.primitive(Shape::Cylinder { r, half_h: h / 2 }, Level::Exact, o.name)
    }

    /// A capsule.
    pub fn capsule(&mut self, o: CapsuleOpts<'_>) -> Result<Solid<Exact>, CadError> {
        let r = Self::positive(NodeKind::Capsule, o.name, "radius", o.radius)?;
        let l = o.length.as_nm();
        if l < 0 {
            return Err(CadError::Argument {
                kind: NodeKind::Capsule,
                name: o.name.map(Name::new),
                arg: "length",
                value: Value::Length(o.length),
                accepted: "zero (a sphere) or more",
            });
        }
        self.primitive(Shape::Capsule { r, half_h: l / 2 }, Level::Exact, o.name)
    }

    /// A cone or frustum.
    pub fn cone(&mut self, o: ConeOpts<'_>) -> Result<Solid<Exact>, CadError> {
        let k = NodeKind::Cone;
        let h = Self::positive(k, o.name, "height", o.height)?;
        let (r0, r1) = (o.bottom.as_nm(), o.top.as_nm());
        if r0 < 0 || r1 < 0 || (r0 == 0 && r1 == 0) {
            let bad = if r0 < 0 || (r0 == 0 && r1 == 0) { o.bottom } else { o.top };
            return Err(CadError::Argument {
                kind: k,
                name: o.name.map(Name::new),
                arg: "radius",
                value: Value::Length(bad),
                accepted: "both radii zero or more, one above zero",
            });
        }
        self.primitive(Shape::Cone { r0, r1, half_h: h / 2 }, Level::Exact, o.name)
    }

    /// A torus.
    pub fn torus(&mut self, o: TorusOpts<'_>) -> Result<Solid<Exact>, CadError> {
        let big = Self::positive(NodeKind::Torus, o.name, "major radius", o.major)?;
        let small = Self::positive(NodeKind::Torus, o.name, "minor radius", o.minor)?;
        self.primitive(Shape::Torus { big, small }, Level::Exact, o.name)
    }

    /// An ellipsoid: a bound, `(|p / r| - 1) min(r)` (1-Lipschitz, zero on the surface).
    pub fn ellipsoid(&mut self, o: EllipsoidOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let k = NodeKind::Ellipsoid;
        let radii = [
            Self::positive(k, o.name, "radius x", o.radii[0])?,
            Self::positive(k, o.name, "radius y", o.radii[1])?,
            Self::positive(k, o.name, "radius z", o.radii[2])?,
        ];
        let level = if self.plant.has(CadPlant::EllipsoidExact) { Level::Exact } else { Level::Bound };
        self.primitive(Shape::Ellipsoid { radii }, level, o.name)
    }

    /// A superellipsoid: a bound, `(|p / r|_e - 1) min(r) / L_e`.
    pub fn superellipsoid(&mut self, o: SuperellipsoidOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let k = NodeKind::Superellipsoid;
        let radii = [
            Self::positive(k, o.name, "radius x", o.radii[0])?,
            Self::positive(k, o.name, "radius y", o.radii[1])?,
            Self::positive(k, o.name, "radius z", o.radii[2])?,
        ];
        if !(o.exponent >= 1.0 && o.exponent <= 1000.0) {
            return Err(CadError::Argument {
                kind: k,
                name: o.name.map(Name::new),
                arg: "exponent",
                value: Value::Ratio(o.exponent),
                accepted: "from 1 (an octahedron) to 1000",
            });
        }
        let e_ppm = core::f64::math::round(o.exponent * 1e6) as i64;
        self.primitive(Shape::Superellipsoid { radii, e_ppm }, Level::Bound, o.name)
    }

    /// A triply periodic minimal surface (TPMS) gyroid sheet: a 1-Lipschitz bound,
    /// `|sin(kx)cos(ky) + sin(ky)cos(kz) + sin(kz)cos(kx)| / (k sqrt(3)) - thickness / 2`
    /// where `k = 2 pi / period` (`||grad g|| <= k sqrt(3)` everywhere by Cauchy-Schwarz).
    pub fn gyroid(&mut self, o: GyroidOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let k = NodeKind::Gyroid;
        let period = Self::positive(k, o.name, "period", o.period)?;
        let thickness = Self::positive(k, o.name, "thickness", o.thickness)?;
        self.primitive(Shape::Gyroid { period, thickness }, Level::Bound, o.name)
    }

    // ---- profiles and what takes them ---------------------------------------------------

    fn add_profile(&mut self, p: &Profile<'_>, name: Option<&str>) -> Result<(ProfileId, Level), CadError> {
        let start = self.segs.len() / SEG_WIDTH;
        let most = self.limits.most_profile_points;
        let mut key_extra: AVec<'a, i64> = Vec::new_in(self.alloc);
        let segs = &mut self.segs;
        let reduced = profile::reduce(
            p,
            name.map(Name::new),
            most,
            |s| {
                segs.extend_from_slice(&s.to_row());
                Ok(())
            },
            |k| key_extra.push(k),
        );
        let (red, closed) = match reduced {
            Ok(v) => v,
            Err(e) => {
                self.segs.truncate(start * SEG_WIDTH);
                return Err(e);
            }
        };
        let mut h = H::new(PROFILE_CONTEXT);
        h.u8(red.tag as u8);
        for v in red.key {
            h.i64(v);
        }
        for v in key_extra.iter() {
            h.i64(*v);
        }
        let digest = h.done();
        if let Some(id) = self.profile_by_digest.get(&digest) {
            self.segs.truncate(start * SEG_WIDTH);
            let id = *id;
            return Ok((id, self.profile_rec(id).level));
        }
        let len = self.segs.len() / SEG_WIDTH - start;
        let id = ProfileId(self.profiles.len() as u32);
        self.profiles.push(ProfileRec { digest, level: red.level, bounds: red.bounds, closed, segs: (start as u32, len as u32) });
        self.profile_by_digest.insert(digest, id);
        Ok((id, red.level))
    }

    /// An exact profile (every form but the superellipse).
    pub fn profile(&mut self, o: ProfileOpts<'_>) -> Result<Prof<Exact>, CadError> {
        if let Profile::Superellipse { exponent, .. } = o.profile {
            return Err(CadError::Argument {
                kind: NodeKind::Profile,
                name: o.name.map(Name::new),
                arg: "profile",
                value: Value::Ratio(exponent),
                accepted: "an exact profile; a superellipse is a bound, made by `superellipse_profile`",
            });
        }
        let (id, _) = self.add_profile(&o.profile, o.name)?;
        Ok(Prof { id, level: PhantomData })
    }

    /// Any profile, as a bound.
    pub fn superellipse_profile(&mut self, o: ProfileOpts<'_>) -> Result<Prof<Bound>, CadError> {
        let (id, _) = self.add_profile(&o.profile, o.name)?;
        Ok(Prof { id, level: PhantomData })
    }

    /// `profile` extruded along `z`, centred: exact for an exact profile.
    pub fn extrude<K: Ex>(&mut self, profile: Prof<K>, o: ExtrudeOpts<'_>) -> Result<Solid<K>, CadError> {
        let h = Self::positive(NodeKind::Extrude, o.name, "height", o.height)?;
        let p = *self.profile_rec(profile.id);
        let b = p.bounds;
        let half = m(h / 2);
        let aabb = Aabb3::new(Vec3::new(b[0], b[1], -half), Vec3::new(b[2], b[3], half));
        Ok(Solid::new(self.intern(Node::Extrude { profile: profile.id, half_h: h / 2 }, p.level, aabb, o.name)?))
    }

    fn revolve_node(&mut self, profile: ProfileId, offset: i64, kind: NodeKind, name: Option<&str>) -> Result<NodeId, CadError> {
        let p = *self.profile_rec(profile);
        let b = p.bounds;
        if b[0] + m(offset) < 0.0 {
            return Err(CadError::Argument {
                kind,
                name: name.map(Name::new),
                arg: "profile",
                value: Value::Length(Length::nm(core::f64::math::round(b[0] * 1e9) as i64)),
                accepted: "a profile on the positive side of the axis (its least x plus the radius at or above zero)",
            });
        }
        let r = m(offset) + b[0].abs().max(b[2].abs());
        let aabb = Aabb3::new(Vec3::new(-r, -r, b[1]), Vec3::new(r, r, b[3]));
        self.intern(Node::Revolve { profile, offset }, p.level, aabb, name)
    }

    /// `profile` revolved a whole turn about `z` (its `x` the distance from the axis): exact
    /// for an exact profile on the positive side.
    pub fn revolve<K: Ex>(&mut self, profile: Prof<K>, o: RevolveOpts<'_>) -> Result<Solid<K>, CadError> {
        Ok(Solid::new(self.revolve_node(profile.id, 0, NodeKind::Revolve, o.name)?))
    }

    /// The wedge between the `+x` axis and the direction at `angle` counterclockwise, about
    /// `z`: two half-spaces, intersected (at most half a turn) or joined.
    fn wedge(&mut self, angle: Angle, kind: NodeKind, name: Option<&str>) -> Result<NodeId, CadError> {
        let turn = Angle::<Plain>::PER_TURN;
        let a = angle.as_microarcsec();
        if a <= 0 || a > turn {
            return Err(CadError::Argument {
                kind,
                name: name.map(Name::new),
                arg: "angle",
                value: Value::Angle(angle),
                accepted: "above zero, at most a whole turn",
            });
        }
        let (s, c) = blaze_math::sin_cos_turns(a, ANGLE_UNIT);
        let half = |g: &mut Self, n: Vec3| -> Result<NodeId, CadError> {
            let frame = match Placement::along(n, Vec3::ZERO) {
                Some(f) => f,
                None => Placement::IDENTITY,
            };
            let h = g.intern(Node::Primitive { shape: Shape::HalfSpace, frame: Placement::IDENTITY }, Level::Exact, shape_box(&Shape::HalfSpace), None)?;
            g.wrap(h, frame, None)
        };
        let h1 = half(self, Vec3::new(0.0, -1.0, 0.0))?;
        let h2 = half(self, Vec3::new(-s, c, 0.0))?;
        let op = if 2 * a <= turn { Boolean::Intersect } else { Boolean::Union };
        self.boolean(op, Edge::Sharp, h1, h2, None)
    }

    /// `profile` revolved through `angle` (a bound: the whole revolution cut by a wedge).
    pub fn revolve_part<K: Ex>(&mut self, profile: Prof<K>, o: RevolvePartOpts<'_>) -> Result<Solid<Bound>, CadError> {
        if o.angle.as_microarcsec() == Angle::<Plain>::PER_TURN {
            return Ok(Solid::new(self.revolve_node(profile.id, 0, NodeKind::Revolve, o.name)?));
        }
        let full = self.revolve_node(profile.id, 0, NodeKind::Revolve, None)?;
        let w = self.wedge(o.angle, NodeKind::Revolve, o.name)?;
        Ok(Solid::new(self.boolean(Boolean::Intersect, Edge::Sharp, full, w, o.name)?))
    }

    /// `profile` swept along the arc of `radius` about `z` from `+x` through `angle`: the
    /// profile's `x` is the offset from the arc outwards, its `y` is `z`.
    pub fn sweep_arc<K: Ex>(&mut self, profile: Prof<K>, o: SweepArcOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let r = Self::positive(NodeKind::SweepArc, o.name, "radius", o.radius)?;
        let full = self.revolve_node(profile.id, r, NodeKind::SweepArc, None)?;
        if o.angle.as_microarcsec() == Angle::<Plain>::PER_TURN {
            if let Some(n) = o.name
                && self.names.get(&full).is_none()
            {
                self.names.insert(full, Name::new(n));
            }
            return Ok(Solid::new(full));
        }
        let w = self.wedge(o.angle, NodeKind::SweepArc, o.name)?;
        Ok(Solid::new(self.boolean(Boolean::Intersect, Edge::Sharp, full, w, o.name)?))
    }

    /// `profile` swept along a polyline: one extrusion a segment, each cut at its joints by
    /// the bisecting planes (mitres). Joints turn at most a quarter turn.
    pub fn sweep_polyline<K: Ex>(&mut self, profile: Prof<K>, o: SweepPolylineOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let kind = NodeKind::SweepPolyline;
        let pts = o.points;
        let bad = |arg: &'static str, value: Value, accepted: &'static str| CadError::Argument { kind, name: o.name.map(Name::new), arg, value, accepted };
        if pts.len() < 2 {
            return Err(bad("points", Value::Count(pts.len() as u64), "at least 2"));
        }
        if pts.len() as u64 > u64::from(self.limits.most_copies) {
            return Err(bad("points", Value::Count(pts.len() as u64), "at most Limits::most_copies"));
        }
        let p = *self.profile_rec(profile.id);
        let reach = p.bounds.iter().fold(0.0f64, |a, v| a.max(v.abs()));
        let at = |i: usize| Vec3::new(pts[i][0].as_m(), pts[i][1].as_m(), pts[i][2].as_m());
        let mut layer: AVec<'a, NodeId> = Vec::with_capacity_in(pts.len() - 1, self.alloc);
        for i in 0..pts.len() - 1 {
            let (a, b) = (at(i), at(i + 1));
            let d = b - a;
            let len = d.length();
            let Some(dir) = d.normalized() else {
                return Err(bad("points", Value::Count(i as u64), "consecutive points apart"));
            };
            // Mitre at each interior end: the segment runs `reach` past it, cut by the bisector.
            let mut cuts: [Option<(Vec3, Vec3)>; 2] = [None, None];
            let (mut ext0, mut ext1) = (0.0, 0.0);
            if i > 0 {
                let prev = (a - at(i - 1)).normalized().unwrap_or(dir);
                if prev.dot(dir) < -1e-12 {
                    return Err(bad("joint", Value::Count(i as u64), "a turn of at most a quarter turn"));
                }
                // The cut keeps the side ahead of the bisector at `a`.
                if let Some(n) = (prev + dir).normalized() {
                    cuts[0] = Some((a, -n));
                }
                ext0 = reach;
            }
            if i + 2 < pts.len() {
                let next = (at(i + 2) - b).normalized().unwrap_or(dir);
                if next.dot(dir) < -1e-12 {
                    return Err(bad("joint", Value::Count(i as u64 + 1), "a turn of at most a quarter turn"));
                }
                if let Some(n) = (dir + next).normalized() {
                    cuts[1] = Some((b, n));
                }
                ext1 = reach;
            }
            let half = len * 0.5 + (ext0 + ext1) * 0.5;
            let centre = a + dir * ((len + ext1 - ext0) * 0.5);
            let half_nm = core::f64::math::round(half * 1e9) as i64;
            let piece = self.extrude(profile, ExtrudeOpts { height: Length::nm(2 * half_nm.max(1)), name: None })?;
            let frame = Placement::along(dir, centre).unwrap_or(Placement::IDENTITY);
            let mut seg = self.wrap(piece.id, frame, None)?;
            for (pt, n) in cuts.into_iter().flatten() {
                let h = self.intern(Node::Primitive { shape: Shape::HalfSpace, frame: Placement::IDENTITY }, Level::Exact, shape_box(&Shape::HalfSpace), None)?;
                let frame = Placement::along(n, pt).unwrap_or(Placement::IDENTITY);
                let h = self.wrap(h, frame, None)?;
                seg = self.boolean(Boolean::Intersect, Edge::Sharp, seg, h, None)?;
            }
            layer.push(seg);
        }
        let id = self.balanced_boolean(Boolean::Union, Edge::Sharp, layer, o.name)?;
        Ok(Solid::new(id))
    }

    /// A loft through stations: between two stations the field is the linear blend of their
    /// profiles' fields in height, divided by `sqrt(1 + k^2)` with `k` the most any two
    /// neighbouring profiles' fields can differ per unit height (their boxes' diameter over the
    /// gap); cut to the first and last heights. A bound.
    pub fn loft(&mut self, o: LoftOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let kind = NodeKind::Loft;
        let st = o.stations;
        let bad = |arg: &'static str, value: Value, accepted: &'static str| CadError::Argument { kind, name: o.name.map(Name::new), arg, value, accepted };
        if st.len() < 2 || st.len() as u64 > u64::from(self.limits.most_stations) {
            return Err(bad("stations", Value::Count(st.len() as u64), "2 to Limits::most_stations"));
        }
        let mut k: f64 = 0.0;
        let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for (i, s) in st.iter().enumerate() {
            let pb = self.profile_rec(s.profile.id).bounds;
            b = [b[0].min(pb[0]), b[1].min(pb[1]), b[2].max(pb[2]), b[3].max(pb[3])];
            if i > 0 {
                let gap = s.z.as_nm() - st[i - 1].z.as_nm();
                if gap <= 0 {
                    return Err(bad("station height", Value::Length(s.z), "heights strictly increasing"));
                }
                let qb = self.profile_rec(st[i - 1].profile.id).bounds;
                let (x0, y0) = (pb[0].min(qb[0]), pb[1].min(qb[1]));
                let (x1, y1) = (pb[2].max(qb[2]), pb[3].max(qb[3]));
                let diam = blaze_math::hypot(x1 - x0, y1 - y0);
                k = k.max(diam / m(gap));
            }
        }
        let start = self.stations.len() as u32;
        for s in st {
            self.stations.push((s.z.as_nm(), s.profile.id));
        }
        let l = blaze_math::sqrt(1.0 + k * k);
        let aabb = Aabb3::new(Vec3::new(b[0], b[1], st[0].z.as_m()), Vec3::new(b[2], b[3], st[st.len() - 1].z.as_m()));
        let node = Node::Loft { start, len: st.len() as u32, lipschitz_ppm: ppm_up(l) };
        let before = self.nodes.len();
        let id = self.intern(node, Level::Bound, aabb, o.name)?;
        if self.nodes.len() == before {
            // Already present: its stations are the earlier copy's; drop these.
            self.stations.truncate(start as usize);
        }
        Ok(Solid::new(id))
    }

    // ---- maps -----------------------------------------------------------------------------

    /// `s` placed by `p` (a rigid map, maybe mirroring): keeps its level.
    pub fn place<K: Ex>(&mut self, s: Solid<K>, p: Placement) -> Result<Solid<K>, CadError> {
        self.check(s.id)?;
        Ok(Solid::new(self.wrap(s.id, p, None)?))
    }

    /// `s` scaled about the origin by `factor`: keeps its level.
    pub fn scale<K: Ex>(&mut self, s: Solid<K>, o: ScaleOpts<'_>) -> Result<Solid<K>, CadError> {
        self.check(s.id)?;
        if !(o.factor > 1e-6 && o.factor < 1e6) {
            return Err(CadError::Argument {
                kind: NodeKind::Scale,
                name: o.name.map(Name::new),
                arg: "factor",
                value: Value::Ratio(o.factor),
                accepted: "from 10^-6 to 10^6",
            });
        }
        let pp12 = core::f64::math::round(o.factor * 1e12) as i64;
        Ok(Solid::new(self.scale_node(s.id, pp12, o.name)?))
    }

    fn scale_node(&mut self, id: NodeId, pp12: i64, name: Option<&str>) -> Result<NodeId, CadError> {
        if pp12 == 1_000_000_000_000 {
            return Ok(id);
        }
        let (p, inner) = self.split(id)?;
        let r = *self.rec(inner);
        let node = match r.node {
            Node::Empty => return Ok(inner),
            Node::Scale { inner: i, factor_pp12 } => {
                let f = (i128::from(factor_pp12) * i128::from(pp12) + 500_000_000_000) / 1_000_000_000_000;
                let f = i64::try_from(f).unwrap_or(i64::MAX);
                if f == 1_000_000_000_000 {
                    return self.wrap(i, p.with_shift_scaled(pp12 as f64 / 1e12), name);
                }
                Node::Scale { inner: i, factor_pp12: f }
            }
            _ => Node::Scale { inner, factor_pp12: pp12 },
        };
        let f = pp12 as f64 / 1e12;
        let aabb = if finite(&r.aabb) { Aabb3::new(r.aabb.lo * f, r.aabb.hi * f) } else { Aabb3::ALL };
        let id = self.intern(node, r.level, aabb, name)?;
        self.wrap(id, p.with_shift_scaled(f), None)
    }

    /// `s` under a general affine map: a bound, divided by the inverse's Frobenius norm.
    pub fn affine<K: Ex>(&mut self, s: Solid<K>, o: AffineOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.check(s.id)?;
        let grid = |v: f64| core::f64::math::round(v * crate::placement::LINEAR_GRID.recip()) as i64;
        let mut key = [0i64; 12];
        let mut rows = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                let g = grid(o.linear.rows[i][j]);
                key[i * 3 + j] = g;
                rows[i][j] = g as f64 * crate::placement::LINEAR_GRID;
            }
        }
        for (i, l) in o.shift.iter().enumerate() {
            key[9 + i] = l.as_nm();
        }
        let shift = Vec3::new(o.shift[0].as_m(), o.shift[1].as_m(), o.shift[2].as_m());
        let Ok(a) = Affine3::general(Mat3::new(rows), shift) else {
            return Err(CadError::Argument {
                kind: NodeKind::Affine,
                name: o.name.map(Name::new),
                arg: "linear",
                value: Value::Ratio(Mat3::new(rows).det()),
                accepted: "an invertible matrix (determinant away from zero)",
            });
        };
        let r = *self.rec(s.id);
        let aabb = if finite(&r.aabb) { a.apply_box(&r.aabb) } else { Aabb3::ALL };
        let node = Node::Affine { inner: s.id, key, lipschitz_ppm: ppm_up(a.lipschitz_of_inverse().max(1.0)) };
        Ok(Solid::new(self.intern(node, Level::Bound, aabb, o.name)?))
    }

    /// `s` and its mirror image across the plane through the origin normal to `axis`, as one
    /// fold: `s` must lie on one side (else [`CadError::Overlap`]). Keeps the level.
    pub fn mirror<K: Ex>(&mut self, s: Solid<K>, o: MirrorOpts<'_>) -> Result<Solid<K>, CadError> {
        self.check(s.id)?;
        let r = *self.rec(s.id);
        let i = o.axis.index();
        let (lo, hi) = (r.aabb.lo.to_array()[i], r.aabb.hi.to_array()[i]);
        let inner = if lo >= 0.0 {
            s.id
        } else if hi <= 0.0 {
            self.wrap(s.id, Placement::mirror(o.axis), None)?
        } else {
            return Err(CadError::Overlap {
                kind: NodeKind::Mirror,
                name: o.name.map(Name::new),
                extent: Length::nm(core::f64::math::round((hi - lo) * 1e9) as i64),
                room: Length::ZERO,
            });
        };
        let ib = self.rec(inner).aabb;
        let mut lo2 = ib.lo.to_array();
        lo2[i] = -ib.hi.to_array()[i];
        let aabb = Aabb3::new(Vec3::from_array(lo2), ib.hi);
        Ok(Solid::new(self.intern(Node::MirrorFold { inner, axis: o.axis }, r.level, aabb, o.name)?))
    }

    /// `count` copies along `axis`, `step` apart. The operand must lie within half a step of
    /// the origin along the axis ([`CadError::Overlap`] otherwise). `Inside` from an exact or
    /// inside-exact operand (see `level.rs`).
    pub fn linear_repeat<K: Ex>(&mut self, s: Solid<K>, o: RepeatOpts<'_>) -> Result<Solid<K::Repeated>, CadError> {
        self.check(s.id)?;
        let kind = NodeKind::LinearRepeat;
        let step = Self::positive(kind, o.name, "step", o.step)?;
        if o.count == 0 || o.count > self.limits.most_copies {
            return Err(CadError::Argument {
                kind,
                name: o.name.map(Name::new),
                arg: "count",
                value: Value::Count(u64::from(o.count)),
                accepted: "1 to Limits::most_copies",
            });
        }
        if o.count == 1 {
            return Ok(Solid::new(s.id));
        }
        let r = *self.rec(s.id);
        if !finite(&r.aabb) {
            return Err(CadError::Unbounded { kind, name: o.name.map(Name::new), what: "the operand" });
        }
        let i = o.axis.index();
        let w = r.aabb.lo.to_array()[i].abs().max(r.aabb.hi.to_array()[i].abs());
        let w_nm = core::f64::math::ceil(w * 1e9) as i64;
        if 2 * w_nm >= step {
            return Err(CadError::Overlap { kind, name: o.name.map(Name::new), extent: Length::nm(2 * w_nm), room: o.step });
        }
        let mut hi = r.aabb.hi.to_array();
        hi[i] += m(step) * f64::from(o.count - 1);
        let aabb = Aabb3::new(r.aabb.lo, Vec3::from_array(hi));
        let node = Node::Repeat { inner: s.id, axis: o.axis, step, count: o.count, extent: w_nm };
        Ok(Solid::new(self.intern(node, r.level.repeated(), aabb, o.name)?))
    }

    /// `count` copies turned evenly about `z`: a union of placed copies (each turn a whole
    /// number of micro-arcseconds: exact when `count` divides 1,296,000,000,000).
    pub fn circular_repeat<K: Join<K>>(&mut self, s: Solid<K>, o: CircularOpts<'_>) -> Result<Solid<K::Union>, CadError> {
        self.check(s.id)?;
        if o.count == 0 || o.count > self.limits.most_copies {
            return Err(CadError::Argument {
                kind: NodeKind::CircularRepeat,
                name: o.name.map(Name::new),
                arg: "count",
                value: Value::Count(u64::from(o.count)),
                accepted: "1 to Limits::most_copies",
            });
        }
        let turn = i128::from(Angle::<Plain>::PER_TURN);
        let mut layer: AVec<'a, NodeId> = Vec::with_capacity_in(o.count as usize, self.alloc);
        for k in 0..o.count {
            let a = (turn * i128::from(k) + i128::from(o.count) / 2) / i128::from(o.count);
            let p = Placement::rotation(Axis::Z, Angle::microarcsec(a as i64));
            layer.push(self.wrap(s.id, p, None)?);
        }
        // A balanced tree of unions: depth log2(count).
        let id = self.balanced_boolean(Boolean::Union, Edge::Sharp, layer, o.name)?;
        Ok(Solid::new(id))
    }

    // ---- booleans -------------------------------------------------------------------------

    /// Reduces a non-empty `layer` into a balanced binary tree of depth `ceil(log2(len))`,
    /// reusing two arena buffers across levels.
    fn balanced_boolean(&mut self, op: Boolean, edge: Edge, mut layer: AVec<'a, NodeId>, name: Option<&str>) -> Result<NodeId, CadError> {
        let mut next: AVec<'a, NodeId> = Vec::with_capacity_in(layer.len().div_ceil(2), self.alloc);
        while layer.len() > 1 {
            next.clear();
            let final_merge = layer.len() == 2;
            for pair in layer.chunks(2) {
                next.push(if pair.len() == 2 {
                    self.boolean(op, edge, pair[0], pair[1], if final_merge { name } else { None })?
                } else {
                    pair[0]
                });
            }
            core::mem::swap(&mut layer, &mut next);
        }
        let id = layer[0];
        if let Some(n) = name
            && self.names.get(&id).is_none()
        {
            self.names.insert(id, Name::new(n));
        }
        Ok(id)
    }

    fn boolean_all<K: Ex>(&mut self, op: Boolean, edge: Edge, kind: NodeKind, solids: &[Solid<K>], name: Option<&str>) -> Result<NodeId, CadError> {
        if solids.is_empty() {
            return Err(CadError::Argument {
                kind,
                name: name.map(Name::new),
                arg: "solids",
                value: Value::Count(0),
                accepted: "at least 1 solid",
            });
        }
        let mut layer: AVec<'a, NodeId> = Vec::with_capacity_in(solids.len(), self.alloc);
        for s in solids {
            self.check(s.id)?;
            layer.push(s.id);
        }
        self.balanced_boolean(op, edge, layer, name)
    }

    fn boolean(&mut self, op: Boolean, edge: Edge, a: NodeId, b: NodeId, name: Option<&str>) -> Result<NodeId, CadError> {
        self.check(a)?;
        self.check(b)?;
        let (ea, eb) = (matches!(self.rec(a).node, Node::Empty), matches!(self.rec(b).node, Node::Empty));
        match op {
            Boolean::Union if ea => return Ok(b),
            Boolean::Union if eb => return Ok(a),
            Boolean::Intersect if ea || eb => return Ok(if ea { a } else { b }),
            Boolean::Difference if ea || eb => return Ok(a),
            _ => {}
        }
        if a == b && op != Boolean::Difference && edge == Edge::Sharp {
            return Ok(a);
        }
        // Hoist a placement both operands share (split only then: splitting interns the
        // unplaced form, which a boolean of differently placed parts never needs).
        let (place, mut a, mut b) = if self.placement_of(a) == self.placement_of(b) && !self.placement_of(a).is_identity() {
            let (pa, ia) = self.split(a)?;
            let (_, ib) = self.split(b)?;
            (pa, ia, ib)
        } else {
            (Placement::IDENTITY, a, b)
        };
        if op.commutes() && !self.plant.has(CadPlant::OperandsUnordered) && self.rec(b).full < self.rec(a).full {
            core::mem::swap(&mut a, &mut b);
        }
        let (ra, rb) = (*self.rec(a), *self.rec(b));
        let level = match (op, edge) {
            (Boolean::Union, Edge::Sharp) => ra.level.union(rb.level),
            (Boolean::Intersect, Edge::Sharp) => ra.level.intersect(rb.level),
            (Boolean::Difference, Edge::Sharp) => ra.level.intersect(rb.level.complement()),
            _ => Level::Bound,
        };
        let grow = match edge {
            Edge::Sharp => 0.0,
            Edge::Round { r } => m(r) * 0.25,
            Edge::Chamfer { r } => m(r),
        };
        let aabb = match op {
            Boolean::Union => ra.aabb.union(&rb.aabb).grow(grow),
            Boolean::Intersect => ra.aabb.intersect(&rb.aabb),
            Boolean::Difference => ra.aabb,
        };
        let id = self.intern(Node::Boolean { op, edge, a, b }, level, aabb, if place.is_identity() { name } else { None })?;
        self.wrap(id, place, if place.is_identity() { None } else { name })
    }

    /// `a` or `b`.
    pub fn union<A: Join<B>, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>) -> Result<Solid<A::Union>, CadError> {
        Ok(Solid::new(self.boolean(Boolean::Union, Edge::Sharp, a.id, b.id, None)?))
    }

    /// `a` and `b`.
    pub fn intersect<A: Join<B>, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>) -> Result<Solid<A::Intersect>, CadError> {
        Ok(Solid::new(self.boolean(Boolean::Intersect, Edge::Sharp, a.id, b.id, None)?))
    }

    /// `a` without `b`.
    pub fn difference<A: Join<B::Complement>, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>) -> Result<Solid<A::Intersect>, CadError> {
        Ok(Solid::new(self.boolean(Boolean::Difference, Edge::Sharp, a.id, b.id, None)?))
    }

    /// Balanced binary tree union of `solids` (depth `ceil(log2(solids.len()))`, preserving level).
    /// Returns [`CadError::Argument`] when `solids` is empty.
    pub fn union_all<K: Join<K>>(&mut self, solids: &[Solid<K>]) -> Result<Solid<K::Union>, CadError> {
        Ok(Solid::new(self.boolean_all(Boolean::Union, Edge::Sharp, NodeKind::Union, solids, None)?))
    }

    /// Balanced binary tree intersection of `solids` (depth `ceil(log2(solids.len()))`, preserving level).
    /// Returns [`CadError::Argument`] when `solids` is empty.
    pub fn intersect_all<K: Join<K>>(&mut self, solids: &[Solid<K>]) -> Result<Solid<K::Intersect>, CadError> {
        Ok(Solid::new(self.boolean_all(Boolean::Intersect, Edge::Sharp, NodeKind::Intersect, solids, None)?))
    }

    fn blend_all<K: Ex>(&mut self, op: Boolean, chamfer: bool, solids: &[Solid<K>], o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let kind = if chamfer { NodeKind::ChamferBoolean } else { NodeKind::SmoothBoolean };
        let r = Self::positive(kind, o.name, "radius", o.radius)?;
        let edge = if chamfer { Edge::Chamfer { r } } else { Edge::Round { r } };
        Ok(Solid::new(self.boolean_all(op, edge, kind, solids, o.name)?))
    }

    /// Balanced binary tree union with a rounded fillet of `radius` at every join.
    pub fn round_union_all<K: Ex>(&mut self, solids: &[Solid<K>], o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.blend_all(Boolean::Union, false, solids, o)
    }

    /// Balanced binary tree intersection with a rounded edge at every join.
    pub fn round_intersect_all<K: Ex>(&mut self, solids: &[Solid<K>], o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.blend_all(Boolean::Intersect, false, solids, o)
    }

    /// Balanced binary tree union with a chamfer at every join.
    pub fn chamfer_union_all<K: Ex>(&mut self, solids: &[Solid<K>], o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.blend_all(Boolean::Union, true, solids, o)
    }

    /// Balanced binary tree intersection with a chamfer at every join.
    pub fn chamfer_intersect_all<K: Ex>(&mut self, solids: &[Solid<K>], o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.blend_all(Boolean::Intersect, true, solids, o)
    }

    fn blend<A: Ex, B: Ex>(&mut self, op: Boolean, chamfer: bool, a: Solid<A>, b: Solid<B>, o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let kind = if chamfer { NodeKind::ChamferBoolean } else { NodeKind::SmoothBoolean };
        let r = Self::positive(kind, o.name, "radius", o.radius)?;
        let edge = if chamfer { Edge::Chamfer { r } } else { Edge::Round { r } };
        Ok(Solid::new(self.boolean(op, edge, a.id, b.id, o.name)?))
    }

    /// A union with a rounded fillet of `radius` where the two meet (quadratic blend).
    pub fn round_union<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.blend(Boolean::Union, false, a, b, o)
    }

    /// An intersection with a rounded edge.
    pub fn round_intersect<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.blend(Boolean::Intersect, false, a, b, o)
    }

    /// A difference with a rounded edge.
    pub fn round_difference<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.blend(Boolean::Difference, false, a, b, o)
    }

    /// A union with a chamfer.
    pub fn chamfer_union<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.blend(Boolean::Union, true, a, b, o)
    }

    /// An intersection with a chamfer.
    pub fn chamfer_intersect<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.blend(Boolean::Intersect, true, a, b, o)
    }

    /// A difference with a chamfer.
    pub fn chamfer_difference<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: BlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.blend(Boolean::Difference, true, a, b, o)
    }

    // ---- offsets --------------------------------------------------------------------------

    /// A unary node that commutes with placement: built on the unplaced operand, placed above.
    fn hoisted(&mut self, id: NodeId, name: Option<&str>, make: impl FnOnce(&Self, NodeId) -> (Node, Level, Aabb3)) -> Result<NodeId, CadError> {
        self.check(id)?;
        let (p, inner) = self.split(id)?;
        let (node, level, aabb) = make(self, inner);
        let named = if p.is_identity() { name } else { None };
        let id = self.intern(node, level, aabb, named)?;
        self.wrap(id, p, if p.is_identity() { None } else { name })
    }

    fn offset(&mut self, id: NodeId, by: Length, erode: bool, name: Option<&str>) -> Result<NodeId, CadError> {
        let kind = if erode { NodeKind::Erode } else { NodeKind::Grow };
        let r = by.as_nm();
        if r < 0 {
            return Err(CadError::Argument {
                kind,
                name: name.map(Name::new),
                arg: "by",
                value: Value::Length(by),
                accepted: "zero or more (grow and erode are separate)",
            });
        }
        if r == 0 {
            return Ok(id);
        }
        self.hoisted(id, name, |g, inner| {
            let ri = *g.rec(inner);
            match (ri.node, erode) {
                (Node::Grow { inner: i, r: r0 }, false) => (Node::Grow { inner: i, r: r0 + r }, Level::Outside, ri.aabb.grow(m(r))),
                (Node::Erode { inner: i, r: r0 }, true) => (Node::Erode { inner: i, r: r0 + r }, Level::Inside, ri.aabb),
                (_, false) => (Node::Grow { inner, r }, Level::Outside, ri.aabb.grow(m(r))),
                (_, true) => (Node::Erode { inner, r }, Level::Inside, ri.aabb),
            }
        })
    }

    /// Grow by `by`: exact outside, from a field exact outside.
    pub fn grow<K: AtLeastOutside>(&mut self, s: Solid<K>, o: OffsetOpts<'_>) -> Result<Solid<Outside>, CadError> {
        Ok(Solid::new(self.offset(s.id, o.by, false, o.name)?))
    }

    /// Erode by `by`: exact inside, from a field exact inside.
    pub fn erode<K: AtLeastInside>(&mut self, s: Solid<K>, o: OffsetOpts<'_>) -> Result<Solid<Inside>, CadError> {
        Ok(Solid::new(self.offset(s.id, o.by, true, o.name)?))
    }

    /// A wall of `thickness` inside the surface of an exact solid: exact outside the wall.
    pub fn shell(&mut self, s: Solid<Exact>, o: ShellOpts<'_>) -> Result<Solid<Outside>, CadError> {
        let t = Self::positive(NodeKind::Shell, o.name, "thickness", o.thickness)?;
        Ok(Solid::new(self.hoisted(s.id, o.name, |g, inner| (Node::Shell { inner, t }, Level::Outside, g.rec(inner).aabb))?))
    }

    /// The exact distance of `s`'s solid: a grid the mesher solves ([`Graph::attach_grid`]).
    pub fn redistance<K: Ex>(&mut self, s: Solid<K>) -> Result<Solid<Exact>, CadError> {
        Ok(Solid::new(self.hoisted(s.id, None, |g, inner| (Node::Redistance { inner }, Level::Exact, g.rec(inner).aabb))?))
    }

    /// Every edge rounded by `radius`: erode, redistance, grow. Compiles once the mesher has
    /// attached the redistance node's grid.
    pub fn round_edges<K: AtLeastInside>(&mut self, s: Solid<K>, o: RoundOpts<'_>) -> Result<Solid<Outside>, CadError> {
        Self::positive(NodeKind::RoundEdges, o.name, "radius", o.radius)?;
        let e = self.erode(s, OffsetOpts { by: o.radius, name: None })?;
        let r = self.redistance(e)?;
        self.grow(r, OffsetOpts { by: o.radius, name: o.name })
    }

    /// Attaches the solved grid of a redistance node.
    pub fn attach_grid(&mut self, s: Solid<Exact>, o: GridOpts<'_>) -> Result<(), CadError> {
        self.check(s.id)?;
        let (_, inner) = self.split(s.id)?;
        let bad = |arg: &'static str, value: Value, accepted: &'static str| CadError::Argument { kind: NodeKind::Redistance, name: o.name.map(Name::new), arg, value, accepted };
        if !matches!(self.rec(inner).node, Node::Redistance { .. }) {
            return Err(bad("node", Value::Count(u64::from(inner.0)), "a redistance node"));
        }
        let n = o.dims.iter().map(|d| u64::from(*d)).product::<u64>();
        if o.dims.iter().any(|d| *d < 2) || n != o.values.len() as u64 || n > u64::from(self.limits.most_grid_values) {
            return Err(bad("dims", Value::Count(n), "at least 2 a side, as many values as points, within Limits::most_grid_values"));
        }
        let step = Self::positive(NodeKind::Redistance, o.name, "step", o.step)?;
        if o.values.iter().any(|v| !v.is_finite()) {
            return Err(bad("values", Value::Count(n), "finite values"));
        }
        let start = self.grid_values.len() as u32;
        self.grid_values.extend_from_slice(o.values);
        let g = GridRec {
            origin: [o.origin[0].as_m(), o.origin[1].as_m(), o.origin[2].as_m()],
            step: m(step),
            dims: o.dims,
            band: o.band.as_m().max(0.0),
            start,
        };
        self.grids.insert(inner, g);
        Ok(())
    }

    // ---- deformations ---------------------------------------------------------------------

    fn deformed(&mut self, kind: NodeKind, id: NodeId, node: Node, name: Option<&str>, accepted: &'static str) -> Result<NodeId, CadError> {
        self.check(id)?;
        let r = *self.rec(id);
        if !finite(&r.aabb) {
            return Err(CadError::Unbounded { kind, name: name.map(Name::new), what: "the operand" });
        }
        let Some((c, l)) = deform(&node, &r.aabb) else {
            return Err(CadError::Argument { kind, name: name.map(Name::new), arg: "operand's extent", value: Value::Level(r.level), accepted });
        };
        let node = match node {
            Node::Twist { inner, rate, .. } => Node::Twist { inner, rate, lipschitz_ppm: ppm_up(l) },
            Node::Bend { inner, radius, .. } => Node::Bend { inner, radius, lipschitz_ppm: ppm_up(l) },
            Node::Taper { inner, top_ppm, z0, z1, .. } => Node::Taper { inner, top_ppm, z0, z1, lipschitz_ppm: ppm_up(l) },
            n => n,
        };
        self.intern(node, Level::Bound, c, name)
    }

    /// The `xy` plane turned by `per_metre` per metre of `z` (a bound).
    pub fn twist<K: Ex>(&mut self, s: Solid<K>, o: TwistOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let node = Node::Twist { inner: s.id, rate: o.per_metre.as_microarcsec(), lipschitz_ppm: 0 };
        if o.per_metre.as_microarcsec() == 0 {
            return Ok(Solid::new(s.id));
        }
        Ok(Solid::new(self.deformed(NodeKind::Twist, s.id, node, o.name, "a bounded operand")?))
    }

    /// Bent about the axis parallel to `y` at height `radius` (a bound). The operand's `x`
    /// extent becomes arc length: it must stay within a quarter turn each side, and its `z`
    /// below `radius`.
    pub fn bend<K: Ex>(&mut self, s: Solid<K>, o: BendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let r = Self::positive(NodeKind::Bend, o.name, "radius", o.radius)?;
        let node = Node::Bend { inner: s.id, radius: r, lipschitz_ppm: 0 };
        Ok(Solid::new(self.deformed(NodeKind::Bend, s.id, node, o.name, "x within a quarter turn of arc each side, z below the radius")?))
    }

    /// `xy` scaled from 1 at the operand's bottom to `top_scale` at its top (a bound).
    pub fn taper<K: Ex>(&mut self, s: Solid<K>, o: TaperOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.check(s.id)?;
        if !(o.top_scale > 0.0 && o.top_scale < 1e6) {
            return Err(CadError::Argument {
                kind: NodeKind::Taper,
                name: o.name.map(Name::new),
                arg: "top_scale",
                value: Value::Ratio(o.top_scale),
                accepted: "above zero, below 10^6",
            });
        }
        let top_ppm = core::f64::math::round(o.top_scale * 1e6) as i64;
        if top_ppm == 1_000_000 {
            return Ok(Solid::new(s.id));
        }
        let b = self.rec(s.id).aabb;
        let (z0, z1) = (core::f64::math::floor(b.lo.z * 1e9) as i64, core::f64::math::ceil(b.hi.z * 1e9) as i64);
        let node = Node::Taper { inner: s.id, top_ppm, z0, z1, lipschitz_ppm: 0 };
        Ok(Solid::new(self.deformed(NodeKind::Taper, s.id, node, o.name, "a bounded operand")?))
    }

    /// A periodic relief `amplitude sin(kx) sin(ky) sin(kz)`, `k = 2 pi / period`, present at
    /// [`Detail::Full`] only: `(f + A P) / (1 + A k sqrt 3)` (a bound; needs a 1-Lipschitz
    /// operand).
    pub fn detail<K: Lipschitz>(&mut self, s: Solid<K>, o: DetailOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let amp = Self::positive(NodeKind::Detail, o.name, "amplitude", o.amplitude)?;
        let period = Self::positive(NodeKind::Detail, o.name, "period", o.period)?;
        Ok(Solid::new(self.hoisted(s.id, o.name, |g, inner| {
            let r = g.rec(inner);
            (Node::Detail { inner, amp, period }, Level::Bound, r.aabb.grow(m(amp)))
        })?))
    }

    // ---- rail-guided lofts ----------------------------------------------------------------

    /// A rail-guided loft through `o.stations` with smooth C2 cubic spline rails for width
    /// (`side_rail`), height (`ridge_rail`), center (`center_x_rail`, `center_y_rail`), and
    /// station `twist`, plus configurable cross-section blend continuity (`Linear`, `SmoothC1`,
    /// `CurvatureC2`) and optional end-cap rounding (`cap_round`).
    ///
    /// When `side_rail` is provided, its value at `z` sets the loft's outer X half-width
    /// relative to station 0's X half-extent (`sx(z) = side_rail(z) / ref_hx`); similarly
    /// `ridge_rail` sets the outer Y half-height (`sy(z) = ridge_rail(z) / ref_hy`). When a
    /// rail is `None`, the corresponding `GuidedStation` field (`scale[0]`, `scale[1]`,
    /// `center[0]`, `center[1]`, `twist`) is interpolated across stations by a C2 natural
    /// cubic spline.
    pub fn guided_loft(&mut self, o: GuidedLoftOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let kind = NodeKind::GuidedLoft;
        let st = o.stations;
        let alloc = self.alloc;
        let most_stations = self.limits.most_stations;
        let bad = |arg: &'static str, value: Value, accepted: &'static str| CadError::Argument {
            kind,
            name: o.name.map(Name::new),
            arg,
            value,
            accepted,
        };
        if st.len() < 2 || st.len() as u64 > u64::from(most_stations) {
            return Err(bad("stations", Value::Count(st.len() as u64), "2 to Limits::most_stations"));
        }
        for (i, s) in st.iter().enumerate() {
            if (s.profile.id.0 as usize) >= self.profiles.len() {
                return Err(CadError::Foreign { id: s.profile.id.0 });
            }
            if !(s.scale[0] > 0.0 && s.scale[0].is_finite()) {
                return Err(bad("station scale x", Value::Ratio(s.scale[0]), "a finite scale above zero"));
            }
            if !(s.scale[1] > 0.0 && s.scale[1].is_finite()) {
                return Err(bad("station scale y", Value::Ratio(s.scale[1]), "a finite scale above zero"));
            }
            if i > 0 && s.z.as_nm() <= st[i - 1].z.as_nm() {
                return Err(bad("station height", Value::Length(s.z), "heights strictly increasing"));
            }
        }
        let z0_nm = st[0].z.as_nm();
        let zn_nm = st[st.len() - 1].z.as_nm();
        let cap_nm = o.cap_round.as_nm();
        if cap_nm < 0 || 2 * cap_nm > zn_nm - z0_nm {
            return Err(bad("cap_round", Value::Length(o.cap_round), "from zero to half the loft height"));
        }

        let p0_bounds = self.profile_rec(st[0].profile.id).bounds;
        let ref_hx = 0.5 * (p0_bounds[2] - p0_bounds[0]);
        let ref_hy = 0.5 * (p0_bounds[3] - p0_bounds[1]);
        if o.side_rail.is_some() && !(ref_hx > 0.0) {
            return Err(bad("side_rail", Value::Ratio(ref_hx), "station 0 with positive X half-extent"));
        }
        if o.ridge_rail.is_some() && !(ref_hy > 0.0) {
            return Err(bad("ridge_rail", Value::Ratio(ref_hy), "station 0 with positive Y half-extent"));
        }

        let spans_start = self.spline_spans.len();
        let build_rail = |rail: GuideRail<'_>,
                          arg: &'static str,
                          require_positive: bool,
                          inv_scale: f64,
                          default_val: f64,
                          out: &mut AVec<'a, SplineSpan>|
         -> Result<(u32, u32), CadError> {
            let pts = rail.points;
            if pts.len() < 2 || pts.len() as u64 > u64::from(most_stations) {
                return Err(bad(arg, Value::Count(pts.len() as u64), "2 to Limits::most_stations rail points"));
            }
            let mut zs: AVec<'a, i64> = Vec::with_capacity_in(pts.len(), alloc);
            let mut ys: AVec<'a, f64> = Vec::with_capacity_in(pts.len(), alloc);
            let mut ts: AVec<'a, Option<f64>> = Vec::with_capacity_in(pts.len(), alloc);
            for (i, pt) in pts.iter().enumerate() {
                let zn = pt.z.as_nm();
                if i > 0 && zn <= zs[i - 1] {
                    return Err(bad(arg, Value::Length(pt.z), "rail heights strictly increasing"));
                }
                if require_positive && pt.value.as_nm() <= 0 {
                    return Err(bad(arg, Value::Length(pt.value), "rail values above zero"));
                }
                if let Some(tg) = pt.tangent
                    && !tg.is_finite()
                {
                    return Err(bad(arg, Value::Ratio(tg), "finite rail tangents"));
                }
                zs.push(zn);
                ys.push(pt.value.as_m() * inv_scale);
                ts.push(pt.tangent.map(|tg| tg * inv_scale));
            }
            solve_cubic_spline(&zs, &ys, &ts, default_val, alloc, out, kind, o.name, arg)
        };

        let build_station_spline = |extract: fn(&GuidedStation) -> f64,
                                    arg: &'static str,
                                    default_val: f64,
                                    out: &mut AVec<'a, SplineSpan>|
         -> Result<(u32, u32), CadError> {
            let mut zs: AVec<'a, i64> = Vec::with_capacity_in(st.len(), alloc);
            let mut ys: AVec<'a, f64> = Vec::with_capacity_in(st.len(), alloc);
            let mut ts: AVec<'a, Option<f64>> = Vec::with_capacity_in(st.len(), alloc);
            for s in st {
                zs.push(s.z.as_nm());
                ys.push(extract(s));
                ts.push(None);
            }
            solve_cubic_spline(&zs, &ys, &ts, default_val, alloc, out, kind, o.name, arg)
        };

        let splines_res: Result<[(u32, u32); 5], CadError> = (|| {
            let sx = match o.side_rail {
                Some(r) => build_rail(r, "side_rail", true, 1.0 / ref_hx, 1.0, &mut self.spline_spans)?,
                None => build_station_spline(|s| s.scale[0], "station scale x", 1.0, &mut self.spline_spans)?,
            };
            let sy = match o.ridge_rail {
                Some(r) => build_rail(r, "ridge_rail", true, 1.0 / ref_hy, 1.0, &mut self.spline_spans)?,
                None => build_station_spline(|s| s.scale[1], "station scale y", 1.0, &mut self.spline_spans)?,
            };
            let cx = match o.center_x_rail {
                Some(r) => build_rail(r, "center_x_rail", false, 1.0, 0.0, &mut self.spline_spans)?,
                None => build_station_spline(|s| s.center[0].as_m(), "station center x", 0.0, &mut self.spline_spans)?,
            };
            let cy = match o.center_y_rail {
                Some(r) => build_rail(r, "center_y_rail", false, 1.0, 0.0, &mut self.spline_spans)?,
                None => build_station_spline(|s| s.center[1].as_m(), "station center y", 0.0, &mut self.spline_spans)?,
            };
            let tw = build_station_spline(|s| s.twist.as_radians(), "station twist", 0.0, &mut self.spline_spans)?;
            Ok([sx, sy, cx, cy, tw])
        })();

        let splines = match splines_res {
            Ok(sp) => sp,
            Err(e) => {
                self.spline_spans.truncate(spans_start);
                return Err(e);
            }
        };

        let z0 = m(z0_nm);
        let zn = m(zn_nm);
        let (sx_min, sx_max, sx_dmax) = spline_range_on(self.spline_slice(splines[0]), 1.0, z0, zn);
        let (sy_min, sy_max, sy_dmax) = spline_range_on(self.spline_slice(splines[1]), 1.0, z0, zn);
        let (cx_min, cx_max, cx_dmax) = spline_range_on(self.spline_slice(splines[2]), 0.0, z0, zn);
        let (cy_min, cy_max, cy_dmax) = spline_range_on(self.spline_slice(splines[3]), 0.0, z0, zn);
        let (_, _, tw_dmax) = spline_range_on(self.spline_slice(splines[4]), 0.0, z0, zn);

        if !(sx_min > 1e-9 && sx_max.is_finite()) {
            self.spline_spans.truncate(spans_start);
            return Err(bad("side_rail", Value::Ratio(sx_min), "strictly positive X scale across the loft"));
        }
        if !(sy_min > 1e-9 && sy_max.is_finite()) {
            self.spline_spans.truncate(spans_start);
            return Err(bad("ridge_rail", Value::Ratio(sy_min), "strictly positive Y scale across the loft"));
        }

        // Tight conservative AABB across all station spans from exact cubic extrema.
        let mut aabb = Aabb3::EMPTY;
        let mut k_blend: f64 = 0.0;
        let mut r_prof_max: f64 = 0.0;
        let w_slope = o.continuity.max_slope();
        for s in st {
            let pb = self.profile_rec(s.profile.id).bounds;
            let px = pb[0].abs().max(pb[2].abs());
            let py = pb[1].abs().max(pb[3].abs());
            r_prof_max = r_prof_max.max(blaze_math::hypot(px, py));
        }
        for i in 0..st.len() - 1 {
            let za_nm = st[i].z.as_nm();
            let zb_nm = st[i + 1].z.as_nm();
            let (za, zb) = (m(za_nm), m(zb_nm));
            let pa = self.profile_rec(st[i].profile.id).bounds;
            let pb = self.profile_rec(st[i + 1].profile.id).bounds;
            let (px0, py0) = (pa[0].min(pb[0]), pa[1].min(pb[1]));
            let (px1, py1) = (pa[2].max(pb[2]), pa[3].max(pb[3]));
            let diam = blaze_math::hypot(px1 - px0, py1 - py0);
            k_blend = k_blend.max(w_slope * diam / m(zb_nm - za_nm));

            let (sxa, sxb, _) = spline_range_on(self.spline_slice(splines[0]), 1.0, za, zb);
            let (sya, syb, _) = spline_range_on(self.spline_slice(splines[1]), 1.0, za, zb);
            let (cxa, cxb, _) = spline_range_on(self.spline_slice(splines[2]), 0.0, za, zb);
            let (cya, cyb, _) = spline_range_on(self.spline_slice(splines[3]), 0.0, za, zb);
            let (ta, tb, _) = spline_range_on(self.spline_slice(splines[4]), 0.0, za, zb);

            let lx0 = (px0 * sxa).min(px0 * sxb);
            let lx1 = (px1 * sxa).max(px1 * sxb);
            let ly0 = (py0 * sya).min(py0 * syb);
            let ly1 = (py1 * sya).max(py1 * syb);
            let (rx0, ry0, rx1, ry1) = if ta == 0.0 && tb == 0.0 {
                (lx0, ly0, lx1, ly1)
            } else if (tb - ta).abs() <= 1e-12 {
                let (sn, cs) = blaze_math::sin_cos(ta);
                let mut xlo = f64::INFINITY;
                let mut ylo = f64::INFINITY;
                let mut xhi = f64::NEG_INFINITY;
                let mut yhi = f64::NEG_INFINITY;
                for x in [lx0, lx1] {
                    for y in [ly0, ly1] {
                        let rx = x * cs - y * sn;
                        let ry = x * sn + y * cs;
                        xlo = xlo.min(rx);
                        xhi = xhi.max(rx);
                        ylo = ylo.min(ry);
                        yhi = yhi.max(ry);
                    }
                }
                (xlo, ylo, xhi, yhi)
            } else {
                let mx = lx0.abs().max(lx1.abs());
                let my = ly0.abs().max(ly1.abs());
                let rho = blaze_math::hypot(mx, my);
                (-rho, -rho, rho, rho)
            };
            let span_box = Aabb3::new(Vec3::new(rx0 + cxa, ry0 + cya, za), Vec3::new(rx1 + cxb, ry1 + cyb, zb));
            aabb = aabb.union(&span_box);
        }

        // Sound analytical Lipschitz bound on `cg = aabb.grow(margin_of(&aabb))`.
        let guarded = sx_dmax > 0.0 || sy_dmax > 0.0 || cx_dmax > 0.0 || cy_dmax > 0.0 || tw_dmax > 0.0;
        let m_min = sx_min.min(sy_min);
        let m_max = sx_max.min(sy_max);
        let m_dmax = sx_dmax.max(sy_dmax);
        let c_dmax = blaze_math::hypot(cx_dmax, cy_dmax);
        let cg = aabb.grow(margin_of(&aabb));
        let dx_max = (cg.lo.x - cx_max).abs().max((cg.hi.x - cx_min).abs());
        let dy_max = (cg.lo.y - cy_max).abs().max((cg.hi.y - cy_min).abs());
        let r_max = blaze_math::hypot(dx_max, dy_max);
        let d_max = r_max / m_min + r_prof_max;
        let scale_rel = (sx_dmax / sx_min).max(sy_dmax / sy_min);
        let k_z = m_dmax * d_max + c_dmax + (tw_dmax + scale_rel) * r_max + m_max * k_blend;
        let lip = blaze_math::sqrt(1.0 + k_z * k_z);
        if !(lip.is_finite() && lip < 1e12) {
            self.spline_spans.truncate(spans_start);
            return Err(bad("stations", Value::Ratio(lip), "a finite Lipschitz bound"));
        }

        let s_min_pp12 = [
            (core::f64::math::floor(sx_min * 1e12) as i64).max(1),
            (core::f64::math::floor(sy_min * 1e12) as i64).max(1),
        ];

        let stations_start = self.stations.len() as u32;
        for s in st {
            self.stations.push((s.z.as_nm(), s.profile.id));
        }
        let idx = self.guided_lofts.len() as u32;
        self.guided_lofts.push(GuidedLoftRec {
            stations_start,
            stations_len: st.len() as u32,
            splines,
            s_min_pp12,
            continuity: o.continuity,
            cap_round: cap_nm,
            lipschitz_ppm: ppm_up(lip),
            guarded,
        });
        let before = self.nodes.len();
        let id = self.intern(Node::GuidedLoft { idx }, Level::Bound, aabb, o.name)?;
        if self.nodes.len() == before {
            self.stations.truncate(stations_start as usize);
            self.spline_spans.truncate(spans_start);
            self.guided_lofts.truncate(idx as usize);
        }
        Ok(Solid::new(id))
    }

    // ---- selective, variable-radius blends & offsets --------------------------------------

    fn build_var_field(
        &mut self,
        kind: NodeKind,
        name: Option<&str>,
        profile: RadiusProfile<'_>,
        select: EdgeSelect,
    ) -> Result<(u32, i64, usize), CadError> {
        let bad = |arg: &'static str, value: Value, accepted: &'static str| CadError::Argument {
            kind,
            name: name.map(Name::new),
            arg,
            value,
            accepted,
        };
        let stops_start = self.radius_stops.len();
        let (canon_prof, r_max_nm, l_prof) = match profile {
            RadiusProfile::Constant(r) => {
                let r_nm = r.as_nm();
                if r_nm < 0 {
                    return Err(bad("profile radius", Value::Length(r), "zero or more"));
                }
                (CanonProfile::Constant { r_nm }, r_nm, 0.0)
            }
            RadiusProfile::Linear { from, to, r0, r1 } => {
                let (r0_nm, r1_nm) = (r0.as_nm(), r1.as_nm());
                if r0_nm < 0 {
                    return Err(bad("r0", Value::Length(r0), "zero or more"));
                }
                if r1_nm < 0 {
                    return Err(bad("r1", Value::Length(r1), "zero or more"));
                }
                let from_nm = [from[0].as_nm(), from[1].as_nm(), from[2].as_nm()];
                let to_nm = [to[0].as_nm(), to[1].as_nm(), to[2].as_nm()];
                let dx = m(to_nm[0] - from_nm[0]);
                let dy = m(to_nm[1] - from_nm[1]);
                let dz = m(to_nm[2] - from_nm[2]);
                let len = blaze_math::sqrt(dx * dx + dy * dy + dz * dz);
                if !(len > 0.0 && len.is_finite()) {
                    return Err(bad("to", Value::Length(Length::ZERO), "`from` and `to` must differ"));
                }
                let l_p = (m(r1_nm) - m(r0_nm)).abs() / len;
                (CanonProfile::Linear { from_nm, to_nm, r0_nm, r1_nm }, r0_nm.max(r1_nm), l_p)
            }
            RadiusProfile::Curve { origin, axis, stops } => {
                if !axis.iter().all(|v| v.is_finite()) {
                    return Err(bad("axis", Value::Ratio(axis[0]), "a finite non-zero axis"));
                }
                let alen = blaze_math::sqrt(axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]);
                if !(alen > 0.0 && alen.is_finite()) {
                    return Err(bad("axis", Value::Ratio(alen), "a non-zero axis"));
                }
                let axis_pp12 = [
                    core::f64::math::round((axis[0] / alen) * 1e12) as i64,
                    core::f64::math::round((axis[1] / alen) * 1e12) as i64,
                    core::f64::math::round((axis[2] / alen) * 1e12) as i64,
                ];
                if stops.len() < 2 || stops.len() as u64 > u64::from(self.limits.most_stations) {
                    return Err(bad("stops", Value::Count(stops.len() as u64), "2 to Limits::most_stations"));
                }
                let mut r_max_nm = 0i64;
                let mut l_p = 0.0f64;
                let origin_nm = [origin[0].as_nm(), origin[1].as_nm(), origin[2].as_nm()];
                for (i, s) in stops.iter().enumerate() {
                    let at_nm = s.at.as_nm();
                    let r_nm = s.radius.as_nm();
                    if r_nm < 0 {
                        self.radius_stops.truncate(stops_start);
                        return Err(bad("stop radius", Value::Length(s.radius), "zero or more"));
                    }
                    if i > 0 {
                        let prev = stops[i - 1];
                        let gap = at_nm - prev.at.as_nm();
                        if gap <= 0 {
                            self.radius_stops.truncate(stops_start);
                            return Err(bad("stop position", Value::Length(s.at), "strictly increasing stop positions"));
                        }
                        l_p = l_p.max(1.5 * (m(r_nm) - m(prev.radius.as_nm())).abs() / m(gap));
                    }
                    r_max_nm = r_max_nm.max(r_nm);
                    self.radius_stops.push((at_nm, r_nm));
                }
                (
                    CanonProfile::Curve { origin_nm, axis_pp12, start: stops_start as u32, len: stops.len() as u32 },
                    r_max_nm,
                    l_p,
                )
            }
        };

        const MIN_FADE_NM: i64 = 1_000;
        let check_fade = |fade: Length| -> Result<i64, CadError> {
            let f = fade.as_nm();
            if f < MIN_FADE_NM {
                Err(bad("fade", Value::Length(fade), "at least 1000 nm (1 micrometre)"))
            } else {
                Ok(f)
            }
        };

        let select_res: Result<(CanonSelect, f64), CadError> = match select {
            EdgeSelect::All => Ok((CanonSelect::All, 0.0)),
            EdgeSelect::Box { lo, hi, fade } => {
                let lo_nm = [lo[0].as_nm(), lo[1].as_nm(), lo[2].as_nm()];
                let hi_nm = [hi[0].as_nm(), hi[1].as_nm(), hi[2].as_nm()];
                for i in 0..3 {
                    if lo_nm[i] > hi_nm[i] {
                        self.radius_stops.truncate(stops_start);
                        return Err(bad("select box", Value::Length(lo[i]), "lo <= hi on every axis"));
                    }
                }
                match check_fade(fade) {
                    Ok(fade_nm) => Ok((CanonSelect::Box { lo_nm, hi_nm, fade_nm }, 1.5 / m(fade_nm))),
                    Err(e) => Err(e),
                }
            }
            EdgeSelect::Ball { center, radius, fade } => match Self::positive(kind, name, "select radius", radius) {
                Ok(radius_nm) => match check_fade(fade) {
                    Ok(fade_nm) => {
                        let center_nm = [center[0].as_nm(), center[1].as_nm(), center[2].as_nm()];
                        Ok((CanonSelect::Ball { center_nm, radius_nm, fade_nm }, 1.5 / m(fade_nm)))
                    }
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            },
            EdgeSelect::Capsule { a, b, radius, fade } => {
                let radius_nm = radius.as_nm();
                if radius_nm < 0 {
                    self.radius_stops.truncate(stops_start);
                    return Err(bad("select radius", Value::Length(radius), "zero or more"));
                }
                match check_fade(fade) {
                    Ok(fade_nm) => {
                        let a_nm = [a[0].as_nm(), a[1].as_nm(), a[2].as_nm()];
                        let b_nm = [b[0].as_nm(), b[1].as_nm(), b[2].as_nm()];
                        Ok((CanonSelect::Capsule { a_nm, b_nm, radius_nm, fade_nm }, 1.5 / m(fade_nm)))
                    }
                    Err(e) => Err(e),
                }
            }
        };

        let (canon_sel, l_mask) = match select_res {
            Ok(v) => v,
            Err(e) => {
                self.radius_stops.truncate(stops_start);
                return Err(e);
            }
        };

        let l_r = l_prof + m(r_max_nm) * l_mask;
        if !(l_r.is_finite() && l_r < 1e12) {
            self.radius_stops.truncate(stops_start);
            return Err(bad("profile", Value::Ratio(l_r), "a finite radius gradient"));
        }
        let idx = self.var_fields.len() as u32;
        self.var_fields.push(VarFieldRec {
            profile: canon_prof,
            select: canon_sel,
            r_max_nm,
            l_r_ppm: ppm_up(l_r),
        });
        Ok((idx, r_max_nm, stops_start))
    }

    fn variable_boolean<A: Ex, B: Ex>(
        &mut self,
        op: Boolean,
        chamfer: bool,
        a: Solid<A>,
        b: Solid<B>,
        o: VariableBlendOpts<'_>,
    ) -> Result<Solid<Bound>, CadError> {
        self.check(a.id)?;
        self.check(b.id)?;
        let kind = if chamfer { NodeKind::VariableChamferBoolean } else { NodeKind::VariableSmoothBoolean };
        let (field, r_max_nm, stops_start) = self.build_var_field(kind, o.name, o.profile, o.select)?;
        let (mut a_id, mut b_id) = (a.id, b.id);
        let (ea, eb) = (matches!(self.rec(a_id).node, Node::Empty), matches!(self.rec(b_id).node, Node::Empty));
        let trivial = match op {
            Boolean::Union if ea => Some(b_id),
            Boolean::Union if eb => Some(a_id),
            Boolean::Intersect if ea || eb => Some(if ea { a_id } else { b_id }),
            Boolean::Difference if ea || eb => Some(a_id),
            _ => None,
        };
        if let Some(id) = trivial {
            self.var_fields.truncate(field as usize);
            self.radius_stops.truncate(stops_start);
            return Ok(Solid::new(id));
        }
        if op.commutes() && !self.plant.has(CadPlant::OperandsUnordered) && self.rec(b_id).full < self.rec(a_id).full {
            core::mem::swap(&mut a_id, &mut b_id);
        }
        let (ra, rb) = (*self.rec(a_id), *self.rec(b_id));
        let grow = if chamfer { m(r_max_nm) } else { m(r_max_nm) * 0.25 };
        let aabb = match op {
            Boolean::Union => ra.aabb.union(&rb.aabb).grow(grow),
            Boolean::Intersect => ra.aabb.intersect(&rb.aabb),
            Boolean::Difference => ra.aabb,
        };
        let before = self.nodes.len();
        let id = self.intern(Node::VariableBoolean { op, chamfer, a: a_id, b: b_id, field }, Level::Bound, aabb, o.name)?;
        if self.nodes.len() == before {
            self.var_fields.truncate(field as usize);
            self.radius_stops.truncate(stops_start);
        }
        Ok(Solid::new(id))
    }

    /// A union with a selective, spatially varying rounded fillet.
    pub fn variable_round_union<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: VariableBlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.variable_boolean(Boolean::Union, false, a, b, o)
    }

    /// An intersection with a selective, spatially varying rounded edge.
    pub fn variable_round_intersect<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: VariableBlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.variable_boolean(Boolean::Intersect, false, a, b, o)
    }

    /// A difference with a selective, spatially varying rounded edge.
    pub fn variable_round_difference<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: VariableBlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.variable_boolean(Boolean::Difference, false, a, b, o)
    }

    /// A union with a selective, spatially varying chamfer.
    pub fn variable_chamfer_union<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: VariableBlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.variable_boolean(Boolean::Union, true, a, b, o)
    }

    /// An intersection with a selective, spatially varying chamfer.
    pub fn variable_chamfer_intersect<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: VariableBlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.variable_boolean(Boolean::Intersect, true, a, b, o)
    }

    /// A difference with a selective, spatially varying chamfer.
    pub fn variable_chamfer_difference<A: Ex, B: Ex>(&mut self, a: Solid<A>, b: Solid<B>, o: VariableBlendOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.variable_boolean(Boolean::Difference, true, a, b, o)
    }

    fn variable_offset<K: Ex>(&mut self, s: Solid<K>, erode: bool, o: VariableOffsetOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.check(s.id)?;
        let kind = if erode { NodeKind::VariableErode } else { NodeKind::VariableGrow };
        let (field, r_max_nm, stops_start) = self.build_var_field(kind, o.name, o.profile, o.select)?;
        if matches!(self.rec(s.id).node, Node::Empty) {
            self.var_fields.truncate(field as usize);
            self.radius_stops.truncate(stops_start);
            return Ok(Solid::new(s.id));
        }
        let r = *self.rec(s.id);
        let aabb = if erode { r.aabb } else { r.aabb.grow(m(r_max_nm)) };
        let before = self.nodes.len();
        let id = self.intern(Node::VariableOffset { inner: s.id, erode, field }, Level::Bound, aabb, o.name)?;
        if self.nodes.len() == before {
            self.var_fields.truncate(field as usize);
            self.radius_stops.truncate(stops_start);
        }
        Ok(Solid::new(id))
    }

    /// Selective, spatially varying inward offset: `(inner(p) + r(p)) / (1 + L_r)` where
    /// `r(p) = profile(p) * mask(p)`.
    pub fn variable_erode<K: Ex>(&mut self, s: Solid<K>, o: VariableOffsetOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.variable_offset(s, true, o)
    }

    /// Selective, spatially varying outward offset: `(inner(p) - r(p)) / (1 + L_r)` where
    /// `r(p) = profile(p) * mask(p)`.
    pub fn variable_grow<K: Ex>(&mut self, s: Solid<K>, o: VariableOffsetOpts<'_>) -> Result<Solid<Bound>, CadError> {
        self.variable_offset(s, false, o)
    }

    // ---- surface curve frame sweep --------------------------------------------------------

    fn sweep_section_profile(&mut self, section: SweepCrossSection, w_nm: i64, d_nm: i64) -> Result<Prof<Exact>, CadError> {
        match section {
            SweepCrossSection::Circular => {
                if w_nm == d_nm {
                    self.profile(ProfileOpts { profile: Profile::Circle { radius: Length::nm((w_nm / 2).max(1)) }, name: None })
                } else {
                    self.profile(ProfileOpts {
                        profile: Profile::RoundedRect {
                            width: Length::nm(w_nm),
                            height: Length::nm(d_nm),
                            radius: Length::nm(w_nm.min(d_nm) / 2),
                        },
                        name: None,
                    })
                }
            }
            SweepCrossSection::Channel => {
                let r = w_nm.min(d_nm) / 4;
                self.profile(ProfileOpts {
                    profile: Profile::RoundedRect { width: Length::nm(w_nm), height: Length::nm(d_nm), radius: Length::nm(r) },
                    name: None,
                })
            }
            SweepCrossSection::Rectangular { round } => {
                let r = round.as_nm();
                if r == 0 {
                    self.profile(ProfileOpts {
                        profile: Profile::Rect { width: Length::nm(w_nm), height: Length::nm(d_nm) },
                        name: None,
                    })
                } else {
                    self.profile(ProfileOpts {
                        profile: Profile::RoundedRect { width: Length::nm(w_nm), height: Length::nm(d_nm), radius: round },
                        name: None,
                    })
                }
            }
            SweepCrossSection::VGroove => {
                let hw = w_nm / 2;
                let hd = d_nm / 2;
                let pts: [Point2; 3] = [
                    [Length::ZERO, Length::nm(-hd)],
                    [Length::nm(hw), Length::nm(hd)],
                    [Length::nm(-hw), Length::nm(hd)],
                ];
                self.profile(ProfileOpts { profile: Profile::Polygon { points: &pts }, name: None })
            }
            SweepCrossSection::HalfRound => {
                let hw = 0.5 * m(w_nm);
                let dm = m(d_nm);
                let mut pts: [Point2; 13] = [[Length::ZERO; 2]; 13];
                for (k, slot) in pts.iter_mut().enumerate() {
                    let th = core::f64::consts::PI * (k as f64) / 12.0;
                    let (s, c) = blaze_math::sin_cos(th);
                    *slot = [
                        Length::nm(core::f64::math::round(hw * c * 1e9) as i64),
                        Length::nm(core::f64::math::round((-0.5 * dm + dm * s) * 1e9) as i64),
                    ];
                }
                self.profile(ProfileOpts { profile: Profile::Polygon { points: &pts }, name: None })
            }
        }
    }

    /// Sweeps a variable-size cross-section along an oriented 3-D surface curve (`frames`),
    /// orienting each segment along `(binormal = normal x tangent, normal, tangent)`, linearly
    /// interpolating `width` and `depth` between consecutive frames, and joining segments in a
    /// balanced union (or smooth union when `smooth_join > 0`).
    pub fn sweep_frames(&mut self, o: SweepFramesOpts<'_>) -> Result<Solid<Bound>, CadError> {
        let kind = NodeKind::SweepFrames;
        let frames = o.frames;
        let bad = |arg: &'static str, value: Value, accepted: &'static str| CadError::Argument {
            kind,
            name: o.name.map(Name::new),
            arg,
            value,
            accepted,
        };
        let min_frames = if o.closed { 3usize } else { 2usize };
        if frames.len() < min_frames || frames.len() as u64 > u64::from(self.limits.most_copies) {
            return Err(bad(
                "frames",
                Value::Count(frames.len() as u64),
                if o.closed { "3 to Limits::most_copies for a closed sweep" } else { "2 to Limits::most_copies" },
            ));
        }
        let join_nm = o.smooth_join.as_nm();
        if join_nm < 0 {
            return Err(bad("smooth_join", Value::Length(o.smooth_join), "zero or more"));
        }
        if let SweepCrossSection::Rectangular { round } = o.section
            && round.as_nm() < 0
        {
            return Err(bad("round", Value::Length(round), "zero or more"));
        }
        for (i, fr) in frames.iter().enumerate() {
            let w_nm = Self::positive(kind, o.name, "width", fr.width)?;
            let d_nm = Self::positive(kind, o.name, "depth", fr.depth)?;
            if let SweepCrossSection::Rectangular { round } = o.section
                && 2 * round.as_nm() > w_nm.min(d_nm)
            {
                return Err(bad("round", Value::Length(round), "at most half of min(width, depth) at every frame"));
            }
            if !fr.tangent.iter().all(|v| v.is_finite()) {
                return Err(bad("tangent", Value::Count(i as u64), "a finite non-zero tangent"));
            }
            if !fr.normal.iter().all(|v| v.is_finite()) {
                return Err(bad("normal", Value::Count(i as u64), "a finite non-zero normal"));
            }
            let tv = Vec3::from_array(fr.tangent);
            let nv = Vec3::from_array(fr.normal);
            let Some(tn) = tv.normalized() else {
                return Err(bad("tangent", Value::Count(i as u64), "a non-zero tangent"));
            };
            let Some(nn) = nv.normalized() else {
                return Err(bad("normal", Value::Count(i as u64), "a non-zero normal"));
            };
            if nn.cross(tn).length() <= 1e-9 {
                return Err(bad("normal", Value::Count(i as u64), "normal not parallel to tangent"));
            }
        }

        let n = frames.len();
        let seg_count = if o.closed { n } else { n - 1 };
        let mut layer: AVec<'a, NodeId> = Vec::with_capacity_in(seg_count, self.alloc);
        let pt_v = |p: [Length; 3]| Vec3::new(p[0].as_m(), p[1].as_m(), p[2].as_m());

        for i in 0..seg_count {
            let j = (i + 1) % n;
            let (f0, f1) = (frames[i], frames[j]);
            let (a, b) = (pt_v(f0.point), pt_v(f1.point));
            let d = b - a;
            let seg_len = d.length();
            let Some(z_axis) = d.normalized() else {
                return Err(bad("frames", Value::Count(i as u64), "consecutive frame points apart"));
            };
            let l_nm = core::f64::math::round(seg_len * 1e9) as i64;
            if l_nm <= 0 {
                return Err(bad("frames", Value::Count(i as u64), "consecutive frame points at least 1 nm apart"));
            }
            let n0 = Vec3::from_array(f0.normal).normalized().unwrap_or(Vec3::new(0.0, 1.0, 0.0));
            let n1 = Vec3::from_array(f1.normal).normalized().unwrap_or(n0);
            let n_avg = (n0 + n1).normalized().unwrap_or(n0);
            let y_raw = n_avg - z_axis * n_avg.dot(z_axis);
            let y_axis = y_raw
                .normalized()
                .or_else(|| (n0 - z_axis * n0.dot(z_axis)).normalized())
                .or_else(|| {
                    let up = if z_axis.z.abs() > 0.9 { Vec3::new(1.0, 0.0, 0.0) } else { Vec3::new(0.0, 0.0, 1.0) };
                    (up - z_axis * up.dot(z_axis)).normalized()
                })
                .unwrap_or(Vec3::new(0.0, 1.0, 0.0));
            let x_axis = y_axis.cross(z_axis).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));

            let (w0, d0) = (f0.width.as_nm(), f0.depth.as_nm());
            let (w1, d1) = (f1.width.as_nm(), f1.depth.as_nm());
            let p0 = self.sweep_section_profile(o.section, w0, d0)?;
            let p1 = self.sweep_section_profile(o.section, w1, d1)?;

            let ext0_nm = if o.closed || i > 0 { core::f64::math::round(0.15 * m(w0.min(d0)) * 1e9) as i64 } else { 0 };
            let ext1_nm = if o.closed || i + 1 < seg_count { core::f64::math::round(0.15 * m(w1.min(d1)) * 1e9) as i64 } else { 0 };

            let seg = if p0.id() == p1.id() {
                let total_nm = (l_nm + ext0_nm + ext1_nm).max(2);
                let piece = self.extrude(p0, ExtrudeOpts { height: Length::nm(total_nm), name: None })?;
                let center = a + z_axis * (0.5 * m(l_nm + ext1_nm - ext0_nm));
                let frame = Placement::from_axes(x_axis, y_axis, z_axis, center);
                self.wrap(piece.id, frame, None)?
            } else {
                let mut st_buf = [Station { z: Length::ZERO, profile: p0 }; 4];
                let mut k = 0usize;
                if ext0_nm > 0 {
                    st_buf[k] = Station { z: Length::nm(-ext0_nm), profile: p0 };
                    k += 1;
                }
                st_buf[k] = Station { z: Length::ZERO, profile: p0 };
                k += 1;
                st_buf[k] = Station { z: Length::nm(l_nm), profile: p1 };
                k += 1;
                if ext1_nm > 0 {
                    st_buf[k] = Station { z: Length::nm(l_nm + ext1_nm), profile: p1 };
                    k += 1;
                }
                let piece = self.loft(LoftOpts { stations: &st_buf[..k], name: None })?;
                let frame = Placement::from_axes(x_axis, y_axis, z_axis, a);
                self.wrap(piece.id, frame, None)?
            };
            layer.push(seg);
        }

        let edge = if join_nm > 0 { Edge::Round { r: join_nm } } else { Edge::Sharp };
        let id = self.balanced_boolean(Boolean::Union, edge, layer, o.name)?;
        Ok(Solid::new(id))
    }
}

// ---- cubic spline solver & exact cubic range analysis -----------------------------------------

fn quant_pp12(v: f64, kind: NodeKind, name: Option<&str>, arg: &'static str) -> Result<i64, CadError> {
    if !(v.is_finite() && v.abs() < 4e6) {
        return Err(CadError::Argument {
            kind,
            name: name.map(Name::new),
            arg,
            value: Value::Ratio(v),
            accepted: "a finite value within +-4e6",
        });
    }
    Ok(core::f64::math::round(v * 1e12) as i64)
}

/// Solves a C2 natural / clamped / mixed-tangent cubic spline through `(zs_nm[i], ys[i])` with
/// optional slopes `tangents[i] = dy/dz` using the exact O(n) Thomas tridiagonal solver,
/// quantizes each span to `10^-12` integer coefficients with exact C0 endpoint continuity, and
/// appends spans to `out`. Returns `(start, 0)` when the curve is identically `default_val`.
#[allow(clippy::too_many_arguments)]
fn solve_cubic_spline<'a>(
    zs_nm: &[i64],
    ys: &[f64],
    tangents: &[Option<f64>],
    default_val: f64,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, SplineSpan>,
    kind: NodeKind,
    name: Option<&str>,
    arg: &'static str,
) -> Result<(u32, u32), CadError> {
    let n = zs_nm.len();
    let start = out.len() as u32;
    let def_q = quant_pp12(default_val, kind, name, arg)?;
    let y0_q = quant_pp12(ys[0], kind, name, arg)?;
    let mut all_const = true;
    for (&y, &t) in ys.iter().zip(tangents.iter()) {
        let yq = quant_pp12(y, kind, name, arg)?;
        if yq != y0_q {
            all_const = false;
            break;
        }
        if let Some(tg) = t
            && quant_pp12(tg, kind, name, arg)? != 0
        {
            all_const = false;
            break;
        }
    }
    if all_const {
        if y0_q == def_q {
            return Ok((start, 0));
        }
        out.push(SplineSpan { z0_nm: zs_nm[0], z1_nm: zs_nm[n - 1], c_pp12: [y0_q, 0, 0, 0] });
        return Ok((start, 1));
    }

    let mut a: AVec<'a, f64> = Vec::with_capacity_in(n, alloc);
    let mut b: AVec<'a, f64> = Vec::with_capacity_in(n, alloc);
    let mut c: AVec<'a, f64> = Vec::with_capacity_in(n, alloc);
    let mut rhs: AVec<'a, f64> = Vec::with_capacity_in(n, alloc);
    let mut d: AVec<'a, f64> = Vec::with_capacity_in(n, alloc);
    for i in 0..n {
        if let Some(tg) = tangents[i] {
            a.push(0.0);
            b.push(1.0);
            c.push(0.0);
            rhs.push(tg);
        } else if i == 0 {
            let h0 = m(zs_nm[1] - zs_nm[0]);
            a.push(0.0);
            b.push(2.0);
            c.push(1.0);
            rhs.push(3.0 * (ys[1] - ys[0]) / h0);
        } else if i == n - 1 {
            let hl = m(zs_nm[n - 1] - zs_nm[n - 2]);
            a.push(1.0);
            b.push(2.0);
            c.push(0.0);
            rhs.push(3.0 * (ys[n - 1] - ys[n - 2]) / hl);
        } else {
            let h0 = m(zs_nm[i] - zs_nm[i - 1]);
            let h1 = m(zs_nm[i + 1] - zs_nm[i]);
            let inv0 = 1.0 / h0;
            let inv1 = 1.0 / h1;
            a.push(inv0);
            b.push(2.0 * (inv0 + inv1));
            c.push(inv1);
            rhs.push(3.0 * ((ys[i] - ys[i - 1]) * inv0 * inv0 + (ys[i + 1] - ys[i]) * inv1 * inv1));
        }
        d.push(0.0);
    }

    // Thomas tridiagonal forward elimination & back substitution (strictly diagonally dominant).
    for i in 1..n {
        let w = a[i] / b[i - 1];
        b[i] -= w * c[i - 1];
        rhs[i] -= w * rhs[i - 1];
    }
    d[n - 1] = rhs[n - 1] / b[n - 1];
    for i in (0..n - 1).rev() {
        d[i] = (rhs[i] - c[i] * d[i + 1]) / b[i];
    }

    let overflow = || CadError::Argument {
        kind,
        name: name.map(Name::new),
        arg,
        value: Value::Ratio(f64::INFINITY),
        accepted: "spline coefficients within +-9e6",
    };

    for i in 0..n - 1 {
        let h = m(zs_nm[i + 1] - zs_nm[i]);
        let y0 = quant_pp12(ys[i], kind, name, arg)?;
        let y1 = quant_pp12(ys[i + 1], kind, name, arg)?;
        let m0 = quant_pp12(h * d[i], kind, name, arg)?;
        let m1 = quant_pp12(h * d[i + 1], kind, name, arg)?;
        let dy = i128::from(y1) - i128::from(y0);
        let c2_128 = 3 * dy - 2 * i128::from(m0) - i128::from(m1);
        let c3_128 = -2 * dy + i128::from(m0) + i128::from(m1);
        let c2 = i64::try_from(c2_128).map_err(|_| overflow())?;
        let c3 = i64::try_from(c3_128).map_err(|_| overflow())?;
        out.push(SplineSpan { z0_nm: zs_nm[i], z1_nm: zs_nm[i + 1], c_pp12: [y0, m0, c2, c3] });
    }
    Ok((start, (n - 1) as u32))
}

fn span_coeffs(s: &SplineSpan) -> [f64; 4] {
    [
        s.c_pp12[0] as f64 / 1e12,
        s.c_pp12[1] as f64 / 1e12,
        s.c_pp12[2] as f64 / 1e12,
        s.c_pp12[3] as f64 / 1e12,
    ]
}

fn eval_cubic(c: &[f64; 4], u: f64) -> f64 {
    c[0] + u * (c[1] + u * (c[2] + u * c[3]))
}

fn eval_cubic_du(c: &[f64; 4], u: f64) -> f64 {
    c[1] + u * (2.0 * c[2] + 3.0 * c[3] * u)
}

/// Exact `(min_val, max_val, max_abs_deriv)` of a piecewise cubic spline over `[lo_z, hi_z]` (metres).
fn spline_range_on(spans: &[SplineSpan], default_val: f64, lo_z: f64, hi_z: f64) -> (f64, f64, f64) {
    if spans.is_empty() {
        return (default_val, default_val, 0.0);
    }
    let mut vmin = f64::INFINITY;
    let mut vmax = f64::NEG_INFINITY;
    let mut dmax: f64 = 0.0;
    let mut touch = |v: f64| {
        vmin = vmin.min(v);
        vmax = vmax.max(v);
    };
    let rz0 = m(spans[0].z0_nm);
    let rzn = m(spans[spans.len() - 1].z1_nm);
    if lo_z <= rz0 {
        let c = span_coeffs(&spans[0]);
        touch(eval_cubic(&c, 0.0));
    }
    if hi_z >= rzn {
        let c = span_coeffs(&spans[spans.len() - 1]);
        touch(eval_cubic(&c, 1.0));
    }
    for sp in spans {
        let za = m(sp.z0_nm);
        let zb = m(sp.z1_nm);
        if zb < lo_z || za > hi_z {
            continue;
        }
        let h = zb - za;
        let u0 = ((lo_z.max(za) - za) / h).clamp(0.0, 1.0);
        let u1 = ((hi_z.min(zb) - za) / h).clamp(0.0, 1.0);
        let c = span_coeffs(sp);
        touch(eval_cubic(&c, u0));
        touch(eval_cubic(&c, u1));
        if c[3].abs() > 1e-18 {
            let disc = c[2] * c[2] - 3.0 * c[1] * c[3];
            if disc >= 0.0 {
                let sd = blaze_math::sqrt(disc);
                let denom = 3.0 * c[3];
                for r in [(-c[2] - sd) / denom, (-c[2] + sd) / denom] {
                    if r > u0 && r < u1 {
                        touch(eval_cubic(&c, r));
                    }
                }
            }
        } else if c[2].abs() > 1e-18 {
            let r = -c[1] / (2.0 * c[2]);
            if r > u0 && r < u1 {
                touch(eval_cubic(&c, r));
            }
        }
        let inv_h = 1.0 / h;
        dmax = dmax.max(eval_cubic_du(&c, u0).abs() * inv_h);
        dmax = dmax.max(eval_cubic_du(&c, u1).abs() * inv_h);
        if c[3].abs() > 1e-18 {
            let u_star = -c[2] / (3.0 * c[3]);
            if u_star > u0 && u_star < u1 {
                dmax = dmax.max(eval_cubic_du(&c, u_star).abs() * inv_h);
            }
        }
    }
    (vmin, vmax, dmax)
}
