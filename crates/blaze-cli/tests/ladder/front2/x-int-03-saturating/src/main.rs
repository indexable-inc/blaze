//! x-int-03: saturating arithmetic clamps to the type's bounds.
//!
//! core's docs, `saturating_add`: "Saturating integer addition. Computes `self + rhs`,
//! saturating at the numeric bounds instead of overflowing." (`_sub`, `_mul` alike.)
//! 1. 250u8 + 10 = 260 > 255: 255.            2. 5u8 - 10 = -5 < 0: 0.
//! 3. i8::MIN - 1 = -129 < -128: -128.         4. 100i8 * 2 = 200 > 127: 127.
//! 5. -100i8 * 2 = -200 < -128: -128.          6. i64::MAX + 1 > i64::MAX: i64::MAX.
//! 7. 100i8 + 27 = 127 exactly: 127 (no clamp needed, the edge itself).
//! A wrong builder that wraps instead of clamping exits 1 (4, not 255).

fn check(a: u8, b: i8, c: i64) -> u8 {
    if a.saturating_add(10) != 255 {
        return 1;
    }
    if 5u8.saturating_sub(a - 240) != 0 {
        return 2;
    }
    if i8::MIN.saturating_sub(1) != -128 {
        return 3;
    }
    if b.saturating_mul(2) != 127 {
        return 4;
    }
    if (-b).saturating_mul(2) != -128 {
        return 5;
    }
    if c.saturating_add(1) != i64::MAX {
        return 6;
    }
    if b.saturating_add(27) != 127 {
        return 7;
    }
    0
}

fn main() -> std::entry::Status {
    std::entry::Status(check(250, 100, i64::MAX))
}
