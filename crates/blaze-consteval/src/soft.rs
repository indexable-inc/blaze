//! IEEE 754 binary floats computed on their bits with integers alone: binary16, binary32,
//! binary64 and binary128 (`f16`, `f32`, `f64`, `f128`). The host has no `f16` or `f128` to
//! lean on, so a constant of either type is evaluated here; `f32` and `f64` keep the host's
//! operations in machine.rs, which give the same bits (the property test below checks this
//! file's binary64 against the host).
//!
//! The pinned library's `core/src/primitive_docs.rs`: `f16` is "the "binary16" type defined in
//! IEEE 754-2008", `f128` "the "binary128" type"; "When a primitive operation (addition,
//! subtraction, multiplication, or division) is performed on this type, the result is rounded
//! according to the roundTiesToEven direction defined in IEEE 754-2008. ... If the result of a
//! sum exactly equals zero, the outcome is +0.0 unless both arguments were negative, then it is
//! -0.0. Subtraction `a - b` is regarded as a sum `a + (-b)`." A NaN result is the preferred
//! NaN (quiet bit set, payload zero, sign clear): Rust leaves which NaN an operation gives
//! open.
//!
//! Every value is unpacked to `m * 2^e` (`m` an integer with the implicit bit, `e` the exponent
//! of its lowest bit), computed exactly or with a sticky bit, and packed by one rounding
//! ([`round_pack`]). A decimal literal is converted once from its digits by exact big-integer
//! arithmetic ([`Format::parse`]), never by way of a narrower float.
//!
//! The same arithmetic lives in blaze-lir's `eval/float.rs` (the reference evaluator) and the
//! decimal conversion in blaze-ladder's `front2/program/half_quad_text.rs`, both private to
//! their crates; the three belong in one home (named in the lane's report).

use core::cmp::Ordering;

use blaze_own::{Alloc, Vec};
use blaze_typeck::FloatTy;

use crate::lit::LitError;

/// The low `n` bits set, for `n` up to 128.
const fn low(n: u32) -> u128 {
    match n {
        128.. => u128::MAX,
        n => (1u128 << n) - 1,
    }
}

/// An IEEE 754 binary interchange format: its exponent and fraction field widths.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Format {
    exp: u32,
    frac: u32,
}

/// What a float's bits hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Nan,
    Inf,
    Zero,
    /// `m * 2^e`, `m` nonzero.
    Finite { m: u128, e: i32 },
}

impl Format {
    /// The format of a float type: field widths from IEEE 754-2008 table 3.5 (binary16: 5 and
    /// 10; binary32: 8 and 23; binary64: 11 and 52; binary128: 15 and 112).
    pub(crate) const fn of(f: FloatTy) -> Format {
        match f {
            FloatTy::F16 => Format { exp: 5, frac: 10 },
            FloatTy::F32 => Format { exp: 8, frac: 23 },
            FloatTy::F64 => Format { exp: 11, frac: 52 },
            FloatTy::F128 => Format { exp: 15, frac: 112 },
        }
    }

    const fn width(self) -> u32 {
        1 + self.exp + self.frac
    }

    const fn bias(self) -> i32 {
        (1 << (self.exp - 1)) - 1
    }

    /// The sign bit: `-x` is `x ^ sign()` ("Non-arithmetic" negation, a NaN's payload kept).
    pub(crate) const fn sign(self) -> u128 {
        1 << (self.width() - 1)
    }

    const fn signed(self, negative: bool) -> u128 {
        match negative {
            true => self.sign(),
            false => 0,
        }
    }

    const fn inf(self, negative: bool) -> u128 {
        self.signed(negative) | (low(self.exp) << self.frac)
    }

    /// The preferred NaN: quiet bit set, payload zero, sign clear.
    pub(crate) const fn nan(self) -> u128 {
        (low(self.exp) << self.frac) | (1 << (self.frac - 1))
    }

    /// The exponent of a subnormal's lowest bit, which is also the smallest normal's.
    const fn tiny(self) -> i32 {
        1 - self.bias() - self.frac as i32
    }

    fn unpack(self, bits: u128) -> (bool, Class) {
        let bits = bits & low(self.width());
        let negative = bits & self.sign() != 0;
        let biased = (bits >> self.frac) & low(self.exp);
        let fraction = bits & low(self.frac);
        let class = match (biased, fraction) {
            (b, 0) if b == low(self.exp) => Class::Inf,
            (b, _) if b == low(self.exp) => Class::Nan,
            (0, 0) => Class::Zero,
            (0, f) => Class::Finite { m: f, e: self.tiny() },
            (b, f) => Class::Finite { m: f | (1 << self.frac), e: b as i32 - self.bias() - self.frac as i32 },
        };
        (negative, class)
    }

    fn is_nan(self, bits: u128) -> bool {
        matches!(self.unpack(bits).1, Class::Nan)
    }

    /// A key ordering every non-NaN value of the format, -0 and +0 equal.
    const fn key(self, bits: u128) -> i128 {
        let magnitude = (bits & low(self.width() - 1)) as i128;
        match bits & self.sign() != 0 {
            true => -magnitude,
            false => magnitude,
        }
    }

    /// `a + b`.
    pub(crate) fn add(self, a: u128, b: u128) -> u128 {
        let ((sa, ca), (sb, cb)) = (self.unpack(a), self.unpack(b));
        match (ca, cb) {
            (Class::Nan, _) | (_, Class::Nan) => self.nan(),
            // "inf - inf" is invalid: NaN.
            (Class::Inf, Class::Inf) if sa != sb => self.nan(),
            (Class::Inf, _) => self.inf(sa),
            (_, Class::Inf) => self.inf(sb),
            (Class::Zero, Class::Zero) => self.signed(sa && sb),
            (Class::Zero, _) => b & low(self.width()),
            (_, Class::Zero) => a & low(self.width()),
            (Class::Finite { m: ma, e: ea }, Class::Finite { m: mb, e: eb }) => {
                // The larger magnitude first: a larger exponent is a larger value (a value with
                // an exponent above the subnormals' is normal, so above every value of a
                // smaller one).
                let ((sx, mx, ex), (sy, my, ey)) = match (ea, ma) >= (eb, mb) {
                    true => ((sa, ma, ea), (sb, mb, eb)),
                    false => ((sb, mb, eb), (sa, ma, ea)),
                };
                // Three guard bits, the smaller aligned with what falls off jammed in. With an
                // alignment of two or more the result keeps at least `frac + 3` bits, so the
                // jam lies below the round bit; with less nothing is jammed and the sum is exact.
                let (mx, my) = (mx << 3, shr_jam(my << 3, (ex - ey) as u32));
                let m = match sx == sy {
                    true => mx + my,
                    false => mx - my,
                };
                match m {
                    // An exact zero of a sum of opposite signs is +0.
                    0 => 0,
                    m => round_pack(self, sx, m, ex - 3, false),
                }
            }
        }
    }

    /// `a - b`, the sum `a + (-b)`.
    pub(crate) fn sub(self, a: u128, b: u128) -> u128 {
        self.add(a, b ^ self.sign())
    }

    /// `a * b`.
    pub(crate) fn mul(self, a: u128, b: u128) -> u128 {
        let ((sa, ca), (sb, cb)) = (self.unpack(a), self.unpack(b));
        let negative = sa != sb;
        match (ca, cb) {
            (Class::Nan, _) | (_, Class::Nan) => self.nan(),
            (Class::Inf, Class::Zero) | (Class::Zero, Class::Inf) => self.nan(),
            (Class::Inf, _) | (_, Class::Inf) => self.inf(negative),
            (Class::Zero, _) | (_, Class::Zero) => self.signed(negative),
            (Class::Finite { m: ma, e: ea }, Class::Finite { m: mb, e: eb }) => {
                let (hi, lo) = mul_wide(ma, mb);
                match hi {
                    0 => round_pack(self, negative, lo, ea + eb, false),
                    hi => {
                        // At most 226 bits (binary128's 113 squared): the top 128 kept, the
                        // rest sticky.
                        let k = bit_len(hi);
                        let m = (lo >> k) | (hi << (128 - k));
                        round_pack(self, negative, m, ea + eb + k as i32, lo & low(k) != 0)
                    }
                }
            }
        }
    }

    /// `a / b`.
    pub(crate) fn div(self, a: u128, b: u128) -> u128 {
        let ((sa, ca), (sb, cb)) = (self.unpack(a), self.unpack(b));
        let negative = sa != sb;
        match (ca, cb) {
            (Class::Nan, _) | (_, Class::Nan) => self.nan(),
            (Class::Inf, Class::Inf) | (Class::Zero, Class::Zero) => self.nan(),
            (Class::Inf, _) | (_, Class::Zero) => self.inf(negative),
            (_, Class::Inf) | (Class::Zero, _) => self.signed(negative),
            (Class::Finite { m: ma, e: ea }, Class::Finite { m: mb, e: eb }) => {
                // Both significands with their leading bit at bit 116, so `a / b` is in
                // (1/2, 2) and the remainder, below `2 b`, shifted once stays below 2^118.
                const LEAD: u32 = 116;
                let (sha, shb) = (LEAD + 1 - bit_len(ma), LEAD + 1 - bit_len(mb));
                let (a, b) = (ma << sha, mb << shb);
                // `frac + 4` quotient bits: `frac + 3` or `frac + 4` significant, two or more
                // below the precision, then the remainder as the sticky bit.
                let steps = self.frac + 4;
                let (mut q, mut r) = (0u128, a);
                for _ in 0..steps {
                    q <<= 1;
                    if r >= b {
                        r -= b;
                        q |= 1;
                    }
                    r <<= 1;
                }
                let e = ea - sha as i32 - (eb - shb as i32) - (steps as i32 - 1);
                round_pack(self, negative, q, e, r != 0)
            }
        }
    }

    /// `a % b`, C's `fmod` (Rust's `%` on floats): `a - n b` for `n` the quotient rounded
    /// toward zero, which is exact, with `a`'s sign. At most the format's exponent range of
    /// steps (32,766 at binary128).
    pub(crate) fn rem(self, a: u128, b: u128) -> u128 {
        let ((sa, ca), (_, cb)) = (self.unpack(a), self.unpack(b));
        match (ca, cb) {
            (Class::Nan, _) | (_, Class::Nan) | (Class::Inf, _) | (_, Class::Zero) => self.nan(),
            (Class::Zero, _) | (_, Class::Inf) => a & low(self.width()),
            (Class::Finite { m: ma, e: ea }, Class::Finite { m: mb, e: eb }) => {
                if (ea, ma) < (eb, mb) {
                    // `|a| < |b|`: `n` is 0.
                    return a & low(self.width());
                }
                // `ea >= eb` here. `(ma * 2^(ea-eb)) mod mb`, one doubling at a time;
                // `r < mb < 2^113`, so `r << 1` never overflows.
                let mut r = ma % mb;
                for _ in 0..(ea - eb) as u32 {
                    r = (r << 1) % mb;
                }
                match r {
                    0 => self.signed(sa),
                    r => round_pack(self, sa, r, eb, false),
                }
            }
        }
    }

    /// The order of `a` and `b`; `None` when either is NaN; -0 equals +0.
    pub(crate) fn compare(self, a: u128, b: u128) -> Option<Ordering> {
        if self.is_nan(a) || self.is_nan(b) {
            return None;
        }
        Some(self.key(a & low(self.width())).cmp(&self.key(b & low(self.width()))))
    }

    /// `a as T` from this format to `to` ("Type cast expressions": exact when widening, the
    /// closest value when narrowing, ties to even); an infinity stays one, a NaN becomes the
    /// preferred NaN.
    pub(crate) fn convert(self, to: Format, a: u128) -> u128 {
        let (negative, class) = self.unpack(a);
        match class {
            Class::Nan => to.nan(),
            Class::Inf => to.inf(negative),
            Class::Zero => to.signed(negative),
            Class::Finite { m, e } => round_pack(to, negative, m, e, false),
        }
    }

    /// The integer `(-1)^negative * magnitude` in this format, rounded to nearest, ties to
    /// even ("Type cast expressions": "with the current set of numeric types, overflow can only
    /// happen with u128 as f32 for values greater or equal to f32::MAX + (0.5 ULP)", and then
    /// infinity). A zero magnitude is a zero of the sign given (a literal `-0` at a float type).
    pub(crate) fn from_int(self, negative: bool, magnitude: u128) -> u128 {
        match magnitude {
            0 => self.signed(negative),
            m => round_pack(self, negative, m, 0, false),
        }
    }

    /// `a as` an integer of `bits` bits, `signed` or not ("Type cast expressions": "Casting
    /// from a float to an integer will round the float towards zero; NaN will return 0;
    /// values larger than the maximum integer value, including INFINITY, will saturate to the
    /// maximum value of the integer type; values smaller than the minimum integer value,
    /// including NEG_INFINITY, will saturate to the minimum value"), as that integer's bits.
    pub(crate) fn to_int(self, a: u128, bits: u32, signed: bool) -> u128 {
        let (negative, class) = self.unpack(a);
        let umax = low(bits);
        let smax = low(bits - 1);
        // The magnitude rounded toward zero; `None` past `u128`.
        let magnitude = match class {
            Class::Nan | Class::Zero => return 0,
            Class::Inf => None,
            Class::Finite { m, e } if e >= 0 => match bit_len(m) + e as u32 <= 128 {
                true => Some(m << e as u32),
                false => None,
            },
            Class::Finite { m, e } => Some(shr_round(m, e.unsigned_abs(), false).0),
        };
        match (signed, negative) {
            (false, true) => 0,
            (false, false) => magnitude.map_or(umax, |v| v.min(umax)),
            (true, false) => magnitude.map_or(smax, |v| v.min(smax)),
            // `smax + 1` is the magnitude of the minimum, whose negation is itself.
            (true, true) => magnitude.map_or(smax + 1, |v| v.min(smax + 1)).wrapping_neg() & umax,
        }
    }

    /// A decimal float literal's token text (`1.5`, `0.1f128`, `1_000.0e-3_f16`,
    /// `1.18973149535723176508575932662800702e+4932_f128`) as the nearest value of this format,
    /// ties to the even significand ("Floating-point literal expressions": "Any suffix is
    /// removed from the string. Any underscores are removed from the string. The rest of the
    /// string is converted to the expression's type"). `alloc` holds the big integers.
    ///
    /// The value is `d * 10^e10` = `n / m` with big integers; `q = floor(n * 2^k / m)` is taken
    /// to 116 or 117 bits with a sticky bit for a nonzero remainder, and rounded once. A text
    /// past the largest binary128 is infinity and one under half the least binary128
    /// subnormal is zero, both clamped before any big arithmetic, so the integers stay within
    /// about 16,600 bits past the text's own digits.
    pub(crate) fn parse(self, text: &str, alloc: Alloc<'_>) -> Result<u128, LitError> {
        let text = text.trim();
        // A float suffix, if any (none is a tail of another).
        let digits_text = ["f128", "f16", "f32", "f64"].iter().find_map(|s| text.strip_suffix(*s)).unwrap_or(text);
        let bytes = digits_text.as_bytes();
        if !bytes.first().is_some_and(u8::is_ascii_digit) {
            return Err(LitError::NotThatKind);
        }
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
                b'e' | b'E' => break,
                _ => return Err(LitError::NotThatKind),
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
                    _ => return Err(LitError::NotThatKind),
                }
                i += 1;
            }
            if !any {
                return Err(LitError::NotThatKind);
            }
            if negative {
                exp = -exp;
            }
        }
        if d.is_zero() {
            return Ok(0);
        }
        // The value is `d * 10^e10`, and `10^(total - 1) <= value < 10^total`.
        let e10 = exp - frac;
        let total = e10 + digits;
        if total - 1 >= QUAD_OVER {
            return Ok(self.inf(false));
        }
        if total <= QUAD_UNDER {
            return Ok(0);
        }
        let (q, k, sticky) = quotient(d, e10, alloc);
        // `|k|` is under 16,600 + 117 after the clamps above.
        let e = i32::try_from(-k).map_err(|_| LitError::TooLarge)?;
        Ok(round_pack(self, false, q, e, sticky))
    }
}

/// `m >> s`, with the last bit shifted out (the round bit) and whether any below it, or
/// `sticky`, was set.
const fn shr_round(m: u128, s: u32, sticky: bool) -> (u128, bool, bool) {
    match s {
        0 => (m, false, sticky),
        1..=127 => (m >> s, (m >> (s - 1)) & 1 == 1, sticky || m & low(s - 1) != 0),
        128 => (0, m >> 127 == 1, sticky || m & low(127) != 0),
        _ => (0, false, sticky || m != 0),
    }
}

/// `m >> s` with every bit shifted out jammed into the lowest bit.
const fn shr_jam(m: u128, s: u32) -> u128 {
    match s {
        0 => m,
        1..=127 => (m >> s) | (m & low(s) != 0) as u128,
        _ => (m != 0) as u128,
    }
}

/// The bit length of `m`.
const fn bit_len(m: u128) -> u32 {
    128 - m.leading_zeros()
}

/// `(-1)^negative * (m + t) * 2^e` rounded to nearest, ties to even, in `format`, where
/// `0 <= t < 1` and `t > 0` exactly when `sticky`. A caller passes `sticky` only with `m` of
/// more than the format's precision plus one bits, so the rounding shifts right by at least
/// two and `t` lies below the round bit.
fn round_pack(format: Format, negative: bool, m: u128, e: i32, sticky: bool) -> u128 {
    let sign = format.signed(negative);
    let n = bit_len(m);
    if n == 0 {
        debug_assert!(!sticky, "a rounding of a sticky bit alone");
        return sign;
    }
    let lead = e + n as i32 - 1;
    let emin = 1 - format.bias();
    // The exponent of the result's lowest bit: the precision below the leading bit, or the
    // subnormals' fixed quantum.
    let mut q = lead.max(emin) - format.frac as i32;
    let (mut m, round, sticky) = match q > e {
        true => shr_round(m, (q - e) as u32, sticky),
        false => {
            debug_assert!(!sticky, "a sticky bit under a value that needs no rounding");
            // At most `frac + 1` bits after the shift: `q >= lead - frac`.
            (m << (e - q) as u32, false, sticky)
        }
    };
    if round && (sticky || m & 1 == 1) {
        m += 1;
    }
    if m >> (format.frac + 1) != 0 {
        // Rounded up to `2^(frac+1)`: exact after one shift.
        m >>= 1;
        q += 1;
    }
    if m >> format.frac == 0 {
        // A subnormal, or zero: the exponent field is 0.
        return sign | m;
    }
    let biased = q + format.frac as i32 + format.bias();
    if biased >= low(format.exp) as i32 {
        return format.inf(negative);
    }
    sign | ((biased as u128) << format.frac) | (m & low(format.frac))
}

/// The product of two `u128`s as `(high, low)`.
const fn mul_wide(a: u128, b: u128) -> (u128, u128) {
    let (a0, a1) = (a & low(64), a >> 64);
    let (b0, b1) = (b & low(64), b >> 64);
    let (p00, p01, p10, p11) = (a0 * b0, a0 * b1, a1 * b0, a1 * b1);
    // Each of the three terms is below 2^64, so the sum is below 2^66.
    let mid = (p00 >> 64) + (p01 & low(64)) + (p10 & low(64));
    let lo = (p00 & low(64)) | (mid << 64);
    let hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    (hi, lo)
}

/// The quotient `q` is below `2^Q_BITS` and at least `2^(Q_BITS - 2)`: binary128's precision
/// and four more bits, so a round bit and a guard are always below its last place.
const Q_BITS: u32 = 117;
/// `10^(t - 1) >= 10^4933` is past `f128::MAX` (about 1.1897e4932) and past the threshold of
/// rounding to infinity: infinity in every format.
const QUAD_OVER: i64 = 4933;
/// `10^t <= 10^-4966` is under half the least `f128` subnormal (2^-16494, about 6.4752e-4966):
/// zero in every format.
const QUAD_UNDER: i64 = -4966;
/// An exponent's digits stop counting at this magnitude: far past both clamps above.
const EXP_CAP: i64 = 1_000_000_000;

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
    use core::cmp::Ordering;

    use blaze_typeck::FloatTy;

    use super::Format;

    const H: Format = Format::of(FloatTy::F16);
    const Q: Format = Format::of(FloatTy::F128);
    const D: Format = Format::of(FloatTy::F64);

    fn parse(f: Format, text: &str) -> u128 {
        f.parse(text, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("{text}: {e:?}"))
    }

    /// The literals of the pinned library's core/src/num/f128.rs and f16.rs round to the bit
    /// patterns IEEE 754 gives the constants they name (binary128: 1 + 15 + 112 bits, bias
    /// 16383; binary16: 1 + 5 + 10 bits, bias 15):
    /// - `f128::MAX` = (2 - 2^-112) * 2^16383: biased exponent 0x7ffe, fraction all ones:
    ///   0x7ffe_ffff_ffff_ffff_ffff_ffff_ffff_ffff; `MIN` the same with the sign bit.
    /// - `f128::MIN_POSITIVE` = 2^-16382, the least normal: biased exponent 1, fraction 0:
    ///   1 << 112.
    /// - `f128::EPSILON` = 2^-112 (`MANTISSA_DIGITS` 113): biased 16383 - 112 = 16271 = 0x3f8f,
    ///   fraction 0: 0x3f8f << 112.
    /// - `f16::MAX` = (2 - 2^-10) * 2^15 = 65504: 0x7bff. `f16::EPSILON` = 2^-10 = 9.765625e-4,
    ///   the literal 9.7656e-4 being 2.5e-9 from it against an ulp of 2^-20 there: biased
    ///   15 - 10 = 5: 0x1400. `f16::MIN_POSITIVE` = 2^-14 = 6.103515625e-5: 0x0400.
    /// - 0.1 at binary128: 1.6 * 2^-4, 0.6 = 0.1001 1001...b: 28 hex nines, the next 1001.. past
    ///   half: up to ...9a; once from the digits, never by way of f64 (which gives
    ///   0x3ffb_9999_9999_9999_a000_...).
    /// A wrong rounding (toward zero) makes `MAX`'s literal, which lies above `MAX`, still
    /// `MAX`, but makes 0.1 end in 9 and `f16`'s 65520 0x7bff; a parse by way of `f64` makes
    /// `MAX` infinity.
    /// Expected line: `test soft::tests::core_literals_are_their_bit_patterns ... ok`.
    #[test]
    fn core_literals_are_their_bit_patterns() {
        assert_eq!(parse(Q, "1.18973149535723176508575932662800702e+4932_f128"), 0x7ffe_ffff_ffff_ffff_ffff_ffff_ffff_ffff);
        assert_eq!(parse(Q, "3.36210314311209350626267781732175260e-4932_f128"), 1 << 112);
        assert_eq!(parse(Q, "1.92592994438723585305597794258492732e-34_f128"), 0x3f8f << 112);
        assert_eq!(parse(Q, "0.1f128"), 0x3ffb_9999_9999_9999_9999_9999_9999_999a);
        assert_eq!(parse(Q, "1.0_f128"), 0x3fff << 112);
        assert_eq!(parse(Q, "0.0_f128"), 0);
        assert_eq!(parse(H, "6.5504e+4_f16"), 0x7bff);
        assert_eq!(parse(H, "9.7656e-4_f16"), 0x1400);
        assert_eq!(parse(H, "6.1035e-5_f16"), 0x0400);
        assert_eq!(parse(H, "65520"), 0x7c00, "a tie with an odd significand rounds up, past MAX: infinity");
        assert_eq!(parse(H, "3.14159265358979323846264338327950288_f16"), 0x4248);
        assert_eq!(parse(D, "0.1"), u128::from(0.1f64.to_bits()));
        assert!(Q.parse(".5", blaze_own::testing::heap().alloc()).is_err());
        assert!(Q.parse("1e", blaze_own::testing::heap().alloc()).is_err());
    }

    /// The arithmetic the constants of core/src/num/f128.rs and f16.rs use: `NAN = 0.0 / 0.0`
    /// is the preferred NaN (f128 0x7fff_8 then zeros, f16 0x7e00); `INFINITY = 1.0 / 0.0`
    /// (exponent all ones, fraction 0); `NEG_INFINITY = -1.0 / 0.0` its negation; `HI =
    /// f128::MAX * 0.5` (in `midpoint`) is `MAX` with its exponent one lower (0x7ffd then ones);
    /// `-x` flips the sign bit alone. 1.5 + 0.25 = 1.75 = 1.11b (0x3fffc << 108); 1.75 - 1.5 =
    /// 0.25 = 2^-2 (0x3ffd << 112); 3 / 4 = 0.75 = 1.1b * 2^-1 (0x3ffe8 << 108).
    /// Expected line: `test soft::tests::arithmetic_of_the_core_constants ... ok`.
    #[test]
    fn arithmetic_of_the_core_constants() {
        let (zero, one, half) = (0u128, 0x3fff << 112, 0x3ffe << 112);
        let max = 0x7ffe_ffff_ffff_ffff_ffff_ffff_ffff_ffffu128;
        assert_eq!(Q.div(zero, zero), 0x7fff_8 << 108);
        assert_eq!(Q.div(one, zero), 0x7fff << 112);
        assert_eq!(Q.div(one ^ Q.sign(), zero), 0xffff << 112);
        assert_eq!(H.div(0, 0), 0x7e00);
        assert_eq!(H.div(0x3c00, 0), 0x7c00);
        assert_eq!(Q.mul(max, half), 0x7ffd_ffff_ffff_ffff_ffff_ffff_ffff_ffff);
        assert_eq!(Q.add(max, max), 0x7fff << 112, "MAX + MAX overflows to infinity");
        let (x15, x025) = (parse(Q, "1.5"), parse(Q, "0.25"));
        assert_eq!(Q.add(x15, x025), 0x3fffc << 108);
        assert_eq!(Q.sub(parse(Q, "1.75"), x15), 0x3ffd << 112);
        assert_eq!(Q.div(parse(Q, "3"), parse(Q, "4")), 0x3ffe8 << 108);
        assert_eq!(Q.sub(one, one), 0, "an exact zero of opposite signs is +0");
        assert_eq!(Q.compare(max, max ^ Q.sign()), Some(Ordering::Greater));
        assert_eq!(Q.compare(0, Q.sign()), Some(Ordering::Equal), "-0 == +0");
        assert_eq!(Q.compare(Q.nan(), one), None);
    }

    /// Casts ("Type cast expressions"): float to integer rounds toward zero and saturates, NaN
    /// to 0; integer to float rounds to nearest; float to float rounds once. 3.9 -> 3; -1.0 as
    /// u8 -> 0; f128::MAX as i32 -> i32::MAX; -inf as i8 -> -128 (0x80); 65504 as f16 is
    /// 0x7bff and 65535 as f16 rounds past MAX to infinity (0x7c00); 0.1 at binary128 as f64
    /// is 0.1f64; 1.0 at binary16 as f128 is 1.0.
    /// Expected line: `test soft::tests::casts_of_f16_and_f128 ... ok`.
    #[test]
    fn casts_of_f16_and_f128() {
        assert_eq!(Q.to_int(parse(Q, "3.9"), 8, false), 3);
        assert_eq!(Q.to_int(parse(Q, "1.0") ^ Q.sign(), 8, false), 0);
        assert_eq!(Q.to_int(0x7ffe_ffff_ffff_ffff_ffff_ffff_ffff_ffff, 32, true), i32::MAX as u128);
        assert_eq!(Q.to_int(0xffff << 112, 8, true), 0x80);
        assert_eq!(Q.to_int(Q.nan(), 32, true), 0);
        assert_eq!(H.from_int(false, 65504), 0x7bff);
        assert_eq!(H.from_int(false, 65535), 0x7c00);
        assert_eq!(Q.from_int(true, 1), (0x3fff << 112) | Q.sign());
        assert_eq!(Q.convert(D, parse(Q, "0.1")), u128::from(0.1f64.to_bits()));
        assert_eq!(H.convert(Q, 0x3c00), 0x3fff << 112);
    }

    /// This file's binary64 gives the host's bits for `+ - * / %` over values that cross every
    /// path (zeros, subnormals, carries, cancellation, overflow, infinities); NaNs compared by
    /// class. A wrong rounding or a lost sticky bit gives another bit pattern on one of the
    /// pairs.
    /// Expected line: `test soft::tests::binary64_agrees_with_the_host ... ok`.
    #[test]
    fn binary64_agrees_with_the_host() {
        let xs = [0.0f64, -0.0, 0.1, 0.2, 0.3, 1.0, -1.5, 3.0, 1e300, -1e300, 2.2250738585072014e-308, 5e-324, 1.0000000000000002, f64::MAX, f64::INFINITY, 7.0 / 3.0];
        for &x in &xs {
            for &y in &xs {
                let (a, b) = (u128::from(x.to_bits()), u128::from(y.to_bits()));
                let same = |got: u128, want: f64, op: &str| {
                    if want.is_nan() {
                        assert!(f64::from_bits(got as u64).is_nan(), "{x:e} {op} {y:e}: NaN wanted");
                    } else {
                        assert_eq!(got, u128::from(want.to_bits()), "{x:e} {op} {y:e}");
                    }
                };
                same(D.add(a, b), x + y, "+");
                same(D.sub(a, b), x - y, "-");
                same(D.mul(a, b), x * y, "*");
                same(D.div(a, b), x / y, "/");
                same(D.rem(a, b), x % y, "%");
                assert_eq!(D.compare(a, b), x.partial_cmp(&y), "{x:e} cmp {y:e}");
            }
        }
    }
}
