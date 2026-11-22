//! Small geometry in f64: [`Vec2`], [`Vec3`], [`Mat3`], [`Affine3`] (a map that keeps its inverse
//! and says whether it keeps distances) and [`Aabb3`]. Every operation is a fixed sequence of
//! IEEE basic operations in the order written (no fused multiply-add, no reordering), so its
//! bits are the same everywhere.

use core::ops::{Add, Mul, Neg, Sub};

/// A point or direction in the plane.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl Vec2 {
    /// `(x, y)`.
    pub const fn new(x: f64, y: f64) -> Vec2 {
        Vec2 { x, y }
    }

    /// `x1 x2 + y1 y2`.
    pub fn dot(self, o: Vec2) -> f64 {
        self.x * o.x + self.y * o.y
    }

    /// `x1 y2 - y1 x2`: the signed area of the parallelogram.
    pub fn cross(self, o: Vec2) -> f64 {
        self.x * o.y - self.y * o.x
    }

    /// The Euclidean length.
    pub fn length(self) -> f64 {
        crate::sqrt(self.dot(self))
    }
}

impl Add for Vec2 {
    type Output = Vec2;
    fn add(self, o: Vec2) -> Vec2 {
        Vec2::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Vec2 {
    type Output = Vec2;
    fn sub(self, o: Vec2) -> Vec2 {
        Vec2::new(self.x - o.x, self.y - o.y)
    }
}

impl Mul<f64> for Vec2 {
    type Output = Vec2;
    fn mul(self, s: f64) -> Vec2 {
        Vec2::new(self.x * s, self.y * s)
    }
}

impl Neg for Vec2 {
    type Output = Vec2;
    fn neg(self) -> Vec2 {
        Vec2::new(-self.x, -self.y)
    }
}

/// A point or direction in space.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    /// The origin.
    pub const ZERO: Vec3 = Vec3::new(0.0, 0.0, 0.0);

    /// `(x, y, z)`.
    pub const fn new(x: f64, y: f64, z: f64) -> Vec3 {
        Vec3 { x, y, z }
    }

    /// `v` in every component.
    pub const fn splat(v: f64) -> Vec3 {
        Vec3::new(v, v, v)
    }

    /// The components in order.
    pub const fn to_array(self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }

    /// From components in order.
    pub const fn from_array(a: [f64; 3]) -> Vec3 {
        Vec3::new(a[0], a[1], a[2])
    }

    /// `x1 x2 + y1 y2 + z1 z2`, summed left to right.
    pub fn dot(self, o: Vec3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    /// The cross product.
    pub fn cross(self, o: Vec3) -> Vec3 {
        Vec3::new(self.y * o.z - self.z * o.y, self.z * o.x - self.x * o.z, self.x * o.y - self.y * o.x)
    }

    /// The Euclidean length.
    pub fn length(self) -> f64 {
        crate::sqrt(self.dot(self))
    }

    /// This direction at length 1, or `None` for the zero vector or a non-finite one.
    pub fn normalized(self) -> Option<Vec3> {
        let l = self.length();
        if l > 0.0 && l < f64::INFINITY { Some(self * (1.0 / l)) } else { None }
    }

    /// Each component's magnitude.
    pub fn abs(self) -> Vec3 {
        Vec3::new(self.x.abs(), self.y.abs(), self.z.abs())
    }

    /// The smaller of each pair of components (`if b < a { b } else { a }`).
    pub fn min(self, o: Vec3) -> Vec3 {
        let m = |a: f64, b: f64| if b < a { b } else { a };
        Vec3::new(m(self.x, o.x), m(self.y, o.y), m(self.z, o.z))
    }

    /// The larger of each pair of components (`if b > a { b } else { a }`).
    pub fn max(self, o: Vec3) -> Vec3 {
        let m = |a: f64, b: f64| if b > a { b } else { a };
        Vec3::new(m(self.x, o.x), m(self.y, o.y), m(self.z, o.z))
    }

    /// The largest component.
    pub fn max_component(self) -> f64 {
        let m = |a: f64, b: f64| if b > a { b } else { a };
        m(m(self.x, self.y), self.z)
    }

    /// Each component times the other's.
    pub fn mul_each(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x * o.x, self.y * o.y, self.z * o.z)
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f64> for Vec3 {
    type Output = Vec3;
    fn mul(self, s: f64) -> Vec3 {
        Vec3::new(self.x * s, self.y * s, self.z * s)
    }
}

impl Neg for Vec3 {
    type Output = Vec3;
    fn neg(self) -> Vec3 {
        Vec3::new(-self.x, -self.y, -self.z)
    }
}

/// A 3 by 3 matrix, by rows.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mat3 {
    pub rows: [[f64; 3]; 3],
}

impl Mat3 {
    /// The identity.
    pub const IDENTITY: Mat3 = Mat3 { rows: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] };

    /// From rows.
    pub const fn new(rows: [[f64; 3]; 3]) -> Mat3 {
        Mat3 { rows }
    }

    /// The diagonal matrix of `d`.
    pub const fn diagonal(d: Vec3) -> Mat3 {
        Mat3 { rows: [[d.x, 0.0, 0.0], [0.0, d.y, 0.0], [0.0, 0.0, d.z]] }
    }

    /// The rotation about x by the angle whose sine and cosine are `(s, c)` (right-handed).
    pub const fn rotation_x(s: f64, c: f64) -> Mat3 {
        Mat3 { rows: [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]] }
    }

    /// The rotation about y by the angle whose sine and cosine are `(s, c)`.
    pub const fn rotation_y(s: f64, c: f64) -> Mat3 {
        Mat3 { rows: [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]] }
    }

    /// The rotation about z by the angle whose sine and cosine are `(s, c)`.
    pub const fn rotation_z(s: f64, c: f64) -> Mat3 {
        Mat3 { rows: [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]] }
    }

    /// Column `j`.
    pub const fn column(&self, j: usize) -> Vec3 {
        Vec3::new(self.rows[0][j], self.rows[1][j], self.rows[2][j])
    }

    /// Row `i`.
    pub const fn row(&self, i: usize) -> Vec3 {
        Vec3::from_array(self.rows[i])
    }

    /// `M v`.
    pub fn apply(&self, v: Vec3) -> Vec3 {
        Vec3::new(self.row(0).dot(v), self.row(1).dot(v), self.row(2).dot(v))
    }

    /// `M N`.
    pub fn mul(&self, n: &Mat3) -> Mat3 {
        let mut rows = [[0.0; 3]; 3];
        for (i, row) in rows.iter_mut().enumerate() {
            for (j, out) in row.iter_mut().enumerate() {
                *out = self.row(i).dot(n.column(j));
            }
        }
        Mat3 { rows }
    }

    /// The transpose.
    pub fn transpose(&self) -> Mat3 {
        Mat3 { rows: [self.column(0).to_array(), self.column(1).to_array(), self.column(2).to_array()] }
    }

    /// The determinant, by the first row's cofactors.
    pub fn det(&self) -> f64 {
        let r = &self.rows;
        r[0][0] * (r[1][1] * r[2][2] - r[1][2] * r[2][1]) - r[0][1] * (r[1][0] * r[2][2] - r[1][2] * r[2][0])
            + r[0][2] * (r[1][0] * r[2][1] - r[1][1] * r[2][0])
    }

    /// The inverse by the adjugate, or `None` when the determinant is zero or not finite.
    pub fn inverse(&self) -> Option<Mat3> {
        let d = self.det();
        if d == 0.0 || !d.is_finite() {
            return None;
        }
        let r = &self.rows;
        let c = |a: usize, b: usize, e: usize, f: usize| r[a][b] * r[e][f] - r[a][f] * r[e][b];
        let adj = [
            [c(1, 1, 2, 2), -c(0, 1, 2, 2), c(0, 1, 1, 2)],
            [-c(1, 0, 2, 2), c(0, 0, 2, 2), -c(0, 0, 1, 2)],
            [c(1, 0, 2, 1), -c(0, 0, 2, 1), c(0, 0, 1, 1)],
        ];
        let inv = 1.0 / d;
        let mut rows = adj;
        for row in &mut rows {
            for v in row.iter_mut() {
                *v *= inv;
            }
        }
        Some(Mat3 { rows })
    }

    /// Each entry's magnitude: `|M| h` bounds `M` applied to a box of half extents `h`.
    pub fn abs(&self) -> Mat3 {
        let mut rows = self.rows;
        for row in &mut rows {
            for v in row.iter_mut() {
                *v = v.abs();
            }
        }
        Mat3 { rows }
    }

    /// The Frobenius norm.
    pub fn frobenius(&self) -> f64 {
        let mut s = 0.0;
        for row in &self.rows {
            for v in row {
                s += v * v;
            }
        }
        crate::sqrt(s)
    }

    /// Whether `M^T M` is exactly the identity in f64: true for signed permutations (quarter
    /// turns and mirrors), which is what an exact rotation is.
    pub fn is_exactly_orthonormal(&self) -> bool {
        self.transpose().mul(self) == Mat3::IDENTITY
    }
}

/// Whether an [`Affine3`] keeps distances, said by how it was made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    /// A rotation and a shift (and maybe a mirror): distances are kept, so a signed distance
    /// stays one through it.
    Keeps,
    /// Anything else: a field through it is a bound, by the factor the map states.
    Distorts,
}

/// `p -> M p + t`, with its inverse kept beside it and whether it keeps distances.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Affine3 {
    m: Mat3,
    t: Vec3,
    inv_m: Mat3,
    inv_t: Vec3,
    metric: Metric,
}

/// Why an [`Affine3`] was refused: its linear part has no inverse.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Singular {
    /// The determinant found.
    pub det: f64,
}

impl Affine3 {
    /// The identity.
    pub const IDENTITY: Affine3 =
        Affine3 { m: Mat3::IDENTITY, t: Vec3::ZERO, inv_m: Mat3::IDENTITY, inv_t: Vec3::ZERO, metric: Metric::Keeps };

    /// A rigid map from a rotation given by its sines and cosines (so `m` is orthonormal to
    /// rounding: its inverse is its transpose) and a shift.
    pub fn rigid(rotation: Mat3, shift: Vec3) -> Affine3 {
        let inv_m = rotation.transpose();
        Affine3 { m: rotation, t: shift, inv_m, inv_t: -inv_m.apply(shift), metric: Metric::Keeps }
    }

    /// Any invertible map. It keeps distances only when `m` is exactly orthonormal.
    pub fn general(m: Mat3, t: Vec3) -> Result<Affine3, Singular> {
        let inv_m = m.inverse().ok_or(Singular { det: m.det() })?;
        let metric = if m.is_exactly_orthonormal() { Metric::Keeps } else { Metric::Distorts };
        Ok(Affine3 { m, t, inv_m, inv_t: -inv_m.apply(t), metric })
    }

    /// The linear part.
    pub fn linear(&self) -> &Mat3 {
        &self.m
    }

    /// The shift.
    pub fn shift(&self) -> Vec3 {
        self.t
    }

    /// The inverse's linear part.
    pub fn inverse_linear(&self) -> &Mat3 {
        &self.inv_m
    }

    /// Whether distances are kept.
    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// `M p + t`.
    pub fn apply(&self, p: Vec3) -> Vec3 {
        self.m.apply(p) + self.t
    }

    /// The inverse map at `p`.
    pub fn apply_inverse(&self, p: Vec3) -> Vec3 {
        self.inv_m.apply(p) + self.inv_t
    }

    /// The inverse map.
    pub fn inverse(&self) -> Affine3 {
        Affine3 { m: self.inv_m, t: self.inv_t, inv_m: self.m, inv_t: self.t, metric: self.metric }
    }

    /// `self` after `inner`: `p -> self(inner(p))`.
    pub fn then_after(&self, inner: &Affine3) -> Affine3 {
        let m = self.m.mul(&inner.m);
        let t = self.m.apply(inner.t) + self.t;
        let inv_m = inner.inv_m.mul(&self.inv_m);
        let inv_t = inner.inv_m.apply(self.inv_t) + inner.inv_t;
        let metric = if self.metric == Metric::Keeps && inner.metric == Metric::Keeps { Metric::Keeps } else { Metric::Distorts };
        Affine3 { m, t, inv_m, inv_t, metric }
    }

    /// The factor a field `f(A^-1 p)` must be divided by to stay 1-Lipschitz: the largest
    /// stretch of `A^-1`, bounded above by its Frobenius norm. 1 for a rigid map.
    pub fn lipschitz_of_inverse(&self) -> f64 {
        match self.metric {
            Metric::Keeps => 1.0,
            Metric::Distorts => self.inv_m.frobenius(),
        }
    }

    /// A box containing the image of `b`: its centre mapped, its half extents by `|M|`.
    pub fn apply_box(&self, b: &Aabb3) -> Aabb3 {
        if b.is_empty() {
            return Aabb3::EMPTY;
        }
        let c = self.apply(b.center());
        let h = self.m.abs().apply(b.half());
        Aabb3::around(c, h)
    }

    /// A box containing the inverse image of `b`: the query box turned into this map's own
    /// frame, bounded per local axis (an oriented box's own extents, not a box around a box).
    pub fn apply_inverse_box(&self, b: &Aabb3) -> Aabb3 {
        self.inverse().apply_box(b)
    }
}

/// An axis-aligned box `lo..=hi`; empty when any `lo > hi`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Aabb3 {
    pub lo: Vec3,
    pub hi: Vec3,
}

impl Aabb3 {
    /// The empty box (the identity of [`Aabb3::union`]).
    pub const EMPTY: Aabb3 = Aabb3 { lo: Vec3::splat(f64::INFINITY), hi: Vec3::splat(f64::NEG_INFINITY) };
    /// All of space.
    pub const ALL: Aabb3 = Aabb3 { lo: Vec3::splat(f64::NEG_INFINITY), hi: Vec3::splat(f64::INFINITY) };

    /// `lo..=hi`.
    pub const fn new(lo: Vec3, hi: Vec3) -> Aabb3 {
        Aabb3 { lo, hi }
    }

    /// The box of centre `c` and half extents `h`, its corners rounded outward by one ulp so it
    /// contains the exact box.
    pub fn around(c: Vec3, h: Vec3) -> Aabb3 {
        let lo = c - h;
        let hi = c + h;
        Aabb3 {
            lo: Vec3::new(crate::next_down(lo.x), crate::next_down(lo.y), crate::next_down(lo.z)),
            hi: Vec3::new(crate::next_up(hi.x), crate::next_up(hi.y), crate::next_up(hi.z)),
        }
    }

    /// Whether no point is inside.
    pub fn is_empty(&self) -> bool {
        !(self.lo.x <= self.hi.x && self.lo.y <= self.hi.y && self.lo.z <= self.hi.z)
    }

    /// The centre.
    pub fn center(&self) -> Vec3 {
        (self.lo + self.hi) * 0.5
    }

    /// The half extents, rounded up.
    pub fn half(&self) -> Vec3 {
        let h = (self.hi - self.lo) * 0.5;
        Vec3::new(crate::next_up(h.x), crate::next_up(h.y), crate::next_up(h.z))
    }

    /// The smallest box holding both.
    pub fn union(&self, o: &Aabb3) -> Aabb3 {
        Aabb3 { lo: self.lo.min(o.lo), hi: self.hi.max(o.hi) }
    }

    /// The common part (maybe empty).
    pub fn intersect(&self, o: &Aabb3) -> Aabb3 {
        Aabb3 { lo: self.lo.max(o.lo), hi: self.hi.min(o.hi) }
    }

    /// Whether the two share a point.
    pub fn overlaps(&self, o: &Aabb3) -> bool {
        !self.intersect(o).is_empty()
    }

    /// Whether `p` is inside or on the boundary.
    pub fn contains(&self, p: Vec3) -> bool {
        p.x >= self.lo.x && p.x <= self.hi.x && p.y >= self.lo.y && p.y <= self.hi.y && p.z >= self.lo.z && p.z <= self.hi.z
    }

    /// Whether `o` lies inside this box.
    pub fn contains_box(&self, o: &Aabb3) -> bool {
        o.is_empty() || (self.contains(o.lo) && self.contains(o.hi))
    }

    /// Grown by `r` on every side, rounded outward (shrunk for a negative `r`).
    pub fn grow(&self, r: f64) -> Aabb3 {
        if self.is_empty() {
            return *self;
        }
        let lo = self.lo - Vec3::splat(r);
        let hi = self.hi + Vec3::splat(r);
        Aabb3 {
            lo: Vec3::new(crate::next_down(lo.x), crate::next_down(lo.y), crate::next_down(lo.z)),
            hi: Vec3::new(crate::next_up(hi.x), crate::next_up(hi.y), crate::next_up(hi.z)),
        }
    }

    /// A lower bound of the distance between a point of `self` and a point of `o` (0 when they
    /// overlap), rounded down.
    pub fn gap(&self, o: &Aabb3) -> f64 {
        let d = |alo: f64, ahi: f64, blo: f64, bhi: f64| {
            let g = if blo > ahi { blo - ahi } else if alo > bhi { alo - bhi } else { 0.0 };
            crate::next_down(g).max(0.0)
        };
        let v = Vec3::new(d(self.lo.x, self.hi.x, o.lo.x, o.hi.x), d(self.lo.y, self.hi.y, o.lo.y, o.hi.y), d(self.lo.z, self.hi.z, o.lo.z, o.hi.z));
        crate::next_down(v.length()).max(0.0)
    }

    /// The longest side.
    pub fn longest_side(&self) -> f64 {
        (self.hi - self.lo).max_component()
    }

    /// The eight corners.
    pub fn corners(&self) -> [Vec3; 8] {
        let (a, b) = (self.lo, self.hi);
        [
            Vec3::new(a.x, a.y, a.z),
            Vec3::new(b.x, a.y, a.z),
            Vec3::new(a.x, b.y, a.z),
            Vec3::new(b.x, b.y, a.z),
            Vec3::new(a.x, a.y, b.z),
            Vec3::new(b.x, a.y, b.z),
            Vec3::new(a.x, b.y, b.z),
            Vec3::new(b.x, b.y, b.z),
        ]
    }

    /// The eight octants, in corner order.
    pub fn octants(&self) -> [Aabb3; 8] {
        let c = self.center();
        self.corners().map(|k| Aabb3 { lo: k.min(c), hi: k.max(c) })
    }
}
