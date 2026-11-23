//! Spatial feature regions and fidelity zones shared by the mesher ([`crate::contour`]),
//! simplifier ([`crate::simplify`]), fidelity checker, and export preview renderer.
//!
//! Marking a keyhole, bevel edge, or mating surface with a [`FeatureRegion`] lets every stage
//! focus precision, geometry preservation, tolerance checking, and preview highlighting right on
//! that feature without blowing up triangle counts or simplification time across the rest of the
//! part.

#![blaze::sans_io]

use blaze_math::{Aabb3, Vec3};
use blaze_units::Length;

/// Exact 3D spatial region identifying a critical geometric feature (keyhole, bevel, mating
/// surface, boss, or slot) in a part's local coordinate frame.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FeatureRegion {
    /// Axis-aligned bounding box (for example a carved keyhole solid's bounds or a mating pad).
    Box(Aabb3),
    /// Sphere centered at `center` with `radius` (ideal for bolt holes, pins, or keyhole bores).
    Ball { center: Vec3, radius: Length },
    /// Swept capsule along segment `a..=b` with `radius` (ideal for bevel edges, keyhole slots,
    /// rabbets, and French-cleat mating seams).
    Capsule { a: Vec3, b: Vec3, radius: Length },
}

impl FeatureRegion {
    /// Axis-aligned bounding box enclosing this region.
    #[inline]
    pub fn aabb(&self) -> Aabb3 {
        match *self {
            FeatureRegion::Box(b) => b,
            FeatureRegion::Ball { center, radius } => {
                let r = radius.as_m().max(0.0);
                let ext = Vec3::new(r, r, r);
                Aabb3::new(center - ext, center + ext)
            }
            FeatureRegion::Capsule { a, b, radius } => {
                let r = radius.as_m().max(0.0);
                let ext = Vec3::new(r, r, r);
                Aabb3::new(a.min(b) - ext, a.max(b) + ext)
            }
        }
    }

    /// Expand this region outward by `margin` on every side.
    #[inline]
    pub fn pad(&self, margin: Length) -> FeatureRegion {
        let m = margin.as_m().max(0.0);
        match *self {
            FeatureRegion::Box(b) => {
                let ext = Vec3::new(m, m, m);
                FeatureRegion::Box(Aabb3::new(b.lo - ext, b.hi + ext))
            }
            FeatureRegion::Ball { center, radius } => {
                FeatureRegion::Ball { center, radius: Length::m((radius.as_m().max(0.0) + m).max(0.0)) }
            }
            FeatureRegion::Capsule { a, b, radius } => {
                FeatureRegion::Capsule { a, b, radius: Length::m((radius.as_m().max(0.0) + m).max(0.0)) }
            }
        }
    }

    /// Exact Euclidean signed distance (metres) from `p` to the region boundary: `<= 0.0` inside,
    /// `> 0.0` outside.
    #[inline]
    pub fn signed_distance(&self, p: Vec3) -> f64 {
        match *self {
            FeatureRegion::Box(b) => {
                let half = (b.hi - b.lo) * 0.5;
                let center = (b.lo + b.hi) * 0.5;
                let q = Vec3::new((p.x - center.x).abs() - half.x, (p.y - center.y).abs() - half.y, (p.z - center.z).abs() - half.z);
                let out = Vec3::new(q.x.max(0.0), q.y.max(0.0), q.z.max(0.0)).length();
                let inside = q.x.max(q.y).max(q.z).min(0.0);
                out + inside
            }
            FeatureRegion::Ball { center, radius } => (p - center).length() - radius.as_m().max(0.0),
            FeatureRegion::Capsule { a, b, radius } => {
                let ab = b - a;
                let denom = ab.dot(ab);
                let t = if denom > 1e-24 { ((p - a).dot(ab) / denom).clamp(0.0, 1.0) } else { 0.0 };
                let closest = a + ab * t;
                (p - closest).length() - radius.as_m().max(0.0)
            }
        }
    }

    /// True when `p` lies inside or on the boundary of this region.
    #[inline]
    pub fn contains(&self, p: Vec3) -> bool {
        self.signed_distance(p) <= 0.0
    }

    /// Conservative-exact intersection test against an axis-aligned box `box3`.
    #[inline]
    pub fn intersects_aabb(&self, box3: &Aabb3) -> bool {
        let mine = self.aabb();
        if mine.hi.x < box3.lo.x
            || mine.lo.x > box3.hi.x
            || mine.hi.y < box3.lo.y
            || mine.lo.y > box3.hi.y
            || mine.hi.z < box3.lo.z
            || mine.lo.z > box3.hi.z
        {
            return false;
        }
        match *self {
            FeatureRegion::Box(_) => true,
            FeatureRegion::Ball { center, radius } => {
                let r = radius.as_m().max(0.0);
                let clamped = clamp_point_to_aabb(center, box3);
                let d = center - clamped;
                d.dot(d) <= r * r
            }
            FeatureRegion::Capsule { a, b, radius } => {
                let r = radius.as_m().max(0.0);
                let r2 = r * r;
                // Check endpoints and closest point along the segment to the box's clamped points.
                let ca = clamp_point_to_aabb(a, box3);
                if (a - ca).dot(a - ca) <= r2 {
                    return true;
                }
                let cb = clamp_point_to_aabb(b, box3);
                if (b - cb).dot(b - cb) <= r2 {
                    return true;
                }
                // Alternating projection (4 steps converges to exact segment-box distance).
                let ab = b - a;
                let denom = ab.dot(ab);
                if !(denom > 1e-24) {
                    return false;
                }
                let mut q = clamp_point_to_aabb((a + b) * 0.5, box3);
                for _ in 0..4 {
                    let t = ((q - a).dot(ab) / denom).clamp(0.0, 1.0);
                    let seg_p = a + ab * t;
                    let box_p = clamp_point_to_aabb(seg_p, box3);
                    let d = seg_p - box_p;
                    if d.dot(d) <= r2 {
                        return true;
                    }
                    q = box_p;
                }
                // Also test slab intersection of segment with the radius-expanded AABB so diagonal
                // segments clipping a corner of the box are never missed.
                let expanded = Aabb3::new(box3.lo - Vec3::new(r, r, r), box3.hi + Vec3::new(r, r, r));
                segment_hits_aabb(a, b, &expanded)
            }
        }
    }

    /// True when triangle `(a, b, c)` overlaps or touches this region.
    #[inline]
    pub fn intersects_triangle(&self, a: Vec3, b: Vec3, c: Vec3) -> bool {
        let tb = Aabb3::new(a.min(b).min(c), a.max(b).max(c));
        if !self.intersects_aabb(&tb) {
            return false;
        }
        let centroid = (a + b + c) * (1.0 / 3.0);
        if self.contains(a) || self.contains(b) || self.contains(c) || self.contains(centroid) {
            return true;
        }
        match *self {
            FeatureRegion::Box(bx) => {
                // Closest point on triangle to box center + edge midpoints.
                let center = (bx.lo + bx.hi) * 0.5;
                let cp = closest_point_on_triangle(center, a, b, c).point;
                self.contains(cp)
                    || self.contains((a + b) * 0.5)
                    || self.contains((b + c) * 0.5)
                    || self.contains((c + a) * 0.5)
            }
            FeatureRegion::Ball { center, radius } => {
                let r = radius.as_m().max(0.0);
                let cp = closest_point_on_triangle(center, a, b, c).point;
                (cp - center).dot(cp - center) <= r * r
            }
            FeatureRegion::Capsule { a: sa, b: sb, radius } => {
                let r = radius.as_m().max(0.0);
                let r2 = r * r;
                let ab = sb - sa;
                let denom = ab.dot(ab);
                if !(denom > 1e-24) {
                    let cp = closest_point_on_triangle(sa, a, b, c).point;
                    return (cp - sa).dot(cp - sa) <= r2;
                }
                let mut t = ((centroid - sa).dot(ab) / denom).clamp(0.0, 1.0);
                for _ in 0..4 {
                    let sp = sa + ab * t;
                    let cp = closest_point_on_triangle(sp, a, b, c).point;
                    if (cp - sp).dot(cp - sp) <= r2 {
                        return true;
                    }
                    t = ((cp - sa).dot(ab) / denom).clamp(0.0, 1.0);
                }
                // Also test endpoints directly against the triangle.
                let ca = closest_point_on_triangle(sa, a, b, c).point;
                let cb = closest_point_on_triangle(sb, a, b, c).point;
                (ca - sa).dot(ca - sa) <= r2 || (cb - sb).dot(cb - sb) <= r2
            }
        }
    }

    /// Characteristic smallest diameter of this region (metres), useful for automatic local
    /// meshing refinement when `min_feature` is not stated explicitly.
    #[inline]
    pub fn minor_extent_m(&self) -> f64 {
        match *self {
            FeatureRegion::Box(b) => {
                let d = b.hi - b.lo;
                d.x.min(d.y).min(d.z).max(0.0)
            }
            FeatureRegion::Ball { radius, .. } | FeatureRegion::Capsule { radius, .. } => (radius.as_m() * 2.0).max(0.0),
        }
    }
}

/// How strongly the mesher and simplifier protect a spatial feature zone.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FeatureProtect {
    /// Preserve tighter error and normal tolerances and bias QEM collapse priority so non-critical
    /// areas absorb triangle-budget reduction first.
    Critical,
    /// Hard-protected: never relax this feature's error ceiling even during Phase-2 budget
    /// reduction (stops with [`crate::Stop::ErrorCeiling`] rather than sacrificing the feature).
    Pinned,
}

/// Spatial fidelity zone passed to the mesher ([`crate::MeshOpts::zones`]) and simplifier
/// ([`crate::DecimateOpts::zones`]) to preserve extra geometry around critical features.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FeatureZone {
    pub region: FeatureRegion,
    /// Tighter geometric error target inside this region (`None` inherits part tolerance).
    pub max_error: Option<Length>,
    /// Tighter local octree feature cell ceiling inside this region (`None` derives from
    /// `max_error` when present).
    pub min_feature: Option<Length>,
    /// Tighter single-step normal-turn guard during simplification as `cos²(max_turn)` (e.g.
    /// `cos²(12°) ≈ 0.956`). `None` uses the simplifier's default guard.
    pub min_cos_sq: Option<f64>,
    /// QEM heap cost multiplier (`>= 1.0`) so non-critical faces collapse before this feature when
    /// competing for a triangle budget.
    pub cost_weight: f64,
    /// Protection policy during Phase-2 budget reduction.
    pub protect: FeatureProtect,
}

/// `cos²(12°)`: normal-turn guard for grooves and narrow channels.
const COS_SQ_12_DEG: f64 = 0.956_772_728_821_300_4;
/// `cos²(10°)`: normal-turn guard for bevels and rounded fillets.
const COS_SQ_10_DEG: f64 = 0.969_846_310_392_954_2;

impl FeatureZone {
    /// Construct a critical protection zone with sensible priority weighting (`16×` for
    /// [`FeatureProtect::Critical`], `64×` for [`FeatureProtect::Pinned`]).
    #[inline]
    pub const fn new(region: FeatureRegion, protect: FeatureProtect) -> FeatureZone {
        FeatureZone {
            region,
            max_error: None,
            min_feature: None,
            min_cos_sq: None,
            cost_weight: match protect {
                FeatureProtect::Critical => 16.0,
                FeatureProtect::Pinned => 64.0,
            },
            protect,
        }
    }

    /// Physical-dimension zone for a groove, slot, or rabbet of `width` and `depth`: resolves
    /// at least 3 cells across the width (`min_feature = width / 3`), bounds geometric error to
    /// `min(width / 8, depth / 5)`, and guards normal turns at `12°`.
    #[inline]
    pub fn for_groove(region: FeatureRegion, width: Length, depth: Length, protect: FeatureProtect) -> FeatureZone {
        let w = width.max(Length::ZERO);
        let d = depth.max(Length::ZERO);
        let mut zone = FeatureZone::new(region, protect);
        zone.min_feature = Some(w / 3.0);
        zone.max_error = Some((w / 8.0).min(d / 5.0));
        zone.min_cos_sq = Some(COS_SQ_12_DEG);
        zone
    }

    /// Physical-dimension zone for a bevel or fillet of `radius`: resolves `radius / 2` cells,
    /// bounds chord sagitta to `radius * 0.035` (~15° arc segments), and guards normal turns at
    /// `10°`.
    #[inline]
    pub fn for_bevel(region: FeatureRegion, radius: Length, protect: FeatureProtect) -> FeatureZone {
        let r = radius.max(Length::ZERO);
        let mut zone = FeatureZone::new(region, protect);
        zone.min_feature = Some(r / 2.0);
        zone.max_error = Some(r * 0.035);
        zone.min_cos_sq = Some(COS_SQ_10_DEG);
        zone
    }

    /// Physical-dimension zone for a mating seam or shadow gap (`gap` wide, `depth` deep):
    /// always [`FeatureProtect::Pinned`] so budget decimation never relaxes the seam, with
    /// `min_feature = gap / 2.5` and `max_error = min(gap / 6, depth / 4)`.
    #[inline]
    pub fn for_seam(region: FeatureRegion, gap: Length, depth: Length) -> FeatureZone {
        let g = gap.max(Length::ZERO);
        let d = depth.max(Length::ZERO);
        let mut zone = FeatureZone::new(region, FeatureProtect::Pinned);
        zone.min_feature = Some(g / 2.5);
        zone.max_error = Some((g / 6.0).min(d / 4.0));
        zone
    }

    /// Physical-dimension zone for engraved/embossed stencils or markings of `stroke_width`:
    /// resolves `stroke_width / 2` cells and bounds error to `stroke_width / 8`.
    #[inline]
    pub fn for_stencil(region: FeatureRegion, stroke_width: Length) -> FeatureZone {
        let sw = stroke_width.max(Length::ZERO);
        let mut zone = FeatureZone::new(region, FeatureProtect::Critical);
        zone.min_feature = Some(sw / 2.0);
        zone.max_error = Some(sw / 8.0);
        zone
    }
}

#[inline]
fn clamp_point_to_aabb(p: Vec3, b: &Aabb3) -> Vec3 {
    Vec3::new(p.x.clamp(b.lo.x, b.hi.x), p.y.clamp(b.lo.y, b.hi.y), p.z.clamp(b.lo.z, b.hi.z))
}

#[inline]
fn segment_hits_aabb(a: Vec3, b: Vec3, bx: &Aabb3) -> bool {
    let d = b - a;
    let mut tmin = 0.0f64;
    let mut tmax = 1.0f64;
    for (o, dir, lo, hi) in [(a.x, d.x, bx.lo.x, bx.hi.x), (a.y, d.y, bx.lo.y, bx.hi.y), (a.z, d.z, bx.lo.z, bx.hi.z)] {
        if dir.abs() <= 1e-18 {
            if o < lo || o > hi {
                return false;
            }
        } else {
            let inv = 1.0 / dir;
            let mut t0 = (lo - o) * inv;
            let mut t1 = (hi - o) * inv;
            if t0 > t1 {
                core::mem::swap(&mut t0, &mut t1);
            }
            tmin = tmin.max(t0);
            tmax = tmax.min(t1);
            if tmin > tmax {
                return false;
            }
        }
    }
    true
}

/// Voronoi region of triangle `(a, b, c)` containing the closest point to a query.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TriangleRegion {
    Face,
    VertA,
    VertB,
    VertC,
    EdgeAB,
    EdgeBC,
    EdgeCA,
}

/// Closest point on triangle `(a, b, c)` together with its non-negative barycentric coordinates
/// (`[w_a, w_b, w_c]`, summing to `1.0`) and Voronoi [`TriangleRegion`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ClosestTrianglePoint {
    pub point: Vec3,
    pub bary: [f64; 3],
    pub region: TriangleRegion,
}

/// Exact Voronoi-region closest point on triangle `(a, b, c)` to query point `p` (Ericson,
/// *Real-Time Collision Detection*, §5.1.5).
#[inline]
pub fn closest_point_on_triangle(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> ClosestTrianglePoint {
    let ab = b - a;
    let ac = c - a;
    let ap = p - a;
    let d1 = ab.dot(ap);
    let d2 = ac.dot(ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return ClosestTrianglePoint { point: a, bary: [1.0, 0.0, 0.0], region: TriangleRegion::VertA };
    }

    let bp = p - b;
    let d3 = ab.dot(bp);
    let d4 = ac.dot(bp);
    if d3 >= 0.0 && d4 <= d3 {
        return ClosestTrianglePoint { point: b, bary: [0.0, 1.0, 0.0], region: TriangleRegion::VertB };
    }

    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let denom = d1 - d3;
        let v = if denom > 1e-24 { d1 / denom } else { 0.0 };
        return ClosestTrianglePoint { point: a + ab * v, bary: [1.0 - v, v, 0.0], region: TriangleRegion::EdgeAB };
    }

    let cp = p - c;
    let d5 = ab.dot(cp);
    let d6 = ac.dot(cp);
    if d6 >= 0.0 && d5 <= d6 {
        return ClosestTrianglePoint { point: c, bary: [0.0, 0.0, 1.0], region: TriangleRegion::VertC };
    }

    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let denom = d2 - d6;
        let w = if denom > 1e-24 { d2 / denom } else { 0.0 };
        return ClosestTrianglePoint { point: a + ac * w, bary: [1.0 - w, 0.0, w], region: TriangleRegion::EdgeCA };
    }

    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let num = d4 - d3;
        let denom = num + (d5 - d6);
        let w = if denom > 1e-24 { num / denom } else { 0.0 };
        return ClosestTrianglePoint { point: b + (c - b) * w, bary: [0.0, 1.0 - w, w], region: TriangleRegion::EdgeBC };
    }

    let sum = va + vb + vc;
    if !(sum > 1e-24) {
        return ClosestTrianglePoint { point: a, bary: [1.0, 0.0, 0.0], region: TriangleRegion::Face };
    }
    let inv = 1.0 / sum;
    let v = vb * inv;
    let w = vc * inv;
    let u = (1.0 - v - w).max(0.0);
    ClosestTrianglePoint { point: a + ab * v + ac * w, bary: [u, v, w], region: TriangleRegion::Face }
}
