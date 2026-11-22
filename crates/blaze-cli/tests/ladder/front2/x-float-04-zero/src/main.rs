//! x-float-04: signed zero: -0.0 equals 0.0 but keeps its sign bit.
//!
//! IEEE 754-2008 section 5.11: "Comparisons shall ignore the sign of zero (so +0 = -0)";
//! section 6.3: division of a nonzero number by a zero gives an infinity whose sign is the
//! exclusive OR of the operands' signs. binary64 puts the sign in bit 63.
//! 1. -0.0 == 0.0.
//! 2. core's `is_sign_negative` of -0.0: true; of 0.0: false.
//! 3. 1.0 / -0.0 = -infinity; 1.0 / 0.0 = +infinity.
//! 4. core's `to_bits`: -0.0 is 0x8000_0000_0000_0000, 0.0 is 0.
//! 5. 0.0 * -1.0 = -0.0 (sign XOR), so its bits are 0x8000_0000_0000_0000.
//! A wrong builder that compares bit patterns exits 1; one that folds -0.0 to 0.0 exits 2.

fn neg(x: f64) -> f64 {
    -x
}

fn main() -> std::entry::Status {
    let z = 0.0f64;
    let nz = neg(z);
    let status = if nz != z {
        1
    } else if !nz.is_sign_negative() || z.is_sign_negative() {
        2
    } else if 1.0 / nz != f64::NEG_INFINITY || 1.0 / z != f64::INFINITY {
        3
    } else if nz.to_bits() != 0x8000_0000_0000_0000 || z.to_bits() != 0 {
        4
    } else if (z * -1.0).to_bits() != 0x8000_0000_0000_0000 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
