//! blaze-math against the platform's libm (D183's deterministic floats, checked from outside the
//! crate): the kernel's every trigonometric and exponential value comes from blaze-math, so this
//! is where the CAD library learns whether those values are right.
//!
//! - THE POINT FUNCTIONS (`sin cos tan asin acos atan atan2 exp ln pow cbrt`) are within the ulp
//!   bound blaze-math's table states, plus one for the platform's own error, of the platform's
//!   value, for every input (trigonometry included at any magnitude: the reduction is
//!   Payne-Hanek's beyond `TRIG_MEDIUM_BELOW`); a zero result has the platform's sign; `sin`
//!   and `cos` are in `[-1, 1]` whatever the bound says. Inputs: zeros of both signs, subnormals
//!   by their bits, the multiples of `pi/2` (small and huge) and their neighbours a few ulp away,
//!   each function's range edges a few ulp either way, huge values, infinities, NaN, random
//!   bits, and log-uniform draws by the exponent field (so no draw collapses to zero).
//!   Control: the harness's `Nudge` (the value moved past the bound before the comparison),
//!   caught as `Ulps`.
//! - THE INTERVAL FORMS (`exp ln sin cos sqrt atan2 pow_const`) contain blaze-math's point value
//!   at every drawn point of the interval, and the platform's within one ulp. Control: `Shrunk`
//!   (the interval replaced by the value at its middle), caught as `NotContained`.

use core::f64::consts::FRAC_PI_2;
use core::fmt;
use std::format;

use blaze_math::{Interval, TRIG_MEDIUM_BELOW, ulp_distance};
use blaze_own::planted::Plants;
use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum F {
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
    Exp,
    Ln,
    Pow,
    Cbrt,
}

const FUNCTIONS: [F; 11] = [F::Sin, F::Cos, F::Tan, F::Asin, F::Acos, F::Atan, F::Atan2, F::Exp, F::Ln, F::Pow, F::Cbrt];

#[derive(Clone, PartialEq, Debug)]
enum MathBroken {
    /// blaze-math farther from the platform than the bound.
    Ulps { f: F, x: f64, y: f64, got: f64, want: f64, ulps: u64, bound: u64 },
    /// A zero of the other sign.
    SignOfZero { f: F, x: f64, y: f64, got: f64, want: f64 },
    /// `sin` or `cos` outside `[-1, 1]`.
    OutOfRange { f: F, x: f64, got: f64 },
    /// An interval form misses a point value.
    NotContained { f: &'static str, lo: f64, hi: f64, at: f64, value: f64, iv: [f64; 2] },
}

impl fmt::Display for MathBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// The harness's own planted bugs: they test the oracles, not blaze-math.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MathPlant {
    None,
    /// The value moved one ulp past the bound before it is compared.
    Nudge,
}

impl Plants for MathPlant {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HullPlant {
    None,
    /// The interval replaced by the value at its middle.
    Shrunk,
}

impl Plants for HullPlant {}

blaze_sim::plants! {
    impl MathPlant breaks MathBroken {
        None,
        Nudge => MathBroken::Ulps { .. },
    }
}

blaze_sim::plants! {
    impl HullPlant breaks MathBroken {
        None,
        Shrunk => MathBroken::NotContained { .. },
    }
}

fn blaze(f: F, x: f64, y: f64) -> f64 {
    match f {
        F::Sin => blaze_math::sin(x),
        F::Cos => blaze_math::cos(x),
        F::Tan => blaze_math::tan(x),
        F::Asin => blaze_math::asin(x),
        F::Acos => blaze_math::acos(x),
        F::Atan => blaze_math::atan(x),
        F::Atan2 => blaze_math::atan2(x, y),
        F::Exp => blaze_math::exp(x),
        F::Ln => blaze_math::ln(x),
        F::Pow => blaze_math::pow(x, y),
        F::Cbrt => blaze_math::cbrt(x),
    }
}

fn platform(f: F, x: f64, y: f64) -> f64 {
    match f {
        F::Sin => x.sin(),
        F::Cos => x.cos(),
        F::Tan => x.tan(),
        F::Asin => x.asin(),
        F::Acos => x.acos(),
        F::Atan => x.atan(),
        F::Atan2 => x.atan2(y),
        F::Exp => x.exp(),
        F::Ln => x.ln(),
        F::Pow => x.powf(y),
        F::Cbrt => x.cbrt(),
    }
}

/// blaze-math's stated bound (its table, rounded up) plus one ulp for the platform's own error.
fn bound(f: F, x: f64, y: f64) -> u64 {
    match f {
        F::Sin | F::Cos | F::Asin | F::Acos | F::Atan | F::Exp | F::Ln | F::Cbrt => 2,
        F::Tan => 4,
        F::Atan2 => 3,
        F::Pow => {
            // The table's bound for `pow` (blaze-math/src/lib.rs, with the double-double `ln_dd`):
            // under `1 + |y ln|x||` ulp, rounded up, plus the platform's one. Exact (0) where
            // IEEE 754 fixes the value or no rounding is left to differ in: `x^0 = 1` and
            // `1^y = 1` (whatever the other operand, NaN too); `y ln|x|` infinite or NaN (a zero
            // or infinite base or exponent, a NaN); and `|y ln|x|| > 750`, where the true value is
            // below half the least subnormal (`e^-750 < 2^-1075`) or above the largest finite
            // (`e^750 > 2^1024`), so both sides round to a signed zero or infinity. Below 750 the
            // bound is at most 752, so nothing saturates.
            let k = (y * x.abs().ln()).abs();
            if y == 0.0 || x == 1.0 || !k.is_finite() || k > 750.0 { 0 } else { (1.0 + k).ceil() as u64 + 1 }
        }
    }
}

/// Special inputs every function sees.
const SPECIAL: [f64; 30] = [
    0.0,
    -0.0,
    5e-324,
    -5e-324,
    2.225_073_858_507_200_9e-308,
    1e-310,
    f64::MIN_POSITIVE,
    1e-8,
    0.5,
    1.0,
    -1.0,
    FRAC_PI_2,
    core::f64::consts::PI,
    core::f64::consts::TAU,
    2.0,
    10.0,
    709.78,
    -745.1,
    1e10,
    1e22,
    1e300,
    f64::MAX,
    -f64::MAX,
    f64::INFINITY,
    f64::NEG_INFINITY,
    f64::NAN,
    TRIG_MEDIUM_BELOW,
    1_647_098.999_999_999_8,
    -3.0,
    0.999_999_999_999_999_9,
];

/// The places each function's method changes range, drawn and then moved a few ulp either way
/// (and given either sign): atan's 7/16, 11/16, 19/16, 39/16 and 2^66; asin's 0.5 and 0.975;
/// exp's overflow and underflow; the least normal; the medium reduction's end; ln's split at
/// `sqrt 2`; 2^-27 (musl's tiny cut for atan and sin).
const EDGES: [f64; 15] = [
    0.4375,
    0.6875,
    1.1875,
    2.4375,
    7.378_697_629_483_820_6e19,
    0.5,
    0.975,
    709.782_712_893_384,
    -745.133_219_101_941_1,
    2.225_073_858_507_201_4e-308,
    TRIG_MEDIUM_BELOW,
    core::f64::consts::SQRT_2,
    core::f64::consts::FRAC_1_SQRT_2,
    7.450_580_596_923_828e-9,
    1.0,
];

/// One input, drawn the way the case's kind says. Every kind but the working range builds its
/// value from bits, so shrinking moves toward a simpler value of the same kind, never to zero
/// by an underflowing product.
fn draw(tape: &Tape, f: F, label: &'static str) -> f64 {
    let sign = |tape: &Tape| if tape.coin("negative") { 1u64 << 63 } else { 0 };
    match tape.index(8, "kind") {
        0 => *tape.pick(&SPECIAL, label).unwrap_or(&0.0),
        1 => {
            // Near a multiple of pi/2, a few ulp away: small multiples and huge ones.
            let k = if tape.coin("huge multiple") {
                tape.int_in(1..=(1i64 << 62), "multiple of pi/2") as f64
            } else {
                tape.int_in(1..=1_000_000i64, "multiple of pi/2") as f64
            };
            let n = tape.int_in(-4..=4i64, "ulps away");
            let base = k * FRAC_PI_2;
            f64::from_bits(base.to_bits().wrapping_add_signed(n) | sign(tape))
        }
        2 => match f {
            F::Asin | F::Acos => tape.f64_in(-1.0..=1.0, label),
            F::Exp => tape.f64_in(-745.0..=709.0, label),
            F::Ln => tape.f64_in(0.0..=1e6, label),
            F::Pow => tape.f64_in(0.0..=10.0, label),
            _ => tape.f64_in(-100.0..=100.0, label),
        },
        3 => f64::from_bits(tape.below(u64::MAX, label)),
        4 => {
            // A subnormal (or a zero) by its bits.
            f64::from_bits(tape.below(1u64 << 52, label) | sign(tape))
        }
        5 => {
            let edge = *tape.pick(&EDGES, "edge").unwrap_or(&1.0);
            let n = tape.int_in(-8..=8i64, "ulps from the edge");
            f64::from_bits(edge.to_bits().wrapping_add_signed(n) ^ sign(tape))
        }
        6 => {
            // Large arguments for the reduction: 2^20 to 2^1024 by the exponent field.
            let e = tape.int_in(1043u64..=2046, "exponent field");
            f64::from_bits((e << 52) | tape.below(1u64 << 52, label) | sign(tape))
        }
        _ => {
            // Log-uniform over every normal: the exponent field and the significand's bits.
            let e = tape.int_in(1u64..=2046, "exponent field");
            f64::from_bits((e << 52) | tape.below(1u64 << 52, label) | sign(tape))
        }
    }
}

fn point_case(tape: &Tape, plant: impl Planted<MathPlant> + Copy) -> Result<(), Broken<MathBroken>> {
    let f = *tape.pick(&FUNCTIONS, "function").unwrap_or(&F::Sin);
    let x = draw(tape, f, "x");
    let y = if matches!(f, F::Atan2 | F::Pow) { draw(tape, f, "y") } else { 0.0 };
    let want = platform(f, x, y);
    let mut got = blaze(f, x, y);
    if matches!(f, F::Sin | F::Cos) && x.is_finite() && !(got.abs() <= 1.0) {
        return Err(Broken::Oracle(MathBroken::OutOfRange { f, x, got }));
    }
    let b = bound(f, x, y);
    if plant.has(MathPlant::Nudge) && got.is_finite() {
        for _ in 0..=b {
            got = blaze_math::next_up(got);
        }
    }
    let ulps = ulp_distance(got, want);
    if ulps > b {
        return Err(Broken::Oracle(MathBroken::Ulps { f, x, y, got, want, ulps, bound: b }));
    }
    if want == 0.0 && got.to_bits() != want.to_bits() {
        return Err(Broken::Oracle(MathBroken::SignOfZero { f, x, y, got, want }));
    }
    Ok(())
}

#[test]
fn blaze_math_agrees_with_the_platform() {
    check(Config::cases(20_000), |tape| point_case(tape, NoPlant));
}

#[test]
fn math_harness_plants_are_caught() {
    controls(Config::cases(200), |tape, plant: MathPlant| point_case(tape, plant));
}

/// The ends of an interval of the function's domain.
fn interval_of(tape: &Tape, f: &'static str) -> (f64, f64) {
    let (lo, hi) = match f {
        "exp" => (-700.0, 700.0),
        "ln" | "sqrt" | "pow_const" => (1e-300, 1e6),
        // Small arguments, and huge ones where the reduction is Payne-Hanek's.
        "sin" | "cos" if tape.coin("huge trigonometric argument") => (-1e300, 1e300),
        "sin" | "cos" => (-1e4, 1e4),
        _ => (-10.0, 10.0),
    };
    let a = tape.f64_in(lo..=hi, "low end");
    let wide = tape.index(3, "width kind");
    let w = match wide {
        0 => 0.0,
        1 => a.abs().max(1e-300) * 2f64.powi(-tape.int_in(1..=50i32, "relative width, binary digits")),
        _ => tape.f64_in(0.0..=10.0, "width"),
    };
    (a, (a + w).min(hi))
}

fn hull_case(tape: &Tape, plant: impl Planted<HullPlant> + Copy) -> Result<(), Broken<MathBroken>> {
    let f = *tape.pick(&["exp", "ln", "sin", "cos", "sqrt", "atan2", "pow_const"], "interval function").unwrap_or(&"exp");
    let (lo, hi) = interval_of(tape, f);
    let x = Interval::new(lo, hi).map_err(|e| Broken::Harness(format!("{e:?}")))?;
    // `pow_const` states `k > 0` (it is monotone increasing there); it does not refuse others.
    let k = *tape.pick(&[2.0, 3.0, 0.5, 1.5, 7.25], "exponent").unwrap_or(&2.0);
    // atan2's second operand: a box away from the cut half the time.
    let (xl, xh) = (tape.f64_in(-10.0..=10.0, "x low"), tape.f64_in(0.0..=3.0, "x width"));
    let other = Interval::new(xl, xl + xh).map_err(|e| Broken::Harness(format!("{e:?}")))?;
    let at_mid = |p: f64| -> (f64, f64) {
        match f {
            "exp" => (blaze_math::exp(p), p.exp()),
            "ln" => (blaze_math::ln(p), p.ln()),
            "sin" => (blaze_math::sin(p), p.sin()),
            "cos" => (blaze_math::cos(p), p.cos()),
            "sqrt" => (blaze_math::sqrt(p), p.sqrt()),
            "pow_const" => (blaze_math::pow(p, k), p.powf(k)),
            _ => (blaze_math::atan2(p, xl), p.atan2(xl)),
        }
    };
    let mut iv = match f {
        "exp" => x.exp(),
        "ln" => x.ln(),
        "sin" => x.sin(),
        "cos" => x.cos(),
        "sqrt" => x.sqrt(),
        "pow_const" => x.pow_const(k),
        _ => x.atan2(other),
    };
    if plant.has(HullPlant::Shrunk) {
        iv = Interval::point(at_mid(x.mid()).0);
    }
    let frac = tape.f64_in(0.0..=1.0, "fraction");
    for p in [lo, hi, x.mid(), lo + (hi - lo) * frac] {
        let p = p.clamp(lo, hi);
        let (value, platform) = at_mid(p);
        let near = |v: f64| iv.contains(v) || ulp_distance(v, iv.lo()) <= 1 || ulp_distance(v, iv.hi()) <= 1;
        if value.is_nan() {
            continue;
        }
        if !iv.contains(value) || !near(platform) {
            return Err(Broken::Oracle(MathBroken::NotContained { f, lo, hi, at: p, value, iv: [iv.lo(), iv.hi()] }));
        }
    }
    Ok(())
}

#[test]
fn intervals_contain_the_point_values() {
    check(Config::cases(5_000), |tape| hull_case(tape, NoPlant));
}

#[test]
fn interval_harness_plants_are_caught() {
    controls(Config::cases(200), |tape, plant: HullPlant| hull_case(tape, plant));
}
