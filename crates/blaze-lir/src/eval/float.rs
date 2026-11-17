//! Soft IEEE 754 floats for the reference evaluator: binary16, binary32, binary64 and
//! binary128 (`f16`, `f32`, `f64`, `f128`), computed on their bits with integers alone, so an
//! answer is the same on every host and `f16` and `f128` (which the host's `f32`/`f64` path in
//! eval/floats.rs cannot hold) are answered at all.
//!
//! Rounding is IEEE 754's roundTiesToEven, as the pinned library states for Rust's floats
//! (core/src/primitive_docs.rs, `f32`): "When a primitive operation (addition, subtraction,
//! multiplication, or division) is performed on this type, the result is rounded according to
//! the roundTiesToEven direction defined in IEEE 754-2008. ... If the result of a sum exactly
//! equals zero, the outcome is +0.0 unless both arguments were negative, then it is -0.0.
//! Subtraction `a - b` is regarded as a sum `a + (-b)`."
//!
//! A NaN result is the "Preferred NaN" of the same file ("The quiet bit is set and the payload
//! is all-zero"), with the sign clear: Rust and LLVM leave which NaN an operation gives open,
//! so a harness compares NaNs by class ([`super::agree`]), never by bits.
//!
//! Every value is unpacked to `m * 2^e` (`m` an integer with the implicit bit, `e` the
//! exponent of its lowest bit), computed exactly or with a sticky bit, and packed by one
//! rounding ([`round_pack`]). No loop runs more than the format's exponent range of steps
//! (`frem` of `f128`: at most 32,766); nothing recurses.

use super::EvalError;
use crate::{FloatCond, Op, Scalar, Ty};

/// The `inst` of an error [`float_bin`] makes, which has no instruction: the caller puts its
/// own in with [`placed`].
const UNPLACED: usize = usize::MAX;

/// `error`, made where no instruction was known, now naming instruction `inst`.
pub(crate) fn placed(error: EvalError, inst: usize) -> EvalError {
    match error {
        EvalError::Unsupported { inst: UNPLACED, op, what } => EvalError::Unsupported { inst, op, what },
        other => other,
    }
}

const fn refuse(op: Op, what: &'static str) -> EvalError {
    EvalError::Unsupported { inst: UNPLACED, op, what }
}

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

impl Format {
    /// The format of a scalar float type, or `None` for any other type. Field widths from
    /// IEEE 754-2008 table 3.5 (binary16: 5 and 10; binary32: 8 and 23; binary64: 11 and 52;
    /// binary128: 15 and 112).
    pub(crate) const fn of(ty: Ty) -> Option<Format> {
        if !ty.is_scalar() {
            return None;
        }
        match ty.elem() {
            Scalar::F16 => Some(Format { exp: 5, frac: 10 }),
            Scalar::F32 => Some(Format { exp: 8, frac: 23 }),
            Scalar::F64 => Some(Format { exp: 11, frac: 52 }),
            Scalar::F128 => Some(Format { exp: 15, frac: 112 }),
            _ => None,
        }
    }

    const fn width(self) -> u32 {
        1 + self.exp + self.frac
    }

    const fn bias(self) -> i32 {
        (1 << (self.exp - 1)) - 1
    }

    const fn sign(self) -> u128 {
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

    /// `bits` with its sign cleared, as a key that orders magnitudes.
    const fn magnitude(self, bits: u128) -> u128 {
        bits & low(self.width() - 1)
    }

    /// A key ordering every non-NaN value of the format, -0 and +0 equal.
    const fn key(self, bits: u128) -> i128 {
        let magnitude = self.magnitude(bits) as i128;
        match bits & self.sign() != 0 {
            true => -magnitude,
            false => magnitude,
        }
    }
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
/// two and `t` lies below the round bit (asserted).
fn round_pack(format: Format, negative: bool, m: u128, e: i32, sticky: bool) -> u128 {
    let sign = format.signed(negative);
    let n = bit_len(m);
    if n == 0 {
        // `0 < t < 1` times `2^e`: no caller makes it (`m` is nonzero whenever `sticky` is).
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
        // A subnormal, or zero: the exponent field is 0 (a subnormal rounded up to the
        // smallest normal has bit `frac` set and is packed below).
        return sign | m;
    }
    let biased = q + format.frac as i32 + format.bias();
    if biased >= low(format.exp) as i32 {
        // "If the true value's magnitude is >= MAX + 2^(MAX_EXP - MANTISSA_DIGITS - 1), the
        // result is inf or -inf (preserving the true value's sign)."
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

/// `a + b` (`b`'s sign already flipped for a subtraction).
fn add(format: Format, a: u128, b: u128) -> u128 {
    let ((sa, ca), (sb, cb)) = (format.unpack(a), format.unpack(b));
    match (ca, cb) {
        (Class::Nan, _) | (_, Class::Nan) => format.nan(),
        // "inf - inf" is invalid: NaN.
        (Class::Inf, Class::Inf) if sa != sb => format.nan(),
        (Class::Inf, _) => format.inf(sa),
        (_, Class::Inf) => format.inf(sb),
        (Class::Zero, Class::Zero) => format.signed(sa && sb),
        (Class::Zero, _) => b & low(format.width()),
        (_, Class::Zero) => a & low(format.width()),
        (Class::Finite { m: ma, e: ea }, Class::Finite { m: mb, e: eb }) => {
            // The larger magnitude first: a larger exponent is a larger value (a value with an
            // exponent above the subnormals' is normal, so above every value of a smaller one).
            let ((sx, mx, ex), (sy, my, ey)) = match (ea, ma) >= (eb, mb) {
                true => ((sa, ma, ea), (sb, mb, eb)),
                false => ((sb, mb, eb), (sa, ma, ea)),
            };
            // Three guard bits, the smaller aligned with what falls off jammed in. With an
            // alignment of two or more the result keeps at least `frac + 3` bits, so the jam
            // lies below the round bit; with less nothing is jammed and the sum is exact.
            let (mx, my) = (mx << 3, shr_jam(my << 3, (ex - ey) as u32));
            let m = match sx == sy {
                true => mx + my,
                false => mx - my,
            };
            match m {
                // An exact zero of a sum of opposite signs is +0.
                0 => 0,
                m => round_pack(format, sx, m, ex - 3, false),
            }
        }
    }
}

fn mul(format: Format, a: u128, b: u128) -> u128 {
    let ((sa, ca), (sb, cb)) = (format.unpack(a), format.unpack(b));
    let negative = sa != sb;
    match (ca, cb) {
        (Class::Nan, _) | (_, Class::Nan) => format.nan(),
        (Class::Inf, Class::Zero) | (Class::Zero, Class::Inf) => format.nan(),
        (Class::Inf, _) | (_, Class::Inf) => format.inf(negative),
        (Class::Zero, _) | (_, Class::Zero) => format.signed(negative),
        (Class::Finite { m: ma, e: ea }, Class::Finite { m: mb, e: eb }) => {
            let (hi, lo) = mul_wide(ma, mb);
            match hi {
                0 => round_pack(format, negative, lo, ea + eb, false),
                hi => {
                    // At most 226 bits (binary128's 113 squared): keep the top 128, the rest sticky.
                    let k = bit_len(hi);
                    let m = (lo >> k) | (hi << (128 - k));
                    round_pack(format, negative, m, ea + eb + k as i32, lo & low(k) != 0)
                }
            }
        }
    }
}

fn div(format: Format, a: u128, b: u128) -> u128 {
    let ((sa, ca), (sb, cb)) = (format.unpack(a), format.unpack(b));
    let negative = sa != sb;
    match (ca, cb) {
        (Class::Nan, _) | (_, Class::Nan) => format.nan(),
        (Class::Inf, Class::Inf) | (Class::Zero, Class::Zero) => format.nan(),
        (Class::Inf, _) | (_, Class::Zero) => format.inf(negative),
        (_, Class::Inf) | (Class::Zero, _) => format.signed(negative),
        (Class::Finite { m: ma, e: ea }, Class::Finite { m: mb, e: eb }) => {
            // Both significands with their leading bit at bit 116, so `a / b` is in (1/2, 2)
            // and the remainder, below `2 b`, shifted once stays below 2^118.
            const LEAD: u32 = 116;
            let (sha, shb) = (LEAD + 1 - bit_len(ma), LEAD + 1 - bit_len(mb));
            let (a, b) = (ma << sha, mb << shb);
            // `frac + 4` quotient bits: `frac + 3` or `frac + 4` significant, two or more below
            // the precision, then the remainder as the sticky bit.
            let steps = format.frac + 4;
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
            round_pack(format, negative, q, e, r != 0)
        }
    }
}

/// `a % b`, C's `fmod` (Rust's `%` on floats; LLVM's `frem`): `a - n b` for `n` the quotient
/// rounded toward zero, which is exact, with `a`'s sign.
fn rem(format: Format, a: u128, b: u128) -> u128 {
    let ((sa, ca), (_, cb)) = (format.unpack(a), format.unpack(b));
    match (ca, cb) {
        (Class::Nan, _) | (_, Class::Nan) | (Class::Inf, _) | (_, Class::Zero) => format.nan(),
        (Class::Zero, _) | (_, Class::Inf) => a & low(format.width()),
        (Class::Finite { m: ma, e: ea }, Class::Finite { m: mb, e: eb }) => {
            if (ea, ma) < (eb, mb) {
                // `|a| < |b|`: `n` is 0.
                return a & low(format.width());
            }
            // `ea >= eb` here (a larger exponent is a larger magnitude). `(ma * 2^(ea-eb)) mod
            // mb`, one doubling at a time; `r < mb < 2^113`, so `r << 1` never overflows.
            let mut r = ma % mb;
            for _ in 0..(ea - eb) as u32 {
                r = (r << 1) % mb;
            }
            match r {
                0 => format.signed(sa),
                r => round_pack(format, sa, r, eb, false),
            }
        }
    }
}

/// Two-operand float operation `op` on values of scalar float type `ty`, given and returned as
/// their bits (the type's width, higher bits ignored on the way in and zero on the way out):
/// `fadd`, `fsub`, `fmul`, `fdiv`, `frem`, `fmin_num`, `fmax_num`, `fminimum`, `fmaximum`
/// and `fcopysign`, by IEEE 754 with roundTiesToEven. An `fmin_num`/`fmax_num` of +0 and -0,
/// whose result LIR leaves open, and every other operation are refused by name.
pub(crate) fn float_bin(op: Op, ty: Ty, a: u128, b: u128) -> Result<u128, EvalError> {
    let Some(format) = Format::of(ty) else {
        return Err(refuse(op, "a two-operand float operation on a type that is not a scalar float"));
    };
    let sign = format.sign();
    let (a, b) = (a & low(format.width()), b & low(format.width()));
    let nan = |x: u128| matches!(format.unpack(x).1, Class::Nan);
    Ok(match op {
        Op::Fadd => add(format, a, b),
        Op::Fsub => add(format, a, b ^ sign),
        Op::Fmul => mul(format, a, b),
        Op::Fdiv => div(format, a, b),
        Op::Frem => rem(format, a, b),
        // "IEEE 754-2008 `minNum`: a NaN operand gives the other (`f32::min`)."
        Op::FminNum | Op::FmaxNum => {
            if nan(a) {
                return Ok(b);
            }
            if nan(b) {
                return Ok(a);
            }
            let (ka, kb) = (format.key(a), format.key(b));
            if ka == kb && a != b {
                return Err(refuse(op, "an `fmin_num` or `fmax_num` of +0 and -0, whose result LIR leaves open"));
            }
            if (ka < kb) == (op == Op::FminNum) { a } else { b }
        }
        // "IEEE 754-2019 `minimum`: a NaN operand gives NaN"; -0 is below +0.
        Op::Fminimum | Op::Fmaximum => {
            if nan(a) || nan(b) {
                return Ok(format.nan());
            }
            let min = op == Op::Fminimum;
            let (ka, kb) = (format.key(a), format.key(b));
            if ka == kb {
                let negative = a & sign != 0;
                return Ok(if negative == min { a } else { b });
            }
            if (ka < kb) == min { a } else { b }
        }
        // "Non-arithmetic": the magnitude of `a`, the sign of `b`, a NaN's payload kept.
        Op::Fcopysign => (a & !sign) | (b & sign),
        _ => return Err(refuse(op, "this operation as a two-operand float operation")),
    })
}

/// `fcmp cond` of `a` and `b`, of scalar float type `ty`: an ordered condition is false and an
/// unordered one true when either is NaN; -0 equals +0.
pub(crate) fn float_cmp(cond: FloatCond, ty: Ty, a: u128, b: u128) -> Result<bool, EvalError> {
    let Some(format) = Format::of(ty) else {
        return Err(refuse(Op::Fcmp, "an `fcmp` of a type that is not a scalar float"));
    };
    let nan = |x: u128| matches!(format.unpack(x).1, Class::Nan);
    let uno = nan(a) || nan(b);
    let (ka, kb) = (format.key(a & low(format.width())), format.key(b & low(format.width())));
    Ok(match cond {
        FloatCond::False => false,
        FloatCond::Oeq => !uno && ka == kb,
        FloatCond::Ogt => !uno && ka > kb,
        FloatCond::Oge => !uno && ka >= kb,
        FloatCond::Olt => !uno && ka < kb,
        FloatCond::Ole => !uno && ka <= kb,
        FloatCond::One => !uno && ka != kb,
        FloatCond::Ord => !uno,
        FloatCond::Uno => uno,
        FloatCond::Ueq => uno || ka == kb,
        FloatCond::Ugt => uno || ka > kb,
        FloatCond::Uge => uno || ka >= kb,
        FloatCond::Ult => uno || ka < kb,
        FloatCond::Ule => uno || ka <= kb,
        FloatCond::Une => uno || ka != kb,
        FloatCond::True => true,
    })
}

/// `fpext` or `fptrunc` (`op`, named in a refusal) of `a` from scalar float type `from` to
/// `to`: exact when widening, rounded to nearest, ties to even when narrowing (Rust's `as`
/// between floats); an infinity stays one, a NaN becomes the preferred NaN.
pub(crate) fn float_convert(op: Op, from: Ty, to: Ty, a: u128) -> Result<u128, EvalError> {
    let (Some(source), Some(target)) = (Format::of(from), Format::of(to)) else {
        return Err(refuse(op, "a float conversion from or to a type that is not a scalar float"));
    };
    let (negative, class) = source.unpack(a);
    Ok(match class {
        Class::Nan => target.nan(),
        Class::Inf => target.inf(negative),
        Class::Zero => target.signed(negative),
        Class::Finite { m, e } => round_pack(target, negative, m, e, false),
    })
}
