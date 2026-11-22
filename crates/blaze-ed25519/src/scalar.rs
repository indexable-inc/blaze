//! Integers modulo the group order L = 2^252 + 27742317777372353535851937790883648493, the
//! order of the base point (RFC 8032, section 5.1): the range check of S, the reduction of the
//! 512-bit challenge hash, and the signed-digit form the double-scalar multiplication walks.
//!
//! A scalar is four 64-bit limbs, little-endian.

use blaze_int::Cast;
use blaze_own::planted::Planted;

use crate::Plant;

/// L, little-endian limbs.
pub(crate) const L: [u64; 4] = [0x5812_631a_5cf5_d3ed, 0x14de_f9de_a2f7_9cd6, 0, 0x1000_0000_0000_0000];

/// floor(2^512 / L), Barrett's constant for [`reduce_wide`], computed by the compiler by long
/// division (`mu`); a test holds it to its definition, mu L <= 2^512 < (mu + 1) L.
pub(crate) const MU: [u64; 5] = mu();

const fn wide(x: u64) -> u128 {
    Cast::<u64, u128>::widen(x)
}

const fn low(x: u128) -> u64 {
    Cast::<u128, u64>::truncate(x)
}

/// The limbs of 32 little-endian bytes.
pub(crate) const fn from_bytes(bytes: &[u8; 32]) -> [u64; 4] {
    let mut limbs = [0u64; 4];
    let mut i = 0;
    while i < 32 {
        limbs[i / 8] |= Cast::<u8, u64>::widen(bytes[i]) << (8 * (i % 8));
        i += 1;
    }
    limbs
}

/// The 32 little-endian bytes of four limbs.
#[cfg(test)]
pub(crate) const fn to_bytes(limbs: &[u64; 4]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = Cast::<u64, u8>::truncate(limbs[i / 8] >> (8 * (i % 8)));
        i += 1;
    }
    out
}

/// `a < b`.
pub(crate) const fn less(a: &[u64; 4], b: &[u64; 4]) -> bool {
    let mut i = 4;
    while i > 0 {
        i -= 1;
        if a[i] != b[i] {
            return a[i] < b[i];
        }
    }
    false
}

/// `a - b`, for `a >= b`.
const fn sub(a: &[u64; 4], b: &[u64; 4]) -> [u64; 4] {
    let mut out = [0u64; 4];
    let mut borrow = false;
    let mut i = 0;
    while i < 4 {
        let (d, b1) = a[i].overflowing_sub(b[i]);
        let (d, b2) = d.overflowing_sub(borrow as u64);
        out[i] = d;
        borrow = b1 | b2;
        i += 1;
    }
    out
}

/// Whether the 32 bytes `s` encode an integer below L: RFC 8032 section 5.1.7 step 1's range
/// check of S, which keeps a signature from having a second form `S + L` (malleability).
pub(crate) fn is_canonical_planted(s: &[u8; 32], plant: impl Planted<Plant>) -> bool {
    if plant.has(Plant::SAtOrderTaken) {
        return !less(&L, &from_bytes(s));
    }
    if plant.has(Plant::STopLimbOnly) {
        return from_bytes(s)[3] <= L[3];
    }
    less(&from_bytes(s), &L)
}

/// [`is_canonical_planted`] with no plant: the tests'.
#[cfg(test)]
pub(crate) fn is_canonical(s: &[u8; 32]) -> bool {
    is_canonical_planted(s, blaze_own::planted::NoPlant)
}

const fn mu() -> [u64; 5] {
    // Bit by bit from bit 512 of 2^512 (its only set bit) down to bit 0; the remainder stays
    // below L < 2^253, so doubling it fits four limbs.
    let mut rem = [0u64; 4];
    let mut quotient = [0u64; 5];
    let mut bit = 513;
    while bit > 0 {
        bit -= 1;
        let incoming = (bit == 512) as u64;
        rem = [rem[0] << 1 | incoming, rem[1] << 1 | rem[0] >> 63, rem[2] << 1 | rem[1] >> 63, rem[3] << 1 | rem[2] >> 63];
        if !less(&rem, &L) {
            rem = sub(&rem, &L);
            quotient[bit / 64] |= 1 << (bit % 64);
        }
    }
    quotient
}

/// The low `C` limbs of `a * b`.
const fn mul_low<const A: usize, const B: usize, const C: usize>(a: &[u64; A], b: &[u64; B]) -> [u64; C] {
    let mut out = [0u64; C];
    let mut i = 0;
    while i < A && i < C {
        let mut carry = 0u64;
        let mut j = 0;
        while j < B && i + j < C {
            let t = wide(out[i + j]) + wide(a[i]) * wide(b[j]) + wide(carry);
            out[i + j] = low(t);
            carry = low(t >> 64);
            j += 1;
        }
        if i + B < C {
            out[i + B] = carry;
        }
        i += 1;
    }
    out
}

/// A 512-bit little-endian integer modulo L (RFC 8032 section 5.1.7 step 2 reduces the challenge
/// hash so), by Barrett reduction: q = floor(x mu / 2^512) is floor(x / L) or one less, so
/// x - q L is below 2L and one conditional subtraction finishes it.
pub(crate) const fn reduce_wide(bytes: &[u8; 64]) -> [u64; 4] {
    let mut x = [0u64; 8];
    let mut i = 0;
    while i < 64 {
        x[i / 8] |= Cast::<u8, u64>::widen(bytes[i]) << (8 * (i % 8));
        i += 1;
    }
    let product: [u64; 13] = mul_low(&x, &MU);
    let q = [product[8], product[9], product[10], product[11], product[12]];
    let ql: [u64; 5] = mul_low(&q, &L);
    // x - q L, modulo 2^320: the true value is below 2L < 2^254, so it is exact.
    let mut r = [0u64; 5];
    let mut borrow = false;
    let mut i = 0;
    while i < 5 {
        let (d, b1) = x[i].overflowing_sub(ql[i]);
        let (d, b2) = d.overflowing_sub(borrow as u64);
        r[i] = d;
        borrow = b1 | b2;
        i += 1;
    }
    let r = [r[0], r[1], r[2], r[3]];
    if less(&r, &L) { r } else { sub(&r, &L) }
}

/// The width-`w` non-adjacent form of `scalar` (below 2^255): digits `d[i]` with
/// `scalar = sum d[i] 2^i`, each zero or odd with `|d[i]| < 2^(w-1)`, and any `w` consecutive
/// digits holding at most one nonzero. Variable time: scalars here are public.
pub(crate) const fn naf(scalar: &[u64; 4], w: u32) -> [i8; 256] {
    let x = [scalar[0], scalar[1], scalar[2], scalar[3], 0];
    let width = 1u64 << w;
    let window_mask = width - 1;
    let mut digits = [0i8; 256];
    let mut pos = 0usize;
    let mut carry = 0u64;
    while pos < 256 {
        let limb = pos / 64;
        let bit = pos % 64;
        let bits = if bit < 64 - Cast::<u32, usize>::widen(w) { x[limb] >> bit } else { (x[limb] >> bit) | (x[limb + 1] << (64 - bit)) };
        let window = carry + (bits & window_mask);
        if window & 1 == 0 {
            // An even window: a zero digit here, and the window moves one place.
            pos += 1;
            continue;
        }
        if window < width / 2 {
            carry = 0;
            digits[pos] = Cast::<u64, i8>::truncate(window);
        } else {
            carry = 1;
            digits[pos] = Cast::<u64, i8>::truncate(window).wrapping_sub(Cast::<u64, i8>::truncate(width));
        }
        pos += Cast::<u32, usize>::widen(w);
    }
    digits
}
