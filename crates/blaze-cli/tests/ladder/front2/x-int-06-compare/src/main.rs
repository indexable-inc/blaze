//! x-int-06: comparisons read the operands' signedness; chains of comparisons combine.
//!
//! The Reference, "Comparison Operators", and core's `Ord` for the integer types: integers are
//! ordered by their mathematical value, so the same bits compare differently by signedness:
//! 1. -1i32 < 1i32 is true (as u32 the bits of -1 would be 4294967295, greater than 1).
//! 2. u32::MAX < 1u32 is false (as i32 its bits would be -1, less than 1).
//! 3. i8::MIN < i8::MAX: -128 < 127, true.
//! 4. -1i64 <= -1i64 and -1i64 >= -1i64: both true; -1i64 > -2i64: true.
//! 5. a chain: 3 < 5 && 5 <= 5 && 5 != 6 && !(6 < 5): true.
//! 6. the larger of (-3, 2) by `if a > b`: 2; core's `Ord::max` of (-3i16, 2i16): 2.
//! A wrong builder that compares signed integers as unsigned exits 1.

fn lt_i(a: i32, b: i32) -> bool {
    a < b
}

fn lt_u(a: u32, b: u32) -> bool {
    a < b
}

fn larger(a: i64, b: i64) -> i64 {
    if a > b { a } else { b }
}

fn check(m: i64) -> u8 {
    if !lt_i(-1, 1) {
        return 1;
    }
    if lt_u(u32::MAX, 1) {
        return 2;
    }
    if !(i8::MIN < i8::MAX) {
        return 3;
    }
    if !(m <= -1 && m >= -1 && m > -2) {
        return 4;
    }
    let (a, b, c) = (3, 5, 6);
    if !(a < b && b <= b && b != c && !(c < b)) {
        return 5;
    }
    if larger(-3, 2) != 2 || (-3i16).max(2) != 2 {
        return 6;
    }
    0
}

fn main() -> std::entry::Status {
    std::entry::Status(check(-1))
}
