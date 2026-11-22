//! x-int-05: integer casts: same size, truncation, zero and sign extension.
//!
//! The Reference, "Type cast expressions", "Numeric cast": "Casting between two integers of the
//! same size (e.g. i32 -> u32) is a no-op (Rust uses 2's complement for negative values of fixed
//! integers)"; "Casting from a larger integer to a smaller integer (e.g. u32 -> u8) will
//! truncate"; "Casting from a smaller integer to a larger integer (e.g. u8 -> u32) will
//! zero-extend if the source is unsigned; sign-extend if the source is signed".
//! 1. -1i8 as u8: bits 0xFF = 255.             2. 300i32 as u8: 300 - 256 = 44.
//! 3. u64::MAX as i32: low 32 bits 0xFFFF_FFFF = -1.
//! 4. -1i8 as i64: sign-extended = -1.        5. -1i8 as u64: 0xFFFF_FFFF_FFFF_FFFF = u64::MAX.
//! 6. 128u8 as i32: zero-extended = 128.       7. 255u8 as i8: bits 0xFF = -1.
//! 8. -128i8 as u32: 0xFFFF_FF80 = 4294967296 - 128 = 4294967168.
//! 9. 0x1_2345_6789i64 as u16: low 16 bits = 0x6789.
//! A wrong builder that zero-extends signed values exits 4 (255, not -1).

fn check(m1: i8, big: i32, max: u64, b: u8, wide: i64) -> u8 {
    if m1 as u8 != 255 {
        return 1;
    }
    if big as u8 != 44 {
        return 2;
    }
    if max as i32 != -1 {
        return 3;
    }
    if m1 as i64 != -1 {
        return 4;
    }
    if m1 as u64 != u64::MAX {
        return 5;
    }
    if b as i32 != 128 {
        return 6;
    }
    if (b + 127) as i8 != -1 {
        return 7;
    }
    if (m1 * 127 - 1) as u32 != 4294967168 {
        return 8;
    }
    if wide as u16 != 0x6789 {
        return 9;
    }
    0
}

fn main() -> std::entry::Status {
    std::entry::Status(check(-1, 300, u64::MAX, 128, 0x1_2345_6789))
}
