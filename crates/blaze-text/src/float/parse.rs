//! Text to a float, correctly rounded (to nearest, ties to even) for every input: the exact
//! conversion, now only the tests' oracle (`Parse` for floats calls `core`'s, which is 50
//! times faster until this crate has an Eisel-Lemire path). [`Number`] is the scanned token.
//!
//! Two paths. The fast one is Clinger's (1990): at most 19 significant digits whose integer
//! `w` is exact in the type (`w <= 2^53`, or `2^24` for `f32`) and a power of ten that is
//! exact too (`|exp10| <= 22`, or 10): one IEEE multiply or divide rounds once, so the result
//! is correctly rounded. Everything else takes the exact one: the "simple decimal conversion"
//! of Nigel Tao's Wuffs and Go's `strconv` (also `core`'s last resort), a decimal of up to 768
//! digits shifted by powers of two until the binary exponent is found, then rounded with a
//! sticky bit for the digits past 768. It is exact and slow (about a microsecond); the
//! Eisel-Lemire path `core` puts between the two is not written yet, so inputs of 17 to 19
//! digits (the shortest form of most random floats) take it.

// Outside the tests only `Number` and `EXP_CLAMP` are used.
#![cfg_attr(not(test), allow(dead_code, reason = "the exact conversion is the tests' oracle"))]

use blaze_int::{Int as _, idx};

use crate::float::Float;

/// The number scanned, borrowed from the text.
pub(crate) struct Number<'a> {
    pub(crate) negative: bool,
    /// Digits before the point.
    pub(crate) int: &'a [u8],
    /// Digits after it.
    pub(crate) frac: &'a [u8],
    /// The exponent written, clamped to `±EXP_CLAMP`.
    pub(crate) exp: i64,
}

/// Past this the value is zero or infinite whatever the digits; keeps the sums in range.
pub(crate) const EXP_CLAMP: i64 = 1 << 20;

/// What the value of `number` is: a float, or out of the type's range.
pub(crate) fn value<F: Float>(number: &Number<'_>) -> Option<F> {
    let sign = |x: F| -> F {
        match number.negative {
            true => F::from_bits64(x.bits64() | (1u64 << (F::MANTISSA_BITS + F::EXPONENT_BITS))),
            false => x,
        }
    };
    // The fast path.
    let mut w = 0u64;
    let mut significant = 0usize;
    for &b in number.int.iter().chain(number.frac) {
        if significant == 0 && b == b'0' {
            continue;
        }
        significant += 1;
        if significant > 19 {
            break;
        }
        w = w * 10 + u64::from(b - b'0');
    }
    if significant == 0 {
        return Some(sign(F::from_bits64(0)));
    }
    if significant <= 19 {
        let exp10 = number.exp - number.frac.len().narrow_or_stop::<i64>();
        if w <= F::FAST_MAX_MANTISSA && exp10.abs() <= F::FAST_MAX_EXP10 {
            return Some(sign(F::fast(w, exp10)));
        }
    }
    slow::<F>(number).map(|bits| sign(F::from_bits64(bits)))
}

const MAX_DIGITS: usize = 768;
const DECIMAL_POINT_RANGE: i32 = 2047;
const MAX_SHIFT: usize = 60;
/// The shift that takes a decimal point of `n` digits down most in one step.
const POWERS: [u8; 19] = [0, 3, 6, 9, 13, 16, 19, 23, 26, 29, 33, 36, 39, 43, 46, 49, 53, 56, 59];

fn get_shift(n: usize) -> usize {
    match POWERS.get(n) {
        Some(&shift) => usize::from(shift),
        None => MAX_SHIFT,
    }
}

/// The decimal digits of `5^s` for `s <= 60` (most significant first) and their count.
static POW5_DIGITS: [([u8; 44], u8); MAX_SHIFT + 1] = {
    let mut out = [([0u8; 44], 0u8); MAX_SHIFT + 1];
    // Digits least significant first while multiplying.
    let mut digits = [0u8; 44];
    digits[0] = 1;
    let mut len = 1usize;
    let mut s = 0;
    while s <= MAX_SHIFT {
        let mut row = [0u8; 44];
        let mut i = 0;
        while i < len {
            row[i] = digits[len - 1 - i];
            i += 1;
        }
        out[s] = (row, blaze_int::Cast::<usize, u8>::truncate(len));
        // digits *= 5
        let mut carry = 0u8;
        let mut j = 0;
        while j < len {
            let v = digits[j] * 5 + carry;
            digits[j] = v % 10;
            carry = v / 10;
            j += 1;
        }
        if carry > 0 {
            digits[len] = carry;
            len += 1;
        }
        s += 1;
    }
    out
};

/// A decimal `0.d1 d2 .. dn * 10^decimal_point`.
struct Decimal {
    num_digits: usize,
    decimal_point: i32,
    /// A nonzero digit past `MAX_DIGITS` was dropped.
    truncated: bool,
    digits: [u8; MAX_DIGITS],
}

impl Decimal {
    fn trim(&mut self) {
        while self.num_digits != 0 && self.digits[self.num_digits - 1] == 0 {
            self.num_digits -= 1;
        }
    }

    /// The integer part rounded half to even, with the dropped digits as a sticky bit.
    fn round(&self) -> u64 {
        if self.num_digits == 0 || self.decimal_point < 0 {
            return 0;
        }
        if self.decimal_point > 18 {
            return u64::MAX;
        }
        let dp = self.decimal_point.cast_unsigned().widen_to::<u64>().truncate_to::<usize>();
        let mut n = 0u64;
        for i in 0..dp {
            n *= 10;
            if i < self.num_digits {
                n += u64::from(self.digits[i]);
            }
        }
        let mut round_up = false;
        if dp < self.num_digits {
            round_up = self.digits[dp] >= 5;
            if self.digits[dp] == 5 && dp + 1 == self.num_digits {
                round_up = self.truncated || (dp != 0 && self.digits[dp - 1] & 1 != 0);
            }
        }
        n + u64::from(round_up)
    }

    /// How many digits `self << shift` gains: those of `2^shift`, less one when the digits
    /// sort below those of `5^shift`.
    fn new_digits(&self, shift: usize) -> usize {
        let delta = digit_count(1u64 << shift);
        let (cutoff, len) = &POW5_DIGITS[shift];
        for (i, &want) in cutoff[..usize::from(*len)].iter().enumerate() {
            if i >= self.num_digits {
                return delta - 1;
            }
            if self.digits[i] != want {
                return if self.digits[i] < want { delta - 1 } else { delta };
            }
        }
        delta
    }

    fn left_shift(&mut self, shift: usize) {
        if self.num_digits == 0 {
            return;
        }
        let num_new_digits = self.new_digits(shift);
        let mut read_index = self.num_digits;
        let mut write_index = self.num_digits + num_new_digits;
        let mut n = 0u64;
        while read_index != 0 {
            read_index -= 1;
            write_index -= 1;
            n += u64::from(self.digits[read_index]) << shift;
            let quotient = n / 10;
            let remainder = n - 10 * quotient;
            if write_index < MAX_DIGITS {
                self.digits[write_index] = remainder.truncate_to::<u8>();
            } else if remainder > 0 {
                self.truncated = true;
            }
            n = quotient;
        }
        while n > 0 {
            write_index -= 1;
            let quotient = n / 10;
            let remainder = n - 10 * quotient;
            if write_index < MAX_DIGITS {
                self.digits[write_index] = remainder.truncate_to::<u8>();
            } else if remainder > 0 {
                self.truncated = true;
            }
            n = quotient;
        }
        self.num_digits = (self.num_digits + num_new_digits).min(MAX_DIGITS);
        self.decimal_point += num_new_digits.narrow_or_stop::<i32>();
        self.trim();
    }

    fn right_shift(&mut self, shift: usize) {
        let mut read_index = 0;
        let mut write_index = 0;
        let mut n = 0u64;
        while (n >> shift) == 0 {
            if read_index < self.num_digits {
                n = 10 * n + u64::from(self.digits[read_index]);
                read_index += 1;
            } else if n == 0 {
                return;
            } else {
                while (n >> shift) == 0 {
                    n *= 10;
                    read_index += 1;
                }
                break;
            }
        }
        self.decimal_point -= read_index.narrow_or_stop::<i32>() - 1;
        if self.decimal_point < -DECIMAL_POINT_RANGE {
            self.num_digits = 0;
            self.decimal_point = 0;
            self.truncated = false;
            return;
        }
        let mask = (1u64 << shift) - 1;
        while read_index < self.num_digits {
            let new_digit = (n >> shift).truncate_to::<u8>();
            n = 10 * (n & mask) + u64::from(self.digits[read_index]);
            read_index += 1;
            self.digits[write_index] = new_digit;
            write_index += 1;
        }
        while n > 0 {
            let new_digit = (n >> shift).truncate_to::<u8>();
            n = 10 * (n & mask);
            if write_index < MAX_DIGITS {
                self.digits[write_index] = new_digit;
                write_index += 1;
            } else if new_digit > 0 {
                self.truncated = true;
            }
        }
        self.num_digits = write_index;
        self.trim();
    }
}

fn digit_count(mut n: u64) -> usize {
    let mut count = 1;
    while n >= 10 {
        n /= 10;
        count += 1;
    }
    count
}

/// The exact path: the float's bits without the sign, or `None` past its range.
fn slow<F: Float>(number: &Number<'_>) -> Option<u64> {
    let mut d = Decimal { num_digits: 0, decimal_point: 0, truncated: false, digits: [0; MAX_DIGITS] };
    // The digits from the first nonzero to the last nonzero, and where the point is.
    let int = trim_start_zeros(number.int);
    let (point, digits_int, digits_frac) = match int.is_empty() {
        false => (int.len().narrow_or_stop::<i64>(), int, number.frac),
        true => {
            let frac = trim_start_zeros(number.frac);
            (frac.len().narrow_or_stop::<i64>() - number.frac.len().narrow_or_stop::<i64>(), &[][..], frac)
        }
    };
    let last_nonzero = digits_int.len() + digits_frac.len() - trailing_zeros(digits_int, digits_frac);
    let mut count = 0usize;
    for &b in digits_int.iter().chain(digits_frac).take(last_nonzero) {
        if count < MAX_DIGITS {
            d.digits[count] = b - b'0';
        }
        count += 1;
    }
    if count == 0 {
        return Some(0);
    }
    if count > MAX_DIGITS {
        d.truncated = true;
        count = MAX_DIGITS;
    }
    d.num_digits = count;
    let decimal_point = point + number.exp;
    if decimal_point < -324 {
        return Some(0);
    }
    if decimal_point >= 310 {
        return None;
    }
    d.decimal_point = decimal_point.narrow_or_stop::<i32>();

    let minimum_exponent = -F::BIAS;
    let infinite_power = (1i32 << F::EXPONENT_BITS) - 1;
    let mantissa_bits = idx(F::MANTISSA_BITS);
    let mut exp2 = 0i32;
    // Shift right toward (1/2 .. 1].
    while d.decimal_point > 0 {
        let shift = get_shift(d.decimal_point.cast_unsigned().widen_to::<u64>().truncate_to::<usize>());
        d.right_shift(shift);
        if d.decimal_point < -DECIMAL_POINT_RANGE {
            return Some(0);
        }
        exp2 += shift.narrow_or_stop::<i32>();
    }
    // Shift left toward (1/2 .. 1].
    while d.decimal_point <= 0 {
        let shift = if d.decimal_point == 0 {
            match d.digits[0] {
                5.. => break,
                0 | 1 => 2,
                _ => 1,
            }
        } else {
            get_shift(d.decimal_point.unsigned_abs().widen_to::<u64>().truncate_to::<usize>())
        };
        d.left_shift(shift);
        if d.decimal_point > DECIMAL_POINT_RANGE {
            return None;
        }
        exp2 -= shift.narrow_or_stop::<i32>();
    }
    // In [1/2, 1); the binary format wants [1, 2).
    exp2 -= 1;
    while minimum_exponent + 1 > exp2 {
        let n = (minimum_exponent + 1 - exp2).cast_unsigned().widen_to::<u64>().truncate_to::<usize>().min(MAX_SHIFT);
        d.right_shift(n);
        exp2 += n.narrow_or_stop::<i32>();
    }
    if exp2 - minimum_exponent >= infinite_power {
        return None;
    }
    // The hidden bit and the explicit ones, rounded.
    d.left_shift(mantissa_bits + 1);
    let mut mantissa = d.round();
    if mantissa >= (1u64 << (mantissa_bits + 1)) {
        // Rounding carried into a new bit.
        d.right_shift(1);
        exp2 += 1;
        mantissa = d.round();
        if exp2 - minimum_exponent >= infinite_power {
            return None;
        }
    }
    let mut power2 = exp2 - minimum_exponent;
    if mantissa < (1u64 << mantissa_bits) {
        power2 -= 1;
    }
    mantissa &= (1u64 << mantissa_bits) - 1;
    Some(mantissa | (power2.narrow_or_stop::<u64>() << mantissa_bits))
}

fn trim_start_zeros(digits: &[u8]) -> &[u8] {
    let zeros = digits.iter().take_while(|&&b| b == b'0').count();
    &digits[zeros..]
}

/// Trailing zeros of `a` then `b` read as one sequence.
fn trailing_zeros(a: &[u8], b: &[u8]) -> usize {
    let in_b = b.iter().rev().take_while(|&&c| c == b'0').count();
    if in_b < b.len() {
        return in_b;
    }
    in_b + a.iter().rev().take_while(|&&c| c == b'0').count()
}
