//! Floats as text: the shortest form that reads back as the same float ([`ryu`]), in exactly
//! `core::fmt`'s `{}` layout ([`display`]) or its `{:?}` layout ([`debug`]), and a fixed number of places ([`fixed`]), exactly
//! `core::fmt`'s `{:.N}`: the exact binary value rounded half to even. Reading them back is
//! [`parse`].

use blaze_int::{Int as _, idx};

use crate::ascii::Ascii;
use crate::int::{Decimal, u64_into};
use crate::put::Put;

pub(crate) mod fixed;
pub(crate) mod parse;
pub(crate) mod ryu;

mod sealed {
    pub trait Sealed {}
}

/// `f32` or `f64`.
pub trait Float: Copy + sealed::Sealed {
    /// Explicit mantissa bits: 52 or 23.
    #[doc(hidden)]
    const MANTISSA_BITS: u32;
    /// Exponent bits: 11 or 8.
    #[doc(hidden)]
    const EXPONENT_BITS: u32;
    /// The exponent bias: 1023 or 127.
    #[doc(hidden)]
    const BIAS: i32;
    /// The largest integer the fast parse path takes exactly: 2^53 or 2^24.
    #[doc(hidden)]
    const FAST_MAX_MANTISSA: u64;
    /// The largest power of ten exact in the type: 22 or 10.
    #[doc(hidden)]
    const FAST_MAX_EXP10: i64;
    /// What a parse past the range expected, naming the type.
    #[doc(hidden)]
    const IN_RANGE: &'static str;
    /// Positive infinity's bits, and the quiet NaN's that `core` writes as `NaN`.
    #[doc(hidden)]
    const INFINITY_BITS: u64;
    #[doc(hidden)]
    const NAN_BITS: u64;
    /// `core`'s correctly rounded conversion of a scanned token; `None` past the range.
    #[doc(hidden)]
    fn core_parse(token: &str) -> Option<Self>;
    #[doc(hidden)]
    fn bits64(self) -> u64;
    #[doc(hidden)]
    fn from_bits64(bits: u64) -> Self;
    /// The same value as an `f64`, exactly.
    #[doc(hidden)]
    fn to_f64(self) -> f64;
    /// Whether `{:?}` writes it with an exponent: nonzero and below `1e-4`, or `1e16` and up.
    #[doc(hidden)]
    fn scientific(self) -> bool;
    /// `w * 10^exp10`, correctly rounded, for `w <= FAST_MAX_MANTISSA` and
    /// `|exp10| <= FAST_MAX_EXP10` (both factors exact, so one IEEE operation rounds once).
    #[doc(hidden)]
    fn fast(w: u64, exp10: i64) -> Self;
}

const POW10_F64: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19,
    1e20, 1e21, 1e22,
];
const POW10_F32: [f32; 11] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10];

impl sealed::Sealed for f64 {}
impl Float for f64 {
    const MANTISSA_BITS: u32 = 52;
    const EXPONENT_BITS: u32 = 11;
    const BIAS: i32 = 1023;
    const FAST_MAX_MANTISSA: u64 = 1 << 53;
    const FAST_MAX_EXP10: i64 = 22;
    const IN_RANGE: &'static str = "a number within f64's range (at most 1.7976931348623157e308)";
    const INFINITY_BITS: u64 = 0x7FF0_0000_0000_0000;
    const NAN_BITS: u64 = 0x7FF8_0000_0000_0000;

    #[inline]
    fn core_parse(token: &str) -> Option<f64> {
        token.parse::<f64>().ok().filter(|x| x.is_finite())
    }
    #[inline(always)]
    fn bits64(self) -> u64 {
        self.to_bits()
    }
    #[inline(always)]
    fn from_bits64(bits: u64) -> f64 {
        f64::from_bits(bits)
    }
    #[inline(always)]
    fn to_f64(self) -> f64 {
        self
    }
    #[inline]
    fn scientific(self) -> bool {
        let abs = f64::from_bits(self.to_bits() & !(1u64 << 63));
        (abs != 0.0 && abs < 1e-4) || abs >= 1e16
    }
    #[inline]
    fn fast(w: u64, exp10: i64) -> f64 {
        // `w <= 2^53`, so both halves and their sum are exact.
        let hi = f64::from((w >> 32).truncate_to::<u32>());
        let lo = f64::from(w.truncate_to::<u32>());
        let w = hi * 4_294_967_296.0 + lo;
        let power = POW10_F64[exp10.unsigned_abs().truncate_to::<usize>()];
        match exp10 < 0 {
            true => w / power,
            false => w * power,
        }
    }
}

impl sealed::Sealed for f32 {}
impl Float for f32 {
    const MANTISSA_BITS: u32 = 23;
    const EXPONENT_BITS: u32 = 8;
    const BIAS: i32 = 127;
    const FAST_MAX_MANTISSA: u64 = 1 << 24;
    const FAST_MAX_EXP10: i64 = 10;
    const IN_RANGE: &'static str = "a number within f32's range (at most 3.4028235e38)";
    const INFINITY_BITS: u64 = 0x7F80_0000;
    const NAN_BITS: u64 = 0x7FC0_0000;

    #[inline]
    fn core_parse(token: &str) -> Option<f32> {
        token.parse::<f32>().ok().filter(|x| x.is_finite())
    }
    #[inline(always)]
    fn bits64(self) -> u64 {
        u64::from(self.to_bits())
    }
    #[inline(always)]
    fn from_bits64(bits: u64) -> f32 {
        f32::from_bits(bits.truncate_to::<u32>())
    }
    #[inline(always)]
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    #[inline]
    fn scientific(self) -> bool {
        let abs = f32::from_bits(self.to_bits() & !(1u32 << 31));
        (abs != 0.0 && abs < 1e-4) || abs >= 1e16
    }
    #[inline]
    fn fast(w: u64, exp10: i64) -> f32 {
        // `w <= 2^24`, so both halves and their sum are exact.
        let hi = f32::from((w >> 16).truncate_to::<u16>());
        let lo = f32::from(w.truncate_to::<u16>());
        let w = hi * 65_536.0 + lo;
        let power = POW10_F32[exp10.unsigned_abs().truncate_to::<usize>()];
        match exp10 < 0 {
            true => w / power,
            false => w * power,
        }
    }
}

/// A float's fields: sign, biased exponent, explicit mantissa.
#[inline(always)]
pub(crate) fn fields<F: Float>(x: F) -> (bool, u32, u64) {
    let bits = x.bits64();
    let mantissa = bits & ((1u64 << F::MANTISSA_BITS) - 1);
    let exponent = ((bits >> F::MANTISSA_BITS) & ((1u64 << F::EXPONENT_BITS) - 1)).truncate_to::<u32>();
    let negative = (bits >> (F::MANTISSA_BITS + F::EXPONENT_BITS)) & 1 == 1;
    (negative, exponent, mantissa)
}

/// A float's shortest round-tripping form before it is laid out.
enum Shortest {
    /// `NaN`, `inf` or `-inf`: one text in both layouts.
    Special(&'static str),
    /// Zero, with its sign.
    Zero { negative: bool },
    /// `digits * 10^e10`, the digits without leading or trailing zeros.
    Digits { negative: bool, digits: Decimal, e10: i32 },
}

#[inline(always)]
fn shortest_of<F: Float>(x: F) -> Shortest {
    let (negative, exponent, mantissa) = fields(x);
    let all_ones = (1u32 << F::EXPONENT_BITS) - 1;
    if exponent == all_ones {
        return Shortest::Special(match (mantissa != 0, negative) {
            (true, _) => "NaN",
            (false, true) => "-inf",
            (false, false) => "inf",
        });
    }
    if exponent == 0 && mantissa == 0 {
        return Shortest::Zero { negative };
    }
    let (value, e10) = ryu::shortest(mantissa, exponent, F::MANTISSA_BITS, F::BIAS);
    let mut digits = Decimal::at_end();
    u64_into(value, &mut digits);
    Shortest::Digits { negative, digits, e10 }
}

/// `x` as `{}` writes it: the shortest digits that read back as `x`, in plain decimal with no
/// exponent and no forced point: `0.1`, `1`, `-0`, `1e16` as `10000000000000000`, `inf`,
/// `NaN`.
pub(crate) fn display<F: Float, P: Put + ?Sized>(x: F, out: &mut P) {
    match shortest_of(x) {
        Shortest::Special(text) => out.put_str(text),
        Shortest::Zero { negative } => out.put_str(if negative { "-0" } else { "0" }),
        Shortest::Digits { negative, digits, e10 } => {
            // Up to 17 digits, a sign, a point and a few zeros fit in one piece; the long zero
            // runs of `1e300` or `5e-324` go out in blocks.
            let digits = digits.as_bytes();
            let n = digits.len().narrow_or_stop::<i32>();
            let point = n + e10;
            let mut text = Ascii::<64>::at_start();
            if negative {
                text.back(b'-');
            }
            if e10 >= 0 {
                text.back_all(digits);
                out.put_str(text.as_str());
                zeros(e10.cast_unsigned(), out);
            } else if point > 0 {
                let point = idx(point.cast_unsigned());
                text.back_all(&digits[..point]);
                text.back(b'.');
                text.back_all(&digits[point..]);
                out.put_str(text.as_str());
            } else {
                text.back_all(b"0.");
                out.put_str(text.as_str());
                zeros(point.unsigned_abs(), out);
                let mut tail = Ascii::<64>::at_start();
                tail.back_all(digits);
                out.put_str(tail.as_str());
            }
        }
    }
}

/// `n` zeros, 64 at a time.
#[inline]
fn zeros<P: Put + ?Sized>(n: u32, out: &mut P) {
    const BLOCK: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    let mut left = idx(n);
    while left > 0 {
        let take = left.min(BLOCK.len());
        out.put_str(&BLOCK[..take]);
        left -= take;
    }
}

/// `x` as `{:?}` writes it: `0.1`, `1.0`, `-0.0`, `1e16`, `1.5e-5`, `inf`, `NaN`.
pub(crate) fn debug<F: Float, P: Put + ?Sized>(x: F, out: &mut P) {
    match shortest_of(x) {
        Shortest::Special(text) => out.put_str(text),
        Shortest::Zero { negative } => out.put_str(if negative { "-0.0" } else { "0.0" }),
        Shortest::Digits { negative, digits, e10 } => {
            let mut text = Ascii::<64>::at_start();
            if negative {
                text.back(b'-');
            }
            lay_out(digits.as_bytes(), e10, x.scientific(), &mut text);
            out.put_str(text.as_str());
        }
    }
}

/// `digits * 10^e10` laid out as `{:?}` does, into `text`.
fn lay_out(digits: &[u8], e10: i32, scientific: bool, text: &mut Ascii<64>) {
    // At most 17 digits.
    let n = digits.len().narrow_or_stop::<i32>();
    if scientific {
        text.back(digits[0]);
        if digits.len() > 1 {
            text.back(b'.');
            text.back_all(&digits[1..]);
        }
        text.back(b'e');
        let exp = n - 1 + e10;
        let mut e = Decimal::at_end();
        u64_into(u64::from(exp.unsigned_abs()), &mut e);
        if exp < 0 {
            text.back(b'-');
        }
        text.back_all(e.as_bytes());
        return;
    }
    // Digits before the point.
    let point = n + e10;
    if e10 >= 0 {
        text.back_all(digits);
        for _ in 0..e10 {
            text.back(b'0');
        }
        text.back_all(b".0");
    } else if point > 0 {
        let point = idx(point.cast_unsigned());
        text.back_all(&digits[..point]);
        text.back(b'.');
        text.back_all(&digits[point..]);
    } else {
        text.back_all(b"0.");
        for _ in 0..point.unsigned_abs() {
            text.back(b'0');
        }
        text.back_all(digits);
    }
}
