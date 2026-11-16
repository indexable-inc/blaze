//! x-float-02: NaN is unordered: unequal to everything, itself included.
//!
//! IEEE 754-2008 section 5.11: "Every NaN shall compare unordered with everything, including
//! itself", and an unordered pair is neither less, equal nor greater; `!=` is true for it. The
//! Reference, "Machine types": `f32`/`f64` are IEEE 754-2008 binary32/binary64. 0.0 / 0.0 is
//! the invalid operation whose result is a NaN (section 7.2).
//! 1. n == n is false.  2. n != n is true.  3. n < 1.0 and n > 1.0 and n >= n are all false.
//! 4. core's `is_nan` is true.
//! 5. core's `f64::max`: "If one of the arguments is NaN, then the other argument is returned":
//!    n.max(1.0) = 1.0.
//! A wrong builder that compares the bit patterns as integers exits 1.

fn div(a: f64, b: f64) -> f64 {
    a / b
}

fn main() -> std::entry::Status {
    let n = div(0.0, 0.0);
    let status = if n == n {
        1
    } else if !(n != n) {
        2
    } else if n < 1.0 || n > 1.0 || n >= n {
        3
    } else if !n.is_nan() {
        4
    } else if n.max(1.0) != 1.0 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
