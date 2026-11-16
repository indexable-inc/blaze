//! x-int-01: wrapping arithmetic at the edges of u8, i8, i64 and u32.
//!
//! core's integer docs, `wrapping_add`: "Wrapping (modular) addition. Computes `self + rhs`,
//! wrapping around at the boundary of the type." (`wrapping_sub` and `wrapping_mul` alike.)
//! So each answer is the exact result modulo 2^bits, read back in the type (two's complement):
//! 1. 250u8 + 10 = 260; 260 - 256 = 4.
//! 2. 0u8 - 1 = -1; -1 + 256 = 255.
//! 3. i8::MAX + 1 = 127 + 1 = 128; 128 - 256 = -128 = i8::MIN.
//! 4. i64::MIN - 1 = -2^63 - 1; + 2^64 = 2^63 - 1 = i64::MAX.
//! 5. u32::MAX * 2 = 2^33 - 2; - 2^32 = 2^32 - 2 = u32::MAX - 1.
//! A wrong builder that adds at a wider width without truncating exits 1 (260 is not 4); one
//! that reads i8 as unsigned exits 3.

fn check(a: u8, b: i8, c: i64, d: u32) -> u8 {
    if a.wrapping_add(10) != 4 {
        return 1;
    }
    if 0u8.wrapping_sub(a - 249) != 255 {
        return 2;
    }
    if b.wrapping_add(1) != i8::MIN {
        return 3;
    }
    if c.wrapping_sub(1) != i64::MAX {
        return 4;
    }
    if d.wrapping_mul(2) != u32::MAX - 1 {
        return 5;
    }
    0
}

fn main() -> std::entry::Status {
    std::entry::Status(check(250, i8::MAX, i64::MIN, u32::MAX))
}
