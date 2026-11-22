//! x-int-09: division and remainder: toward zero, the sign of the dividend, unsigned divide.
//!
//! The Reference, "Arithmetic and Logical Binary Operators": "Integer division rounds towards
//! zero." and for `%`: "Rust uses a remainder defined with truncating division. Given
//! `remainder = dividend % divisor`, the remainder will have the same sign as the dividend."
//! 1. -7 / 2 = -3.5 toward zero = -3.        2. -7 % 2 = -7 - 2 * (-3) = -1.
//! 3. 7 % -2 = 7 - (-2) * (-3) = 1.          4. -7 / -2 = 3.5 toward zero = 3.
//! 5. core's `rem_euclid`: "the least nonnegative remainder": -7 = 2 * (-4) + 1: 1;
//!    `div_euclid`: -4.
//! 6. 4000000000u32 / 3 = 1333333333 (3 * 1333333333 = 3999999999), remainder 1. Read as i32
//!    the dividend is negative (-294967296): a signed divide gives -98322432.
//! A wrong builder that floors exits 1 (-4); one that divides u32 signed exits 6.

fn div(a: i32, b: i32) -> i32 {
    a / b
}

fn rem(a: i32, b: i32) -> i32 {
    a % b
}

fn check(big: u32) -> u8 {
    if div(-7, 2) != -3 {
        return 1;
    }
    if rem(-7, 2) != -1 {
        return 2;
    }
    if rem(7, -2) != 1 {
        return 3;
    }
    if div(-7, -2) != 3 {
        return 4;
    }
    if (-7i32).rem_euclid(2) != 1 || (-7i32).div_euclid(2) != -4 {
        return 5;
    }
    if big / 3 != 1333333333 || big % 3 != 1 {
        return 6;
    }
    0
}

fn main() -> std::entry::Status {
    std::entry::Status(check(4000000000))
}
