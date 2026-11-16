//! Bit-packing: `n` values of `w` bits each (0 to 64), end to end, least significant bit first,
//! value `i` at bit `i * w` of the packed bytes. The horizontal order, not FastLanes' interleaved
//! one (columns.md, "The bit-packing layout"): value `i` is one unaligned 8-byte load at byte
//! `i * w / 8`, a shift by `i * w % 8` and a mask, the floor for reading one value; a block of
//! eight values is exactly `w` bytes, so the vector unpack takes eight lanes whose byte offsets
//! and shifts are fixed by `w` alone.
//!
//! Every packed area is followed by at least [`PAD`] readable bytes (the next block, or the
//! zero bytes a column ends with), so the load of the last value never reads past the column.
//! A width above 56 bits can straddle nine bytes: its read takes the ninth byte too.

use core::simd::Simd;

use blaze_int::Int;
use blaze_own::AVec;

use super::plant::{self, Plant};
use super::EncError;

/// Values in a block.
pub const BLOCK: usize = 1024;

/// Bytes every column ends with, so the 8-byte load of its last packed value stays inside it.
pub const PAD: usize = 8;

/// The fewest bits that hold `max`: 0 for 0, 64 for anything at or above `1 << 63`.
#[inline]
pub fn width_of(max: u64) -> u8 {
    (u64::BITS - max.leading_zeros()).truncate_to::<u8>()
}

/// The low `w` bits set.
#[inline(always)]
pub fn mask(w: u8) -> u64 {
    if w == 0 { 0 } else { u64::MAX >> (64 - u32::from(w)) }
}

/// Bytes `n` values of `w` bits take.
#[inline]
pub fn packed_len(n: usize, w: u8) -> usize {
    (n * usize::from(w)).div_ceil(8)
}

/// The little-endian `u64` at byte `at`; past the end is a column `open` should have refused.
#[inline(always)]
fn load64(data: &[u8], at: usize) -> u64 {
    match data.get(at..).and_then(<[u8]>::first_chunk::<8>) {
        Some(b) => u64::from_le_bytes(*b),
        None => past_end(at, data.len()),
    }
}

#[cold]
#[inline(never)]
#[track_caller]
fn past_end(at: usize, len: usize) -> ! {
    panic!("blaze_columns::enc::bits: a load at byte {at} of {len}: a packed area without its padding (a column `open` did not check)")
}

/// Value `i` of the values of width `w` packed at the start of `data`.
#[inline(always)]
pub fn get(data: &[u8], i: usize, w: u8) -> u64 {
    if w == 0 {
        return 0;
    }
    let bit = i * usize::from(w);
    let at = bit >> 3;
    let shift = (bit & 7).truncate_to::<u32>();
    let low = load64(data, at) >> shift;
    let value = if u32::from(w) + shift > 64 {
        // At most 7 bits past the eight bytes: the ninth byte holds them.
        let ninth = match data.get(at + 8) {
            Some(&byte) => byte,
            None => past_end(at + 8, data.len()),
        };
        low | (u64::from(ninth) << (64 - shift))
    } else {
        low
    };
    value & mask(w)
}

/// Appends `n` values of width `w`, value `j` being `value(j)` (which must fit `w` bits; the
/// bits above are dropped and a debug build stops on them). Refused with nothing written.
pub fn pack(n: usize, w: u8, out: &mut AVec<'_, u8>, value: impl Fn(usize) -> u64) -> Result<(), EncError> {
    let bytes = packed_len(n, w);
    out.try_reserve(bytes)?;
    if w == 0 {
        return Ok(());
    }
    let start = out.len();
    // Reserved above: this does not allocate.
    out.resize(start + bytes, 0);
    let dst = &mut out[start..];
    let keep = if plant::on(Plant::PackDropsTopBit) && w > 1 { mask(w - 1) } else { mask(w) };
    let (mut acc, mut held, mut at) = (0u128, 0u32, 0usize);
    for j in 0..n {
        let v = value(j);
        debug_assert!(v & !mask(w) == 0, "value {v} does not fit {w} bits");
        acc |= u128::from(v & keep) << held;
        held += u32::from(w);
        if held >= 64 {
            dst[at..at + 8].copy_from_slice(&acc.truncate_to::<u64>().to_le_bytes());
            acc >>= 64;
            held -= 64;
            at += 8;
        }
    }
    let rest = blaze_int::idx(held.div_ceil(8));
    dst[at..at + rest].copy_from_slice(&acc.truncate_to::<u64>().to_le_bytes()[..rest]);
    Ok(())
}

/// Fills `out` with the first `out.len()` values of width `w` at the start of `data`, one
/// [`get`] each: the scalar twin of [`unpack`], which the tests hold equal to it.
pub fn unpack_scalar(data: &[u8], w: u8, out: &mut [u64]) {
    for (j, slot) in out.iter_mut().enumerate() {
        *slot = get(data, j, w);
    }
}

/// Lanes of the vector unpack: eight values of `w` bits are `w` whole bytes.
const LANES: usize = 8;

/// Fills `out` with the first `out.len()` values of width `w` at the start of `data`: eight
/// at a time in a `Simd<u64, 8>`, each lane loaded at its fixed byte offset in the group and
/// shifted by its fixed amount, the mask applied to all eight; the scalar [`get`] for the last
/// few and for widths above 56 bits (which straddle nine bytes).
pub fn unpack(data: &[u8], w: u8, out: &mut [u64]) {
    if w == 0 {
        out.fill(0);
        return;
    }
    if w > 56 {
        unpack_scalar(data, w, out);
        return;
    }
    let wide = usize::from(w);
    let mut offsets = [0usize; LANES];
    let mut shifts = [0u64; LANES];
    for lane in 0..LANES {
        offsets[lane] = (lane * wide) >> 3;
        shifts[lane] = ((lane * wide) & 7).widen_to::<u64>();
    }
    if plant::on(Plant::LaneShiftShort) && shifts[LANES - 1] > 0 {
        shifts[LANES - 1] -= 1;
    }
    let shifts = Simd::<u64, LANES>::from_array(shifts);
    let keep = Simd::<u64, LANES>::splat(mask(w));
    let groups = out.len() / LANES;
    for (g, chunk) in out[..groups * LANES].chunks_exact_mut(LANES).enumerate() {
        let base = g * wide;
        let lanes = Simd::<u64, LANES>::from_array([
            load64(data, base + offsets[0]),
            load64(data, base + offsets[1]),
            load64(data, base + offsets[2]),
            load64(data, base + offsets[3]),
            load64(data, base + offsets[4]),
            load64(data, base + offsets[5]),
            load64(data, base + offsets[6]),
            load64(data, base + offsets[7]),
        ]);
        ((lanes >> shifts) & keep).copy_to_slice(chunk);
    }
    for j in groups * LANES..out.len() {
        out[j] = get(data, j, w);
    }
}
