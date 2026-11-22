//! A float with a fixed number of places, as `core::fmt`'s `{:.N}`: the exact binary value
//! rounded half to even at the last place, the sign of a negative zero kept.
//!
//! The common case (`places <= 17` and `2^-127 <= |x| < 2^127`, or smaller) is one `u128`
//! multiply and shift; the rest is exact in [`Big`].

use blaze_int::{Int as _, idx};

use crate::ascii::Ascii;
use crate::big::Big;
use crate::float::{Float, fields};
use crate::int::{Decimal, u64_into, u128_into};
use crate::put::Put;

const POW10_U128: [u128; 18] = {
    let mut out = [1u128; 18];
    let mut i = 1;
    while i < 18 {
        out[i] = out[i - 1] * 10;
        i += 1;
    }
    out
};

/// The most fraction digits the exact path computes: past `2^-1074`'s 1074 they are all zero.
const MOST_FRACTION: usize = 1080;

pub(crate) fn fixed<F: Float, P: Put + ?Sized>(x: F, places: usize, out: &mut P) {
    // Every `f32` is exactly an `f64`, and `{:.N}` writes the exact value.
    let x = x.to_f64();
    let (negative, exponent, mantissa) = fields(x);
    if exponent == 0x7FF {
        return match (mantissa != 0, negative) {
            (true, _) => out.put_str("NaN"),
            (false, true) => out.put_str("-inf"),
            (false, false) => out.put_str("inf"),
        };
    }
    if negative {
        out.put_byte(b'-');
    }
    // |x| = m * 2^e.
    let (m, e) = match exponent {
        0 => (mantissa, -1074i32),
        _ => (mantissa | (1u64 << 52), exponent.cast_signed() - 1075),
    };
    if places <= 17 {
        if e >= 0 && e <= 74 {
            let mut d = Decimal::at_end();
            u128_into(u128::from(m) << e.cast_unsigned(), &mut d);
            out.put_str(d.as_str());
            return zeros_after_point(places, out);
        }
        if e < 0 {
            let k = e.unsigned_abs();
            let scaled = if k <= 127 {
                let n = u128::from(m) * POW10_U128[places];
                let q = n >> k;
                let r = n - (q << k);
                let half = 1u128 << (k - 1);
                q + u128::from(r > half || (r == half && q & 1 == 1))
            } else {
                // m * 10^places < 2^110, so the value is below 2^-18: it rounds to 0.
                0
            };
            return scaled_out(scaled, places, out);
        }
    }
    exact(m, e, places, out);
}

fn zeros_after_point<P: Put + ?Sized>(places: usize, out: &mut P) {
    if places > 0 {
        out.put_byte(b'.');
        for _ in 0..places {
            out.put_byte(b'0');
        }
    }
}

/// `scaled / 10^places` written with `places` places (`places <= 17`).
fn scaled_out<P: Put + ?Sized>(scaled: u128, places: usize, out: &mut P) {
    let mut d = Decimal::at_end();
    u128_into(scaled, &mut d);
    while d.len() < places + 1 {
        d.front_digit(&crate::ascii::LOWER, 0);
    }
    let text = d.as_str();
    let point = text.len() - places;
    out.put_str(&text[..point]);
    if places > 0 {
        out.put_byte(b'.');
        out.put_str(&text[point..]);
    }
}

/// The exact path, in [`Big`]: any `places`, any exponent.
fn exact<P: Put + ?Sized>(m: u64, e: i32, places: usize, out: &mut P) {
    if e >= 0 {
        integer_out(Big::from_u64_shl(m, e.cast_unsigned()), out);
        return zeros_after_point(places, out);
    }
    let k = e.unsigned_abs();
    let (mut int, mut frac) = match k < 64 {
        true => (m >> k, Big::from_u64(m & ((1u64 << k) - 1))),
        false => (0, Big::from_u64(m)),
    };
    // The exact expansion of frac / 2^k has at most k digits.
    let wanted = places.min(idx(k)).min(MOST_FRACTION);
    let mut digits = [b'0'; MOST_FRACTION];
    for slot in digits.iter_mut().take(wanted) {
        frac.mul_small(10);
        let d = frac.bits_at(k).truncate_to::<u8>();
        frac.truncate_bits(k);
        *slot = b'0' + d;
    }
    if !frac.is_zero() {
        // Round half to even at the last place written.
        let above_half = frac.bit(k - 1) && !frac.zero_below(k - 1);
        let exactly_half = frac.bit(k - 1) && frac.zero_below(k - 1);
        let last_odd = match wanted {
            0 => int & 1 == 1,
            _ => (digits[wanted - 1] - b'0') & 1 == 1,
        };
        if above_half || (exactly_half && last_odd) {
            let mut carry = true;
            for slot in digits[..wanted].iter_mut().rev() {
                if *slot == b'9' {
                    *slot = b'0';
                } else {
                    *slot += 1;
                    carry = false;
                    break;
                }
            }
            if carry {
                int += 1;
            }
        }
    }
    let mut d = Decimal::at_end();
    u64_into(int, &mut d);
    out.put_str(d.as_str());
    if places > 0 {
        out.put_byte(b'.');
        let mut text = Ascii::<MOST_FRACTION>::at_start();
        text.back_all(&digits[..wanted]);
        out.put_str(text.as_str());
        for _ in wanted..places {
            out.put_byte(b'0');
        }
    }
}

/// A [`Big`] integer's decimal digits: 19 at a time.
fn integer_out<P: Put + ?Sized>(mut n: Big, out: &mut P) {
    const TEN_19: u64 = 10_000_000_000_000_000_000;
    let mut text = Ascii::<400>::at_end();
    loop {
        let chunk = n.div_small(TEN_19);
        let mut d = Decimal::at_end();
        u64_into(chunk, &mut d);
        if n.is_zero() {
            for &b in d.as_bytes().iter().rev() {
                text.front(b);
            }
            break;
        }
        for &b in d.as_bytes().iter().rev() {
            text.front(b);
        }
        for _ in d.len()..19 {
            text.front(b'0');
        }
    }
    out.put_str(text.as_str());
}
