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
use crate::node::{Boolean, Detail, Digest, Edge, Node, NodeId, ProfileId, Shape};
use crate::placement::{ANGLE_UNIT, Axis, Placement};
use crate::plant::CadPlant;
use crate::profile::{self, Closed, Profile, SEG_WIDTH};

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
        }
        (f.done(), c.done())
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
        let mut acc: Option<NodeId> = None;
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
            acc = Some(match acc {
                None => seg,
                Some(u) => self.boolean(Boolean::Union, Edge::Sharp, u, seg, None)?,
            });
        }
        let id = acc.unwrap_or(self.empty()?.id);
        if let Some(n) = o.name
            && self.names.get(&id).is_none()
        {
            self.names.insert(id, Name::new(n));
        }
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
        while layer.len() > 1 {
            let mut next: AVec<'a, NodeId> = Vec::with_capacity_in(layer.len().div_ceil(2), self.alloc);
            for pair in layer.chunks(2) {
                next.push(if pair.len() == 2 { self.boolean(Boolean::Union, Edge::Sharp, pair[0], pair[1], None)? } else { pair[0] });
            }
            layer = next;
        }
        let id = layer[0];
        if let Some(n) = o.name
            && self.names.get(&id).is_none()
        {
            self.names.insert(id, Name::new(n));
        }
        Ok(Solid::new(id))
    }

    // ---- booleans -------------------------------------------------------------------------

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
}
