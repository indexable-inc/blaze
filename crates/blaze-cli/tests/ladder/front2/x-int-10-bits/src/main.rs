//! x-int-10: bitwise operators and core's bit-counting methods.
//!
//! The Reference, "Negation operators": `!` on integers is "Bitwise NOT"; in two's complement
//! !x = -x - 1. `&`, `|`, `^` are bitwise AND, OR, XOR.
//! 1. !0u8 = 0xFF = 255; !5i32 = -5 - 1 = -6.
//! 2. 0xF0 & 0x3C = 1111_0000 & 0011_1100 = 0011_0000 = 0x30.
//! 3. 0xF0 | 0x0F = 0xFF; 0xFF ^ 0x0F = 0xF0.
//! 4. core: `leading_zeros` of 1u32: bits 0..01, 31 zeros above; `trailing_zeros` of 0x80u8: 7.
//! 5. `count_ones` of u64::MAX: 64; of 0b1011u8: 3.
//! 6. `swap_bytes` of 0x1234u16: 0x3412; `rotate_left(1)` of 0x81u8 = 1000_0001 -> 0000_0011 = 3.
//! A wrong builder that negates (`-x`) for `!` exits 1 (-5, not -6).

fn check(z: u8, f: i32, hi: u8) -> u8 {
    if !z != 255 || !f != -6 {
        return 1;
    }
    if hi & 0x3C != 0x30 {
        return 2;
    }
    if hi | 0x0F != 0xFF || 0xFFu8 ^ 0x0F != hi {
        return 3;
    }
    if 1u32.leading_zeros() != 31 || 0x80u8.trailing_zeros() != 7 {
        return 4;
    }
    if u64::MAX.count_ones() != 64 || 0b1011u8.count_ones() != 3 {
        return 5;
    }
    if 0x1234u16.swap_bytes() != 0x3412 || 0x81u8.rotate_left(1) != 3 {
        return 6;
    }
    0
}

fn main() -> std::entry::Status {
    std::entry::Status(check(0, 5, 0xF0))
}
