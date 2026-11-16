//! x-int-07: u128 multiply, shift, compare and truncate (no division: that is its own rung).
//!
//! Exact arithmetic, step by step (2^64 = 18446744073709551616):
//! 1. (1 << 64) * 3 = 3 * 18446744073709551616 = 55340232221128654848.
//! 2. (2^64 - 1)^2 = 2^128 - 2^65 + 1
//!    = 340282366920938463463374607431768211456 - 36893488147419103232 + 1
//!    = 340282366920938463426481119284349108225.
//! 3. (1 << 100) >> 99 = 2.
//! 4. 2^64 > 2^64 - 1: true (a compare of the low halves alone says 0 > MAX: false).
//! 5. ((1 << 64) + 5) as u64: the Reference, "Numeric cast": "Casting from a larger integer to a
//!    smaller integer (e.g. u32 -> u8) will truncate": the low 64 bits, 5.
//! 6. core's `wrapping_add`: u128::MAX + 1 wraps to 0.
//! A wrong builder that drops the high half of a product exits 1 (0, not 3 * 2^64); one whose
//! high-half multiply misses the cross terms exits 2.

fn check(one: u128, m: u64) -> u8 {
    let two64 = one << 64;
    if two64 * 3 != 55340232221128654848 {
        return 1;
    }
    if m as u128 * m as u128 != 340282366920938463426481119284349108225 {
        return 2;
    }
    if (one << 100) >> 99 != 2 {
        return 3;
    }
    if !(two64 > m as u128) {
        return 4;
    }
    if (two64 + 5) as u64 != 5 {
        return 5;
    }
    if u128::MAX.wrapping_add(one) != 0 {
        return 6;
    }
    0
}

fn main() -> std::entry::Status {
    std::entry::Status(check(1, u64::MAX))
}
