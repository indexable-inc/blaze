//! The crate's promises, each with its oracle:
//!
//! - the exact functions (rounding, `abs`, `copysign`, `sqrt`) equal `core`'s intrinsics on
//!   drawn inputs, every bit;
//! - known values: exact where the method makes them exact, within the stated bound where not;
//! - symmetry (odd and even functions) and monotonicity on drawn inputs;
//! - every vector form equals its scalar twin lane for lane, bit for bit, at 1, 2, 4 and 8
//!   lanes, f64 and f32;
//! - [`Interval`] contains the exact result of `+ - * /` and `sqrt` on small drawn dyadic
//!   inputs, judged by exact integer arithmetic; a planted sum that is not widened and a planted
//!   product from two corners are each caught (controls);
//! - [`sin_cos_turns`] is exact at the quarter, eighth and twelfth turns.

use core::{fmt, simd::Simd};

use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};

use crate::{
    Interval, IntervalPlant, Real, TurnUnit, acos, asin, atan, atan2, cbrt, ceil, cos, exp, exp2, floor, hypot, ln, log2, next_down,
    next_up, pow, round, round_ties_even, sin, sin_cos_turns, sqrt, tan, trunc, ulp_distance,
};

const SPECIALS: [f64; 16] = [
    0.0,
    -0.0,
    1.0,
    -1.0,
    0.5,
    -0.5,
    f64::INFINITY,
    f64::NEG_INFINITY,
    f64::NAN,
    f64::MIN_POSITIVE,
    5e-324,
    -5e-324,
    1e300,
    -1e300,
    4_503_599_627_370_496.5,
    2.5,
];

/// A drawn f64: a special value, a value in a working range, or any bits.
fn draw(tape: &Tape) -> f64 {
    match tape.int_in(0u8..=3, "kind") {
        0 => *tape.one_of(&SPECIALS, "special"),
        1 => tape.f64_in(-10.0..=10.0, "small"),
        2 => tape.f64_in(-1e6..=1e6, "medium"),
        _ => tape.f64("any"),
    }
}

fn same(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

#[derive(Clone, Debug, PartialEq)]
enum MathBroken {
    /// A function differs from its reference.
    Differs { what: &'static str, x: f64, got: f64, want: f64 },
    /// A symmetry or order failed.
    Shape { what: &'static str, x: f64, y: f64 },
    /// An interval does not contain the exact result.
    Escapes { op: &'static str, a: (f64, f64), b: (f64, f64), got: (f64, f64) },
}

impl fmt::Display for MathBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MathBroken::Differs { what, x, got, want } => write!(f, "{what}({x:e}) = {got:e}, the reference says {want:e}"),
            MathBroken::Shape { what, x, y } => write!(f, "{what} fails at {x:e}, {y:e}"),
            MathBroken::Escapes { op, a, b, got } => write!(f, "{a:?} {op} {b:?} = {got:?} does not hold the exact result"),
        }
    }
}

#[test]
fn the_exact_functions_equal_cores_intrinsics() {
    check(Config::cases(4000), |tape| {
        let x = draw(tape);
        let y = draw(tape);
        let pairs: [(&'static str, f64, f64); 7] = [
            ("trunc", trunc(x), core::f64::math::trunc(x)),
            ("floor", floor(x), core::f64::math::floor(x)),
            ("ceil", ceil(x), core::f64::math::ceil(x)),
            ("round", round(x), core::f64::math::round(x)),
            ("round_ties_even", round_ties_even(x), core::f64::math::round_ties_even(x)),
            ("sqrt", sqrt(x), core::f64::math::sqrt(x)),
            ("copysign", crate::copysign(x, y), x.copysign(y)),
        ];
        for (what, got, want) in pairs {
            if !same(got, want) {
                return Err(Broken::Oracle(MathBroken::Differs { what, x, got, want }));
            }
        }
        Ok(())
    });
}

#[test]
fn known_values() {
    assert_eq!(sin(0.0), 0.0);
    assert_eq!(cos(0.0), 1.0);
    assert_eq!(exp(0.0), 1.0);
    assert_eq!(ln(1.0), 0.0);
    assert_eq!(log2(8.0), 3.0);
    assert_eq!(log2(0.5), -1.0);
    assert_eq!(exp2(10.0), 1024.0);
    assert_eq!(exp2(-1074.0), 5e-324);
    assert_eq!(hypot(3.0, 4.0), 5.0);
    assert_eq!(atan(1.0), core::f64::consts::FRAC_PI_4);
    assert_eq!(asin(1.0), core::f64::consts::FRAC_PI_2);
    assert_eq!(acos(1.0), 0.0);
    assert_eq!(acos(-1.0), core::f64::consts::PI);
    assert_eq!(atan2(0.0, -1.0), core::f64::consts::PI);
    assert_eq!(atan2(-0.0, -1.0), -core::f64::consts::PI);
    assert_eq!(atan2(1.0, 0.0), core::f64::consts::FRAC_PI_2);
    assert_eq!(pow(2.0, 0.0), 1.0);
    assert_eq!(pow(-2.0, f64::INFINITY), f64::INFINITY);
    assert!(pow(-2.0, 0.5).is_nan());
    assert_eq!(pow(0.0, -1.0), f64::INFINITY);
    assert_eq!(pow(-0.0, -1.0), f64::NEG_INFINITY);
    assert_eq!(cbrt(-0.0).to_bits(), (-0.0f64).to_bits());
    assert_eq!(exp(1000.0), f64::INFINITY);
    assert_eq!(exp(-1000.0), 0.0);
    assert_eq!(ln(0.0), f64::NEG_INFINITY);
    assert!(ln(-1.0).is_nan());
    // Within the stated bound of the correctly rounded constants.
    let near = |got: f64, want: f64, ulps: u64| assert!(ulp_distance(got, want) <= ulps, "{got:e} against {want:e}");
    near(exp(1.0), core::f64::consts::E, 1);
    near(ln(core::f64::consts::E), 1.0, 1);
    near(sin(core::f64::consts::FRAC_PI_6), 0.5, 1);
    // Not 0.5: `FRAC_PI_3` (0x1.0c152382d7366p0) lies 1.07e-16 above pi/3, so its cosine is
    // 0.49999999999999990715..., whose correctly rounded value is 0.5 - 2^-53: two steps under 0.5,
    // because the spacing halves below 0.5. (`FRAC_PI_6` lies above pi/6 too, and its sine rounds
    // to 0.5 exactly.)
    near(cos(core::f64::consts::FRAC_PI_3), 0.499_999_999_999_999_9, 1);
    near(tan(core::f64::consts::FRAC_PI_4), 1.0, 3);
    near(cbrt(27.0), 3.0, 1);
    near(pow(2.0, 10.0), 1024.0, 12);
    near(pow(10.0, -2.0), 0.01, 6);
}

#[test]
fn whole_fractions_of_a_turn_are_exact() {
    let unit = TurnUnit::new(1_296_000_000_000).unwrap();
    let q = 324_000_000_000;
    let rt = 0.707_106_781_186_547_6;
    let r3 = 0.866_025_403_784_438_6;
    let cases: [(i64, (f64, f64)); 10] = [
        (0, (0.0, 1.0)),
        (q, (1.0, 0.0)),
        (2 * q, (0.0, -1.0)),
        (3 * q, (-1.0, 0.0)),
        (-q, (-1.0, 0.0)),
        (q / 2, (rt, rt)),
        (q / 3, (0.5, r3)),
        (2 * q / 3, (r3, 0.5)),
        (5 * q, (1.0, 0.0)),
        (-7 * q / 2, (rt, rt)),
    ];
    for (n, (s, c)) in cases {
        let (gs, gc) = sin_cos_turns(n, unit);
        assert_eq!((gs.to_bits(), gc.to_bits()), (s.to_bits(), c.to_bits()), "{n} parts");
    }
    assert!(TurnUnit::new(6).is_err());
}

#[test]
fn odd_even_and_ordered_on_drawn_inputs() {
    check(Config::cases(4000), |tape| {
        let x = tape.f64_in(-1e5..=1e5, "x");
        let y = tape.f64_in(-1e5..=1e5, "y");
        let odd: [(&'static str, fn(f64) -> f64); 5] = [("sin", sin), ("atan", atan), ("cbrt", cbrt), ("tan", tan), ("asin", asin)];
        for (what, f) in odd {
            let v = if what == "asin" { x / 1e5 } else { x };
            if !same(f(-v), -f(v)) {
                return Err(Broken::Oracle(MathBroken::Shape { what, x: v, y: -v }));
            }
        }
        if !same(cos(-x), cos(x)) {
            return Err(Broken::Oracle(MathBroken::Shape { what: "cos even", x, y: -x }));
        }
        let (lo, hi) = if x <= y { (x, y) } else { (y, x) };
        let ordered: [(&'static str, fn(f64) -> f64, f64, f64); 5] = [
            ("exp", exp, lo / 1e3, hi / 1e3),
            ("exp2", exp2, lo / 1e3, hi / 1e3),
            ("ln", ln, lo.abs().min(hi.abs()), lo.abs().max(hi.abs())),
            ("atan", atan, lo, hi),
            ("cbrt", cbrt, lo, hi),
        ];
        for (what, f, a, b) in ordered {
            if f(a) > f(b) {
                return Err(Broken::Oracle(MathBroken::Shape { what, x: a, y: b }));
            }
        }
        Ok(())
    });
}

macro_rules! lanes_equal_scalar {
    ($tape:ident, $n:literal, [$($f:ident),*], [$($g:ident),*]) => {{
        let xs: [f64; $n] = core::array::from_fn(|_| draw($tape));
        let ys: [f64; $n] = core::array::from_fn(|_| draw($tape));
        let v = Simd::<f64, $n>::from_array(xs);
        let w = Simd::<f64, $n>::from_array(ys);
        $(
            let lanes = $f::<Simd<f64, $n>>(v).to_array();
            for i in 0..$n {
                let s = $f::<f64>(xs[i]);
                if !same(lanes[i], s) {
                    return Err(Broken::Oracle(MathBroken::Differs { what: stringify!($f), x: xs[i], got: lanes[i], want: s }));
                }
            }
        )*
        $(
            let lanes = $g::<Simd<f64, $n>>(v, w).to_array();
            for i in 0..$n {
                let s = $g::<f64>(xs[i], ys[i]);
                if !same(lanes[i], s) {
                    return Err(Broken::Oracle(MathBroken::Differs { what: stringify!($g), x: xs[i], got: lanes[i], want: s }));
                }
            }
        )*
    }};
}

fn lanes_case(tape: &Tape) -> Result<(), Broken<MathBroken>> {
    use crate::fns::{abs, copysign};
    lanes_equal_scalar!(tape, 1, [sin, cos, tan, asin, acos, atan, exp, exp2, ln, log2, cbrt, sqrt, floor, ceil, round, trunc, abs], [atan2, pow, hypot, copysign]);
    lanes_equal_scalar!(tape, 2, [sin, cos, tan, asin, acos, atan, exp, exp2, ln, log2, cbrt, sqrt, floor, ceil, round, trunc, abs], [atan2, pow, hypot, copysign]);
    lanes_equal_scalar!(tape, 4, [sin, cos, tan, asin, acos, atan, exp, exp2, ln, log2, cbrt, sqrt, floor, ceil, round, trunc, abs], [atan2, pow, hypot, copysign]);
    lanes_equal_scalar!(tape, 8, [sin, cos, tan, asin, acos, atan, exp, exp2, ln, log2, cbrt, sqrt, floor, ceil, round, trunc, abs], [atan2, pow, hypot, copysign]);
    // The f32 forms.
    let xs: [f32; 4] = core::array::from_fn(|_| draw(tape) as f32);
    let v = Simd::<f32, 4>::from_array(xs);
    let pairs: [(&'static str, [f32; 4], fn(f32) -> f32); 4] = [
        ("sinf", crate::sinf_lanes(v).to_array(), crate::sinf),
        ("expf", crate::expf_lanes(v).to_array(), crate::expf),
        ("lnf", crate::lnf_lanes(v).to_array(), crate::lnf),
        ("atanf", crate::atanf_lanes(v).to_array(), crate::atanf),
    ];
    for (what, lanes, f) in pairs {
        for i in 0..4 {
            let s = f(xs[i]);
            if !same(f64::from(lanes[i]), f64::from(s)) {
                return Err(Broken::Oracle(MathBroken::Differs { what, x: f64::from(xs[i]), got: f64::from(lanes[i]), want: f64::from(s) }));
            }
        }
    }
    Ok(())
}

#[test]
fn every_vector_form_equals_its_scalar_twin_bit_for_bit() {
    check(Config::cases(1500), lanes_case);
}

/// An exact dyadic rational `m * 2^e`, for the containment oracle.
#[derive(Clone, Copy, Debug)]
struct Dyadic {
    m: i128,
    e: i32,
}

impl Dyadic {
    fn of(x: f64) -> Dyadic {
        if x == 0.0 {
            return Dyadic { m: 0, e: 0 };
        }
        let b = x.to_bits();
        let sign = if b >> 63 == 1 { -1 } else { 1 };
        let ex = i32::try_from((b >> 52) & 0x7ff).unwrap();
        let frac = i128::from(b & ((1 << 52) - 1));
        let (m, e) = if ex == 0 { (frac, -1074) } else { (frac | (1 << 52), ex - 1075) };
        Dyadic { m: sign * m, e }
    }

    fn add(self, o: Dyadic) -> Dyadic {
        let e = self.e.min(o.e);
        Dyadic { m: (self.m << (self.e - e)) + (o.m << (o.e - e)), e }
    }

    fn neg(self) -> Dyadic {
        Dyadic { m: -self.m, e: self.e }
    }

    fn mul(self, o: Dyadic) -> Dyadic {
        Dyadic { m: self.m * o.m, e: self.e + o.e }
    }

    /// The position of the top bit of `|m|` plus `e`: which binade the value is in.
    fn top(self) -> i64 {
        let bits = 128 - i64::from(self.m.unsigned_abs().leading_zeros());
        bits + i64::from(self.e)
    }

    /// `self <= o` exactly, for two values in one binade (the shift is then under 107 bits).
    fn le_same_binade(self, o: Dyadic) -> bool {
        self.add(o.neg()).m <= 0
    }
}

/// `a <= b` exactly: by sign and binade first, so the subtraction only meets values of one
/// binade and no shift overflows an i128.
fn dyadic_le(a: Dyadic, b: Dyadic) -> bool {
    let sa = a.m.signum();
    let sb = b.m.signum();
    if sa != sb {
        return sa < sb;
    }
    if sa == 0 {
        return true;
    }
    let (ta, tb) = (a.top(), b.top());
    if ta != tb {
        return if sa > 0 { ta < tb } else { ta > tb };
    }
    a.le_same_binade(b)
}

fn containment(tape: &Tape, plant: impl Planted<IntervalPlant>) -> Result<(), Broken<MathBroken>> {
    // Two kinds of end. Short: a 16-bit integer times 2^-12..=2^12; two of them add and
    // multiply EXACTLY, so alone they never show a missing outward step (that is how the
    // planted unwidened sum once escaped every case). Full: a whole 53-bit significand times
    // 2^-60..=2^8, so a sum across magnitudes or a product almost always rounds, and the planted
    // ends then miss the exact value on one side about half the time. Every exact value still
    // fits an i128: a Dyadic's exponent stays in -64..=8, so a sum shifts a 53-bit significand
    // by at most 72 bits (126 bits with the carry), and a product is 106 bits.
    let small = |t: &Tape| {
        if t.coin("full significand") {
            let m = t.int_in((1i64 << 52)..=((1i64 << 53) - 1), "m53");
            let e = t.int_in(-60i32..=8, "e53");
            let v = m as f64 * crate::exp2(f64::from(e));
            if t.coin("negative") { -v } else { v }
        } else {
            let m = t.int_in(-(1i64 << 16)..=(1 << 16), "m");
            let e = t.int_in(-12i32..=12, "e");
            m as f64 * crate::exp2(f64::from(e))
        }
    };
    let ends = |t: &Tape| {
        let a = small(t);
        let b = small(t);
        if a <= b { (a, b) } else { (b, a) }
    };
    let (alo, ahi) = ends(tape);
    let (blo, bhi) = ends(tape);
    let a = Interval::new(alo, ahi).unwrap();
    let b = Interval::new(blo, bhi).unwrap();
    // A point of each: an end, or another small dyadic clamped into the interval.
    let pick = |t: &Tape, lo: f64, hi: f64| match t.int_in(0u8..=2, "at") {
        0 => lo,
        1 => hi,
        _ => small(t).clamp(lo, hi),
    };
    let x = pick(tape, alo, ahi);
    let y = pick(tape, blo, bhi);
    let (dx, dy) = (Dyadic::of(x), Dyadic::of(y));
    let inside = |r: Interval, exact: Dyadic| dyadic_le(Dyadic::of(r.lo()), exact) && dyadic_le(exact, Dyadic::of(r.hi()));
    let escapes = |op, got: Interval| MathBroken::Escapes { op, a: (alo, ahi), b: (blo, bhi), got: (got.lo(), got.hi()) };
    let sum = a.add_with(b, plant);
    if !inside(sum, dx.add(dy)) {
        return Err(Broken::Oracle(escapes("+", sum)));
    }
    let diff = a.sub(b);
    if !inside(diff, dx.add(dy.neg())) {
        return Err(Broken::Oracle(escapes("-", diff)));
    }
    let prod = a.mul_with(b, plant);
    if !inside(prod, dx.mul(dy)) {
        return Err(Broken::Oracle(escapes("*", prod)));
    }
    // x / y in q exactly when lo * y <= x <= hi * y (y > 0) or the reverse (y < 0).
    let q = a.div(b);
    if y != 0.0 && q.lo().is_finite() && q.hi().is_finite() {
        let (l, h) = (Dyadic::of(q.lo()).mul(dy), Dyadic::of(q.hi()).mul(dy));
        let ok = if y > 0.0 { dyadic_le(l, dx) && dyadic_le(dx, h) } else { dyadic_le(h, dx) && dyadic_le(dx, l) };
        if !ok {
            return Err(Broken::Oracle(escapes("/", q)));
        }
    }
    // sqrt x in r exactly when lo^2 <= x <= hi^2 (x >= 0).
    if x >= 0.0 {
        let r = a.sqrt();
        let (l, h) = (Dyadic::of(r.lo()), Dyadic::of(r.hi()));
        if !(dyadic_le(l.mul(l), dx) && dyadic_le(dx, h.mul(h))) {
            return Err(Broken::Oracle(escapes("sqrt", r)));
        }
    }
    Ok(())
}

blaze_sim::plants! {
    impl IntervalPlant breaks MathBroken {
        None,
        AddNotWidened => MathBroken::Escapes { op: "+", .. },
        MulTwoCorners => MathBroken::Escapes { op: "*", .. },
    }
}

#[test]
fn intervals_hold_the_exact_result() {
    check(Config::cases(6000), |tape| containment(tape, NoPlant));
}

#[test]
fn every_interval_plant_is_caught_by_its_invariant() {
    controls(Config::cases(6000), |tape, plant: IntervalPlant| containment(tape, plant));
}

#[test]
fn next_up_and_down_step_one_value() {
    assert_eq!(next_up(1.0), 1.0 + f64::EPSILON);
    assert_eq!(next_down(1.0), 1.0 - f64::EPSILON / 2.0);
    assert_eq!(next_up(-0.0), 5e-324);
    assert_eq!(next_down(0.0), -5e-324);
    assert_eq!(next_up(f64::INFINITY), f64::INFINITY);
    assert_eq!(next_up(f64::MAX), f64::INFINITY);
    assert_eq!(<f64 as Real>::splat(2.0), 2.0);
}
