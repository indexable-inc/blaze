//! A dev-only harness: the largest difference in ulp between each blaze-math function and the
//! platform's `std` one, on a fixed grid and on pseudo-random inputs (a fixed-seed xorshift, so
//! every run draws the same points). The platform is not the reference of truth (its libm has
//! its own error), so a difference here of the stated bound plus the platform's (glibc and
//! Apple's libm are under 1 ulp for these) is expected; anything larger is a bug to look at.
//! Run with `cargo run --release -p blaze-math --example ulp`.

use blaze_math::ulp_distance;

/// A fixed-seed xorshift64*: the same inputs every run.
struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in `lo..hi`.
    fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        lo + (hi - lo) * u
    }

    /// Any finite f64 by its bits.
    fn bits(&mut self) -> f64 {
        loop {
            let v = f64::from_bits(self.next());
            if v.is_finite() {
                return v;
            }
        }
    }
}

struct Worst {
    name: &'static str,
    ulps: u64,
    at: (f64, f64),
}

fn unary(name: &'static str, ours: fn(f64) -> f64, theirs: fn(f64) -> f64, lo: f64, hi: f64) -> Worst {
    let mut d = Draw(0x9e37_79b9_7f4a_7c15);
    let mut w = Worst { name, ulps: 0, at: (0.0, 0.0) };
    for i in 0..2_000_000u32 {
        let x = if i % 4 == 3 { d.bits() } else { d.uniform(lo, hi) };
        let (a, b) = (ours(x), theirs(x));
        let u = ulp_distance(a, b);
        if u > w.ulps && !(a.is_nan() && b.is_nan()) {
            w = Worst { name, ulps: u, at: (x, 0.0) };
        }
    }
    w
}

fn binary(name: &'static str, ours: fn(f64, f64) -> f64, theirs: fn(f64, f64) -> f64, lo: f64, hi: f64) -> Worst {
    let mut d = Draw(0x6a09_e667_f3bc_c908);
    let mut w = Worst { name, ulps: 0, at: (0.0, 0.0) };
    for _ in 0..2_000_000u32 {
        let (x, y) = (d.uniform(lo, hi), d.uniform(lo, hi));
        let (a, b) = (ours(x, y), theirs(x, y));
        let u = ulp_distance(a, b);
        if u > w.ulps && !(a.is_nan() && b.is_nan()) {
            w = Worst { name, ulps: u, at: (x, y) };
        }
    }
    w
}

fn main() {
    let rows = [
        unary("sin", blaze_math::sin::<f64>, f64::sin, -1e6, 1e6),
        unary("cos", blaze_math::cos::<f64>, f64::cos, -1e6, 1e6),
        unary("tan", blaze_math::tan::<f64>, f64::tan, -1e3, 1e3),
        unary("asin", blaze_math::asin::<f64>, f64::asin, -1.0, 1.0),
        unary("acos", blaze_math::acos::<f64>, f64::acos, -1.0, 1.0),
        unary("atan", blaze_math::atan::<f64>, f64::atan, -50.0, 50.0),
        unary("exp", blaze_math::exp::<f64>, f64::exp, -745.0, 709.0),
        unary("exp2", blaze_math::exp2::<f64>, f64::exp2, -1074.0, 1023.0),
        unary("ln", blaze_math::ln::<f64>, f64::ln, 0.0, 1e10),
        unary("log2", blaze_math::log2::<f64>, f64::log2, 0.0, 1e10),
        unary("cbrt", blaze_math::cbrt::<f64>, f64::cbrt, -1e9, 1e9),
        unary("sqrt", blaze_math::sqrt::<f64>, f64::sqrt, 0.0, 1e9),
        unary("floor", blaze_math::floor::<f64>, f64::floor, -1e17, 1e17),
        unary("round", blaze_math::round::<f64>, f64::round, -1e17, 1e17),
        binary("atan2", blaze_math::atan2::<f64>, f64::atan2, -10.0, 10.0),
        binary("hypot", blaze_math::hypot::<f64>, f64::hypot, -1e200, 1e200),
        binary("pow", blaze_math::pow::<f64>, f64::powf, 0.0, 8.0),
    ];
    println!("{:<8} {:>12}  at", "function", "worst ulp");
    for w in rows {
        println!("{:<8} {:>12}  {:e}, {:e}", w.name, w.ulps, w.at.0, w.at.1);
    }
}
