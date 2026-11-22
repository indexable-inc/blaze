//! [`Interval`]: a closed range of reals `lo..=hi` that contains the exact result of every
//! operation on any reals in its operands, under round-to-nearest, WITHOUT changing the
//! rounding mode (rule 75: no ambient state, and `core` cannot change it anyway).
//!
//! THE METHOD: each endpoint is computed in round-to-nearest and then moved one ulp outward
//! (`next_down` for `lo`, `next_up` for `hi`). Round-to-nearest of `+ - * /` and `sqrt` is
//! within half an ulp of the exact value, so one ulp outward contains it. A function of this
//! crate with a stated bound of `k` ulp is widened by `k + 1` ulp. The choice against
//! error-free transformations (two-sum, two-product, to know the exact error and widen only
//! when there is one): those cost three to six times the operations for a range at most two ulp
//! narrower, and the narrower range would not change a single decision of the kernel's
//! subdivision at the tolerances it runs at. Exact operations (negation, `abs`, `min`, `max`,
//! the rounding functions) are not widened.
//!
//! An operand that could make a point result NaN (`0 * inf`, `inf - inf`, a division by an
//! interval holding zero) gives [`Interval::ENTIRE`]; `0 * inf` is taken as `0`, as IEEE 1788
//! does for intervals, because a point inside an interval is finite.

use crate::{fns, next_down, next_up};
use blaze_own::planted::{NoPlant, Planted, Plants};

/// The bugs a control can plant in the interval operations (D165, rule 78).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntervalPlant {
    /// The operations as they are.
    None,
    /// A sum's endpoints are rounded to nearest and not moved outward.
    AddNotWidened,
    /// A product's endpoints are taken from the two pairs `lo*lo`, `hi*hi` only.
    MulTwoCorners,
}

impl Plants for IntervalPlant {}

/// A closed range `lo..=hi` of reals, `lo <= hi`, either end possibly infinite.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Interval {
    lo: f64,
    hi: f64,
}

/// Why an [`Interval`] was refused.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum IntervalError {
    /// An end is NaN.
    Nan,
    /// `lo > hi`.
    Reversed { lo: f64, hi: f64 },
}

#[inline]
fn widen(lo: f64, hi: f64) -> Interval {
    if lo.is_nan() || hi.is_nan() {
        return Interval::ENTIRE;
    }
    Interval { lo: next_down(lo), hi: next_up(hi) }
}

#[inline]
fn widen_by(lo: f64, hi: f64, ulps: u32) -> Interval {
    if lo.is_nan() || hi.is_nan() {
        return Interval::ENTIRE;
    }
    let (mut l, mut h) = (lo, hi);
    for _ in 0..ulps {
        l = next_down(l);
        h = next_up(h);
    }
    Interval { lo: l, hi: h }
}

#[inline]
fn min2(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

#[inline]
fn max2(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// `a * b` with `0 * inf = 0`.
#[inline]
fn mul0(a: f64, b: f64) -> f64 {
    if a == 0.0 || b == 0.0 { 0.0 } else { a * b }
}

impl Interval {
    /// Every real.
    pub const ENTIRE: Interval = Interval { lo: f64::NEG_INFINITY, hi: f64::INFINITY };

    /// `lo..=hi`.
    pub fn new(lo: f64, hi: f64) -> Result<Interval, IntervalError> {
        if lo.is_nan() || hi.is_nan() {
            return Err(IntervalError::Nan);
        }
        if lo > hi {
            return Err(IntervalError::Reversed { lo, hi });
        }
        Ok(Interval { lo, hi })
    }

    /// The one point `x` (`ENTIRE` for NaN).
    pub fn point(x: f64) -> Interval {
        if x.is_nan() { Interval::ENTIRE } else { Interval { lo: x, hi: x } }
    }

    /// The hull of two values in either order (`ENTIRE` if either is NaN).
    pub fn hull_of(a: f64, b: f64) -> Interval {
        if a.is_nan() || b.is_nan() { Interval::ENTIRE } else { Interval { lo: min2(a, b), hi: max2(a, b) } }
    }

    /// The low end.
    pub fn lo(self) -> f64 {
        self.lo
    }

    /// The high end.
    pub fn hi(self) -> f64 {
        self.hi
    }

    /// The midpoint (finite ends), or a finite point inside.
    pub fn mid(self) -> f64 {
        if self.lo.is_finite() && self.hi.is_finite() {
            let m = self.lo * 0.5 + self.hi * 0.5;
            max2(self.lo, min2(self.hi, m))
        } else if self.lo.is_finite() {
            self.lo
        } else if self.hi.is_finite() {
            self.hi
        } else {
            0.0
        }
    }

    /// Half the width, rounded up.
    pub fn radius(self) -> f64 {
        next_up((self.hi - self.lo) * 0.5)
    }

    /// Whether `x` lies in the interval.
    pub fn contains(self, x: f64) -> bool {
        self.lo <= x && x <= self.hi
    }

    /// Whether `o` lies inside this one.
    pub fn contains_interval(self, o: Interval) -> bool {
        self.lo <= o.lo && o.hi <= self.hi
    }

    /// The smallest interval holding both.
    pub fn hull(self, o: Interval) -> Interval {
        Interval { lo: min2(self.lo, o.lo), hi: max2(self.hi, o.hi) }
    }

    /// `self + o`.
    pub fn add(self, o: Interval) -> Interval {
        self.add_with(o, NoPlant)
    }

    /// `self + o` through the plant seam.
    pub fn add_with(self, o: Interval, plant: impl Planted<IntervalPlant>) -> Interval {
        let (lo, hi) = (self.lo + o.lo, self.hi + o.hi);
        if plant.has(IntervalPlant::AddNotWidened) {
            return Interval::hull_of(lo, hi);
        }
        widen(lo, hi)
    }

    /// `self - o`.
    pub fn sub(self, o: Interval) -> Interval {
        widen(self.lo - o.hi, self.hi - o.lo)
    }

    /// `-self` (exact).
    pub fn neg(self) -> Interval {
        Interval { lo: -self.hi, hi: -self.lo }
    }

    /// `self * o`.
    pub fn mul(self, o: Interval) -> Interval {
        self.mul_with(o, NoPlant)
    }

    /// `self * o` through the plant seam.
    pub fn mul_with(self, o: Interval, plant: impl Planted<IntervalPlant>) -> Interval {
        let a = mul0(self.lo, o.lo);
        let d = mul0(self.hi, o.hi);
        if plant.has(IntervalPlant::MulTwoCorners) {
            return widen(min2(a, d), max2(a, d));
        }
        let b = mul0(self.lo, o.hi);
        let c = mul0(self.hi, o.lo);
        widen(min2(min2(a, b), min2(c, d)), max2(max2(a, b), max2(c, d)))
    }

    /// `self * k` for a constant `k`.
    pub fn scale(self, k: f64) -> Interval {
        self.mul(Interval::point(k))
    }

    /// `self + k` for a constant `k`.
    pub fn shift(self, k: f64) -> Interval {
        self.add(Interval::point(k))
    }

    /// `self / o`; `ENTIRE` when `o` holds zero.
    pub fn div(self, o: Interval) -> Interval {
        if o.lo <= 0.0 && o.hi >= 0.0 {
            return Interval::ENTIRE;
        }
        let a = self.lo / o.lo;
        let b = self.lo / o.hi;
        let c = self.hi / o.lo;
        let d = self.hi / o.hi;
        widen(min2(min2(a, b), min2(c, d)), max2(max2(a, b), max2(c, d)))
    }

    /// `|self|` (exact).
    pub fn abs(self) -> Interval {
        if self.lo >= 0.0 {
            self
        } else if self.hi <= 0.0 {
            self.neg()
        } else {
            Interval { lo: 0.0, hi: max2(-self.lo, self.hi) }
        }
    }

    /// `self^2`, which is never below zero.
    pub fn square(self) -> Interval {
        let a = self.abs();
        let r = widen(a.lo * a.lo, a.hi * a.hi);
        Interval { lo: max2(r.lo, 0.0), hi: r.hi }
    }

    /// `sqrt` of the part of `self` at or above zero.
    pub fn sqrt(self) -> Interval {
        let lo = max2(self.lo, 0.0);
        let hi = max2(self.hi, 0.0);
        let r = widen(fns::sqrt(lo), fns::sqrt(hi));
        Interval { lo: max2(r.lo, 0.0), hi: r.hi }
    }

    /// The pointwise smaller (exact).
    pub fn min(self, o: Interval) -> Interval {
        Interval { lo: min2(self.lo, o.lo), hi: min2(self.hi, o.hi) }
    }

    /// The pointwise larger (exact).
    pub fn max(self, o: Interval) -> Interval {
        Interval { lo: max2(self.lo, o.lo), hi: max2(self.hi, o.hi) }
    }

    /// `round` (halves away from zero) of each point (exact, monotone).
    pub fn round(self) -> Interval {
        Interval { lo: fns::round(self.lo), hi: fns::round(self.hi) }
    }

    /// `floor` of each point (exact, monotone).
    pub fn floor(self) -> Interval {
        Interval { lo: fns::floor(self.lo), hi: fns::floor(self.hi) }
    }

    /// `e^x`: monotone, widened by 2 ulp, never below zero.
    pub fn exp(self) -> Interval {
        let r = widen_by(fns::exp(self.lo), fns::exp(self.hi), 2);
        Interval { lo: max2(r.lo, 0.0), hi: r.hi }
    }

    /// `ln` of the part above zero (`-inf` at the low end if it reaches zero).
    pub fn ln(self) -> Interval {
        let lo = if self.lo <= 0.0 { f64::NEG_INFINITY } else { fns::ln(self.lo) };
        let hi = if self.hi <= 0.0 { f64::NEG_INFINITY } else { fns::ln(self.hi) };
        widen_by(lo, hi, 2)
    }

    /// `x^k` for a constant `k > 0` on the part of `self` at or above zero (monotone there),
    /// widened by [`fns::pow`]'s bound.
    pub fn pow_const(self, k: f64) -> Interval {
        let lo = max2(self.lo, 0.0);
        let hi = max2(self.hi, 0.0);
        let plo = fns::pow(lo, k);
        let phi = fns::pow(hi, k);
        // pow's bound is 1 + |k ln x| ulp: widen by a relative amount that covers it.
        let slack = |x: f64, v: f64| {
            let l = if x > 0.0 { fns::ln(x).abs() } else { 0.0 };
            v * ((3.0 + k.abs() * l) * f64::EPSILON)
        };
        let r = widen(plo - slack(lo, plo), phi + slack(hi, phi));
        Interval { lo: max2(r.lo, 0.0), hi: r.hi }
    }

    /// `sin` of each point: `[-1, 1]` when the interval is wide or unbounded, else the ends'
    /// values widened by 2 ulp with each crest inside raising an end to `+-1`.
    pub fn sin(self) -> Interval {
        trig(self, 0.5)
    }

    /// `cos` of each point (as [`Interval::sin`], crests at whole turns).
    pub fn cos(self) -> Interval {
        trig(self, 0.0)
    }

    /// `atan2(y, x)` over the box `y = self`, `x = x`: `[-pi, pi]` when the box holds the origin
    /// or meets the cut along negative `x`, else the hull of the four corners' angles (the angle
    /// is monotone along each edge of such a box) widened by 3 ulp.
    pub fn atan2(self, x: Interval) -> Interval {
        let pi = Interval { lo: -PI_UP, hi: PI_UP };
        if x.lo <= 0.0 && self.lo <= 0.0 && self.hi >= 0.0 {
            return pi;
        }
        let a = fns::atan2(self.lo, x.lo);
        let b = fns::atan2(self.lo, x.hi);
        let c = fns::atan2(self.hi, x.lo);
        let d = fns::atan2(self.hi, x.hi);
        let r = widen_by(min2(min2(a, b), min2(c, d)), max2(max2(a, b), max2(c, d)), 3);
        Interval { lo: max2(r.lo, -PI_UP), hi: min2(r.hi, PI_UP) }
    }

    /// `if a < b { c } else { d }` over intervals: `c` where every `a` is below every `b`, `d`
    /// where none is, else the hull.
    pub fn select_lt(a: Interval, b: Interval, c: Interval, d: Interval) -> Interval {
        if a.hi < b.lo {
            c
        } else if a.lo >= b.hi {
            d
        } else {
            c.hull(d)
        }
    }
}

/// pi rounded up.
const PI_UP: f64 = 3.141_592_653_589_793_6;

/// sin (phase 1/2: crests at a quarter turn) or cos (phase 0: crests at whole turns) of an
/// interval, by its crest and trough positions in turns.
fn trig(x: Interval, phase: f64) -> Interval {
    let unit = Interval { lo: -1.0, hi: 1.0 };
    // An infinite end or a width of most of a turn: every value. The ends' point values are
    // within 1 ulp for every finite argument (the reduction is exact enough everywhere), so the
    // rest is sound at any magnitude; where the crest test's margin below exceeds a half turn
    // (|x| beyond about 1e12) it finds a crest and a trough and the answer is again `[-1, 1]`.
    if !(x.lo.is_finite() && x.hi.is_finite()) || x.hi - x.lo >= 6.0 {
        return unit;
    }
    let f = |v: f64| if phase == 0.0 { fns::cos(v) } else { fns::sin(v) };
    let (a, b) = (f(x.lo), f(x.hi));
    let mut r = widen_by(min2(a, b), max2(a, b), 2);
    // In half turns: a crest of sin at 1/2 + 2k, a trough at -1/2 + 2k; of cos at 2k and 1 + 2k.
    // The ends in half turns, widened by a margin above the product's rounding.
    let inv_pi = 0.318_309_886_183_790_7;
    let margin = 1e-12 * (1.0 + x.lo.abs().max(x.hi.abs()));
    let lo = x.lo * inv_pi - margin;
    let hi = x.hi * inv_pi + margin;
    let crest = if phase == 0.0 { 0.0 } else { 0.5 };
    let holds = |at: f64| {
        // The least `at + 2k` at or above `lo`.
        let k = fns::ceil((lo - at) * 0.5);
        at + 2.0 * k <= hi
    };
    if holds(crest) {
        r.hi = 1.0;
    }
    if holds(crest + 1.0) {
        r.lo = -1.0;
    }
    Interval { lo: max2(r.lo, -1.0), hi: min2(r.hi, 1.0) }
}
