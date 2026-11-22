//! Integers as text: decimal two digits at a time from a 200-byte table, written backwards into
//! a stack buffer; hex and binary from their bits.
//!
//! [`Integer`] is implemented for every primitive integer and sealed. Hex and binary of a
//! signed value are its two's-complement bits at its own width, as `core::fmt`'s `{:x}` and
//! `{:b}` are.

use blaze_int::{Int as _, idx};
use blaze_own::planted::{NoPlant, Planted};

use crate::ascii::{Ascii, LOWER, UPPER};
use crate::plant::TextPlant;

/// Room for any integer in any form here: 128 binary digits.
pub(crate) const ROOM: usize = 128;

pub(crate) type Digits = Ascii<ROOM>;

/// Room for any integer in decimal with its sign: `i128::MIN` is 40 bytes. Small, so a call
/// zeroes five words, not sixteen.
pub(crate) type Decimal = Ascii<40>;

mod sealed {
    pub trait Sealed {}
}

/// A primitive integer.
pub trait Integer: Copy + sealed::Sealed {
    /// Its width in bits.
    #[doc(hidden)]
    const BITS: u32;
    /// Whether it is below zero.
    #[doc(hidden)]
    fn negative(self) -> bool;
    /// Its magnitude: `|self|`, which fits the unsigned type of its width.
    #[doc(hidden)]
    fn magnitude(self) -> u128;
    /// Its bits, two's complement at its own width, zero-extended.
    #[doc(hidden)]
    fn bits(self) -> u128;
    /// Its decimal digits (no sign) before the first of `out`, under a control's plant
    /// ([`NoPlant`] where it ships).
    #[doc(hidden)]
    fn decimal_into_planted<P: Planted<TextPlant>>(self, out: &mut Decimal, plant: P);
    /// Its decimal digits (no sign) before the first of `out`.
    #[doc(hidden)]
    #[inline(always)]
    fn decimal_into(self, out: &mut Decimal) {
        self.decimal_into_planted(out, NoPlant);
    }
}

/// `n`'s decimal digits before the first of `out`, two at a time.
#[inline]
pub(crate) fn u64_into<const N: usize>(n: u64, out: &mut Ascii<N>) {
    u64_into_planted(n, out, NoPlant);
}

/// [`u64_into`] under a control's plant, which reaches every two-digit table read.
#[inline]
pub(crate) fn u64_into_planted<const N: usize, P: Planted<TextPlant>>(mut n: u64, out: &mut Ascii<N>, plant: P) {
    // Past eight digits, the low eight at a time: one 64-bit division, then 32-bit arithmetic
    // on two independent halves of four, which the core overlaps.
    while n >= 100_000_000 {
        let low = (n % 100_000_000).truncate_to::<u32>();
        n /= 100_000_000;
        eight_into(low, out, plant);
    }
    let mut m = n.truncate_to::<u32>();
    while m >= 100 {
        let r = m % 100;
        m /= 100;
        out.front_pair(idx(r), plant);
    }
    if m >= 10 {
        out.front_pair(idx(m), plant);
    } else {
        out.front_digit(&LOWER, idx(m));
    }
}

/// Exactly eight digits of `n` (below 10^8), zeros kept.
#[inline(always)]
fn eight_into<const N: usize, P: Planted<TextPlant>>(n: u32, out: &mut Ascii<N>, plant: P) {
    let (high, low) = (n / 10_000, n % 10_000);
    out.front_pair(idx(low % 100), plant);
    out.front_pair(idx(low / 100), plant);
    out.front_pair(idx(high % 100), plant);
    out.front_pair(idx(high / 100), plant);
}

const TEN_19: u64 = 10_000_000_000_000_000_000;

/// `n`'s decimal digits before the first of `out`: 19 digits at a time through `u64`.
#[inline]
pub(crate) fn u128_into<const N: usize>(n: u128, out: &mut Ascii<N>) {
    u128_into_planted(n, out, NoPlant);
}

/// [`u128_into`] under a control's plant.
#[inline]
pub(crate) fn u128_into_planted<const N: usize, P: Planted<TextPlant>>(mut n: u128, out: &mut Ascii<N>, plant: P) {
    loop {
        if let Ok(small) = n.narrow::<u64>() {
            u64_into_planted(small, out, plant);
            return;
        }
        let chunk = (n % u128::from(TEN_19)).truncate_to::<u64>();
        n /= u128::from(TEN_19);
        let before = out.len();
        u64_into_planted(chunk, out, plant);
        // Each chunk below the top one is exactly 19 digits.
        for _ in out.len() - before..19 {
            out.front_digit(&LOWER, 0);
        }
    }
}

/// `bits`' digits in base `1 << shift` (1 binary, 4 hex), at least `width` of them (zero
/// padded), before the first of `out`.
#[inline]
pub(crate) fn radix_pow2_into(mut bits: u128, shift: u32, table: &[u8; 16], width: usize, out: &mut Digits) {
    let mask = (1u128 << shift) - 1;
    let start = out.len();
    loop {
        out.front_digit(table, (bits & mask).truncate_to::<usize>());
        bits >>= shift;
        if bits == 0 {
            break;
        }
    }
    let width = width.min(ROOM);
    while out.len() - start < width {
        out.front_digit(table, 0);
    }
}

pub(crate) fn table(upper: bool) -> &'static [u8; 16] {
    match upper {
        true => &UPPER,
        false => &LOWER,
    }
}

/// The value as a `u64`: itself for `u64`, a widening for the rest (`blaze_int` has no
/// widening of a type to itself).
trait To64: Copy {
    fn to64(self) -> u64;
}

macro_rules! to64 {
    ($($t:ident)*) => {$(
        impl To64 for $t {
            #[inline(always)]
            fn to64(self) -> u64 {
                self.widen_to::<u64>()
            }
        }
    )*};
}

to64!(u8 u16 u32 usize);

impl To64 for u64 {
    #[inline(always)]
    fn to64(self) -> u64 {
        self
    }
}

macro_rules! small_unsigned {
    ($($t:ident)*) => {$(
        impl sealed::Sealed for $t {}
        impl Integer for $t {
            const BITS: u32 = $t::BITS;
            #[inline(always)]
            fn negative(self) -> bool {
                false
            }
            #[inline(always)]
            fn magnitude(self) -> u128 {
                self.widen_to::<u128>()
            }
            #[inline(always)]
            fn bits(self) -> u128 {
                self.widen_to::<u128>()
            }
            #[inline(always)]
            fn decimal_into_planted<P: Planted<TextPlant>>(self, out: &mut Decimal, plant: P) {
                u64_into_planted(self.to64(), out, plant);
            }
        }
    )*};
}

macro_rules! small_signed {
    ($($t:ident)*) => {$(
        impl sealed::Sealed for $t {}
        impl Integer for $t {
            const BITS: u32 = $t::BITS;
            #[inline(always)]
            fn negative(self) -> bool {
                self < 0
            }
            #[inline(always)]
            fn magnitude(self) -> u128 {
                self.unsigned_abs().widen_to::<u128>()
            }
            #[inline(always)]
            fn bits(self) -> u128 {
                self.cast_unsigned().widen_to::<u128>()
            }
            #[inline(always)]
            fn decimal_into_planted<P: Planted<TextPlant>>(self, out: &mut Decimal, plant: P) {
                u64_into_planted(self.unsigned_abs().to64(), out, plant);
            }
        }
    )*};
}

small_unsigned!(u8 u16 u32 u64 usize);
small_signed!(i8 i16 i32 i64 isize);

impl sealed::Sealed for u128 {}
impl Integer for u128 {
    const BITS: u32 = u128::BITS;
    #[inline(always)]
    fn negative(self) -> bool {
        false
    }
    #[inline(always)]
    fn magnitude(self) -> u128 {
        self
    }
    #[inline(always)]
    fn bits(self) -> u128 {
        self
    }
    #[inline(always)]
    fn decimal_into_planted<P: Planted<TextPlant>>(self, out: &mut Decimal, plant: P) {
        u128_into_planted(self, out, plant);
    }
}

impl sealed::Sealed for i128 {}
impl Integer for i128 {
    const BITS: u32 = i128::BITS;
    #[inline(always)]
    fn negative(self) -> bool {
        self < 0
    }
    #[inline(always)]
    fn magnitude(self) -> u128 {
        self.unsigned_abs()
    }
    #[inline(always)]
    fn bits(self) -> u128 {
        self.cast_unsigned()
    }
    #[inline(always)]
    fn decimal_into_planted<P: Planted<TextPlant>>(self, out: &mut Decimal, plant: P) {
        u128_into_planted(self.unsigned_abs(), out, plant);
    }
}

/// `x` in decimal with its sign.
#[inline(always)]
pub(crate) fn decimal<I: Integer>(x: I) -> Decimal {
    decimal_planted(x, NoPlant)
}

/// [`decimal`] under a control's plant: the path every decimal writer takes.
#[inline(always)]
pub(crate) fn decimal_planted<I: Integer, P: Planted<TextPlant>>(x: I, plant: P) -> Decimal {
    let mut out = Decimal::at_end();
    x.decimal_into_planted(&mut out, plant);
    if x.negative() {
        out.front(b'-');
    }
    out
}

/// The hex digit count of a width: `BITS / 4`.
pub(crate) fn nibbles<I: Integer>() -> usize {
    idx(I::BITS / 4)
}
