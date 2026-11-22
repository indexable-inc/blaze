//! Heads appended in their shortest form (moved from blaze-pack's and blaze-watch's `cbor`).

use core::alloc::Allocator;

use blaze_data::Int;
use blaze_own::Vec;

use crate::{major, wide};

pub(crate) const FALSE: u8 = 0xf4;
pub(crate) const TRUE: u8 = 0xf5;
pub(crate) const NULL: u8 = 0xf6;
pub(crate) const HALF: u8 = 0xf9;
pub(crate) const SINGLE: u8 = 0xfa;
pub(crate) const DOUBLE: u8 = 0xfb;
/// The one NaN written, as a half.
pub(crate) const NAN_HALF: u16 = 0x7e00;

/// A head of `major` and `n` in its shortest form, for a sink that is not a blaze-own `Vec`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Head {
    bytes: [u8; 9],
    len: usize,
}

impl Head {
    pub fn new(major: u8, n: u64) -> Head {
        let major = major << 5;
        let be = n.to_be_bytes();
        let mut bytes = [0u8; 9];
        let (info, len) = match n {
            0..=23 => (be[7], 1),
            24..=0xff => (24, 2),
            0x100..=0xffff => (25, 3),
            0x1_0000..=0xffff_ffff => (26, 5),
            _ => (27, 9),
        };
        bytes[0] = major | info;
        if len > 1 {
            bytes[1..len].copy_from_slice(&be[9 - len..]);
        }
        Head { bytes, len }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// The bytes a head of `n` takes: what a writer that sizes a frame before it writes adds up.
pub const fn head_len(n: u64) -> usize {
    match n {
        0..=23 => 1,
        24..=0xff => 2,
        0x100..=0xffff => 3,
        0x1_0000..=0xffff_ffff => 5,
        _ => 9,
    }
}

fn head<A: Allocator>(out: &mut Vec<u8, A>, major: u8, n: u64) {
    out.extend_from_slice(Head::new(major, n).as_bytes());
}

pub fn uint<A: Allocator>(out: &mut Vec<u8, A>, n: u64) {
    head(out, major::UINT, n);
}

/// The negative integer `-1 - n`.
pub fn negative<A: Allocator>(out: &mut Vec<u8, A>, n: u64) {
    head(out, major::NEGATIVE, n);
}

/// An integer of either sign.
pub fn int<A: Allocator>(out: &mut Vec<u8, A>, n: Int) {
    match n.as_u64() {
        Some(n) => uint(out, n),
        None => match n.as_i64() {
            // -1 - (!v) == v, and !v >= 0 for a negative v.
            Some(v) => negative(out, (!v).cast_unsigned()),
            None => unreachable!("an Int is a u64 or a negative i64"),
        },
    }
}

pub fn bytes<A: Allocator>(out: &mut Vec<u8, A>, bytes: &[u8]) {
    head(out, major::BYTES, wide(bytes.len()));
    out.extend_from_slice(bytes);
}

/// The head of a byte string of `len` bytes, which the caller appends (from parts).
pub fn bytes_head<A: Allocator>(out: &mut Vec<u8, A>, len: usize) {
    head(out, major::BYTES, wide(len));
}

pub fn text<A: Allocator>(out: &mut Vec<u8, A>, text: &str) {
    head(out, major::TEXT, wide(text.len()));
    out.extend_from_slice(text.as_bytes());
}

pub fn boolean<A: Allocator>(out: &mut Vec<u8, A>, value: bool) {
    out.push(if value { TRUE } else { FALSE });
}

pub fn null<A: Allocator>(out: &mut Vec<u8, A>) {
    out.push(NULL);
}

/// The head of an array of `len` items, which follow it.
pub fn array<A: Allocator>(out: &mut Vec<u8, A>, len: u64) {
    head(out, major::ARRAY, len);
}

/// The head of a map of `len` entries, whose keys and values follow, keys in order.
pub fn map<A: Allocator>(out: &mut Vec<u8, A>, len: u64) {
    head(out, major::MAP, len);
}

/// `x` in the shortest of half, single and double precision that holds it exactly; every NaN
/// as `f9 7e00`.
pub fn float<A: Allocator>(out: &mut Vec<u8, A>, x: f64) {
    if x.is_nan() {
        out.push(HALF);
        out.extend_from_slice(&NAN_HALF.to_be_bytes());
    } else if let Some(half) = to_half(x) {
        out.push(HALF);
        out.extend_from_slice(&half.to_be_bytes());
    } else if let Some(single) = to_single(x) {
        out.push(SINGLE);
        out.extend_from_slice(&single.to_bits().to_be_bytes());
    } else {
        out.push(DOUBLE);
        out.extend_from_slice(&x.to_bits().to_be_bytes());
    }
}

/// `x` as a single if that is exact.
pub(crate) fn to_single(x: f64) -> Option<f32> {
    // Rounds to nearest (the one float narrowing core has); exact when it comes back with the
    // same bits.
    let single = x as f32;
    (f64::from(single).to_bits() == x.to_bits()).then_some(single)
}

/// `x` (not NaN) as a half's bits if that is exact.
pub(crate) fn to_half(x: f64) -> Option<u16> {
    let bits = x.to_bits();
    let sign = if bits >> 63 == 1 { 0x8000u16 } else { 0 };
    let exp = (bits >> 52) & 0x7ff;
    let mantissa = bits & ((1 << 52) - 1);
    if exp == 0x7ff {
        return Some(sign | 0x7c00);
    }
    if exp == 0 {
        // Zero, or a double's subnormal, far below a half's least.
        return (mantissa == 0).then_some(sign);
    }
    let e = i64::try_from(exp).unwrap_or_else(|_| unreachable!("eleven bits")) - 1023;
    let low = |n: u32| -> u16 { u16::try_from(n).unwrap_or_else(|_| unreachable!("fits a half")) };
    if (-14..=15).contains(&e) {
        if mantissa & ((1 << 42) - 1) != 0 {
            return None;
        }
        let biased = u32::try_from(e + 15).unwrap_or_else(|_| unreachable!("1..=30"));
        let top = u32::try_from(mantissa >> 42).unwrap_or_else(|_| unreachable!("ten bits"));
        return Some(sign | low(biased << 10) | low(top));
    }
    if (-24..-14).contains(&e) {
        let full = (1u64 << 52) | mantissa;
        let shift = u32::try_from(28 - e).unwrap_or_else(|_| unreachable!("43..=52"));
        if full & ((1 << shift) - 1) != 0 {
            return None;
        }
        let m = u32::try_from(full >> shift).unwrap_or_else(|_| unreachable!("under 1024"));
        return Some(sign | low(m));
    }
    None
}

/// A half's bits as the double it is.
pub(crate) fn from_half(half: u16) -> f64 {
    let sign = if half & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (half >> 10) & 0x1f;
    let mantissa = f64::from(half & 0x3ff);
    let magnitude = match exp {
        0 => mantissa * (1.0 / 16_777_216.0),
        31 if mantissa == 0.0 => f64::INFINITY,
        // The one NaN, its bits fixed rather than left to the sign's multiplication.
        31 => return f64::NAN,
        _ => {
            // 2^(exp - 25), built from its bits: core has no `powi`.
            let scale = f64::from_bits(u64::from(exp + 1023 - 25) << 52);
            (1024.0 + mantissa) * scale
        }
    };
    sign * magnitude
}
