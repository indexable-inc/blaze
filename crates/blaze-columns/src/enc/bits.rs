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

use core::simd::{Simd, ToBytes, num::SimdUint};

use blaze_int::Int;

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
    get_bit(data, i * usize::from(w), w)
}

/// The value of width `w` starting at bit `bit` of `data`: one unaligned 8-byte load, a shift
/// and a mask (and a ninth byte above 56 bits). A width of 0 loads nothing.
#[inline(always)]
pub fn get_bit(data: &[u8], bit: usize, w: u8) -> u64 {
    if w == 0 {
        return 0;
    }
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
/// bits above are dropped and a debug build stops on them). Refused with nothing written. In
/// any allocator: blaze-parquet packs its hybrid runs and delta miniblocks with it (Parquet's
/// bit-packing is this layout).
pub fn pack<A: core::alloc::Allocator>(
    n: usize,
    w: u8,
    out: &mut blaze_own::Vec<u8, A>,
    value: impl Fn(usize) -> u64,
) -> Result<(), EncError> {
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

/// Values a group of the vector unpack: eight values of `w` bits are `w` whole bytes, so every
/// group starts on a byte and has the same shape.
const GROUP: usize = 8;

/// How one width's group of eight unpacks: the shape is fixed by the width alone, so it is a
/// table built at compile time ([`KERNELS`]).
///
/// A group is cut into parts, each one 16-byte load at `load[p]` bytes into the group, one
/// byte shuffle (`swizzle_dyn`, a `tbl` on NEON, `pshufb` on SSSE3) that moves each value's
/// bytes to the bottom of its lane, a per-lane right shift by `shift` and a mask. Up to 25
/// bits a value and its shift fit a `u32` lane: two parts of four lanes. Up to 56 bits they
/// fit a `u64` lane: four parts of two. Past 56 a value can need a ninth byte: scalar.
#[derive(Clone, Copy)]
struct Kernel {
    /// Two parts of four `u32` lanes, else four of two `u64` lanes.
    narrow: bool,
    load: [usize; 4],
    shuffle: [[u8; 16]; 4],
    /// Value `v` of the group's shift within its lane.
    shift: [u64; GROUP],
}

const NO_KERNEL: Kernel = Kernel { narrow: false, load: [0; 4], shuffle: [[0; 16]; 4], shift: [0; GROUP] };

#[allow(blaze::as_cast, reason = "a `const fn` table: byte offsets below 16 and shifts below 8, which fit their types")]
const fn kernel(w: usize) -> Kernel {
    let mut k = NO_KERNEL;
    if w == 0 || w > 56 {
        return k;
    }
    k.narrow = w <= 25;
    let (parts, lanes, bytes) = if k.narrow { (2, 4, 4) } else { (4, 2, 8) };
    let mut p = 0;
    while p < parts {
        let start = (p * lanes * w) >> 3;
        k.load[p] = start;
        let mut lane = 0;
        while lane < lanes {
            let v = p * lanes + lane;
            let bit = v * w;
            k.shift[v] = (bit & 7) as u64;
            let mut t = 0;
            while t < bytes {
                // Below 16: the furthest is (3 * 25 + 7) / 8 + 3 = 13, or (56 + 7) / 8 + 7 = 14.
                k.shuffle[p][lane * bytes + t] = ((bit >> 3) - start + t) as u8;
                t += 1;
            }
            lane += 1;
        }
        p += 1;
    }
    k
}

/// [`kernel`] of every width.
static KERNELS: [Kernel; 65] = {
    let mut all = [NO_KERNEL; 65];
    let mut w = 0;
    while w < 65 {
        all[w] = kernel(w);
        w += 1;
    }
    all
};

/// 16 bytes of `data` from `at`.
#[inline(always)]
fn load16(data: &[u8], at: usize) -> Simd<u8, 16> {
    match data.get(at..).and_then(<[u8]>::first_chunk::<16>) {
        Some(b) => Simd::from_array(*b),
        None => past_end(at, data.len()),
    }
}

/// Fills `out` with the first `out.len()` values of width `w` at the start of `data`, a group
/// of eight at a time by [`Kernel`]'s shuffle; the scalar [`get`] for the groups whose 16-byte
/// loads would pass the end of `data`, the last few values, and widths above 56 bits. Needs
/// [`PAD`] readable bytes after the packed values, as [`get`] does.
pub fn unpack(data: &[u8], w: u8, out: &mut [u64]) {
    if w == 0 {
        out.fill(0);
        return;
    }
    if w > 56 {
        unpack_scalar(data, w, out);
        return;
    }
    let k = &KERNELS[usize::from(w)];
    let wide = usize::from(w);
    let mut shift = k.shift;
    if plant::on(Plant::LaneShiftShort) && shift[GROUP - 1] > 0 {
        shift[GROUP - 1] -= 1;
    }
    // The groups whose every load stays inside `data`: group `g`'s last part loads 16 bytes at
    // `g * w + load[3 or 1]`.
    let last = if k.narrow { k.load[1] } else { k.load[3] };
    let fit = match data.len().checked_sub(16 + last) {
        Some(room) => room / wide + 1,
        None => 0,
    };
    let groups = (out.len() / GROUP).min(fit);
    if k.narrow {
        let keep = Simd::<u32, 4>::splat(mask(w).truncate_to::<u32>());
        let shifts = [0, 1].map(|p| Simd::<u32, 4>::from_array(core::array::from_fn(|l| shift[4 * p + l].truncate_to::<u32>())));
        let shuffles = [Simd::from_array(k.shuffle[0]), Simd::from_array(k.shuffle[1])];
        for (g, chunk) in out[..groups * GROUP].chunks_exact_mut(GROUP).enumerate() {
            let base = g * wide;
            for p in 0..2 {
                let lanes = Simd::<u32, 4>::from_le_bytes(load16(data, base + k.load[p]).swizzle_dyn(shuffles[p]));
                ((lanes >> shifts[p]) & keep).cast::<u64>().copy_to_slice(&mut chunk[4 * p..4 * p + 4]);
            }
        }
    } else {
        let keep = Simd::<u64, 2>::splat(mask(w));
        let shifts = [0, 1, 2, 3].map(|p| Simd::<u64, 2>::from_array([shift[2 * p], shift[2 * p + 1]]));
        let shuffles = k.shuffle.map(Simd::from_array);
        for (g, chunk) in out[..groups * GROUP].chunks_exact_mut(GROUP).enumerate() {
            let base = g * wide;
            for p in 0..4 {
                let lanes = Simd::<u64, 2>::from_le_bytes(load16(data, base + k.load[p]).swizzle_dyn(shuffles[p]));
                ((lanes >> shifts[p]) & keep).copy_to_slice(&mut chunk[2 * p..2 * p + 2]);
            }
        }
    }
    for j in groups * GROUP..out.len() {
        out[j] = get(data, j, w);
    }
}
