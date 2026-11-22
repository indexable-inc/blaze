//! x-float-03: float to integer casts truncate toward zero and SATURATE; NaN gives 0.
//!
//! The Reference, "Type cast expressions", "Numeric cast": "Casting from a float to an integer
//! will round the float towards zero"; "NaN will return 0"; "Values larger than the maximum
//! integer value, including INFINITY, will saturate to the maximum value of the integer type";
//! "Values smaller than the minimum integer value, including NEG_INFINITY, will saturate to the
//! minimum value of the integer type".
//! 1. 1e10 as i32 = i32::MAX (1e10 > 2147483647).    2. -1e10 as i32 = i32::MIN.
//! 3. NaN as i32 = 0.                                  4. -1.5 as u8 = 0 (below u8's minimum).
//! 5. 300.7 as u8 = 255.                               6. -2.9 as i32 = -2; 2.9 as i32 = 2.
//! 7. INFINITY as u64 = u64::MAX.
//! A wrong builder that emits the bare hardware convert without clamping exits 1 or 3 (x86
//! gives 0x80000000 for both); one that rounds to nearest exits 6.

fn to_i32(x: f64) -> i32 {
    x as i32
}

fn to_u8(x: f64) -> u8 {
    x as u8
}

fn check(big: f64, zero: f64) -> u8 {
    if to_i32(big) != i32::MAX {
        return 1;
    }
    if to_i32(-big) != i32::MIN {
        return 2;
    }
    if to_i32(zero / zero) != 0 {
        return 3;
    }
    if to_u8(-1.5) != 0 {
        return 4;
    }
    if to_u8(300.7) != 255 {
        return 5;
    }
    if to_i32(-2.9) != -2 || to_i32(2.9) != 2 {
        return 6;
    }
    if (1.0 / zero) as u64 != u64::MAX {
        return 7;
    }
    0
}

fn main() -> std::entry::Status {
    std::entry::Status(check(1e10, 0.0))
}
