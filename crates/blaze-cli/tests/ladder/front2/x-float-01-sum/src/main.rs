//! x-float-01: binary floating point rounds each operation: 0.1 + 0.2 is not 0.3 in f64.
//!
//! The Reference, "Machine types": "`f32` and `f64` ... the IEEE 754-2008 binary32 and binary64
//! floating-point types". Each literal and each sum is rounded to nearest, ties to even:
//! f64: 0.1 = 0x3FB999999999999A, 0.2 = 0x3FC999999999999A; their exact sum rounds to
//! 0x3FD3333333333334, while 0.3 = 0x3FD3333333333333: one unit in the last place apart. In
//! [0.25, 0.5) that unit is 2^-2 * 2^-52 = 2^-54, and the difference of two floats this close
//! is exact (Sterbenz), so:
//! 1. 0.1 + 0.2 != 0.3.
//! 2. (0.1 + 0.2) - 0.3 = 2^-54, so times 2^54 (= 18014398509481984, exact) is 1.0.
//! f32: 0.1f32 = 0.100000001490116.., 0.2f32 = 0.200000002980232..; exact sum 0.300000004470348..;
//! the f32 neighbours are 0.29999998211860657 and 0.30000001192092896 (= 0.3f32, ulp 2^-25);
//! the sum is nearer the second, so:
//! 3. 0.1f32 + 0.2f32 == 0.3f32 (an f32 add done in f64 and not rounded back gives false).
//! A wrong builder that folds the decimal text exactly exits 1.

fn add64(a: f64, b: f64) -> f64 {
    a + b
}

fn add32(a: f32, b: f32) -> f32 {
    a + b
}

fn main() -> std::entry::Status {
    let s = add64(0.1, 0.2);
    let status = if s == 0.3 {
        1
    } else if (s - 0.3) * 18014398509481984.0 != 1.0 {
        2
    } else if add32(0.1, 0.2) != 0.3 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
