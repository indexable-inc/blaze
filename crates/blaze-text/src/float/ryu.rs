//! The shortest decimal that reads back as a float: Ryu (Ulf Adams, "Ryū: fast float-to-string
//! conversion", PLDI 2018), its `d2s` with 125-bit tables, which this module computes at
//! compile time from their definitions instead of carrying them as literals:
//!
//! - `POW5[i] = 5^i` scaled to exactly 125 bits (`5^i >> (bitlen(5^i) - 125)`, shifted left
//!   when shorter);
//! - `POW5_INV[i] = floor(2^(bitlen(5^i) - 1 + 125) / 5^i) + 1`.
//!
//! The same core serves `f32`: its mantissa is narrower than `f64`'s, which only shrinks the
//! error the tables' precision has to cover; the boundary and trailing-zero tests are written
//! for any mantissa below 2^55.

use blaze_int::{Int as _, idx};

use crate::big::{Big, pow5_u64};

const POW5_BITCOUNT: i32 = 125;
const POW5_INV_BITCOUNT: i32 = 125;
const POW5_LEN: usize = 326;
const POW5_INV_LEN: usize = 342;

pub(crate) static POW5: [u128; POW5_LEN] = pow5_table();
pub(crate) static POW5_INV: [u128; POW5_INV_LEN] = pow5_inv_table();

const fn pow5_table() -> [u128; POW5_LEN] {
    let mut out = [0u128; POW5_LEN];
    let mut pow = Big::from_u64(1);
    let mut i = 0;
    while i < POW5_LEN {
        let len = pow.bit_len();
        out[i] = if len >= 125 { pow.bits_at(len - 125) } else { pow.bits_at(0) << (125 - len) };
        pow.mul_small(5);
        i += 1;
    }
    out
}

const fn pow5_inv_table() -> [u128; POW5_INV_LEN] {
    let mut out = [0u128; POW5_INV_LEN];
    let mut pow = Big::from_u64(1);
    let mut i = 0;
    while i < POW5_INV_LEN {
        let len = pow.bit_len();
        let mut x = Big::pow2(len - 1 + 125);
        let mut left = blaze_int::Cast::<usize, u32>::truncate(i);
        while left >= 27 {
            x.div_small(pow5_u64(27));
            left -= 27;
        }
        if left > 0 {
            x.div_small(pow5_u64(left));
        }
        assert!(x.bits_at(128) == 0, "blaze_text::ryu: an inverse past 128 bits");
        out[i] = x.bits_at(0) + 1;
        pow.mul_small(5);
        i += 1;
    }
    out
}

/// The number of bits of `5^e` (1 for `e = 0`), for `0 <= e <= 1700`.
#[inline(always)]
pub(crate) const fn pow5bits(e: i32) -> i32 {
    ((e * 1_217_359) >> 19) + 1
}

/// `floor(e * log10(2))`, for `0 <= e <= 1650`.
#[inline(always)]
const fn log10_pow2(e: i32) -> i32 {
    (e * 78_913) >> 18
}

/// `floor(e * log10(5))`, for `0 <= e <= 2620`.
#[inline(always)]
const fn log10_pow5(e: i32) -> i32 {
    (e * 732_923) >> 20
}

#[inline]
fn pow5_factor(mut v: u64) -> i32 {
    let mut count = 0;
    while v > 0 && v % 5 == 0 {
        v /= 5;
        count += 1;
    }
    count
}

#[inline]
fn multiple_of_pow5(v: u64, p: i32) -> bool {
    pow5_factor(v) >= p
}

#[inline]
fn multiple_of_pow2(v: u64, p: i32) -> bool {
    v & ((1u64 << p.cast_unsigned()) - 1) == 0
}

/// `floor(m * mul / 2^j)`, for `64 <= j < 192`, `m < 2^55`, `mul < 2^126`.
#[inline(always)]
fn mul_shift(m: u64, mul: u128, j: i32) -> u64 {
    let lo = u128::from(mul.truncate_to::<u64>());
    let hi = mul >> 64;
    let b0 = u128::from(m) * lo;
    let b2 = u128::from(m) * hi;
    (((b0 >> 64) + b2) >> (j - 64).cast_unsigned()).truncate_to::<u64>()
}

fn table_index(i: i32) -> usize {
    idx(i.cast_unsigned())
}

/// The shortest `(digits, exponent)` with `digits * 10^exponent` reading back as the float of
/// `ieee_mantissa` and `ieee_exponent` (its fields; not zero, not infinite or NaN), the one
/// nearest the exact value among them, an exact tie to the larger (as `core::fmt` does).
pub(crate) fn shortest(ieee_mantissa: u64, ieee_exponent: u32, mantissa_bits: u32, bias: i32) -> (u64, i32) {
    let mbits = mantissa_bits.cast_signed();
    let (e2, m2) = match ieee_exponent {
        0 => (1 - bias - mbits - 2, ieee_mantissa),
        _ => (ieee_exponent.cast_signed() - bias - mbits - 2, (1u64 << mantissa_bits) | ieee_mantissa),
    };
    let accept_bounds = m2 & 1 == 0;

    // The value, its upper and lower halfway points, times 4 (two more bits).
    let mv = 4 * m2;
    let mm_shift = u64::from(ieee_mantissa != 0 || ieee_exponent <= 1);

    let mut vm_trailing_zeros = false;
    let mut vr_trailing_zeros = false;
    let (mut vr, mut vp, mut vm, e10);
    if e2 >= 0 {
        let q = log10_pow2(e2) - i32::from(e2 > 3);
        e10 = q;
        let k = POW5_INV_BITCOUNT + pow5bits(q) - 1;
        let i = -e2 + q + k;
        let mul = POW5_INV[table_index(q)];
        vr = mul_shift(4 * m2, mul, i);
        vp = mul_shift(4 * m2 + 2, mul, i);
        vm = mul_shift(4 * m2 - 1 - mm_shift, mul, i);
        if q <= 21 {
            // Only one of mp, mv and mm can be a multiple of 5, if any.
            if mv % 5 == 0 {
                vr_trailing_zeros = multiple_of_pow5(mv, q);
            } else if accept_bounds {
                vm_trailing_zeros = multiple_of_pow5(mv - 1 - mm_shift, q);
            } else {
                vp -= u64::from(multiple_of_pow5(mv + 2, q));
            }
        }
    } else {
        let q = log10_pow5(-e2) - i32::from(-e2 > 1);
        e10 = q + e2;
        let i = -e2 - q;
        let k = pow5bits(i) - POW5_BITCOUNT;
        let j = q - k;
        let mul = POW5[table_index(i)];
        vr = mul_shift(4 * m2, mul, j);
        vp = mul_shift(4 * m2 + 2, mul, j);
        vm = mul_shift(4 * m2 - 1 - mm_shift, mul, j);
        if q <= 1 {
            // mv = 4 * m2 has at least two trailing zero bits.
            vr_trailing_zeros = true;
            if accept_bounds {
                // mm = mv - 1 - mm_shift has one trailing zero bit when mm_shift is 1.
                vm_trailing_zeros = mm_shift == 1;
            } else {
                // mp = mv + 2 has at least one.
                vp -= 1;
            }
        } else if q < 63 {
            vr_trailing_zeros = multiple_of_pow2(mv, q);
        }
    }

    // The shortest digits in the interval.
    let mut removed = 0i32;
    let mut last_removed = 0u64;
    let output = if vm_trailing_zeros || vr_trailing_zeros {
        // The general case (under 1% of inputs).
        while vp / 10 > vm / 10 {
            vm_trailing_zeros &= vm % 10 == 0;
            vr_trailing_zeros &= last_removed == 0;
            last_removed = vr % 10;
            vr /= 10;
            vp /= 10;
            vm /= 10;
            removed += 1;
        }
        if vm_trailing_zeros {
            while vm % 10 == 0 {
                vr_trailing_zeros &= last_removed == 0;
                last_removed = vr % 10;
                vr /= 10;
                vp /= 10;
                vm /= 10;
                removed += 1;
            }
        }
        // An exact tie (`last_removed == 5` with nothing after it) rounds up, away from zero:
        // `core::fmt`'s Grisu picks the upper of two equally near candidates (its weeding
        // stops there), and this output is byte-equal to `{:?}`. Ryu's paper rounds to even.
        let _ = vr_trailing_zeros;
        vr + u64::from((vr == vm && (!accept_bounds || !vm_trailing_zeros)) || last_removed >= 5)
    } else {
        // The common case.
        let mut round_up = false;
        if vp / 100 > vm / 100 {
            round_up = vr % 100 >= 50;
            vr /= 100;
            vp /= 100;
            vm /= 100;
            removed += 2;
        }
        while vp / 10 > vm / 10 {
            round_up = vr % 10 >= 5;
            vr /= 10;
            vp /= 10;
            vm /= 10;
            removed += 1;
        }
        vr + u64::from(vr == vm || round_up)
    };
    (output, e10 + removed)
}
