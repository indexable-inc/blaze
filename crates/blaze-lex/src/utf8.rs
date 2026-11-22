//! UTF-8 validation: Keiser and Lemire's lookup ("Validating UTF-8 In Less Than One Instruction
//! Per Byte", 2021), with an all-ASCII block of 64 bytes taken by one test.
//!
//! Each byte is checked against the one before it by three 16-entry tables (the previous byte's
//! high nibble, its low nibble, and this byte's high nibble), whose `&` is nonzero exactly where
//! the pair is an error of a two-byte window (a lead without its continuation, a continuation
//! without its lead, an overlong form, a surrogate, a value past U+10FFFF); the third and fourth
//! bytes of a sequence are checked by the bytes two and three back. An error anywhere sets a lane
//! of an accumulator, tested once a block; the first block with one is then walked by the
//! scalar twin, from the start of the character it may begin inside, for the exact offset.

use core::simd::Simd;
use core::simd::num::SimdUint;

use crate::Cpu;
use crate::cpu::Path;
#[cfg(target_arch = "x86_64")]
use crate::vector::Avx2;
use crate::vector::{Lookup, Portable, back, table};

/// Text proven to be UTF-8: the one way from bytes to `&str` (rule 74).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Utf8<'a> {
    text: &'a str,
}

impl<'a> Utf8<'a> {
    /// The proven text.
    pub fn as_str(self) -> &'a str {
        self.text
    }
}

impl core::ops::Deref for Utf8<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        self.text
    }
}

/// Bytes that are not UTF-8: the bytes before `valid_up_to` are, and a sequence that is not
/// starts there (the same offset as `core::str::Utf8Error::valid_up_to`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invalid {
    pub valid_up_to: usize,
}

impl core::fmt::Display for Invalid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "not UTF-8: the sequence at byte {} is invalid or incomplete", self.valid_up_to)
    }
}

impl core::error::Error for Invalid {}

/// `bytes` as text, or the offset of its first invalid sequence.
pub fn validate(cpu: Cpu, bytes: &[u8]) -> Result<Utf8<'_>, Invalid> {
    let invalid = match cpu.path {
        Path::Scalar => first_invalid_scalar(bytes),
        Path::Portable => first_invalid::<Portable, 16>(Portable, bytes),
        #[cfg(target_arch = "x86_64")]
        Path::Avx2(avx2) => {
            // SAFETY: `first_invalid_avx2` needs AVX2, which the `Avx2` value proves.
            unsafe { first_invalid_avx2(avx2, bytes) }
        }
    };
    match invalid {
        Some(valid_up_to) => Err(Invalid { valid_up_to }),
        None => {
            // SAFETY: the check found no invalid sequence in `bytes`, so they are UTF-8.
            let text = unsafe { core::str::from_utf8_unchecked(bytes) };
            Ok(Utf8 { text })
        }
    }
}

/// The scalar twin: the offset of the first sequence that is not UTF-8 (Unicode's table 3-7 of
/// well-formed byte sequences), or `None`.
pub(crate) fn first_invalid_scalar(bytes: &[u8]) -> Option<usize> {
    let mut at = 0;
    while at < bytes.len() {
        let lead = bytes[at];
        if lead < 0x80 {
            at += 1;
            continue;
        }
        let width = match lead {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => return Some(at),
        };
        let second = match bytes.get(at + 1) {
            Some(&second) => second,
            None => return Some(at),
        };
        let second_fits = match lead {
            0xE0 => (0xA0..=0xBF).contains(&second),
            0xED => (0x80..=0x9F).contains(&second),
            0xF0 => (0x90..=0xBF).contains(&second),
            0xF4 => (0x80..=0x8F).contains(&second),
            _ => (0x80..=0xBF).contains(&second),
        };
        if !second_fits {
            return Some(at);
        }
        for k in 2..width {
            match bytes.get(at + k) {
                Some(&next) if (0x80..=0xBF).contains(&next) => {}
                _ => return Some(at),
            }
        }
        at += width;
    }
    None
}

// The error bits of a pair of bytes (simdjson's names).
const TOO_SHORT: u8 = 1 << 0;
const TOO_LONG: u8 = 1 << 1;
const OVERLONG_3: u8 = 1 << 2;
const TOO_LARGE: u8 = 1 << 3;
const SURROGATE: u8 = 1 << 4;
const OVERLONG_2: u8 = 1 << 5;
const TOO_LARGE_1000: u8 = 1 << 6;
const OVERLONG_4: u8 = 1 << 6;
const TWO_CONTS: u8 = 1 << 7;
const CARRY: u8 = TOO_SHORT | TOO_LONG | TWO_CONTS;

/// By the previous byte's high nibble.
const BYTE_1_HIGH: [u8; 16] = [
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    TWO_CONTS,
    TWO_CONTS,
    TWO_CONTS,
    TWO_CONTS,
    TOO_SHORT | OVERLONG_2,
    TOO_SHORT,
    TOO_SHORT | OVERLONG_3 | SURROGATE,
    TOO_SHORT | TOO_LARGE | TOO_LARGE_1000 | OVERLONG_4,
];

/// By the previous byte's low nibble.
const BYTE_1_LOW: [u8; 16] = [
    CARRY | OVERLONG_3 | OVERLONG_2 | OVERLONG_4,
    CARRY | OVERLONG_2,
    CARRY,
    CARRY,
    CARRY | TOO_LARGE,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000 | SURROGATE,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
];

/// By this byte's high nibble.
const BYTE_2_HIGH: [u8; 16] = [
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_LONG | OVERLONG_2 | TWO_CONTS | OVERLONG_3 | TOO_LARGE_1000 | OVERLONG_4,
    TOO_LONG | OVERLONG_2 | TWO_CONTS | OVERLONG_3 | TOO_LARGE,
    TOO_LONG | OVERLONG_2 | TWO_CONTS | SURROGATE | TOO_LARGE,
    TOO_LONG | OVERLONG_2 | TWO_CONTS | SURROGATE | TOO_LARGE,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
];

/// The tables at `N` lanes, made once a call.
struct Tables<const N: usize> {
    byte_1_high: Simd<u8, N>,
    byte_1_low: Simd<u8, N>,
    byte_2_high: Simd<u8, N>,
    /// The largest last three bytes that open no sequence still incomplete at the end.
    incomplete_above: Simd<u8, N>,
}

impl<const N: usize> Tables<N> {
    #[inline(always)]
    fn new() -> Self {
        let incomplete_above = Simd::from_array(core::array::from_fn(|lane| match N - lane {
            1 => 0xC0 - 1,
            2 => 0xE0 - 1,
            3 => 0xF0 - 1,
            _ => 0xFF,
        }));
        Tables {
            byte_1_high: table(&BYTE_1_HIGH),
            byte_1_low: table(&BYTE_1_LOW),
            byte_2_high: table(&BYTE_2_HIGH),
            incomplete_above,
        }
    }
}

/// The error lanes of `input`, given the vector before it.
#[inline(always)]
fn errors<L: Lookup<N>, const N: usize>(
    lookup: L,
    tables: &Tables<N>,
    prev: Simd<u8, N>,
    input: Simd<u8, N>,
) -> Simd<u8, N> {
    let four = Simd::splat(4);
    let prev1 = back::<1, N>(prev, input);
    let special = lookup.lookup(tables.byte_1_high, prev1 >> four)
        & lookup.lookup(tables.byte_1_low, prev1 & Simd::splat(0x0F))
        & lookup.lookup(tables.byte_2_high, input >> four);
    let prev2 = back::<2, N>(prev, input);
    let prev3 = back::<3, N>(prev, input);
    // Only a third byte's lead (111_____) two back, or a fourth's (1111____) three back, leaves
    // a lane at or above 0x80.
    let third = prev2.saturating_sub(Simd::splat(0xE0 - 0x80));
    let fourth = prev3.saturating_sub(Simd::splat(0xF0 - 0x80));
    let must_continue = (third | fourth) & Simd::splat(0x80);
    must_continue ^ special
}

/// Lanes of `input`'s end that open a sequence the next bytes must finish.
#[inline(always)]
fn incomplete<const N: usize>(tables: &Tables<N>, input: Simd<u8, N>) -> Simd<u8, N> {
    input.saturating_sub(tables.incomplete_above)
}

/// The vector form at `N` lanes.
#[inline(always)]
fn first_invalid<L: Lookup<N>, const N: usize>(lookup: L, bytes: &[u8]) -> Option<usize> {
    let tables = Tables::<N>::new();
    let zero = Simd::<u8, N>::splat(0);
    let high = Simd::<u8, N>::splat(0x80);
    let mut prev = zero;
    let (blocks, tail) = bytes.as_chunks::<64>();
    let mut padded = [0u8; 64];
    padded[..tail.len()].copy_from_slice(tail);
    let last = if tail.is_empty() { None } else { Some(&padded) };
    for (index, block) in blocks.iter().chain(last).enumerate() {
        let mut any = zero;
        let mut at = 0;
        while at < 64 {
            any |= Simd::<u8, N>::from_slice(&block[at..at + N]);
            at += N;
        }
        let mut error = zero;
        if (any & high).simd_eq_zero() {
            error = incomplete(&tables, prev);
            prev = Simd::from_slice(&block[64 - N..]);
        } else {
            let mut at = 0;
            while at < 64 {
                let input = Simd::<u8, N>::from_slice(&block[at..at + N]);
                error |= errors(lookup, &tables, prev, input);
                prev = input;
                at += N;
            }
        }
        if !error.simd_eq_zero() {
            return Some(exact(bytes, index * 64));
        }
    }
    if !incomplete(&tables, prev).simd_eq_zero() {
        return Some(exact(bytes, bytes.len()));
    }
    None
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn first_invalid_avx2(avx2: Avx2, bytes: &[u8]) -> Option<usize> {
    first_invalid::<Avx2, 32>(avx2, bytes)
}

/// The exact offset, given that the vector check passed every block before `block` and found
/// an error in the block at `block` (or, at the end, an incomplete sequence): the bytes before
/// the character that may straddle into the block are valid, so the scalar twin, started at
/// that character, finds the first invalid sequence.
fn exact(bytes: &[u8], block: usize) -> usize {
    let mut start = block;
    let mut stepped = 0;
    while stepped < 3 && start > 0 {
        start -= 1;
        stepped += 1;
        if !(0x80..=0xBF).contains(&bytes[start]) {
            break;
        }
        if stepped == 3 {
            // Three continuation bytes before the block: the four-byte character they end is
            // complete, so the block starts a character.
            start = block;
        }
    }
    match first_invalid_scalar(&bytes[start..]) {
        Some(offset) => start + offset,
        None => panic!(
            "blaze_lex::utf8: the vector check found an error in the block at byte {block} that the scalar check, from byte {start}, does not (a bug in one of them)"
        ),
    }
}

/// `simd_eq(0).all()`, by a reduction that compiles to one `umaxv` or `vptest`.
trait EqZero {
    fn simd_eq_zero(self) -> bool;
}

impl<const N: usize> EqZero for Simd<u8, N> {
    #[inline(always)]
    fn simd_eq_zero(self) -> bool {
        self.reduce_or() == 0
    }
}
