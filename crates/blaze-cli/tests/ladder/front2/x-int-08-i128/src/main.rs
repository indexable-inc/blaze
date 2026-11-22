//! x-int-08: i128 sign: casts, multiply, compare and arithmetic shift across the halves.
//!
//! The Reference, "Numeric cast": same size is "a no-op (Rust uses 2's complement ...)";
//! smaller to larger "sign-extend if the source is signed". `>>`: "Arithmetic right shift on
//! signed integer types".
//! 1. -1i128 as u128: all 128 bits set = u128::MAX.
//! 2. -5i128 * 3 = -15.
//! 3. i128::MIN < 0: true (its high half is 0x8000.. ; read unsigned it would be huge).
//! 4. -1i64 as i128 = -1 (the high half filled with ones).
//! 5. -(2^70) >> 70 = -1 exactly (the shift crosses from the high half into the low).
//! 6. (-(2^70)) as u64: the low 64 bits of -(2^70) are 0 (2^70 is a multiple of 2^64).
//! A wrong builder that zero-extends the high half on a cast exits 4.

fn check(m1: i128, five: i128, small: i64) -> u8 {
    if m1 as u128 != u128::MAX {
        return 1;
    }
    if -five * 3 != -15 {
        return 2;
    }
    if !(i128::MIN < m1 + 1) {
        return 3;
    }
    if small as i128 != -1 {
        return 4;
    }
    let big = -(1i128 << 70);
    if big >> 70 != -1 {
        return 5;
    }
    if big as u64 != 0 {
        return 6;
    }
    0
}

fn main() -> std::entry::Status {
    std::entry::Status(check(-1, 5, -1))
}
