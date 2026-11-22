//! x-float-05: integer to float rounds to nearest, ties to even; f64 to f32 overflows to infinity.
//!
//! The Reference, "Numeric cast": "Casting from an integer to float will produce the closest
//! possible float", its footnote: "if necessary, rounding is according to roundTiesToEven
//! mode"; "Casting from an f64 to an f32 will produce the closest possible f32";
//! "on overflow, infinity (of the same sign as the input) is produced".
//! 1. 16777217 = 2^24 + 1 lies halfway between the f32 neighbours 2^24 and 2^24 + 2 (f32 has a
//!    24-bit significand); the tie goes to the even significand: 16777216.0.
//! 2. 16777219 = 2^24 + 3: halfway between 2^24 + 2 and 2^24 + 4; even is 2^24 + 4 = 16777220.0.
//! 3. u64::MAX = 2^64 - 1 as f32: nearest is 2^64 = 18446744073709551616.0.
//! 4. i64::MAX as f64 = 2^63 (nearest), and back to i64 saturates ("Values larger than the
//!    maximum integer value ... will saturate"): i64::MAX.
//! 5. 1e40f64 as f32: f32::MAX is about 3.4e38, so +infinity.
//! A wrong builder that truncates instead of rounding exits 2 (16777218.0).

fn to_f32(x: i32) -> f32 {
    x as f32
}

fn main() -> std::entry::Status {
    let big = 1e40f64;
    let status = if to_f32(16777217) != 16777216.0 {
        1
    } else if to_f32(16777219) != 16777220.0 {
        2
    } else if u64::MAX as f32 != 18446744073709551616.0 {
        3
    } else if (i64::MAX as f64) as i64 != i64::MAX {
        4
    } else if big as f32 != f32::INFINITY {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
