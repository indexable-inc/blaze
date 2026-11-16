//! x-int-04: shifts: left, logical right on unsigned, arithmetic right on signed.
//!
//! The Reference, "Arithmetic and Logical Binary Operators", `>>`: "Arithmetic right shift on
//! signed integer types, logical right shift on unsigned integer types." Arithmetic right shift
//! by n is floor division by 2^n (the sign bit is copied in).
//! 1. 1u32 << 31 = 2^31 = 2147483648.
//! 2. -16i32 >> 2 = -16 / 4 = -4.
//! 3. -7i32 >> 1 = floor(-3.5) = -4 (bits ..11111001 -> ..11111100). Toward zero would be -3.
//! 4. -1i32 >> 31 = -1 (all ones stay all ones).
//! 5. 0x8000_0000u32 >> 31 = 1 (zeros shifted in).
//! 6. core's `wrapping_shl`: "yields `self << mask(rhs)`, where `mask` removes any high-order
//!    bits of `rhs` that would cause the shift to exceed the bitwidth": 33 & 31 = 1, 1 << 1 = 2.
//! A wrong builder that shifts signed values logically exits 2 (0x3FFFFFFC, not -4).

fn shl(x: u32, n: u32) -> u32 {
    x << n
}

fn sar(x: i32, n: u32) -> i32 {
    x >> n
}

fn shr(x: u32, n: u32) -> u32 {
    x >> n
}

fn main() -> std::entry::Status {
    let status = if shl(1, 31) != 2147483648 {
        1
    } else if sar(-16, 2) != -4 {
        2
    } else if sar(-7, 1) != -4 {
        3
    } else if sar(-1, 31) != -1 {
        4
    } else if shr(0x8000_0000, 31) != 1 {
        5
    } else if 1u32.wrapping_shl(33) != 2 {
        6
    } else {
        0
    };
    std::entry::Status(status)
}
