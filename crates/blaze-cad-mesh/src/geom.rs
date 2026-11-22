//! The plain geometry this crate needs and nothing more: `[f32; 3]` points as stored, `[f64; 3]`
//! for every sum and product, a box, and `sqrt`. It is ONE module on purpose: blaze-math's
//! vector, box and correctly rounded `sqrt` replace it whole once that crate lands, and nothing
//! outside this file names these helpers' bodies.
//!
//! Every operation is an IEEE basic operation or `sqrt`, each correctly rounded, in the order
//! written (Rust does not contract `a * b + c` into a fused multiply-add), so the same inputs give
//! the same bits on every machine.

#![blaze::sans_io]

/// A stored point or direction.
pub type P3 = [f32; 3];
/// A point or direction being computed with.
pub type D3 = [f64; 3];

/// The correctly rounded square root (IEEE 754 `squareRoot`).
#[inline]
pub fn sqrt(x: f64) -> f64 {
    core::f64::math::sqrt(x)
}

/// `x` rounded to the nearest integer, ties to even (IEEE 754 `roundToIntegralTiesToEven`).
#[inline]
pub fn round_even(x: f64) -> f64 {
    core::f64::math::round_ties_even(x)
}

/// A distance in metres as a [`Length`](blaze_units::Length) rounded UP to the next whole
/// nanometre, so a bound computed in f64 stays a bound once keyed: `x · 10^9` is rounded once,
/// so the ceiling of the next f64 above it is at or above the true product's ceiling. Negative,
/// NaN and infinite inputs are a caller's bug (a distance is finite and not negative): NaN and
/// negatives give zero, and a value past the `Length` range (9.2 · 10^9 m) saturates at it.
#[inline]
pub fn length_up(x: f64) -> blaze_units::Length {
    if !(x > 0.0) {
        return blaze_units::Length::ZERO;
    }
    let v = x * 1e9;
    if !(v < 9.2e18) {
        return blaze_units::Length::nm(i64::MAX);
    }
    // `ceil` of a finite value below 9.2 · 10^18 is a whole number in range: the cast is exact.
    blaze_units::Length::nm(core::f64::math::ceil(next_up(v)) as i64)
}

/// The least f64 above a finite positive `x`.
#[inline]
fn next_up(x: f64) -> f64 {
    f64::from_bits(x.to_bits() + 1)
}

/// `x` rounded toward zero; only a planted bug uses it.
#[inline]
pub(crate) fn trunc(x: f64) -> f64 {
    core::f64::math::trunc(x)
}

/// The point widened, exactly.
#[inline]
pub fn wide(p: P3) -> D3 {
    [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])]
}

/// The point rounded to the nearest `f32` per coordinate (`as` rounds to nearest, ties to even).
#[inline]
pub fn narrow(p: D3) -> P3 {
    [p[0] as f32, p[1] as f32, p[2] as f32]
}

#[inline]
pub fn add(a: D3, b: D3) -> D3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

#[inline]
pub fn sub(a: D3, b: D3) -> D3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

#[inline]
pub fn scale(a: D3, s: f64) -> D3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

#[inline]
pub fn dot(a: D3, b: D3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[inline]
pub fn cross(a: D3, b: D3) -> D3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

#[inline]
pub fn length(a: D3) -> f64 {
    sqrt(dot(a, a))
}

#[inline]
pub fn distance(a: D3, b: D3) -> f64 {
    length(sub(a, b))
}

/// `a` scaled to length 1, or `None` for the zero vector (or one whose square underflows).
#[inline]
pub fn unit(a: D3) -> Option<D3> {
    let len = length(a);
    if len > 0.0 && len.is_finite() { Some(scale(a, 1.0 / len)) } else { None }
}

/// Twice the area vector of the triangle `a b c` (its normal, as long as twice its area),
/// counter-clockwise seen from where it points.
#[inline]
pub fn tri_normal(a: D3, b: D3, c: D3) -> D3 {
    cross(sub(b, a), sub(c, a))
}

/// `x` with negative zero made positive zero: the one zero a key or a digest sees.
#[inline]
pub fn plus_zero(x: f32) -> f32 {
    if x == 0.0 { 0.0 } else { x }
}

/// The point with each negative zero made positive.
#[inline]
pub fn plus_zeros(p: P3) -> P3 {
    [plus_zero(p[0]), plus_zero(p[1]), plus_zero(p[2])]
}

/// The total order of points the sorts here use: by `x`, then `y`, then `z`, each by IEEE
/// `totalOrder` (so it is total even where a point is not finite).
#[inline]
pub fn point_order(a: P3, b: P3) -> core::cmp::Ordering {
    a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])).then(a[2].total_cmp(&b[2]))
}

/// The least `f32` not below `x` (a radius or a bound rounded up, never down).
#[inline]
pub fn up_f32(x: f64) -> f32 {
    let near = x as f32;
    if f64::from(near) >= x || !near.is_finite() { near } else { next_up_f32(near) }
}

/// The next `f32` above `x` (IEEE `nextUp`) for finite `x`.
#[inline]
fn next_up_f32(x: f32) -> f32 {
    if x == 0.0 {
        f32::from_bits(1)
    } else if x > 0.0 {
        f32::from_bits(x.to_bits() + 1)
    } else {
        f32::from_bits(x.to_bits() - 1)
    }
}

/// An axis-aligned box. `EMPTY` (min `+inf`, max `-inf`) holds no point and grows to the first
/// point included.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Aabb {
    pub min: P3,
    pub max: P3,
}

impl Aabb {
    pub const EMPTY: Aabb = Aabb { min: [f32::INFINITY; 3], max: [f32::NEG_INFINITY; 3] };

    /// The box grown to hold `p`.
    #[inline]
    pub fn include(&mut self, p: P3) {
        for axis in 0..3 {
            if p[axis] < self.min[axis] {
                self.min[axis] = p[axis];
            }
            if p[axis] > self.max[axis] {
                self.max[axis] = p[axis];
            }
        }
    }

    /// Whether no point was included.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.min[0] > self.max[0]
    }

    /// `max - min` per axis, in `f64` (exact for any two `f32`s within 2^29 of each other's
    /// exponent, and correctly rounded otherwise); zero for an empty box.
    #[inline]
    pub fn extent(&self) -> D3 {
        if self.is_empty() { [0.0; 3] } else { sub(wide(self.max), wide(self.min)) }
    }

    /// The middle of the box.
    #[inline]
    pub fn centre(&self) -> D3 {
        scale(add(wide(self.min), wide(self.max)), 0.5)
    }

    /// Whether `p` is inside or on the box.
    #[inline]
    pub fn contains(&self, p: P3) -> bool {
        (0..3).all(|axis| self.min[axis] <= p[axis] && p[axis] <= self.max[axis])
    }

    /// The least of the three extents: how thin the box is.
    #[inline]
    pub fn thinnest(&self) -> f64 {
        let e = self.extent();
        e[0].min(e[1]).min(e[2])
    }
}

/// A 3x3 matrix as rows.
pub type M3 = [[f64; 3]; 3];

/// The symmetric 3x3 system `a x = b` solved by Cramer's rule, or `None` where `|det a|` is not
/// above `tiny` (the system is singular for the caller's purpose).
pub fn solve3(a: M3, b: D3, tiny: f64) -> Option<D3> {
    let det = dot(a[0], cross(a[1], a[2]));
    if !(det.abs() > tiny) || !det.is_finite() {
        return None;
    }
    let col = |i: usize| -> D3 { [a[0][i], a[1][i], a[2][i]] };
    let (c0, c1, c2) = (col(0), col(1), col(2));
    let x = dot(b, cross(c1, c2)) / det;
    let y = dot(c0, cross(b, c2)) / det;
    let z = dot(c0, cross(c1, b)) / det;
    let out = [x, y, z];
    if out.iter().all(|v| v.is_finite()) { Some(out) } else { None }
}
