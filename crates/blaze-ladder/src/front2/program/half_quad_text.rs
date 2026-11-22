//! A float literal's text rounded once to the nearest `f16` and once to the nearest `f128`,
//! for the seam's `float_lit_half_quad` and `float_pat_half_quad`.
//!
//! The Reference, "Literal expressions", "Floating-point literal expressions": "The value of
//! the expression is determined from the string representation of the token as follows: Any
//! suffix is removed from the string. Any underscores are removed from the string. The rest of
//! the string is converted to the expression's type as if by `f32::from_str` or
//! `f64::from_str`." At `f16` and `f128` the rule is the same at those types: the pinned
//! library's `core/src/primitive_docs.rs` names them "the "binary16" type defined in IEEE
//! 754-2008" and "the "binary128" type", and a decimal is converted to the nearest value of
//! the format, ties to the even significand (IEEE 754-2008 4.3.1, roundTiesToEven), once,
//! from the digits.
//!
//! The pinned library has no such parse to borrow: `core/src/num/float_parse.rs` implements
//! `FromStr` for `f16` only `#[cfg(target_has_reliable_f16)]` (blaze's own build would need
//! the `f16` feature), and for `f128` not at all. So this file rounds by exact integer
//! arithmetic: the value is `d * 10^e10` = `n / m` with big integers; `q = floor(n * 2^k / m)`
//! is taken to 116 or 117 bits with a sticky bit for a nonzero remainder; each format is
//! rounded from `q` and that bit. `q` is truncated, never rounded, so neither format is
//! rounded twice.
//!
//! A literal past the largest `f128` is infinity in both formats and one under half the
//! least `f128` subnormal is zero (both clamped before any big arithmetic, so the integers stay
//! within about 16,600 bits past the text's own digits). That a literal out of its type's
//! range is an error is the `overflowing_literals` lint's to say, before this value is read.

use core::cmp::Ordering;
use core::fmt;

use blaze_own::{Alloc, Vec};

/// A literal's value in both formats: the bits of the nearest binary16 and binary128.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HalfQuad {
    pub(super) half: u16,
    pub(super) quad: u128,
}

/// Why a text is no decimal float literal, by the form met; `at` is a byte offset into the
/// literal's own text (trimmed, its suffix kept).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HalfQuadRefusal {
    /// The text does not start with a decimal digit (`.5`, an empty text).
    NoLeadingDigit,
    /// A byte that is no decimal digit where a digit, `_`, `.` or an exponent was wanted.
    NotADigit { at: u32 },
    /// An `e` or `E` with no decimal digit after its sign (`1e`, `1e+`).
    EmptyExponent,
    /// A second `.`, or a `.` inside the exponent.
    StrayPoint { at: u32 },
}

impl HalfQuadRefusal {
    /// The form, as a refusal's `what` names it.
    pub(super) fn what(self) -> &'static str {
        match self {
            HalfQuadRefusal::NoLeadingDigit => "a float literal that does not start with a decimal digit",
            HalfQuadRefusal::NotADigit { .. } => "a float literal with a byte that is no decimal digit",
            HalfQuadRefusal::EmptyExponent => "a float literal whose exponent has no digit",
            HalfQuadRefusal::StrayPoint { .. } => "a float literal with a second `.` or a `.` in its exponent",
        }
    }
}

impl fmt::Display for HalfQuadRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HalfQuadRefusal::NotADigit { at } | HalfQuadRefusal::StrayPoint { at } => write!(f, "{} (byte {at})", self.what()),
            HalfQuadRefusal::NoLeadingDigit | HalfQuadRefusal::EmptyExponent => f.write_str(self.what()),
        }
    }
}

impl core::error::Error for HalfQuadRefusal {}

/// An IEEE 754 binary interchange format: its significand's precision (the hidden bit
/// counted) and its exponent field's width.
#[derive(Clone, Copy)]
struct Format {
    precision: u32,
    exp_bits: u32,
}

/// binary16: 11 bits of precision, 5 of exponent.
const HALF: Format = Format { precision: 11, exp_bits: 5 };
/// binary128: 113 bits of precision, 15 of exponent.
const QUAD: Format = Format { precision: 113, exp_bits: 15 };

impl Format {
    /// The exponent bias, which is also the largest unbiased exponent (emax).
    fn bias(self) -> i64 {
        (1i64 << (self.exp_bits - 1)) - 1
    }

    /// The least normal unbiased exponent (emin = 1 - emax).
    fn emin(self) -> i64 {
        1 - self.bias()
    }

    /// The bits of positive infinity: the exponent field all ones, the significand zero.
    fn infinity(self) -> u128 {
        ((1u128 << self.exp_bits) - 1) << (self.precision - 1)
    }
}

/// The quotient `q` is below `2^Q_BITS` and at least `2^(Q_BITS - 2)`: `QUAD`'s precision
/// and four more bits, so a rounding bit and a guard are always below `QUAD`'s last place.
const Q_BITS: u32 = 117;
/// `10^(t - 1) >= 10^4933` is past `f128::MAX` (about 1.1897e4932) and past the threshold of
/// rounding to infinity (`MAX` and half its ulp, also under 1e4933): infinity.
const QUAD_OVER: i64 = 4933;
/// `10^t <= 10^-4966` is under half the least `f128` subnormal (2^-16494, about 6.4752e-4966;
/// its half about 3.2376e-4966): zero.
const QUAD_UNDER: i64 = -4966;
/// An exponent's digits stop counting at this magnitude: far past both clamps above.
const EXP_CAP: i64 = 1_000_000_000;

/// `text` (`1.5`, `0.1f128`, `1_000.0e-3_f16`, a pattern's magnitude) rounded to the
/// nearest binary16 and binary128 (the module's rule). `alloc` holds the big integers.
pub(super) fn half_quad(text: &str, alloc: Alloc<'_>) -> Result<HalfQuad, HalfQuadRefusal> {
    let text = text.trim();
    // "Any suffix is removed": a float suffix, if any (none is a tail of another).
    let digits_text = ["f128", "f16", "f32", "f64"].iter().find_map(|s| text.strip_suffix(*s)).unwrap_or(text);
    let bytes = digits_text.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_digit) {
        return Err(HalfQuadRefusal::NoLeadingDigit);
    }
    let at = |i: usize| u32::try_from(i).unwrap_or(u32::MAX);
    let mut d = Big::new(alloc);
    // Significant digits in `d` (from the first nonzero one), and digits after the point.
    let mut digits: i64 = 0;
    let mut frac: i64 = 0;
    let mut point = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            c @ b'0'..=b'9' => {
                if point {
                    frac += 1;
                }
                if !(d.is_zero() && c == b'0') {
                    d.mul_add_small(10, u32::from(c - b'0'));
                    digits += 1;
                }
            }
            b'_' => {}
            b'.' if !point => point = true,
            b'.' => return Err(HalfQuadRefusal::StrayPoint { at: at(i) }),
            b'e' | b'E' => break,
            _ => return Err(HalfQuadRefusal::NotADigit { at: at(i) }),
        }
        i += 1;
    }
    let mut exp: i64 = 0;
    if i < bytes.len() {
        i += 1;
        let negative = bytes.get(i) == Some(&b'-');
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let mut any = false;
        while i < bytes.len() {
            match bytes[i] {
                c @ b'0'..=b'9' => {
                    any = true;
                    exp = (exp * 10 + i64::from(c - b'0')).min(EXP_CAP);
                }
                b'_' => {}
                b'.' => return Err(HalfQuadRefusal::StrayPoint { at: at(i) }),
                _ => return Err(HalfQuadRefusal::NotADigit { at: at(i) }),
            }
            i += 1;
        }
        if !any {
            return Err(HalfQuadRefusal::EmptyExponent);
        }
        if negative {
            exp = -exp;
        }
    }
    if d.is_zero() {
        return Ok(HalfQuad { half: 0, quad: 0 });
    }
    // The value is `d * 10^e10`, and `10^(total - 1) <= value < 10^total`.
    let e10 = exp - frac;
    let total = e10 + digits;
    if total - 1 >= QUAD_OVER {
        return Ok(HalfQuad { half: half_bits(HALF.infinity()), quad: QUAD.infinity() });
    }
    if total <= QUAD_UNDER {
        return Ok(HalfQuad { half: 0, quad: 0 });
    }
    let (q, k, sticky) = quotient(d, e10, alloc);
    Ok(HalfQuad { half: half_bits(round(HALF, q, k, sticky)), quad: round(QUAD, q, k, sticky) })
}

/// A binary16's bits from [`round`]'s, which are at most `HALF.infinity()` (0x7c00).
fn half_bits(bits: u128) -> u16 {
    // `round` returns at most the format's infinity, which is 15 bits wide at binary16.
    bits as u16
}

/// `(q, k, sticky)` with `d * 10^e10 = (q + f) * 2^-k`, `0 <= f < 1`, `sticky` = `f != 0`,
/// and `2^(Q_BITS - 2) <= q < 2^Q_BITS`. `d` is nonzero.
fn quotient<'a>(d: Big<'a>, e10: i64, alloc: Alloc<'a>) -> (u128, i64, bool) {
    let mut n = d;
    let mut m = Big::new(alloc);
    m.mul_add_small(1, 1);
    if e10 >= 0 {
        n.mul_pow10(e10.unsigned_abs());
    } else {
        m.mul_pow10(e10.unsigned_abs());
    }
    // n / m lies in [2^(bn - bm - 1), 2^(bn - bm + 1)); scaled by 2^k it lies in
    // [2^(Q_BITS - 2), 2^Q_BITS).
    let k = i64::from(Q_BITS - 1) - (n.bits() - m.bits());
    if k >= 0 {
        n.shl(k.unsigned_abs());
    } else {
        m.shl(k.unsigned_abs());
    }
    // Restoring division, one bit of `q` a step from bit Q_BITS - 1 down: `m` is the divisor
    // shifted to the step's bit.
    m.shl(u64::from(Q_BITS - 1));
    let mut q = 0u128;
    for bit in (0..Q_BITS).rev() {
        if n.cmp(&m) != Ordering::Less {
            n.sub(&m);
            q |= 1u128 << bit;
        }
        m.shr1();
    }
    (q, k, !n.is_zero())
}

/// The bits of the value `(q + f) * 2^-k` (`f` in `[0, 1)`, nonzero when `sticky`) in
/// format `fmt`, rounded to nearest, ties to even; infinity past the largest finite value.
/// `q` is at least `2^(Q_BITS - 2)`.
fn round(fmt: Format, q: u128, k: i64, sticky: bool) -> u128 {
    let p = i64::from(fmt.precision);
    let len = i64::from(128 - q.leading_zeros());
    // floor(log2(value)).
    let e = len - 1 - k;
    if e > fmt.bias() {
        return fmt.infinity();
    }
    // A subnormal keeps emin's last place.
    let eq = e.max(fmt.emin());
    // Bits of `q` under the format's last place (2^(eq - p + 1)): at least 3 (Q_BITS - p at
    // binary128).
    let drop = eq - (p - 1) + k;
    let (m, half, rest) = if drop > 128 {
        // The value is under 2^(len - k) <= a quarter of the last place: rounds to zero.
        (0, false, true)
    } else if drop == 128 {
        (0, q >> 127 == 1, q & ((1u128 << 127) - 1) != 0)
    } else {
        let d = drop as u32;
        (q >> d, (q >> (d - 1)) & 1 == 1, q & ((1u128 << (d - 1)) - 1) != 0)
    };
    let up = half && (sticky || rest || m & 1 == 1);
    let m = m + u128::from(up);
    // The biased exponent less one, so that adding `m` (its hidden bit at p - 1 for a normal,
    // none for a subnormal, 2^p after a carry) lands in the right field: 0 for a subnormal.
    let base = u128::from((eq + fmt.bias() - 1).unsigned_abs());
    ((base << (p - 1)) + m).min(fmt.infinity())
}

/// A nonnegative integer as little-endian 32-bit limbs, with no zero limb on top.
struct Big<'a> {
    limbs: Vec<u32, Alloc<'a>>,
}

impl<'a> Big<'a> {
    fn new(alloc: Alloc<'a>) -> Big<'a> {
        Big { limbs: Vec::new_in(alloc) }
    }

    fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    /// `self = self * mul + add` (`mul` nonzero).
    fn mul_add_small(&mut self, mul: u32, add: u32) {
        let mut carry = u64::from(add);
        for limb in self.limbs.iter_mut() {
            let x = u64::from(*limb) * u64::from(mul) + carry;
            *limb = x as u32;
            carry = x >> 32;
        }
        if carry != 0 {
            self.limbs.push(carry as u32);
        }
    }

    /// `self = self * 10^n`.
    fn mul_pow10(&mut self, mut n: u64) {
        while n >= 9 {
            self.mul_add_small(1_000_000_000, 0);
            n -= 9;
        }
        if n > 0 {
            self.mul_add_small(10u32.pow(n as u32), 0);
        }
    }

    /// The number of significant bits (0 for zero).
    fn bits(&self) -> i64 {
        match self.limbs.last() {
            None => 0,
            Some(&top) => (self.limbs.len() as i64 - 1) * 32 + i64::from(32 - top.leading_zeros()),
        }
    }

    /// `self = self << n`.
    fn shl(&mut self, n: u64) {
        if self.is_zero() || n == 0 {
            return;
        }
        let bits = (n % 32) as u32;
        if bits != 0 {
            let mut carry = 0u32;
            for limb in self.limbs.iter_mut() {
                let x = *limb;
                *limb = (x << bits) | carry;
                carry = x >> (32 - bits);
            }
            if carry != 0 {
                self.limbs.push(carry);
            }
        }
        let words = (n / 32) as usize;
        if words != 0 {
            let old = self.limbs.len();
            self.limbs.resize(old + words, 0);
            self.limbs.copy_within(0..old, words);
            self.limbs[..words].fill(0);
        }
    }

    /// `self = self >> 1`.
    fn shr1(&mut self) {
        let mut carry = 0u32;
        for limb in self.limbs.iter_mut().rev() {
            let x = *limb;
            *limb = (x >> 1) | (carry << 31);
            carry = x & 1;
        }
        if self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    fn cmp(&self, other: &Big<'_>) -> Ordering {
        match self.limbs.len().cmp(&other.limbs.len()) {
            Ordering::Equal => {}
            unequal => return unequal,
        }
        for (a, b) in self.limbs.iter().rev().zip(other.limbs.iter().rev()) {
            match a.cmp(b) {
                Ordering::Equal => {}
                unequal => return unequal,
            }
        }
        Ordering::Equal
    }

    /// `self = self - other`, where `other <= self`.
    fn sub(&mut self, other: &Big<'_>) {
        let mut borrow = 0u64;
        for (i, limb) in self.limbs.iter_mut().enumerate() {
            let a = u64::from(*limb);
            let b = u64::from(other.limbs.get(i).copied().unwrap_or(0)) + borrow;
            if a >= b {
                *limb = (a - b) as u32;
                borrow = 0;
            } else {
                *limb = (a + (1u64 << 32) - b) as u32;
                borrow = 1;
            }
        }
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::Alloc;

    use super::{HalfQuad, HalfQuadRefusal, half_quad};

    fn hq(text: &str) -> HalfQuad {
        half_quad(text, Alloc::global()).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    /// The Reference, "Floating-point literal expressions": the suffix and the underscores
    /// are removed and the rest is converted to the type (the pinned library's
    /// `primitive_docs.rs`: binary16 and binary128) to nearest, ties to even. Every
    /// expectation by hand from the formats (binary16: 1 + 5 + 10 bits, bias 15; binary128:
    /// 1 + 15 + 112 bits, bias 16383):
    /// - 1.5 = 1.1b * 2^0: 0x3e00 and 0x3fff8 followed by 27 hex zeros.
    /// - 0.1 = 1.6 * 2^-4; 0.6 = 0.1001 1001 ... b: binary16 keeps 10 bits 1001100110, the
    ///   next 0110.. rounds down: 0x2e66; binary128 keeps 28 hex nines, the next 1001.. is
    ///   past half: up to ...9a, exponent 16379 = 0x3ffb.
    /// - 100 = 1.5625 * 2^6 (0.5625 = 0.1001b): 0x5640 and 0x40059 then zeros.
    /// - 65520 is half way between binary16's largest, 65504 (0x7bff, an odd significand),
    ///   and 2^16: ties to even rounds to 2^16, past the format: infinity, 0x7c00.
    /// - 2^-24 = 5.9604644775390625e-8 is binary16's least subnormal, 0x0001; 2^-25 is half
    ///   of it, a tie between 0 and 1: even, 0. 1e-7 is about 1.68 of 2^-24: 0x0002.
    /// - binary128's least subnormal is 2^-16494, about 6.4752e-4966: 1e-4965 is about 1.544
    ///   of it (2), 4e-4966 about 0.618 (1), 3e-4966 about 0.463 (0); 1e4933 is past
    ///   `f128::MAX` (about 1.19e4932): infinity, 0x7fff then zeros.
    ///
    /// A wrong fix fails it: rounding the f128 by way of f64 gives 0.1 as 0x3ffb999999999999a
    /// then zeros; a round half up makes 2^-25 0x0001; a round toward zero makes 1e-7 0x0001
    /// and 65520 0x7bff.
    /// Expected line: `test front2::program::half_quad_text::tests::literals_round_once_to_binary16_and_binary128 ... ok`.
    #[test]
    fn literals_round_once_to_binary16_and_binary128() {
        let z = |hex_digits: u32| hex_digits * 4;
        assert_eq!(hq("1.5"), HalfQuad { half: 0x3e00, quad: 0x3fff8 << z(27) });
        assert_eq!(hq("1_5e-1_f16"), HalfQuad { half: 0x3e00, quad: 0x3fff8 << z(27) });
        assert_eq!(hq("0.1f128"), HalfQuad { half: 0x2e66, quad: 0x3ffb_9999_9999_9999_9999_9999_9999_999a });
        assert_eq!(hq("100"), HalfQuad { half: 0x5640, quad: 0x40059 << z(27) });
        assert_eq!(hq("2."), HalfQuad { half: 0x4000, quad: 0x4000 << z(28) });
        assert_eq!(hq("0.0"), HalfQuad { half: 0, quad: 0 });
        assert_eq!(hq("65504").half, 0x7bff);
        assert_eq!(hq("65520").half, 0x7c00);
        assert_eq!(hq("1e5").half, 0x7c00);
        assert_eq!(hq("5.9604644775390625e-8").half, 0x0001);
        assert_eq!(hq("2.98023223876953125e-8").half, 0x0000);
        assert_eq!(hq("1e-7").half, 0x0002);
        assert_eq!(hq("1e-4965").quad, 2);
        assert_eq!(hq("4e-4966").quad, 1);
        assert_eq!(hq("3e-4966").quad, 0);
        assert_eq!(hq("1e-5000").quad, 0);
        assert_eq!(hq("1e4933"), HalfQuad { half: 0x7c00, quad: 0x7fff << z(28) });
        // Not a decimal float literal: refused by the form.
        let no = |t: &str| half_quad(t, Alloc::global()).expect_err(t);
        assert_eq!(no(".5"), HalfQuadRefusal::NoLeadingDigit);
        assert_eq!(no("1e"), HalfQuadRefusal::EmptyExponent);
        assert_eq!(no("1e+_f128"), HalfQuadRefusal::EmptyExponent);
        assert_eq!(no("1x5"), HalfQuadRefusal::NotADigit { at: 1 });
        assert_eq!(no("1.2.3"), HalfQuadRefusal::StrayPoint { at: 3 });
    }
}
