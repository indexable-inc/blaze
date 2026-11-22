//! The crate's vector routines in `core::simd` (docs/v2/plan/design/std.md, "SIMD"), each with
//! its scalar twin, which the tests hold equal on random input at every alignment and which
//! `examples/cost.rs` measures against:
//!
//! - [`match_len`]: how far two positions of the deflater's input agree, 16 bytes a compare;
//! - [`adler_run`]: Adler-32's two sums over a run, 32 bytes a step (the weighted sum
//!   `b += 32 a + sum((32 - i) x_i)` as one widening multiply and two reductions).
//!
//! Both are public only so the cost example can time each against its twin.
//!
//! CRC-32 stays slicing-by-eight here: a vector CRC needs carry-less multiply or the CRC
//! instructions (`core::arch`, rule 2), which is a measured step of its own.

#![blaze::sans_io]

use core::simd::{Simd, cmp::SimdPartialEq, num::SimdUint};

const LANES: usize = 16;

/// The length of the common prefix of `data[a..]` and `data[b..]`, from `from` up to `most`
/// (`a < b`, `b + most <= data.len()`).
#[inline]
pub fn match_len(data: &[u8], a: usize, b: usize, from: usize, most: usize) -> usize {
    let mut l = from;
    while l + LANES <= most {
        let x = Simd::<u8, LANES>::from_slice(&data[a + l..a + l + LANES]);
        let y = Simd::<u8, LANES>::from_slice(&data[b + l..b + l + LANES]);
        let differ = x.simd_ne(y).to_bitmask();
        if differ != 0 {
            return l + differ.trailing_zeros() as usize;
        }
        l += LANES;
    }
    while l < most && data[a + l] == data[b + l] {
        l += 1;
    }
    l
}

/// [`match_len`]'s scalar twin.
pub fn match_len_scalar(data: &[u8], a: usize, b: usize, from: usize, most: usize) -> usize {
    let mut l = from;
    while l < most && data[a + l] == data[b + l] {
        l += 1;
    }
    l
}

const STEP: usize = 32;

/// The weights of one step's bytes in `b`: 32 for the first, down to 1 for the last.
const WEIGHTS: Simd<u16, STEP> = {
    let mut w = [0u16; STEP];
    let mut i = 0;
    while i < STEP {
        w[i] = (STEP - i) as u16;
        i += 1;
    }
    Simd::from_array(w)
};

/// Adler-32's sums `(a, b)` after `run`, unreduced: the caller keeps `run` at most zlib's NMAX
/// (5552) bytes so neither overflows, and reduces after.
#[inline]
pub fn adler_run(mut a: u32, mut b: u32, run: &[u8]) -> (u32, u32) {
    let (steps, rest) = run.as_chunks::<STEP>();
    for step in steps {
        let x: Simd<u16, STEP> = Simd::from_array(*step).cast();
        // Each sum fits: 32 bytes of at most 255 (8160), and weights at most 32 (8160 * 32 / 2
        // < 2^18): reduced in u32.
        let sum = x.cast::<u32>().reduce_sum();
        let weighted = (x.cast::<u32>() * WEIGHTS.cast::<u32>()).reduce_sum();
        b += STEP as u32 * a + weighted;
        a += sum;
    }
    adler_run_scalar(a, b, rest)
}

/// [`adler_run`]'s scalar twin.
#[inline]
pub fn adler_run_scalar(mut a: u32, mut b: u32, run: &[u8]) -> (u32, u32) {
    for &byte in run {
        a += u32::from(byte);
        b += a;
    }
    (a, b)
}
