//! The crate's constants and functions against values computed exactly, in integers, inside the
//! test: no platform libm and nothing recalled.
//!
//! - THE TABLE: [`fns::TWO_OVER_PI`] is `floor(2/pi * 2^1344)`, recomputed from Machin's formula
//!   (`pi = 16 atan(1/5) - 4 atan(1/239)`, each series in 1536-bit fixed point with floored
//!   terms) and a bit-by-bit long division, every word compared.
//! - THE SPLITS: every split constant has the bit count its exact products need and sums to its
//!   value within the stated distance (pi/2 in four parts, 2/pi, ln 2 in two and in a double
//!   plus tail, 1/ln 2 rounded and in two parts), judged exactly in that fixed point.
//! - THE FUNCTIONS: `sin cos tan exp ln atan asin acos` at drawn arguments (huge trigonometric
//!   ones included, so Payne-Hanek's path) are within their stated bound in ulp of an exact
//!   value: 256-bit fixed point, the argument reduced with the recomputed 2/pi, Taylor series for
//!   sine, cosine and the exponential, `2 atanh` for the logarithm, Euler's series for the
//!   arctangent, a digit-by-digit square root for asin. Control: the value moved away from the exact
//!   one by twice the bound plus two ulp (`Nudge`), caught as `Ulps`.
//! - THE REDUCTION: `rem_pio2` returns `n` with `n mod 4` the quadrant of the nearest (or, at a
//!   rounding tie, the adjacent) multiple of pi/2, and `y0 + y1` within `2^-100 |y| + 2^-130` of
//!   the exact remainder, on small arguments, near multiples of pi/2 small and huge, huge
//!   arguments, and `6381956970095103 * 2^797` (the double closest to a multiple of pi/2).
//!   Controls: the tail `y1` dropped (caught as `Reduced`), `n` off by one (`Quadrant`).

use core::{cmp::Ordering, f64::consts::FRAC_PI_2, fmt};

use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};

use crate::fns::{
    self, INVLN2, INVPIO2, IVLN2_HI, IVLN2_LO, LN2, LN2_HI, LN2_LO, LN2_TAIL, PIO2_1, PIO2_2, PIO2_3, PIO2_3T, PIO2_HI, PIO2_LO,
};
use crate::{acos, asin, atan, cos, exp, ln, next_down, next_up, sin, tan};

// ---- word arithmetic on little-endian slices ----------------------------------------------------

/// `a += b` (`b` may be shorter); the carry out.
fn sl_add(a: &mut [u64], b: &[u64]) -> u64 {
    let mut carry = 0u64;
    for (i, w) in a.iter_mut().enumerate() {
        let bi = b.get(i).copied().unwrap_or(0);
        let (s1, c1) = w.overflowing_add(bi);
        let (s2, c2) = s1.overflowing_add(carry);
        *w = s2;
        carry = u64::from(c1) + u64::from(c2);
    }
    carry
}

/// `a -= b` for `a >= b` (`b` may be shorter).
fn sl_sub(a: &mut [u64], b: &[u64]) {
    let mut borrow = 0u64;
    for (i, w) in a.iter_mut().enumerate() {
        let bi = b.get(i).copied().unwrap_or(0);
        let (d1, o1) = w.overflowing_sub(bi);
        let (d2, o2) = d1.overflowing_sub(borrow);
        *w = d2;
        borrow = u64::from(o1) + u64::from(o2);
    }
    assert_eq!(borrow, 0, "a subtraction went below zero");
}

/// The order of two numbers of any lengths.
fn sl_cmp(a: &[u64], b: &[u64]) -> Ordering {
    for i in (0..a.len().max(b.len())).rev() {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x.cmp(&y);
        }
    }
    Ordering::Equal
}

/// `a = 2 a + bit_in`; the bit shifted out.
fn sl_shl1(a: &mut [u64], bit_in: u64) -> u64 {
    let mut c = bit_in;
    for w in a.iter_mut() {
        let out = *w >> 63;
        *w = (*w << 1) | c;
        c = out;
    }
    c
}

/// Bit `i` of `a` (0 beyond its end).
fn sl_bit(a: &[u64], i: usize) -> u64 {
    a.get(i / 64).map_or(0, |w| (w >> (i % 64)) & 1)
}

/// The number of bits up to the top set one (0 for zero).
fn sl_bit_len(a: &[u64]) -> u32 {
    for (i, &w) in a.iter().enumerate().rev() {
        if w != 0 {
            return 64 * i as u32 + 64 - w.leading_zeros();
        }
    }
    0
}

fn sl_is_zero(a: &[u64]) -> bool {
    a.iter().all(|&w| w == 0)
}

/// `out = a >> by` (as many words as `out` holds).
fn sl_shr_into(a: &[u64], by: u32, out: &mut [u64]) {
    let (w, b) = ((by / 64) as usize, by % 64);
    for (i, o) in out.iter_mut().enumerate() {
        let lo = a.get(i + w).copied().unwrap_or(0);
        let hi = if b == 0 { 0 } else { a.get(i + w + 1).copied().unwrap_or(0) << (64 - b) };
        *o = (lo >> b) | hi;
    }
}

/// `out = a << by`; asserts nothing set is lost past `out`'s end.
fn sl_shl_into(a: &[u64], by: u32, out: &mut [u64]) {
    assert!(sl_bit_len(a) + by <= 64 * out.len() as u32, "a left shift overflows its words");
    let (w, b) = ((by / 64) as usize, by % 64);
    for (i, o) in out.iter_mut().enumerate() {
        *o = if i < w {
            0
        } else {
            let lo = a.get(i - w).copied().unwrap_or(0) << b;
            let below = if b == 0 || i == w { 0 } else { a.get(i - w - 1).copied().unwrap_or(0) >> (64 - b) };
            lo | below
        };
    }
}

/// `a = floor(a / d)`; the remainder.
fn sl_div_small(a: &mut [u64], d: u64) -> u64 {
    let d = u128::from(d);
    let mut r = 0u128;
    for w in a.iter_mut().rev() {
        let cur = (r << 64) | u128::from(*w);
        *w = (cur / d) as u64;
        r = cur % d;
    }
    r as u64
}

/// `a *= k`; asserts no carry out.
fn sl_mul_small(a: &mut [u64], k: u64) {
    let mut carry = 0u128;
    for w in a.iter_mut() {
        let t = u128::from(*w) * u128::from(k) + carry;
        *w = t as u64;
        carry = t >> 64;
    }
    assert_eq!(carry, 0, "a product overflows its words");
}

// ---- f64 by its parts -----------------------------------------------------------------------------

/// `|x| = m 2^e`, `m` the integer significand (subnormals too).
fn parts(x: f64) -> (u64, i32) {
    let b = x.to_bits() & !(1u64 << 63);
    let field = (b >> 52) as i32;
    let frac = b & ((1u64 << 52) - 1);
    if field == 0 { (frac, -1074) } else { (frac | (1u64 << 52), field - 1075) }
}

/// `ulp(x) = 2^ulp_exp(x)` for finite `x`.
fn ulp_exp(x: f64) -> i32 {
    let field = ((x.to_bits() >> 52) & 0x7ff) as i32;
    if field == 0 { -1074 } else { field - 1075 }
}

/// 2^k exactly, `k` in -1022..=1023.
fn pow2(k: i32) -> f64 {
    assert!((-1022..=1023).contains(&k), "2^{k} is not a normal double");
    f64::from_bits(((k + 1023) as u64) << 52)
}

// ---- 1664-bit integers: Machin's pi and ln 2, and long division --------------------------------

const WL: usize = 26;
type Wide = [u64; WL];
/// The fixed point of the series: values are times 2^N.
const N: u32 = 1536;

fn wide_pow2(k: u32) -> Wide {
    let mut w = [0u64; WL];
    w[(k / 64) as usize] = 1u64 << (k % 64);
    w
}

/// `atan(1/k) 2^N` (alternating) or `atanh(1/k) 2^N` (all added), every term floored, so a few
/// thousand units of 2^-N low at most.
fn arc_inv(k: u64, alternating: bool) -> Wide {
    let mut s = [0u64; WL];
    let mut p = wide_pow2(N);
    sl_div_small(&mut p, k);
    let mut i = 0u64;
    while !sl_is_zero(&p) {
        let mut t = p;
        sl_div_small(&mut t, 2 * i + 1);
        if alternating && i % 2 == 1 {
            sl_sub(&mut s, &t);
        } else {
            assert_eq!(sl_add(&mut s, &t), 0);
        }
        sl_div_small(&mut p, k * k);
        i += 1;
    }
    s
}

/// pi 2^N by Machin's formula.
fn machin_pi() -> Wide {
    let mut a = arc_inv(5, true);
    sl_mul_small(&mut a, 16);
    let mut b = arc_inv(239, true);
    sl_mul_small(&mut b, 4);
    sl_sub(&mut a, &b);
    a
}

/// ln 2 2^N as `2 atanh(1/3)`.
fn machin_ln2() -> Wide {
    let mut a = arc_inv(3, false);
    sl_mul_small(&mut a, 2);
    a
}

/// `floor(2^k / d)` by restoring division, one numerator bit at a time.
fn div_pow2(k: u32, d: &Wide) -> Wide {
    let mut r = [0u64; WL];
    let mut q = [0u64; WL];
    for i in (0..=k).rev() {
        assert_eq!(sl_shl1(&mut r, u64::from(i == k)), 0);
        if sl_cmp(&r, d) != Ordering::Less {
            sl_sub(&mut r, d);
            let i = i as usize;
            assert!(i < 64 * WL, "the quotient overflows its words");
            q[i / 64] |= 1u64 << (i % 64);
        }
    }
    q
}

/// `floor(2/pi 2^1344)` from pi `2^N`: what [`fns::TWO_OVER_PI`] must hold, least significant
/// word first.
fn two_over_pi_from(pi: &Wide) -> Wide {
    div_pow2(1345 + N, pi)
}

/// `|x| 2^scale` exactly (asserts it is an integer).
fn wide_of_f64(x: f64, scale: u32) -> Wide {
    let (m, e) = parts(x);
    let sh = e + scale as i32;
    assert!(sh >= 0, "{x:e} is not a whole number at scale 2^{scale}");
    let mut out = [0u64; WL];
    sl_shl_into(&[m], sh as u32, &mut out);
    out
}

fn wide_sum(xs: &[f64], scale: u32) -> Wide {
    let mut s = [0u64; WL];
    for &x in xs {
        assert!(x > 0.0);
        assert_eq!(sl_add(&mut s, &wide_of_f64(x, scale)), 0);
    }
    s
}

fn abs_diff(a: &Wide, b: &Wide) -> Wide {
    let (mut big, small) = if sl_cmp(a, b) != Ordering::Less { (*a, b) } else { (*b, a) };
    sl_sub(&mut big, small);
    big
}

/// `|a - b| < 2^log2` for two values at scale `2^scale`.
fn within_pow2(a: &Wide, b: &Wide, scale: u32, log2: i32) -> bool {
    let bound = scale as i32 + log2;
    assert!(bound > 8, "the scale is too coarse for 2^{log2}");
    sl_bit_len(&abs_diff(a, b)) <= bound as u32
}

/// `x` within half an ulp of `exact / 2^scale` (`exact` floored, so within one unit).
fn within_half_ulp(x: f64, exact: &Wide, scale: u32) -> bool {
    within_pow2(&wide_of_f64(x, scale), exact, scale, ulp_exp(x) - 1)
}

/// The trailing zero bits of the 53-bit significand: `53 - that` significant bits.
fn trailing_zeros(x: f64) -> u32 {
    parts(x).0.trailing_zeros()
}

#[test]
fn two_over_pi_is_machins() {
    let q = two_over_pi_from(&machin_pi());
    for (i, &word) in fns::TWO_OVER_PI.iter().enumerate() {
        assert_eq!(q[20 - i], word, "TWO_OVER_PI[{i}] is {word:#018x}; Machin's formula gives {:#018x}", q[20 - i]);
    }
    assert!(sl_is_zero(&q[21..]), "2/pi 2^1344 is below 2^1344");
}

#[test]
fn the_pi_over_two_split_is_exact_enough() {
    let pi = machin_pi();
    // pi/2 at scale 2^(N+1) is `pi` at scale 2^N.
    let s = N + 1;
    // Cody-Waite needs `n * PIO2_k` exact for `|n| <= 2^20`: at most 33 significant bits each.
    for (name, part) in [("PIO2_1", PIO2_1), ("PIO2_2", PIO2_2), ("PIO2_3", PIO2_3)] {
        assert!(trailing_zeros(part) >= 20, "{name} has {} significant bits, more than 33", 53 - trailing_zeros(part));
    }
    // The four parts sum to pi/2 within 2^-155 (2^20 of them stay within 2^-135).
    let sum = wide_sum(&[PIO2_1, PIO2_2, PIO2_3, PIO2_3T], s);
    assert!(within_pow2(&sum, &pi, s, -155), "PIO2_1 + PIO2_2 + PIO2_3 + PIO2_3T is farther than 2^-155 from pi/2");
    // asin's, acos's and Payne-Hanek's last product: pi/2 rounded, and the rest to 2^-105.
    assert!(within_half_ulp(PIO2_HI, &pi, s), "PIO2_HI is not pi/2 rounded");
    assert!(within_pow2(&wide_sum(&[PIO2_HI, PIO2_LO], s), &pi, s, -105), "PIO2_HI + PIO2_LO is farther than 2^-105 from pi/2");
    // The medium reduction's quotient: 2/pi rounded.
    let two_over_pi = two_over_pi_from(&pi);
    assert!(within_half_ulp(INVPIO2, &two_over_pi, 1344), "INVPIO2 is not 2/pi rounded");
}

#[test]
fn the_ln_two_splits_are_exact_enough() {
    let ln2 = machin_ln2();
    let inv = div_pow2(2 * N, &ln2);
    // exp's and ln's `k LN2_HI` exact for `|k| <= 2^21`: at most 32 significant bits; the pair
    // within half an ulp of LN2_LO (2^-86) of ln 2.
    assert!(trailing_zeros(LN2_HI) >= 21, "LN2_HI has more than 32 significant bits");
    assert!(within_pow2(&wide_sum(&[LN2_HI, LN2_LO], N), &ln2, N, -86), "LN2_HI + LN2_LO is farther than 2^-86 from ln 2");
    // exp2's `r ln2` as an exact product plus a tail: ln 2 rounded, and the rest to 2^-109.
    assert!(within_half_ulp(LN2, &ln2, N), "LN2 is not ln 2 rounded");
    assert!(within_pow2(&wide_sum(&[LN2, LN2_TAIL], N), &ln2, N, -109), "LN2 + LN2_TAIL is farther than 2^-109 from ln 2");
    // exp's quotient: 1/ln 2 rounded.
    assert!(within_half_ulp(INVLN2, &inv, N), "INVLN2 is not 1/ln 2 rounded");
    // log2's `hi * IVLN2_HI` exact for a 21-bit `hi`: at most 32 significant bits; the pair
    // within 2^-86 of 1/ln 2.
    assert!(trailing_zeros(IVLN2_HI) >= 21, "IVLN2_HI has more than 32 significant bits");
    assert!(within_pow2(&wide_sum(&[IVLN2_HI, IVLN2_LO], N), &inv, N, -86), "IVLN2_HI + IVLN2_LO is farther than 2^-86 from 1/ln 2");
}

// ---- 256-bit signed fixed point -------------------------------------------------------------------

const FL: usize = 6;
/// Fraction bits: a value is `mag / 2^FB`, so up to 2^128 in magnitude.
const FB: u32 = 256;

#[derive(Clone, Copy)]
struct Fx {
    neg: bool,
    mag: [u64; FL],
}

impl Fx {
    const ZERO: Fx = Fx { neg: false, mag: [0; FL] };
    const ONE: Fx = Fx { neg: false, mag: [0, 0, 0, 0, 1, 0] };

    /// 2^k (`k` in -256..128).
    fn pow2(k: i32) -> Fx {
        let b = (FB as i32 + k) as u32;
        let mut mag = [0u64; FL];
        mag[(b / 64) as usize] = 1u64 << (b % 64);
        Fx { neg: false, mag }
    }

    /// `x 2^shift`, exact when its lowest bit is at or above 2^-256 (else truncated).
    fn of_f64_shift(x: f64, shift: i32) -> Fx {
        let (m, e) = parts(x);
        let e2 = e + shift + FB as i32;
        let mut mag = [0u64; FL];
        if e2 >= 0 {
            sl_shl_into(&[m], e2 as u32, &mut mag);
        } else if e2 > -64 {
            mag[0] = m >> -e2;
        }
        Fx { neg: x.is_sign_negative(), mag }
    }

    fn of_f64(x: f64) -> Fx {
        Fx::of_f64_shift(x, 0)
    }

    /// A value at scale 2^scale, cut to this one's fraction bits.
    fn of_wide(w: &Wide, scale: u32) -> Fx {
        let mut mag = [0u64; FL];
        sl_shr_into(w, scale - FB, &mut mag);
        Fx { neg: false, mag }
    }

    fn is_zero(self) -> bool {
        sl_is_zero(&self.mag)
    }

    fn negated(self) -> Fx {
        Fx { neg: !self.neg, mag: self.mag }
    }

    fn abs(self) -> Fx {
        Fx { neg: false, mag: self.mag }
    }

    /// `|self| > |o|`.
    fn above(self, o: Fx) -> bool {
        sl_cmp(&self.mag, &o.mag) == Ordering::Greater
    }

    fn plus(self, o: Fx) -> Fx {
        if self.neg == o.neg {
            let mut m = self.mag;
            assert_eq!(sl_add(&mut m, &o.mag), 0, "fixed point overflow");
            Fx { neg: self.neg, mag: m }
        } else if sl_cmp(&self.mag, &o.mag) != Ordering::Less {
            let mut m = self.mag;
            sl_sub(&mut m, &o.mag);
            Fx { neg: self.neg, mag: m }
        } else {
            let mut m = o.mag;
            sl_sub(&mut m, &self.mag);
            Fx { neg: o.neg, mag: m }
        }
    }

    fn minus(self, o: Fx) -> Fx {
        self.plus(o.negated())
    }

    /// The product, its magnitude floored.
    fn times(self, o: Fx) -> Fx {
        let mut p = [0u64; 2 * FL];
        for (i, &a) in self.mag.iter().enumerate() {
            if a == 0 {
                continue;
            }
            let mut carry = 0u128;
            for (j, &b) in o.mag.iter().enumerate() {
                let t = u128::from(a) * u128::from(b) + u128::from(p[i + j]) + carry;
                p[i + j] = t as u64;
                carry = t >> 64;
            }
            p[i + FL] = carry as u64;
        }
        assert!(sl_is_zero(&p[4 + FL..]), "fixed point overflow");
        let mut mag = [0u64; FL];
        mag.copy_from_slice(&p[4..4 + FL]);
        Fx { neg: self.neg != o.neg, mag }
    }

    fn mul_small(self, k: u64) -> Fx {
        let mut mag = self.mag;
        sl_mul_small(&mut mag, k);
        Fx { neg: self.neg, mag }
    }

    fn div_small(self, d: u64) -> Fx {
        let mut mag = self.mag;
        sl_div_small(&mut mag, d);
        Fx { neg: self.neg, mag }
    }

    /// `self >> by` in magnitude.
    fn shifted_down(self, by: u32) -> Fx {
        let mut mag = [0u64; FL];
        sl_shr_into(&self.mag, by, &mut mag);
        Fx { neg: self.neg, mag }
    }

    /// The quotient, its magnitude floored: `(|a| 2^256) / |b|` by restoring division.
    fn over(self, o: Fx) -> Fx {
        assert!(!o.is_zero(), "division by zero in the oracle");
        let mut num = [0u64; 2 * FL];
        num[4..4 + FL].copy_from_slice(&self.mag);
        let mut r = [0u64; FL + 1];
        let mut q = [0u64; 2 * FL];
        for i in (0..sl_bit_len(&num) as usize).rev() {
            assert_eq!(sl_shl1(&mut r, sl_bit(&num, i)), 0);
            if sl_cmp(&r, &o.mag) != Ordering::Less {
                sl_sub(&mut r, &o.mag);
                q[i / 64] |= 1u64 << (i % 64);
            }
        }
        assert!(sl_is_zero(&q[FL..]), "fixed point overflow");
        let mut mag = [0u64; FL];
        mag.copy_from_slice(&q[..FL]);
        Fx { neg: self.neg != o.neg, mag }
    }

    /// The square root of a value at or above zero, floored: digit by digit on `mag 2^256`.
    fn sqrt(self) -> Fx {
        assert!(!self.neg || self.is_zero(), "square root of a negative value in the oracle");
        let mut num = [0u64; 2 * FL];
        num[4..4 + FL].copy_from_slice(&self.mag);
        let mut rem = [0u64; FL + 2];
        let mut root = [0u64; FL + 2];
        let pairs = sl_bit_len(&num).div_ceil(2) as usize;
        for p in (0..pairs).rev() {
            assert_eq!(sl_shl1(&mut rem, sl_bit(&num, 2 * p + 1)), 0);
            assert_eq!(sl_shl1(&mut rem, sl_bit(&num, 2 * p)), 0);
            let mut trial = root;
            sl_shl1(&mut trial, 0);
            sl_shl1(&mut trial, 1);
            if sl_cmp(&rem, &trial) != Ordering::Less {
                sl_sub(&mut rem, &trial);
                sl_shl1(&mut root, 1);
            } else {
                sl_shl1(&mut root, 0);
            }
        }
        assert!(sl_is_zero(&root[FL..]));
        let mut mag = [0u64; FL];
        mag.copy_from_slice(&root[..FL]);
        Fx { neg: false, mag }
    }

    /// The nearest double, about (for error ratios only).
    fn to_f64(self) -> f64 {
        let mut v = 0.0;
        for (i, &w) in self.mag.iter().enumerate().rev() {
            v += w as f64 * pow2(64 * i as i32 - FB as i32);
        }
        if self.neg { -v } else { v }
    }
}

// ---- the exact functions ------------------------------------------------------------------------

/// The constants every oracle shares, computed once per test.
struct Exact {
    /// `floor(2/pi 2^1344)`.
    two_over_pi: Wide,
    pio2: Fx,
    ln2: Fx,
}

impl Exact {
    fn new() -> Exact {
        let pi = machin_pi();
        let two_over_pi = two_over_pi_from(&pi);
        Exact { two_over_pi, pio2: Fx::of_wide(&pi, N + 1), ln2: Fx::of_wide(&machin_ln2(), N) }
    }

    /// `x 2/pi = n + f`, `n` the nearest integer: `(n mod 4, f)`, `|f| <= 1/2`, for finite `x` with
    /// `|x| >= 2^-100`. `|x| = m 2^s`: `x 2/pi = m Q 2^(s - 1344)`, `Q` the table, whose
    /// truncation moves the fraction by under `m 2^(s - 1344) <= 2^-320`.
    fn reduce(&self, x: f64) -> (u64, Fx) {
        let (m, s) = parts(x);
        assert!(m != 0 && (-152..=971).contains(&s), "{x:e} is outside the oracle's range");
        let mut p = self.two_over_pi;
        sl_mul_small(&mut p, m);
        let fb = (1344 - s) as u32;
        let mut whole = [0u64; 1];
        sl_shr_into(&p, fb, &mut whole);
        let mut n = whole[0] & 3;
        let mut frac = [0u64; FL];
        sl_shr_into(&p, fb - FB, &mut frac[..4]);
        let mut f = Fx { neg: false, mag: frac };
        if frac[3] >> 63 == 1 {
            // At or above one half: the next integer is nearer.
            f = Fx::ONE.minus(f).negated();
            n = (n + 1) & 3;
        }
        if x < 0.0 {
            f = f.negated();
            n = n.wrapping_neg() & 3;
        }
        (n, f)
    }

    /// `(sin x, cos x)`: the reduction, then Taylor series on `|r| <= pi/4`.
    fn sin_cos(&self, x: f64) -> (Fx, Fx) {
        let (n, f) = self.reduce(x);
        let r = f.times(self.pio2);
        let r2 = r.times(r);
        let (mut s, mut t, mut k) = (r, r, 1u64);
        while !t.is_zero() && k < 200 {
            t = t.times(r2).div_small((2 * k) * (2 * k + 1)).negated();
            s = s.plus(t);
            k += 1;
        }
        let (mut c, mut t, mut k) = (Fx::ONE, Fx::ONE, 1u64);
        while !t.is_zero() && k < 200 {
            t = t.times(r2).div_small((2 * k - 1) * (2 * k)).negated();
            c = c.plus(t);
            k += 1;
        }
        match n {
            0 => (s, c),
            1 => (c, s.negated()),
            2 => (s.negated(), c.negated()),
            _ => (c.negated(), s),
        }
    }

    /// `e^x = 2^k e^r`, `r = x - k ln2`: `(e^r, k)`.
    fn exp(&self, x: f64) -> (Fx, i32) {
        let k = crate::round(x * core::f64::consts::LOG2_E) as i32;
        let kl = self.ln2.mul_small(u64::from(k.unsigned_abs()));
        let r = Fx::of_f64(x).minus(if k < 0 { kl.negated() } else { kl });
        let (mut s, mut t, mut i) = (Fx::ONE, Fx::ONE, 1u64);
        while !t.is_zero() && i < 200 {
            t = t.times(r).div_small(i);
            s = s.plus(t);
            i += 1;
        }
        (s, k)
    }

    /// `ln x = e ln2 + 2 atanh((m - 1)/(m + 1))`, `x = m 2^e`, `m` in `[1, 2)`, positive normal `x`.
    fn ln(&self, x: f64) -> Fx {
        let field = ((x.to_bits() >> 52) & 0x7ff) as i32;
        assert!(x > 0.0 && field != 0 && field != 0x7ff, "{x:e} is not a positive normal");
        let m = f64::from_bits((x.to_bits() & ((1u64 << 52) - 1)) | (1023u64 << 52));
        let e = field - 1023;
        let mf = Fx::of_f64(m);
        let u = mf.minus(Fx::ONE).over(mf.plus(Fx::ONE));
        let u2 = u.times(u);
        let (mut s, mut pw, mut i) = (Fx::ZERO, u, 0u64);
        while !pw.is_zero() && i < 400 {
            s = s.plus(pw.div_small(2 * i + 1));
            pw = pw.times(u2);
            i += 1;
        }
        let el = self.ln2.mul_small(u64::from(e.unsigned_abs()));
        s.mul_small(2).plus(if e < 0 { el.negated() } else { el })
    }

    /// `atan t` for `0 <= t <= 1`: Euler's series `sum 2^2n (n!)^2/(2n+1)! t^(2n+1)/(1+t^2)^(n+1)`,
    /// each term the last times `y 2n/(2n+1)`, `y = t^2/(1+t^2) <= 1/2`.
    fn atan_unit(&self, t: Fx) -> Fx {
        let t2 = t.times(t);
        let inv = Fx::ONE.over(Fx::ONE.plus(t2));
        let y = t2.times(inv);
        let mut term = t.times(inv);
        let (mut s, mut n) = (term, 1u64);
        while !term.is_zero() && n < 1000 {
            term = term.times(y).mul_small(2 * n).div_small(2 * n + 1);
            s = s.plus(term);
            n += 1;
        }
        s
    }

    fn atan(&self, x: f64) -> Fx {
        let a = x.abs();
        let v = if a <= 1.0 {
            self.atan_unit(Fx::of_f64(a))
        } else if a < pow2(64) {
            self.pio2.minus(self.atan_unit(Fx::ONE.over(Fx::of_f64(a))))
        } else {
            // atan(1/a) = 1/a within a^-3/3 <= 2^-192; 1/a rounded is within 2^-117 of it.
            self.pio2.minus(Fx::of_f64(1.0 / a))
        };
        if x < 0.0 { v.negated() } else { v }
    }

    /// `asin x = atan(x / sqrt(1 - x^2))` for `|x| <= 1`.
    fn asin(&self, x: f64) -> Fx {
        let a = x.abs();
        let v = if a == 1.0 {
            self.pio2
        } else {
            let xa = Fx::of_f64(a);
            let s = Fx::ONE.minus(xa.times(xa)).sqrt();
            if xa.above(s) { self.pio2.minus(self.atan_unit(s.over(xa))) } else { self.atan_unit(xa.over(s)) }
        };
        if x < 0.0 { v.negated() } else { v }
    }
}

/// `|got 2^shift - want|` in ulp of `got` (scaled the same); infinite for a non-finite `got` or one
/// too far for the fixed point.
fn ulps_off(got: f64, shift: i32, want: Fx) -> f64 {
    if !got.is_finite() {
        return f64::INFINITY;
    }
    if got == 0.0 {
        return if want.is_zero() { 0.0 } else { f64::INFINITY };
    }
    if ulp_exp(got) + 52 + shift > 120 {
        return f64::INFINITY;
    }
    let d = Fx::of_f64_shift(got, shift).minus(want).abs().to_f64();
    d * pow2(-(ulp_exp(got) + shift))
}

// ---- the properties -----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum G {
    Sin,
    Cos,
    Tan,
    Exp,
    Ln,
    Atan,
    Asin,
    Acos,
}

const GS: [G; 8] = [G::Sin, G::Cos, G::Tan, G::Exp, G::Ln, G::Atan, G::Asin, G::Acos];

#[derive(Clone, Debug, PartialEq)]
enum PreciseBroken {
    /// Farther from the exact value than the stated bound.
    Ulps { f: G, x: f64, got: f64, ulps: f64, bound: f64 },
    /// `rem_pio2`'s `n` is not the quadrant of a multiple of pi/2 next to `x`.
    Quadrant { x: f64, n: u64, exact: u64 },
    /// `rem_pio2`'s `y0 + y1` is farther than `2^-100 |y| + 2^-130` from the exact remainder.
    Reduced { x: f64, y0: f64, y1: f64, err: f64 },
}

impl fmt::Display for PreciseBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

blaze_sim::plants! {
    /// The function harness's planted bug: it tests the oracle, not blaze-math.
    enum FnPlant breaks PreciseBroken {
        None,
        /// The value moved away from the exact one by twice the bound plus two ulp (so past
        /// the bound even where the ulp doubles on the way) before it is judged.
        Nudge => PreciseBroken::Ulps { .. },
    }
}

blaze_sim::plants! {
    /// The reduction harness's planted bugs.
    enum ReducePlant breaks PreciseBroken {
        None,
        /// `y1` dropped.
        DropTail => PreciseBroken::Reduced { .. },
        /// `n` one more.
        QuadrantOff => PreciseBroken::Quadrant { .. },
    }
}

/// `6381956970095103 * 2^797`: no double is closer to a multiple of pi/2 (about 2^-61 away).
const HARDEST: f64 = f64::from_bits((1872u64 << 52) | (6_381_956_970_095_103 - (1u64 << 52)));

fn sign(tape: &Tape) -> u64 {
    if tape.coin("negative") { 1u64 << 63 } else { 0 }
}

/// A double with its exponent field in `lo..=hi` and any significand.
fn by_field(tape: &Tape, lo: u64, hi: u64) -> f64 {
    let e = tape.int_in(lo..=hi, "exponent field");
    f64::from_bits((e << 52) | tape.below(1u64 << 52, "significand"))
}

/// Near a multiple of pi/2, small or huge, a few ulp away.
fn near_multiple(tape: &Tape) -> f64 {
    let k = if tape.coin("huge multiple") {
        tape.int_in(1i64..=(1i64 << 62), "multiple of pi/2")
    } else {
        tape.int_in(1i64..=1_000_000, "multiple of pi/2")
    };
    let n = tape.int_in(-4i64..=4, "ulps away");
    f64::from_bits((k as f64 * FRAC_PI_2).to_bits().wrapping_add_signed(n))
}

/// An argument in the oracle's exact range for `g`.
fn draw_for(tape: &Tape, g: G) -> f64 {
    match g {
        G::Sin | G::Cos | G::Tan => match tape.index(3, "kind") {
            // 2^-100 to the largest double: Cody-Waite and Payne-Hanek.
            0 => f64::from_bits(by_field(tape, 923, 2046).to_bits() | sign(tape)),
            1 => f64::from_bits(near_multiple(tape).to_bits() | sign(tape)),
            _ => f64::from_bits(HARDEST.to_bits() | sign(tape)),
        },
        G::Exp => {
            if tape.coin("small argument") {
                f64::from_bits(by_field(tape, 923, 1031).to_bits() | sign(tape))
            } else {
                // Normal results only.
                tape.f64_in(-708.0..=709.7, "x")
            }
        }
        G::Ln => {
            if tape.coin("near one") {
                f64::from_bits(1f64.to_bits().wrapping_add_signed(tape.int_in(-64i64..=64, "ulps from one")))
            } else {
                by_field(tape, 1, 2046)
            }
        }
        G::Atan => f64::from_bits(by_field(tape, 923, 2046).to_bits() | sign(tape)),
        G::Asin | G::Acos => match tape.index(3, "kind") {
            0 => f64::from_bits(by_field(tape, 923, 1022).to_bits() | sign(tape)),
            1 => f64::from_bits((1f64.to_bits() - tape.int_in(1u64..=64, "ulps below one")) | sign(tape)),
            _ => f64::from_bits(1f64.to_bits() | sign(tape)),
        },
    }
}

/// The crate's stated bound: under 1 ulp, tan under 2.5 (taken as 3).
fn stated(g: G) -> f64 {
    if g == G::Tan { 3.0 } else { 1.0 }
}

fn function_case(tape: &Tape, ex: &Exact, plant: impl Planted<FnPlant>) -> Result<(), Broken<PreciseBroken>> {
    let f = *tape.one_of(&GS, "function");
    let x = draw_for(tape, f);
    let (mut got, want, shift) = match f {
        G::Sin => (sin(x), ex.sin_cos(x).0, 0),
        G::Cos => (cos(x), ex.sin_cos(x).1, 0),
        G::Tan => {
            let (s, c) = ex.sin_cos(x);
            (tan(x), s.over(c), 0)
        }
        G::Exp => {
            let (w, k) = ex.exp(x);
            (exp(x), w, -k)
        }
        G::Ln => (ln(x), ex.ln(x), 0),
        G::Atan => (atan(x), ex.atan(x), 0),
        G::Asin => (asin(x), ex.asin(x), 0),
        G::Acos => (acos(x), ex.pio2.minus(ex.asin(x)), 0),
    };
    let bound = stated(f);
    if plant.has(FnPlant::Nudge) && got.is_finite() && got != 0.0 && ulp_exp(got) + 52 + shift <= 120 {
        let low = Fx::of_f64_shift(got, shift).minus(want).neg;
        for _ in 0..(2 * bound as u32 + 2) {
            got = if low { next_down(got) } else { next_up(got) };
        }
    }
    let ulps = ulps_off(got, shift, want);
    if !(ulps <= bound) {
        return Err(Broken::Oracle(PreciseBroken::Ulps { f, x, got, ulps, bound }));
    }
    Ok(())
}

#[test]
fn functions_within_their_bound_of_the_exact_value() {
    let ex = Exact::new();
    check(Config::cases(300), |tape| function_case(tape, &ex, NoPlant));
}

#[test]
fn function_oracle_plants_are_caught() {
    let ex = Exact::new();
    controls(Config::cases(100), |tape, plant: FnPlant| function_case(tape, &ex, plant));
}

fn reduce_case(tape: &Tape, ex: &Exact, plant: impl Planted<ReducePlant>) -> Result<(), Broken<PreciseBroken>> {
    let x = match tape.index(4, "kind") {
        // 2^-100 to 2^21: Cody-Waite, and the first Payne-Hanek binade.
        0 => f64::from_bits(by_field(tape, 923, 1043).to_bits() | sign(tape)),
        1 => f64::from_bits(near_multiple(tape).to_bits() | sign(tape)),
        2 => f64::from_bits(by_field(tape, 1043, 2046).to_bits() | sign(tape)),
        _ => f64::from_bits(HARDEST.to_bits() | sign(tape)),
    };
    let (mut n, y0, mut y1) = fns::rem_pio2::<f64>(x);
    if plant.has(ReducePlant::DropTail) {
        y1 = 0.0;
    }
    if plant.has(ReducePlant::QuadrantOff) {
        n = n.wrapping_add(1);
    }
    let (exact, f) = ex.reduce(x);
    // At a near tie the medium reduction's rounded quotient may take the other neighbour; its
    // remainder is then `f -+ 1`, still at most 1/2 (plus the quotient's rounding) in size.
    let f_n = match exact.wrapping_sub(n) & 3 {
        0 => f,
        1 => f.plus(Fx::ONE),
        3 => f.minus(Fx::ONE),
        _ => return Err(Broken::Oracle(PreciseBroken::Quadrant { x, n: n & 3, exact })),
    };
    if f_n.above(Fx::ONE.div_small(2).plus(Fx::pow2(-20))) {
        return Err(Broken::Oracle(PreciseBroken::Quadrant { x, n: n & 3, exact }));
    }
    let want = f_n.times(ex.pio2);
    let err = Fx::of_f64(y0).plus(Fx::of_f64(y1)).minus(want).abs();
    let allowed = want.abs().shifted_down(100).plus(Fx::pow2(-130));
    if err.above(allowed) {
        return Err(Broken::Oracle(PreciseBroken::Reduced { x, y0, y1, err: err.to_f64() }));
    }
    Ok(())
}

#[test]
fn the_reduction_is_exact_to_its_bound() {
    let ex = Exact::new();
    check(Config::cases(2000), |tape| reduce_case(tape, &ex, NoPlant));
}

#[test]
fn reduction_plants_are_caught() {
    let ex = Exact::new();
    controls(Config::cases(300), |tape, plant: ReducePlant| reduce_case(tape, &ex, plant));
}
