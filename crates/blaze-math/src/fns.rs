//! The elementary functions, each written once over [`Real`] (one lane or many, the same bits per
//! lane). Every method is the fdlibm one as musl writes it (FreeBSD's `msun`, Sun's fdlibm 5.3):
//! the same argument reduction with the same split constants, the same polynomial or rational
//! approximation with the same coefficients, the same order of operations. What differs is only
//! the shape: musl branches on ranges, and here every range is computed and the right one
//! selected, so the code has no branch a lane could take apart from its neighbours. Where a
//! musl branch only skips work (a second reduction round, a tiny-argument shortcut), the work
//! is always done: that is never less accurate, and the bound stated is musl's.
//!
//! Only IEEE basic operations (`+ - * /`, `sqrt`, and `fma` where a method names it) in a fixed
//! order, so a result is the same on every machine (rule 25). Nothing here calls the platform's
//! libm, which is the point of the crate: `core`'s `cbrt` does, and no two libms agree.

use crate::real::{Real, SIGN};

/// 1.5 * 2^52: adding it to an f64 of magnitude below 2^51 rounds to an integer (ties to even)
/// and leaves that integer, in two's complement, in the low bits of the sum.
pub(crate) const TOINT: f64 = 6_755_399_441_055_744.0;
const TOINT_BITS: u64 = 0x4338_0000_0000_0000;
const ALL: u64 = u64::MAX;

#[inline]
fn not<R: Real>(b: R::Bits) -> R::Bits {
    b ^ R::bits(ALL)
}

/// `x` rounded to an integer, ties to even, for `|x| < 2^51`.
#[inline]
fn round_even_small<R: Real>(x: R) -> R {
    (x + R::splat(TOINT)) - R::splat(TOINT)
}

/// The integer value of an integral `x` (`|x| < 2^51`), in two's complement.
#[inline]
fn int_bits<R: Real>(x: R) -> R::Bits {
    R::bits_sub((x + R::splat(TOINT)).to_bits(), R::bits(TOINT_BITS))
}

/// The f64 of a small two's complement integer (`|k| < 2^51`).
#[inline]
fn from_int_bits<R: Real>(k: R::Bits) -> R {
    R::from_bits(R::bits_add(k, R::bits(TOINT_BITS))) - R::splat(TOINT)
}

/// 2^k for an integral `k` in -1022..=1023.
#[inline]
fn pow2<R: Real>(k: R) -> R {
    R::from_bits(R::bits_shl(R::bits_add(int_bits(k), R::bits(1023)), 52))
}

/// `y * 2^k` for an integral `k` in -1100..=1100, rounded once (two exact-or-once steps).
#[inline]
fn scale<R: Real>(y: R, k: R) -> R {
    let k1 = floor(k * R::splat(0.5));
    let k2 = k - k1;
    y * pow2(k1) * pow2(k2)
}

/// The whole part of `x`, toward zero (exact).
#[inline]
pub fn trunc<R: Real>(x: R) -> R {
    let b = x.to_bits();
    let e = R::bits_shr(b, 52) & R::bits(0x7ff);
    let small = R::bits_lt(e, R::bits(1023));
    let big = !R::bits_lt(e, R::bits(1075));
    let shift = R::select_bits(small | big, R::bits(0), R::bits_sub(R::bits(1075), e));
    let low = R::bits_sub(R::bits_shl_each(R::bits(1), shift), R::bits(1));
    let kept = b & not::<R>(low);
    R::from_bits(R::select_bits(small, b & R::bits(SIGN), kept))
}

/// The greatest integer not above `x` (exact).
#[inline]
pub fn floor<R: Real>(x: R) -> R {
    let t = trunc(x);
    R::select(x.lt(t), t - R::splat(1.0), t)
}

/// The least integer not below `x` (exact).
#[inline]
pub fn ceil<R: Real>(x: R) -> R {
    let t = trunc(x);
    R::select(t.lt(x), t + R::splat(1.0), t)
}

/// `x` rounded to the nearest integer, halves away from zero (exact).
#[inline]
pub fn round<R: Real>(x: R) -> R {
    let t = trunc(x);
    let d = x - t;
    R::select(R::splat(0.5).le(d.abs()), t + R::splat(1.0).copysign(x), t)
}

/// `x` rounded to the nearest integer, halves to even (exact).
#[inline]
pub fn round_ties_even<R: Real>(x: R) -> R {
    let two52 = R::splat(4_503_599_627_370_496.0);
    let a = x.abs();
    R::select(a.lt(two52), ((a + two52) - two52).copysign(x), x)
}

/// `|x|`.
#[inline]
pub fn abs<R: Real>(x: R) -> R {
    x.abs()
}

/// `|x|` with the sign of `sign`.
#[inline]
pub fn copysign<R: Real>(x: R, sign: R) -> R {
    x.copysign(sign)
}

/// The exactly rounded square root.
#[inline]
pub fn sqrt<R: Real>(x: R) -> R {
    x.sqrt()
}

/// `x * a + b` rounded once: the fused form, only where a caller asks for it by this name.
#[inline]
pub fn mul_add<R: Real>(x: R, a: R, b: R) -> R {
    x.fma(a, b)
}

// ---- sin, cos, tan: the reduction by pi/2, then fdlibm's __sin, __cos --------------------------
//
// THE REDUCTION, `x = n pi/2 + y`, `|y| <= pi/4`, `y` carried as two doubles `y0 + y1`:
// - `|x| <` [`TRIG_MEDIUM_BELOW`] (2^20 pi/2): Cody and Waite. `n = round(x 2/pi)` fits 20
//   bits; pi/2 is split in three parts of at most 33 significant bits (`PIO2_1..3`) and a 53-bit
//   tail (`PIO2_3T`), their sum within 2^-159 of pi/2. Each `n * PIO2_k` is then exact (20 + 33
//   bits), `x - n PIO2_1` is exact (Sterbenz: the two are within a factor of two), the next two
//   subtractions are taken as exact two-sums, so `x - n (PIO2_1 + PIO2_2 + PIO2_3)` is known
//   exactly as `r3 + e2 + e3`, and only the tail product `n PIO2_3T` (under 2^-82) and the sum
//   of the three small terms are rounded: `y0 + y1` is within 2^-135 absolute plus 2^-104
//   relative of `y`, and no double is closer than about 2^-61 to a multiple of pi/2 (below).
//   (fdlibm's medium case skips the rounds it does not need and drops the second round's
//   rounding error when it takes the third; here every round runs and every error is kept.)
// - Beyond: Payne and Hanek, in integers. `|x| = m 2^s` (`m` the 53-bit significand). Only the
//   bits of 2/pi of weight 2^-j with `j >= s - 2` can change `x 2/pi` modulo 8, so a window of
//   192 of them from [`TWO_OVER_PI`] times `m` (a 245-bit product) holds `x 2/pi mod 8` with an
//   absolute error under 2^-136: the integer part's low three bits are `n mod 8`, the fraction
//   (rounded to nearest, so in `[-1/2, 1/2]`) is normalised to 128 bits, made a double-double and
//   multiplied by pi/2 as a double-double (an exact product by fused multiply-add). The closest
//   any finite double comes to a multiple of pi/2 is about 2^-61 (`6381956970095103 * 2^797`,
//   Kahan and McDonald's search), so 2^-136 absolute is over 70 bits below the least `|y|`.
//
// Both run on every lane and the lane's range selects (the large one lane by lane through
// [`Real::map_triple`], which returns at once for a lane in the medium range): the same bits as
// the scalar form. `n` is only needed modulo 4.

/// 2/pi rounded to f64 (within 0.36 ulp).
pub(crate) const INVPIO2: f64 = 6.366_197_723_675_813_824_33e-01;
/// pi/2's first 33 bits (31 significant: the next two are zero), then the next 33 (32
/// significant), the next 33 (28 significant), and the rest rounded to 53 bits: the sum is within
/// 2^-159 of pi/2 (the test `the_pi_over_two_split_is_exact_enough` checks every part's bit count
/// and the sum against pi from Machin's formula).
pub(crate) const PIO2_1: f64 = 1.570_796_326_734_125_614_17e+00;
pub(crate) const PIO2_2: f64 = 6.077_100_506_303_965_976_60e-11;
pub(crate) const PIO2_3: f64 = 2.022_266_248_711_166_455_80e-21;
pub(crate) const PIO2_3T: f64 = 8.478_427_660_368_899_569_97e-32;

/// 2/pi in binary, 64 bits a word, most significant first: word `i` holds the bits of weight
/// 2^-(64 i + 1) down to 2^-(64 i + 64); the whole is `floor(2/pi * 2^1344)`. The largest finite
/// double needs bits down to 2^-1160 (`s = 971`, a window of 192 from `s - 2`); the rest is
/// margin. DERIVED, NOT RECALLED: `examples/two_over_pi.rs` prints it from Machin's formula
/// (`pi = 16 atan(1/5) - 4 atan(1/239)`) in integer arithmetic at 1536 bits and a long division,
/// and the test `two_over_pi_is_machins` recomputes every word the same way and compares.
pub(crate) const TWO_OVER_PI: [u64; 21] = [
    0xa2f9_836e_4e44_1529, 0xfc27_57d1_f534_ddc0, 0xdb62_9599_3c43_9041,
    0xfe51_63ab_debb_c561, 0xb724_6e3a_424d_d2e0, 0x0649_2eea_09d1_921c,
    0xfe1d_eb1c_b129_a73e, 0xe882_35f5_2ebb_4484, 0xe99c_7026_b45f_7e41,
    0x3991_d639_8353_39f4, 0x9c84_5f8b_bdf9_283b, 0x1ff8_97ff_de05_980f,
    0xef2f_118b_5a0a_6d1f, 0x6d36_7ecf_27cb_09b7, 0x4f46_3f66_9e5f_ea2d,
    0x7527_bac7_ebe5_f17b, 0x3d07_39f7_8a52_92ea, 0x6bfb_5fb1_1f8d_5d08,
    0x5603_3046_fc7b_6bab, 0xf0cf_bc20_9af4_361d, 0xa9e3_9161_5ee6_1b08,
];

const S1: f64 = -1.666_666_666_666_663_243_48e-01;
const S2: f64 = 8.333_333_333_322_489_461_24e-03;
const S3: f64 = -1.984_126_982_985_794_931_34e-04;
const S4: f64 = 2.755_731_370_707_006_767_89e-06;
const S5: f64 = -2.505_076_025_340_686_341_95e-08;
const S6: f64 = 1.589_690_995_211_550_102_21e-10;

const C1: f64 = 4.166_666_666_666_660_190_37e-02;
const C2: f64 = -1.388_888_888_887_410_957_49e-03;
const C3: f64 = 2.480_158_728_947_672_941_78e-05;
const C4: f64 = -2.755_731_435_139_066_330_35e-07;
const C5: f64 = 2.087_572_321_298_174_827_90e-09;
const C6: f64 = -1.135_964_755_778_819_482_65e-11;

/// Below this `|x|` the reduction is Cody and Waite's (the quotient fits 20 bits); at and above
/// it, Payne and Hanek's. 2^20 * pi/2 = 1647099.35 rounded down. Both are within the bounds the
/// crate states, so this is where the method changes, not where accuracy ends.
pub const TRIG_MEDIUM_BELOW: f64 = 1_647_099.0;

/// `a + b` as `s + e` exactly (Knuth's two-sum: no condition on the operands' sizes).
#[inline]
fn two_sum<R: Real>(a: R, b: R) -> (R, R) {
    let s = a + b;
    let bb = s - a;
    (s, (a - (s - bb)) + (b - bb))
}

/// `x = n * pi/2 + (y0 + y1)`, `|y0 + y1| <= pi/4` (about): Cody and Waite below
/// [`TRIG_MEDIUM_BELOW`], Payne and Hanek at and above (see the section's head). Returns `n`
/// in two's complement (only `n mod 4` is meaningful beyond the medium range) and the two parts
/// of the reduced argument. Infinite or NaN `x` gives meaningless parts; the callers select NaN.
#[inline]
pub(crate) fn rem_pio2<R: Real>(x: R) -> (R::Bits, R, R) {
    let raw = x * R::splat(INVPIO2) + R::splat(TOINT);
    let n = R::bits_sub(raw.to_bits(), R::bits(TOINT_BITS));
    let f = raw - R::splat(TOINT);
    // Exact: `f * PIO2_1` has at most 20 + 33 bits and `x` is within a factor of two of it.
    let r1 = x - f * R::splat(PIO2_1);
    // `r1 - f PIO2_2 = r2 + e2` and `r2 - f PIO2_3 = r3 + e3` exactly (both products exact).
    let (r2, e2) = two_sum(r1, -(f * R::splat(PIO2_2)));
    let (r3, e3) = two_sum(r2, -(f * R::splat(PIO2_3)));
    // What is still to subtract: the tail's product less the two sums' errors. Written as a
    // subtraction so `x = -0` keeps its sign (`-0 - +0 = -0`).
    let w = f * R::splat(PIO2_3T) - (e2 + e3);
    let y0 = r3 - w;
    let y1 = (r3 - y0) - w;
    let (nl, yl0, yl1) = x.map_triple(rem_pio2_large);
    let large = R::splat(TRIG_MEDIUM_BELOW).le(x.abs());
    (R::select_bits(large, int_bits(nl), n), R::select(large, yl0, y0), R::select(large, yl1, y1))
}

/// 2^k for an integer `k` in -1022..=1023, exactly.
#[inline]
fn pow2i(k: i32) -> f64 {
    f64::from_bits(((k + 1023) as u64) << 52)
}

/// `p << by` on a 256-bit number (four words, least significant first), `by < 256`.
fn shl256(p: [u64; 4], by: u32) -> [u64; 4] {
    let (words, bits) = ((by / 64) as usize, by % 64);
    let mut out = [0u64; 4];
    for i in words..4 {
        let lo = p[i - words];
        let below = if bits == 0 || i - words == 0 { 0 } else { p[i - words - 1] >> (64 - bits) };
        out[i] = (lo << bits) | below;
    }
    out
}

/// `p >> by` on a 256-bit number (four words, least significant first), `by < 256`.
fn shr256(p: [u64; 4], by: u32) -> [u64; 4] {
    let (words, bits) = ((by / 64) as usize, by % 64);
    let mut out = [0u64; 4];
    for i in 0..4 - words {
        let hi = p[i + words];
        let above = if bits == 0 || i + words + 1 >= 4 { 0 } else { p[i + words + 1] << (64 - bits) };
        out[i] = (hi >> bits) | above;
    }
    out
}

/// The Payne-Hanek reduction of one finite `x` with `|x| >=` [`TRIG_MEDIUM_BELOW`] (see the
/// section's head): `(n mod 8, y0, y1)`, `n` as a small f64. Returns zeros at once for any other
/// `x` (a lane the medium reduction serves, or infinite or NaN): its result is not selected.
pub(crate) fn rem_pio2_large(x: f64) -> (f64, f64, f64) {
    let ax = x.abs();
    if !(TRIG_MEDIUM_BELOW <= ax && ax <= f64::MAX) {
        return (0.0, 0.0, 0.0);
    }
    let b = ax.to_bits();
    // `|x| = m 2^s`: a normal number here (|x| >= 2^20), so the hidden bit is set.
    let m = (b & 0x000f_ffff_ffff_ffff) | (1 << 52);
    let s = ((b >> 52) as i32) - 1075;
    // The window's first bit has weight 2^-jlo; bits of larger weight only add multiples of 8.
    let jlo = if s - 2 > 1 { s - 2 } else { 1 };
    let start = (jlo - 1) as usize; // 0-based from the table's first bit
    let (wi, sh) = (start / 64, (start % 64) as u32);
    let word = |k: usize| -> u64 {
        let hi = TWO_OVER_PI[wi + k] << sh;
        let lo = if sh == 0 { 0 } else { TWO_OVER_PI[wi + k + 1] >> (64 - sh) };
        hi | lo
    };
    // The 192-bit window, least significant word first.
    let win = [word(2), word(1), word(0)];
    let mut p = [0u64; 4];
    let mut carry: u128 = 0;
    for k in 0..3 {
        let t = u128::from(m) * u128::from(win[k]) + carry;
        p[k] = t as u64;
        carry = t >> 64;
    }
    p[3] = carry as u64;
    // `x 2/pi = p 2^-q (mod 8)`: `q` in 189..=224.
    let q = ((jlo - 1) + 192 - s) as u32;
    let mut n = shr256(p, q)[0] & 7;
    // The fraction as a 256-bit fixed point `g / 2^256`, rounded to nearest: `[-1/2, 1/2]`.
    let mut g = shl256(p, 256 - q);
    let neg = g[3] >> 63 == 1;
    if neg {
        // `g := 2^256 - g`, the fraction's magnitude below the next integer.
        let mut c = 1u64;
        for w in g.iter_mut() {
            let (v, o) = (!*w).overflowing_add(c);
            *w = v;
            c = u64::from(o);
        }
        n = (n + 1) & 7;
    }
    let lz = if g[3] != 0 {
        g[3].leading_zeros()
    } else if g[2] != 0 {
        64 + g[2].leading_zeros()
    } else if g[1] != 0 {
        128 + g[1].leading_zeros()
    } else if g[0] != 0 {
        192 + g[0].leading_zeros()
    } else {
        // Never for a double (2/pi is irrational and the window's error is far below its least
        // fraction); an exact zero is still the right answer for a zero fraction.
        return (n as f64, 0.0, 0.0);
    };
    let h = shl256(g, lz);
    let (a, bl) = (h[3], h[2]);
    // `a 2^64 + bl` to a double-double: `a` rounded, and the rounding's remainder (|rem| <= 2^10)
    // joined to `bl` exactly in an i128 before its own rounding.
    let a_hi = a as f64;
    let rem = i128::from(a) - (a_hi as i128);
    let rest = ((rem << 64) + i128::from(bl)) as f64;
    let lz = lz as i32;
    let f_hi = a_hi * pow2i(-64 - lz);
    let f_lo = rest * pow2i(-128 - lz);
    // `(f_hi + f_lo) * pi/2`: the high product exactly by fused multiply-add.
    let ph = f_hi * PIO2_HI;
    let pe = f_hi.fma(PIO2_HI, -ph);
    let lo = pe + (f_hi * PIO2_LO + f_lo * PIO2_HI);
    let mut y0 = ph + lo;
    let mut y1 = (ph - y0) + lo;
    if neg {
        (y0, y1) = (-y0, -y1);
    }
    if x < 0.0 {
        (y0, y1) = (-y0, -y1);
        n = n.wrapping_neg() & 7;
    }
    (n as f64, y0, y1)
}

/// sin on `[-pi/4, pi/4]` of `x + y` (`|y| < ulp(x)/2`): a degree-13 odd minimax polynomial.
#[inline]
pub(crate) fn ksin<R: Real>(x: R, y: R) -> R {
    let z = x * x;
    let w = z * z;
    let r = R::splat(S2) + z * (R::splat(S3) + z * R::splat(S4)) + z * w * (R::splat(S5) + z * R::splat(S6));
    let v = z * x;
    x - ((z * (R::splat(0.5) * y - v * r) - y) - v * R::splat(S1))
}

/// cos on `[-pi/4, pi/4]` of `x + y`: a degree-14 even minimax polynomial, `1 - z/2` taken
/// with its rounding error carried.
#[inline]
pub(crate) fn kcos<R: Real>(x: R, y: R) -> R {
    let z = x * x;
    let w = z * z;
    let r = z * (R::splat(C1) + z * (R::splat(C2) + z * R::splat(C3)))
        + w * w * (R::splat(C4) + z * (R::splat(C5) + z * R::splat(C6)));
    let hz = R::splat(0.5) * z;
    let w = R::splat(1.0) - hz;
    w + (((R::splat(1.0) - w) - hz) + (z * r - x * y))
}

/// Picks by the quadrant `q = n & 3`: `q0, q1, q2, q3`.
#[inline]
fn by_quadrant<R: Real>(n: R::Bits, q0: R, q1: R, q2: R, q3: R) -> R {
    let q = n & R::bits(3);
    let low = R::select(R::bits_eq(q, R::bits(0)), q0, q1);
    let high = R::select(R::bits_eq(q, R::bits(2)), q2, q3);
    R::select(R::bits_lt(q, R::bits(2)), low, high)
}

/// `(sin x, cos x)` from one reduction. Under 1 ulp each for every finite `x`. NaN for
/// infinite or NaN `x`.
#[inline]
pub fn sin_cos<R: Real>(x: R) -> (R, R) {
    let (n, y0, y1) = rem_pio2(x);
    let s = ksin(y0, y1);
    let c = kcos(y0, y1);
    let sin = by_quadrant(n, s, c, -s, -c);
    let cos = by_quadrant(n, c, -s, -c, s);
    let finite = x.abs().lt(R::splat(f64::INFINITY));
    let nan = x - x;
    (R::select(finite, sin, nan), R::select(finite, cos, nan))
}

/// `sin x` (see [`sin_cos`]).
#[inline]
pub fn sin<R: Real>(x: R) -> R {
    sin_cos(x).0
}

/// `cos x` (see [`sin_cos`]).
#[inline]
pub fn cos<R: Real>(x: R) -> R {
    sin_cos(x).1
}

/// `tan x` as the quotient of the two kernels on one reduction: each under 1 ulp, so the
/// quotient is under 2.5 ulp, for every finite `x`.
#[inline]
pub fn tan<R: Real>(x: R) -> R {
    let (s, c) = sin_cos(x);
    s / c
}

// ---- atan, atan2: fdlibm's s_atan.c, e_atan2.c -------------------------------------------------

const ATANHI: [f64; 4] =
    [4.636_476_090_008_060_935_15e-01, 7.853_981_633_974_482_789_99e-01, 9.827_937_232_473_290_540_82e-01, 1.570_796_326_794_896_558_00e+00];
const ATANLO: [f64; 4] =
    [2.269_877_745_296_168_709_24e-17, 3.061_616_997_868_383_017_93e-17, 1.390_331_103_123_099_845_16e-17, 6.123_233_995_736_766_035_87e-17];
const AT: [f64; 11] = [
    3.333_333_333_333_293_180_27e-01,
    -1.999_999_999_987_648_324_76e-01,
    1.428_571_427_250_346_637_11e-01,
    -1.111_111_040_546_235_578_80e-01,
    9.090_887_133_436_506_561_96e-02,
    -7.691_876_205_044_829_994_95e-02,
    6.661_073_137_387_531_206_69e-02,
    -5.833_570_133_790_573_486_45e-02,
    4.976_877_994_615_932_360_17e-02,
    -3.653_157_274_421_691_552_70e-02,
    1.628_582_011_536_578_236_23e-02,
];

/// `atan x`: reduced to `|t| <= 7/16` around 0, 0.5, 1, 1.5 or infinity by the identity
/// `atan x = atan c + atan((x - c)/(1 + c x))`, then a degree-22 odd minimax polynomial split in
/// even and odd halves. Under 1 ulp.
#[inline]
pub fn atan<R: Real>(x: R) -> R {
    let a = x.abs();
    let one = R::splat(1.0);
    // The reduced argument and its table entry for each of musl's ranges.
    let t0 = (R::splat(2.0) * a - one) / (R::splat(2.0) + a);
    let t1 = (a - one) / (a + one);
    let t2 = (a - R::splat(1.5)) / (one + R::splat(1.5) * a);
    let t3 = -one / a;
    let in0 = a.lt(R::splat(0.6875));
    let in1 = a.lt(R::splat(1.1875));
    let in2 = a.lt(R::splat(2.4375));
    let pick = |v: [R; 4]| R::select(in0, v[0], R::select(in1, v[1], R::select(in2, v[2], v[3])));
    let tiny = a.lt(R::splat(0.4375));
    let t = R::select(tiny, x, pick([t0, t1, t2, t3]));
    let hi = pick([R::splat(ATANHI[0]), R::splat(ATANHI[1]), R::splat(ATANHI[2]), R::splat(ATANHI[3])]);
    let lo = pick([R::splat(ATANLO[0]), R::splat(ATANLO[1]), R::splat(ATANLO[2]), R::splat(ATANLO[3])]);
    let z = t * t;
    let w = z * z;
    let c = |i: usize| R::splat(AT[i]);
    let s1 = z * (c(0) + w * (c(2) + w * (c(4) + w * (c(6) + w * (c(8) + w * c(10))))));
    let s2 = w * (c(1) + w * (c(3) + w * (c(5) + w * (c(7) + w * c(9)))));
    let near0 = t - t * (s1 + s2);
    let far = (hi - ((t * (s1 + s2) - lo) - t)).copysign(x);
    let huge = R::splat(7.378_697_629_483_820_646_4e19).le(a); // 2^66
    let r = R::select(tiny, near0, far);
    let r = R::select(huge, (R::splat(ATANHI[3]) + R::splat(ATANLO[3])).copysign(x), r);
    // `x = -0`: `t - t * 0` is `-0 - -0 = +0`; musl returns `x` itself below 2^-27.
    let r = R::select(x.eq(R::splat(0.0)), x, r);
    R::select(x.is_nan(), x, r)
}

const PI: f64 = 3.141_592_653_589_793_116_0e+00;
const PI_LO: f64 = 1.224_646_799_147_353_177_2e-16;
const PI_2: f64 = 1.570_796_326_794_896_558_00e+00;
const PI_4: f64 = 7.853_981_633_974_482_789_99e-01;

/// `atan2(y, x)`, the angle of `(x, y)` in `[-pi, pi]`: fdlibm's case analysis of signs, zeros
/// and infinities, then `atan |y/x|` moved into the right quadrant with pi carried in two parts.
/// Under 2 ulp (fdlibm's statement).
#[inline]
pub fn atan2<R: Real>(y: R, x: R) -> R {
    let zero = R::splat(0.0);
    let inf = R::splat(f64::INFINITY);
    let ys = y.sign_bit();
    let xs = x.sign_bit();
    let ax = x.abs();
    let ay = y.abs();
    // The general case.
    let hx = R::bits_shr(ax.to_bits(), 32);
    let hy = R::bits_shr(ay.to_bits(), 32);
    let steep = R::bits_lt(R::bits_add(hx, R::bits(64 << 20)), hy);
    let flat = R::bits_lt(R::bits_add(hy, R::bits(64 << 20)), hx);
    let z = R::select(xs & flat, zero, atan(ay / ax));
    let pos = R::select(xs, R::splat(PI) - (z - R::splat(PI_LO)), z);
    let general = R::select(ys, -pos, pos);
    let half_pi = R::splat(PI_2).copysign(y);
    let general = R::select(steep, half_pi, general);
    // x infinite.
    let both_inf = ay.eq(inf);
    let q_inf = R::select(xs, R::splat(3.0) * R::splat(PI_4), R::splat(PI_4));
    let x_inf = R::select(both_inf, q_inf, R::select(xs, R::splat(PI), zero));
    let x_inf = R::select(ys, -x_inf, x_inf);
    // y infinite (x finite): +-pi/2.
    let r = R::select(ay.eq(inf), half_pi, general);
    let r = R::select(ax.eq(inf), x_inf, r);
    // x zero (y non-zero): +-pi/2. y zero: +-0 or +-pi.
    let r = R::select(ax.eq(zero), half_pi, r);
    let y_zero = R::select(xs, R::splat(PI).copysign(y), y);
    let r = R::select(ay.eq(zero), y_zero, r);
    // x == 1: atan(y) exactly as fdlibm.
    let r = R::select(x.eq(R::splat(1.0)), atan(y), r);
    R::select(x.is_nan() | y.is_nan(), x + y, r)
}

// ---- asin, acos: fdlibm's e_asin.c, e_acos.c ---------------------------------------------------

/// pi/2 as a double-double: rounded to f64 (within 0.28 ulp), and what rounding left off
/// (within 0.13 ulp of the rest): asin's and acos's, and the Payne-Hanek reduction's last product.
pub(crate) const PIO2_HI: f64 = 1.570_796_326_794_896_558_00e+00;
pub(crate) const PIO2_LO: f64 = 6.123_233_995_736_766_035_87e-17;
const PS: [f64; 6] = [
    1.666_666_666_666_666_574_15e-01,
    -3.255_658_186_224_009_154_05e-01,
    2.012_125_321_348_629_258_81e-01,
    -4.005_553_450_067_941_140_27e-02,
    7.915_349_942_898_145_321_76e-04,
    3.479_331_075_960_211_675_70e-05,
];
const QS: [f64; 4] =
    [-2.403_394_911_734_414_218_78e+00, 2.020_945_760_233_505_694_71e+00, -6.882_839_716_054_532_930_30e-01, 7.703_815_055_590_193_527_91e-02];

/// fdlibm's rational approximation of `(asin(sqrt z) - sqrt z) / z^(3/2)`.
#[inline]
fn asin_r<R: Real>(z: R) -> R {
    let p = |i: usize| R::splat(PS[i]);
    let q = |i: usize| R::splat(QS[i]);
    let num = z * (p(0) + z * (p(1) + z * (p(2) + z * (p(3) + z * (p(4) + z * p(5))))));
    let den = R::splat(1.0) + z * (q(0) + z * (q(1) + z * (q(2) + z * q(3))));
    num / den
}

/// `s` with its low 32 bits cleared: a value whose square is exact.
#[inline]
fn high_half<R: Real>(s: R) -> R {
    R::from_bits(s.to_bits() & R::bits(0xffff_ffff_0000_0000))
}

/// `asin x` on `[-1, 1]`: below 0.5 `x + x R(x^2)`; above, `pi/2 - 2 asin(sqrt((1 - x)/2))` with
/// the square root split so its error is carried. Under 1 ulp. NaN outside `[-1, 1]`.
#[inline]
pub fn asin<R: Real>(x: R) -> R {
    let a = x.abs();
    let near = x + x * asin_r(x * x);
    let z = (R::splat(1.0) - a) * R::splat(0.5);
    let s = z.sqrt();
    let r = asin_r(z);
    let steep = R::splat(PIO2_HI) - (R::splat(2.0) * (s + s * r) - R::splat(PIO2_LO));
    let f = high_half(s);
    let c = (z - f * f) / (s + f);
    let half = R::splat(0.5 * PIO2_HI);
    let mid = half - (R::splat(2.0) * s * r - (R::splat(PIO2_LO) - R::splat(2.0) * c) - (half - R::splat(2.0) * f));
    let far = R::select(R::splat(0.975).le(a), steep, mid).copysign(x);
    let v = R::select(a.lt(R::splat(0.5)), near, far);
    let v = R::select(a.eq(R::splat(1.0)), x * R::splat(PIO2_HI), v);
    let out = R::splat(1.0).lt(a) | x.is_nan();
    R::select(out, (x - x) / (x - x), v)
}

/// `acos x` on `[-1, 1]`, the same approximation as [`asin`] by fdlibm's three ranges. Under 1
/// ulp. NaN outside `[-1, 1]`.
#[inline]
pub fn acos<R: Real>(x: R) -> R {
    let a = x.abs();
    let one = R::splat(1.0);
    let near = R::splat(PIO2_HI) - (x - (R::splat(PIO2_LO) - x * asin_r(x * x)));
    let zn = (one + x) * R::splat(0.5);
    let sn = zn.sqrt();
    let wn = asin_r(zn) * sn - R::splat(PIO2_LO);
    let neg = R::splat(2.0) * (R::splat(PIO2_HI) - (sn + wn));
    let zp = (one - x) * R::splat(0.5);
    let sp = zp.sqrt();
    let df = high_half(sp);
    let c = (zp - df * df) / (sp + df);
    let wp = asin_r(zp) * sp + c;
    let pos = R::splat(2.0) * (df + wp);
    let v = R::select(a.lt(R::splat(0.5)), near, R::select(x.lt(R::splat(0.0)), neg, pos));
    let v = R::select(x.eq(one), R::splat(0.0), v);
    let v = R::select(x.eq(-one), R::splat(2.0 * PIO2_HI), v);
    let out = one.lt(a) | x.is_nan();
    R::select(out, (x - x) / (x - x), v)
}

// ---- exp, exp2: fdlibm's e_exp.c ---------------------------------------------------------------

pub(crate) const LN2_HI: f64 = 6.931_471_803_691_238_164_90e-01;
pub(crate) const LN2_LO: f64 = 1.908_214_929_270_587_700_02e-10;
pub(crate) const INVLN2: f64 = 1.442_695_040_888_963_387_00e+00;
const P1: f64 = 1.666_666_666_666_660_190_37e-01;
const P2: f64 = -2.777_777_777_701_559_338_42e-03;
const P3: f64 = 6.613_756_321_437_934_361_17e-05;
const P4: f64 = -1.653_390_220_546_525_153_90e-06;
const P5: f64 = 4.138_136_797_057_238_460_39e-08;
const EXP_OVER: f64 = 7.097_827_128_933_839_730_96e+02;
const EXP_UNDER: f64 = -7.451_332_191_019_411_084_2e+02;
/// ln 2 rounded to f64, and what rounding left off.
pub(crate) const LN2: f64 = 6.931_471_805_599_453_094_17e-01;
pub(crate) const LN2_TAIL: f64 = 2.319_046_813_846_299_558_42e-17;

/// `e^(hi - lo) * 2^k` for `|hi - lo| <= ln2/2` (about): fdlibm's Remez rational form
/// `1 + r + r c/(2 - c)`, `c` a degree-5 polynomial in `r^2`.
#[inline]
fn exp_kernel<R: Real>(hi: R, lo: R, k: R) -> R {
    let x = hi - lo;
    let xx = x * x;
    let c = x - xx * (R::splat(P1) + xx * (R::splat(P2) + xx * (R::splat(P3) + xx * (R::splat(P4) + xx * R::splat(P5)))));
    let y = R::splat(1.0) + (x * c / (R::splat(2.0) - c) - lo + hi);
    scale(y, k)
}

/// `e^x`: `x = k ln2 + r` with ln 2 in two parts (the high one 32 bits, so `k * LN2_HI` is
/// exact), then [`exp_kernel`]. Under 1 ulp; `inf` above 709.78, `0` below -745.13.
#[inline]
pub fn exp<R: Real>(x: R) -> R {
    let k = round_even_small(R::select(x.abs().lt(R::splat(1000.0)), x, R::splat(0.0)) * R::splat(INVLN2));
    let hi = x - k * R::splat(LN2_HI);
    let lo = k * R::splat(LN2_LO);
    let r = exp_kernel(hi, lo, k);
    let r = R::select(R::splat(EXP_OVER).lt(x), R::splat(f64::INFINITY), r);
    let r = R::select(x.lt(R::splat(EXP_UNDER)), R::splat(0.0), r);
    R::select(x.is_nan(), x, r)
}

/// `2^x`: `x = k + r` exactly (`|r| <= 1/2`), `r ln2` as a sum of two by a fused multiply-add
/// (its error-free product), then [`exp_kernel`]. Under 1 ulp; `inf` from 1024, `0` below
/// -1075.
#[inline]
pub fn exp2<R: Real>(x: R) -> R {
    let inside = x.abs().lt(R::splat(2048.0));
    let xs = R::select(inside, x, R::splat(0.0));
    let k = round_even_small(xs);
    let r = xs - k;
    let hi = r * R::splat(LN2);
    let tail = r.fma(R::splat(LN2), -hi) + r * R::splat(LN2_TAIL);
    let v = exp_kernel(hi, -tail, k);
    let v = R::select(R::splat(1024.0).le(x), R::splat(f64::INFINITY), v);
    let v = R::select(x.lt(R::splat(-1075.0)), R::splat(0.0), v);
    R::select(x.is_nan(), x, v)
}

// ---- ln, log2: fdlibm's e_log.c, musl's log2.c ------------------------------------------------

const LG: [f64; 7] = [
    6.666_666_666_666_735_130e-01,
    3.999_999_999_940_941_908e-01,
    2.857_142_874_366_239_149e-01,
    2.222_219_843_214_978_396e-01,
    1.818_357_216_161_805_012e-01,
    1.531_383_769_920_937_332e-01,
    1.479_819_860_511_658_591e-01,
];
pub(crate) const IVLN2_HI: f64 = 1.442_695_040_721_446_275_71e+00;
pub(crate) const IVLN2_LO: f64 = 1.675_171_316_488_651_183_53e-10;

/// `x = 2^k (1 + f)`, `sqrt(1/2) <= 1 + f < sqrt 2`, for positive finite `x` (subnormals scaled
/// by 2^54 first). Returns `(k, f, s, hfsq, R)` of fdlibm's log: `ln(1 + f) = f - hfsq + s (hfsq +
/// R)`, `s = f/(2 + f)`, `R` a degree-14 minimax polynomial in `s`.
#[inline]
fn log_parts<R: Real>(x: R) -> (R, R, R, R, R) {
    let sub = x.lt(R::splat(2.225_073_858_507_201_4e-308));
    let xs = R::select(sub, x * R::splat(18_014_398_509_481_984.0), x);
    let k0 = R::select(sub, R::splat(-54.0), R::splat(0.0));
    let b = xs.to_bits();
    let hx = R::bits_add(R::bits_shr(b, 32), R::bits(0x3ff0_0000 - 0x3fe6_a09e));
    let kb = R::bits_sub(R::bits_shr(hx, 20), R::bits(0x3ff));
    let k = from_int_bits::<R>(kb) + k0;
    let hm = R::bits_add(hx & R::bits(0x000f_ffff), R::bits(0x3fe6_a09e));
    let m = R::from_bits(R::bits_shl(hm, 32) | (b & R::bits(0xffff_ffff)));
    let f = m - R::splat(1.0);
    let hfsq = R::splat(0.5) * f * f;
    let s = f / (R::splat(2.0) + f);
    let z = s * s;
    let w = z * z;
    let l = |i: usize| R::splat(LG[i]);
    let t1 = w * (l(1) + w * (l(3) + w * l(5)));
    let t2 = z * (l(0) + w * (l(2) + w * (l(4) + w * l(6))));
    (k, f, s, hfsq, t2 + t1)
}

/// The special values every logarithm shares: `-inf` at zero, NaN below zero or at NaN, `inf`
/// at `inf`.
#[inline]
fn log_special<R: Real>(x: R, v: R) -> R {
    let zero = R::splat(0.0);
    let v = R::select(x.eq(zero), R::splat(f64::NEG_INFINITY), v);
    let v = R::select(x.eq(R::splat(f64::INFINITY)), x, v);
    R::select(x.lt(zero) | x.is_nan(), (x - x) / zero, v)
}

/// `ln x`: [`log_parts`], then `k ln2` in two parts added last. Under 1 ulp.
#[inline]
pub fn ln<R: Real>(x: R) -> R {
    let (k, f, s, hfsq, r) = log_parts(x);
    let v = s * (hfsq + r) + k * R::splat(LN2_LO) - hfsq + f + k * R::splat(LN2_HI);
    log_special(x, v)
}

/// `log2 x`: [`log_parts`], with `f - hfsq` split so its high part times `1/ln2`'s high part
/// is exact (musl's log2). Under 1 ulp.
#[inline]
pub fn log2<R: Real>(x: R) -> R {
    let (k, f, s, hfsq, r) = log_parts(x);
    let hi = high_half(f - hfsq);
    let lo = f - hi - hfsq + s * (hfsq + r);
    let val_hi = hi * R::splat(IVLN2_HI);
    let val_lo = (lo + hi) * R::splat(IVLN2_LO) + lo * R::splat(IVLN2_HI);
    let w = k + val_hi;
    let val_lo = val_lo + ((k - w) + val_hi);
    log_special(x, val_lo + w)
}

// ---- pow, hypot, cbrt ---------------------------------------------------------------------------

/// `ln x` as a double-double `hi + lo` for positive finite `x`: [`log_parts`]' terms summed by
/// exact two-sums (`k LN2_HI` is exact: 11 bits times 32), `f^2/2`'s rounding error kept by a
/// fused multiply-add (`0.5 f` is exact). What is left is the polynomial's own error (fdlibm:
/// 2^-58.45) and the rounding of the small cubic term `s (hfsq + R)` (at most `f^2/4` of the
/// value): about 2^-56 relative, against [`ln`]'s 2^-53.
#[inline]
fn ln_dd<R: Real>(x: R) -> (R, R) {
    let (k, f, s, hfsq, r) = log_parts(x);
    let hf_err = (R::splat(0.5) * f).fma(f, -hfsq);
    let (a, ea) = two_sum(k * R::splat(LN2_HI), f);
    let (b, eb) = two_sum(a, -hfsq);
    let tail = s * (hfsq + r) + k * R::splat(LN2_LO) - hf_err;
    let (c, ec) = two_sum(b, tail);
    (c, ea + eb + ec)
}

/// `x^y` as `e^(y ln|x|)`: the logarithm as a double-double ([`ln_dd`]), the product `y ln|x|`
/// as a double-double (an error-free product by fused multiply-add, plus `y` times the
/// logarithm's low part), and the low part applied as `e^h (1 + l)`. The error budget: exp's 1
/// ulp plus `|y ln x|` times the logarithm's 2^-56, so about `1 + |y ln x| / 8` ulp; the stated
/// bound is `1 + |y ln x|` ulp (a prototype of this method measured 33 ulp at `|y ln x| = 279`;
/// the one before it, with a plain logarithm, broke `2 + |y ln x|`). fdlibm's own pow (under 1
/// ulp everywhere, a logarithm to 2^-70) is the end state. IEEE 754's special cases: `x^0 = 1`,
/// `1^y = 1`, a negative base only to an integer power, signed zeros and infinities by the
/// exponent's parity.
#[inline]
pub fn pow<R: Real>(x: R, y: R) -> R {
    let zero = R::splat(0.0);
    let one = R::splat(1.0);
    let inf = R::splat(f64::INFINITY);
    let ax = x.abs();
    let ay = y.abs();
    let (lh, ll) = ln_dd(R::select(ax.eq(zero) | ax.eq(inf) | ax.is_nan(), one, ax));
    let h0 = y * lh;
    // Beyond 1000 the result is 0 or inf whatever the low part (and an infinite `h0` would
    // make it NaN): drop it there.
    let near = h0.abs().lt(R::splat(1000.0));
    let lo0 = R::select(near, y.fma(lh, -h0) + y * ll, zero);
    let h = h0 + lo0;
    let lo = R::select(near, (h0 - h) + lo0, zero);
    let e = exp(h);
    let finite = zero.lt(e) & e.lt(inf);
    let core = R::select(finite, e.fma(lo, e), e);
    // Integer and odd exponents.
    let integral = floor(y).eq(y);
    let half = y * R::splat(0.5);
    let odd = integral & !floor(half).eq(half) & ay.lt(R::splat(9_007_199_254_740_992.0));
    let neg_result = x.sign_bit() & odd;
    // |x| zero or infinite.
    let at_zero = R::select(y.lt(zero), inf, zero);
    let at_inf = R::select(y.lt(zero), zero, inf);
    let mag = R::select(ax.eq(zero), at_zero, R::select(ax.eq(inf), at_inf, core));
    let mag = R::select(neg_result, -mag, mag);
    // y infinite.
    let grows = one.lt(ax).eq_mask(zero.lt(y));
    let at_yinf = R::select(ax.eq(one), one, R::select(grows, inf, zero));
    let v = R::select(ay.eq(inf), at_yinf, mag);
    // A negative finite base to a non-integer finite power.
    let bad = x.lt(zero) & !integral & ax.lt(inf) & ay.lt(inf);
    let v = R::select(bad, (x - x) / zero, v);
    let v = R::select(x.is_nan() | y.is_nan(), x + y, v);
    R::select(y.eq(zero) | x.eq(one), one, v)
}

/// A mask equality, for [`pow`]'s case table.
trait MaskEq: Sized {
    fn eq_mask(self, o: Self) -> Self;
}
impl<M: Copy + core::ops::BitAnd<Output = M> + core::ops::BitOr<Output = M> + core::ops::Not<Output = M>> MaskEq for M {
    #[inline]
    fn eq_mask(self, o: M) -> M {
        (self & o) | (!self & !o)
    }
}

/// `sqrt(x^2 + y^2)` without spurious overflow or underflow: both scaled by a power of two
/// when the larger is beyond 2^500 or below 2^-500, the squares and their sum carried exactly
/// as sums of two (error-free products by fused multiply-add), the root corrected by one Newton
/// step on that exact sum. Under 1 ulp. `inf` if either is infinite (even with a NaN), else NaN
/// if either is NaN.
#[inline]
pub fn hypot<R: Real>(x: R, y: R) -> R {
    let ax = x.abs();
    let ay = y.abs();
    let big = R::select(ax.lt(ay), ay, ax);
    let small = R::select(ax.lt(ay), ax, ay);
    let up = big.lt(R::splat(3.054_936_363_499_604_7e-151)); // 2^-500
    let down = R::splat(3.273_390_607_896_141_9e150).lt(big); // 2^500
    let sc = R::select(up, R::splat(4.149_515_568_880_993e180), R::select(down, R::splat(2.409_919_865_102_884_6e-181), R::splat(1.0)));
    let unsc = R::select(up, R::splat(2.409_919_865_102_884_6e-181), R::select(down, R::splat(4.149_515_568_880_993e180), R::splat(1.0)));
    let a = big * sc;
    let b = small * sc;
    let a2 = a * a;
    let a2l = a.fma(a, -a2);
    let b2 = b * b;
    let b2l = b.fma(b, -b2);
    let s = a2 + b2;
    let bb = s - a2;
    let err = (a2 - (s - bb)) + (b2 - bb);
    let tail = a2l + b2l + err;
    let h = s.sqrt();
    let res = h.fma(-h, s) + tail;
    let hc = R::select(h.eq(R::splat(0.0)), h, h + res / (R::splat(2.0) * h));
    let v = hc * unsc;
    let inf = R::splat(f64::INFINITY);
    let v = R::select(x.is_nan() | y.is_nan(), x + y, v);
    R::select(ax.eq(inf) | ay.eq(inf), inf, v)
}

const B1: u64 = 715_094_163;
const B2: u64 = 696_219_795;
const CP: [f64; 5] = [1.875_951_824_271_770_096_43, -1.884_979_795_433_771_698_75, 1.621_429_720_105_354_466_140, -0.758_397_934_778_766_047_437, 0.145_996_192_886_612_446_982];

/// `cbrt x`: an exponent-divided-by-three first guess (Kahan's bit trick), a polynomial step to
/// 23 bits, rounded to 21 bits, then one Newton step in full precision (musl's s_cbrt.c, from
/// FreeBSD). Under 0.667 ulp. Exact on `+-0`, `+-inf`, NaN.
#[inline]
pub fn cbrt<R: Real>(x: R) -> R {
    let b = x.to_bits();
    let sign = b & R::bits(SIGN);
    let hx = R::bits_shr(b, 32) & R::bits(0x7fff_ffff);
    let sub = R::bits_lt(hx, R::bits(0x0010_0000));
    let scaled = x * R::splat(18_014_398_509_481_984.0);
    let hs = R::bits_shr(scaled.to_bits(), 32) & R::bits(0x7fff_ffff);
    let h = R::select_bits(sub, hs, hx);
    let third = R::bits_shr(R::bits_mul(h, R::bits(0xaaaa_aaab)), 33);
    let guess = R::bits_add(third, R::select_bits(sub, R::bits(B2), R::bits(B1)));
    let mut t = R::from_bits(sign | R::bits_shl(guess, 32));
    let c = |i: usize| R::splat(CP[i]);
    let r = (t * t) * (t / x);
    t = t * ((c(0) + r * (c(1) + r * c(2))) + ((r * r * r) * (c(3) + r * c(4))));
    let tb = R::bits_add(t.to_bits(), R::bits(0x8000_0000)) & R::bits(0xffff_ffff_c000_0000);
    t = R::from_bits(tb);
    let s = t * t;
    let r = x / s;
    let w = t + t;
    let r = (r - t) / (w + r);
    let t = t + t * r;
    let special = x.eq(R::splat(0.0)) | !x.abs().lt(R::splat(f64::INFINITY));
    R::select(special, x + x, t)
}
