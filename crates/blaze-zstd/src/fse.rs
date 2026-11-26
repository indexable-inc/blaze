//! RFC 8878 §4.1 Finite State Entropy (FSE) tables, sequence code tables, and bitstreams.

use core::alloc::Allocator;

use blaze_own::blaze_int::Cast;
use blaze_own::{AllocFailure, Int, Pow2Array, Vec, low_u32};

use crate::ZstdError;

pub(crate) const MAX_FSE_LOG: u8 = 9;
pub(crate) const MAX_FSE_TABLE_SIZE: usize = 1 << MAX_FSE_LOG;
pub(crate) const MAX_FSE_SYMBOLS: usize = 64;

pub(crate) const LL_DEFAULT_LOG: u8 = 6;
pub(crate) const OF_DEFAULT_LOG: u8 = 5;
pub(crate) const ML_DEFAULT_LOG: u8 = 6;

pub(crate) const LL_MAX_CODE: usize = 35;
pub(crate) const OF_MAX_CODE: usize = 31;
pub(crate) const ML_MAX_CODE: usize = 52;

pub(crate) const LL_MAX_LOG: u8 = 9;
pub(crate) const OF_MAX_LOG: u8 = 8;
pub(crate) const ML_MAX_LOG: u8 = 9;

/// RFC 8878 §3.1.1.3.2.2 default Literals_Length distribution (accuracy log 6, 36 symbols).
pub(crate) const LL_DEFAULT_NORM: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];

/// RFC 8878 §3.1.1.3.2.2 default Offset distribution (accuracy log 5, 29 symbols).
pub(crate) const OF_DEFAULT_NORM: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

/// RFC 8878 §3.1.1.3.2.2 default Match_Length distribution (accuracy log 6, 53 symbols).
pub(crate) const ML_DEFAULT_NORM: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

/// RFC 8878 §3.1.1.3.2.1.1 Literals_Length baselines for codes 0..=35 (padded to 64 for masked indexing).
const LL_BASE: Pow2Array<u32, 64> = Pow2Array::new([
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1_024, 2_048, 4_096, 8_192, 16_384, 32_768, 65_536,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
]);

/// RFC 8878 §3.1.1.3.2.1.1 Literals_Length extra bit counts for codes 0..=35 (padded to 64).
const LL_BITS: Pow2Array<u8, 64> = Pow2Array::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
]);

/// RFC 8878 §3.1.1.3.2.1.1 Match_Length baselines for codes 0..=52 (padded to 64 for masked indexing).
const ML_BASE: Pow2Array<u32, 64> = Pow2Array::new([
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1_027,
    2_051, 4_099, 8_195, 16_387, 32_771, 65_539,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
]);

/// RFC 8878 §3.1.1.3.2.1.1 Match_Length extra bit counts for codes 0..=52 (padded to 64).
const ML_BITS: Pow2Array<u8, 64> = Pow2Array::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
]);

const fn pack_seq(code: u32, bits: u32, base: u32) -> u32 {
    code | (bits << 8) | (base << 16)
}

/// Packed 32-bit LL encoding table for `0..64`: `code | (bits << 8) | (base << 16)`.
pub(crate) const LL_ENC_PACKED: [u32; 64] = [
    pack_seq(0, 0, 0), pack_seq(1, 0, 1), pack_seq(2, 0, 2), pack_seq(3, 0, 3),
    pack_seq(4, 0, 4), pack_seq(5, 0, 5), pack_seq(6, 0, 6), pack_seq(7, 0, 7),
    pack_seq(8, 0, 8), pack_seq(9, 0, 9), pack_seq(10, 0, 10), pack_seq(11, 0, 11),
    pack_seq(12, 0, 12), pack_seq(13, 0, 13), pack_seq(14, 0, 14), pack_seq(15, 0, 15),
    pack_seq(16, 1, 16), pack_seq(16, 1, 16), pack_seq(17, 1, 18), pack_seq(17, 1, 18),
    pack_seq(18, 1, 20), pack_seq(18, 1, 20), pack_seq(19, 1, 22), pack_seq(19, 1, 22),
    pack_seq(20, 2, 24), pack_seq(20, 2, 24), pack_seq(20, 2, 24), pack_seq(20, 2, 24),
    pack_seq(21, 2, 28), pack_seq(21, 2, 28), pack_seq(21, 2, 28), pack_seq(21, 2, 28),
    pack_seq(22, 3, 32), pack_seq(22, 3, 32), pack_seq(22, 3, 32), pack_seq(22, 3, 32),
    pack_seq(22, 3, 32), pack_seq(22, 3, 32), pack_seq(22, 3, 32), pack_seq(22, 3, 32),
    pack_seq(23, 3, 40), pack_seq(23, 3, 40), pack_seq(23, 3, 40), pack_seq(23, 3, 40),
    pack_seq(23, 3, 40), pack_seq(23, 3, 40), pack_seq(23, 3, 40), pack_seq(23, 3, 40),
    pack_seq(24, 4, 48), pack_seq(24, 4, 48), pack_seq(24, 4, 48), pack_seq(24, 4, 48),
    pack_seq(24, 4, 48), pack_seq(24, 4, 48), pack_seq(24, 4, 48), pack_seq(24, 4, 48),
    pack_seq(24, 4, 48), pack_seq(24, 4, 48), pack_seq(24, 4, 48), pack_seq(24, 4, 48),
    pack_seq(24, 4, 48), pack_seq(24, 4, 48), pack_seq(24, 4, 48), pack_seq(24, 4, 48),
];

/// Packed 32-bit ML encoding table for `match_len - 3` in `0..128`: `code | (bits << 8) | (base << 16)`.
pub(crate) const ML_ENC_PACKED: [u32; 128] = [
    pack_seq(0, 0, 3), pack_seq(1, 0, 4), pack_seq(2, 0, 5), pack_seq(3, 0, 6),
    pack_seq(4, 0, 7), pack_seq(5, 0, 8), pack_seq(6, 0, 9), pack_seq(7, 0, 10),
    pack_seq(8, 0, 11), pack_seq(9, 0, 12), pack_seq(10, 0, 13), pack_seq(11, 0, 14),
    pack_seq(12, 0, 15), pack_seq(13, 0, 16), pack_seq(14, 0, 17), pack_seq(15, 0, 18),
    pack_seq(16, 0, 19), pack_seq(17, 0, 20), pack_seq(18, 0, 21), pack_seq(19, 0, 22),
    pack_seq(20, 0, 23), pack_seq(21, 0, 24), pack_seq(22, 0, 25), pack_seq(23, 0, 26),
    pack_seq(24, 0, 27), pack_seq(25, 0, 28), pack_seq(26, 0, 29), pack_seq(27, 0, 30),
    pack_seq(28, 0, 31), pack_seq(29, 0, 32), pack_seq(30, 0, 33), pack_seq(31, 0, 34),
    pack_seq(32, 1, 35), pack_seq(32, 1, 35), pack_seq(33, 1, 37), pack_seq(33, 1, 37),
    pack_seq(34, 1, 39), pack_seq(34, 1, 39), pack_seq(35, 1, 41), pack_seq(35, 1, 41),
    pack_seq(36, 2, 43), pack_seq(36, 2, 43), pack_seq(36, 2, 43), pack_seq(36, 2, 43),
    pack_seq(37, 2, 47), pack_seq(37, 2, 47), pack_seq(37, 2, 47), pack_seq(37, 2, 47),
    pack_seq(38, 3, 51), pack_seq(38, 3, 51), pack_seq(38, 3, 51), pack_seq(38, 3, 51),
    pack_seq(38, 3, 51), pack_seq(38, 3, 51), pack_seq(38, 3, 51), pack_seq(38, 3, 51),
    pack_seq(39, 3, 59), pack_seq(39, 3, 59), pack_seq(39, 3, 59), pack_seq(39, 3, 59),
    pack_seq(39, 3, 59), pack_seq(39, 3, 59), pack_seq(39, 3, 59), pack_seq(39, 3, 59),
    pack_seq(40, 4, 67), pack_seq(40, 4, 67), pack_seq(40, 4, 67), pack_seq(40, 4, 67),
    pack_seq(40, 4, 67), pack_seq(40, 4, 67), pack_seq(40, 4, 67), pack_seq(40, 4, 67),
    pack_seq(40, 4, 67), pack_seq(40, 4, 67), pack_seq(40, 4, 67), pack_seq(40, 4, 67),
    pack_seq(40, 4, 67), pack_seq(40, 4, 67), pack_seq(40, 4, 67), pack_seq(40, 4, 67),
    pack_seq(41, 4, 83), pack_seq(41, 4, 83), pack_seq(41, 4, 83), pack_seq(41, 4, 83),
    pack_seq(41, 4, 83), pack_seq(41, 4, 83), pack_seq(41, 4, 83), pack_seq(41, 4, 83),
    pack_seq(41, 4, 83), pack_seq(41, 4, 83), pack_seq(41, 4, 83), pack_seq(41, 4, 83),
    pack_seq(41, 4, 83), pack_seq(41, 4, 83), pack_seq(41, 4, 83), pack_seq(41, 4, 83),
    pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99),
    pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99),
    pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99),
    pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99),
    pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99),
    pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99),
    pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99),
    pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99), pack_seq(42, 5, 99),
];

/// Branchless single-LUT LL encoder for valid `lit_len < 131_072` (no `Option` overhead).
#[inline(always)]
pub(crate) fn encode_ll_raw(lit_len: u32) -> (u8, u32, u32) {
    if lit_len < 64 {
        let idx = lit_len.widen_to::<usize>() & 63;
        let packed = LL_ENC_PACKED.get(idx).copied().unwrap_or(0);
        let code = packed.truncate_to::<u8>();
        let bits = (packed >> 8) & 0xFF;
        let base = packed >> 16;
        return (code, lit_len - base, bits);
    }
    if lit_len < 128 {
        return (25, lit_len - 64, 6);
    }
    let bits = 31u32 - lit_len.leading_zeros();
    let code = (bits + 19).truncate_to::<u8>();
    let base = 1u32 << bits;
    (code, lit_len - base, bits)
}

/// Branchless single-LUT ML encoder for valid `match_len in 3..=131_074` (no `Option` overhead).
#[inline(always)]
pub(crate) fn encode_ml_raw(match_len: u32) -> (u8, u32, u32) {
    let m = match_len.saturating_sub(3);
    if m < 128 {
        let idx = m.widen_to::<usize>() & 127;
        let packed = ML_ENC_PACKED.get(idx).copied().unwrap_or(0);
        let code = packed.truncate_to::<u8>();
        let bits = (packed >> 8) & 0xFF;
        let base = packed >> 16;
        return (code, match_len - base, bits);
    }
    let bits = 31u32 - m.leading_zeros();
    let code = (bits + 36).truncate_to::<u8>();
    let base = (1u32 << bits) + 3;
    (code, match_len - base, bits)
}

/// Branchless OF encoder for valid `offset_value >= 1` (no `Option` overhead).
#[inline(always)]
pub(crate) fn encode_of_raw(offset_value: u32) -> (u8, u32, u32) {
    let bits = 31u32 - offset_value.leading_zeros();
    let code = bits.truncate_to::<u8>();
    (code, offset_value - (1u32 << bits), bits)
}

/// Fast branchless single-LUT LL encoder for `lit_len: u32` (`0..=131_071`).
#[inline(always)]
pub(crate) fn encode_ll_fast(lit_len: u32) -> Option<(u8, u32, u32)> {
    if lit_len >= 131_072 {
        return None;
    }
    Some(encode_ll_raw(lit_len))
}

/// Fast branchless single-LUT ML encoder for `match_len: u32` (`3..=131_074`).
#[inline(always)]
pub(crate) fn encode_ml_fast(match_len: u32) -> Option<(u8, u32, u32)> {
    if match_len < 3 || match_len >= 131_075 {
        return None;
    }
    Some(encode_ml_raw(match_len))
}

/// Fast branchless OF encoder for `offset_value: u32` (`>= 1`).
#[inline(always)]
pub(crate) fn encode_of_fast(offset_value: u32) -> Option<(u8, u32, u32)> {
    if offset_value == 0 {
        return None;
    }
    Some(encode_of_raw(offset_value))
}

/// Encode a literal length (`0..=131_071`) into `(ll_code, extra_val, extra_bits)` in O(1).
#[cfg(test)]
#[inline(always)]
pub(crate) fn encode_ll(lit_len: usize) -> Option<(u8, u32, u32)> {
    let len_u32 = u32::try_from(lit_len).ok()?;
    encode_ll_fast(len_u32)
}

/// Encode a match length (`3..=131_074`) into `(ml_code, extra_val, extra_bits)` in O(1).
#[cfg(test)]
#[inline(always)]
pub(crate) fn encode_ml(match_len: usize) -> Option<(u8, u32, u32)> {
    let len_u32 = u32::try_from(match_len).ok()?;
    encode_ml_fast(len_u32)
}

/// Encode an `Offset_Value` (`>= 1`) into `(of_code, extra_val, extra_bits)`.
#[cfg(test)]
#[inline(always)]
pub(crate) fn encode_of(offset_value: u32) -> Option<(u8, u32, u32)> {
    encode_of_fast(offset_value)
}

// ---------------------------------------------------------------------------
// Reverse bitstream writer (RFC 8878 §4.1 / §4.2); reader lives in `blaze_own::RevBitCursor`.
// ---------------------------------------------------------------------------

/// Writes bits LSB-first in bulk 4-byte flushes and appends the RFC 8878 terminating `1` sentinel
/// bit so that [`blaze_own::RevBitCursor`] pops fields in the exact reverse order they were written.
pub(crate) struct BitWriter<A: Allocator> {
    out: Vec<u8, A>,
    buf: u64,
    bits: u32,
}

impl<A: Allocator> BitWriter<A> {
    pub(crate) fn try_with_capacity_in(cap: usize, alloc: A) -> Result<Self, AllocFailure> {
        Ok(Self { out: Vec::try_with_capacity_in(cap, alloc)?, buf: 0, bits: 0 })
    }

    #[inline(always)]
    pub(crate) fn write_bits(&mut self, val: u32, n: u32) -> Result<(), AllocFailure> {
        if n == 0 {
            return Ok(());
        }
        let masked = if n >= 32 { u64::from(val) } else { u64::from(val) & ((1u64 << n) - 1) };
        self.buf |= masked << self.bits;
        self.bits += n;
        if self.bits >= 32 {
            self.out.try_extend_from_slice(&self.buf.truncate_to::<u32>().to_le_bytes())?;
            self.buf >>= 32;
            self.bits -= 32;
        }
        Ok(())
    }

    /// Flush into an existing `Vec<u8, A>` (appending the finished reverse bitstream).
    pub(crate) fn finish_into(mut self, dst: &mut Vec<u8, A>) -> Result<(), AllocFailure> {
        while self.bits >= 8 {
            self.out.try_push(self.buf.truncate_to::<u8>()).map_err(|r| r.why)?;
            self.buf >>= 8;
            self.bits -= 8;
        }
        self.buf |= 1u64 << self.bits;
        self.out.try_push(self.buf.truncate_to::<u8>()).map_err(|r| r.why)?;
        dst.try_extend_from_slice(self.out.as_slice())
    }
}

// ---------------------------------------------------------------------------
// Forward bitstream reader & writer for FSE table headers (RFC 8878 §4.1.1)
// ---------------------------------------------------------------------------

struct FwdBitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
    total_bits: usize,
}

impl<'a> FwdBitReader<'a> {
    fn new(bytes: &'a [u8]) -> Option<Self> {
        let total_bits = bytes.len().checked_mul(8)?;
        Some(Self { bytes, bit_pos: 0, total_bits })
    }

    #[inline]
    fn peek(&self, n: u32) -> u32 {
        if n == 0 || n > 24 {
            return 0;
        }
        let byte_idx = self.bit_pos / 8;
        let bit_off = (self.bit_pos & 7).truncate_to::<u32>();
        let b0 = self.bytes.get(byte_idx).copied().unwrap_or(0);
        let b1 = self.bytes.get(byte_idx + 1).copied().unwrap_or(0);
        let b2 = self.bytes.get(byte_idx + 2).copied().unwrap_or(0);
        let b3 = self.bytes.get(byte_idx + 3).copied().unwrap_or(0);
        let word = u32::from_le_bytes([b0, b1, b2, b3]) >> bit_off;
        word & ((1u32 << n) - 1)
    }

    #[inline(always)]
    fn consume(&mut self, n: u32) -> bool {
        let adv = n.widen_to::<usize>();
        let Some(next) = self.bit_pos.checked_add(adv) else {
            return false;
        };
        if next > self.total_bits {
            return false;
        }
        self.bit_pos = next;
        true
    }

    #[inline]
    const fn bytes_consumed(&self) -> usize {
        self.bit_pos.div_ceil(8)
    }
}

/// Forward LSB-first bit writer for RFC 8878 §4.1.1 FSE distribution headers.
pub(crate) struct FwdBitWriter<'a, A: Allocator> {
    out: &'a mut Vec<u8, A>,
    buf: u64,
    bits: u32,
}

impl<'a, A: Allocator> FwdBitWriter<'a, A> {
    pub(crate) fn new(out: &'a mut Vec<u8, A>) -> Self {
        Self { out, buf: 0, bits: 0 }
    }

    #[inline]
    pub(crate) fn write(&mut self, val: u32, n: u32) -> Result<(), AllocFailure> {
        if n == 0 {
            return Ok(());
        }
        let masked = if n >= 32 { u64::from(val) } else { u64::from(val) & ((1u64 << n) - 1) };
        self.buf |= masked << self.bits;
        self.bits += n;
        while self.bits >= 8 {
            self.out.try_push(self.buf.truncate_to::<u8>()).map_err(|r| r.why)?;
            self.buf >>= 8;
            self.bits -= 8;
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<(), AllocFailure> {
        if self.bits > 0 {
            self.out.try_push(self.buf.truncate_to::<u8>()).map_err(|r| r.why)?;
        }
        Ok(())
    }
}

fn write_fse_header_inner<A: Allocator>(
    norm: &[i16],
    accuracy_log: u8,
    out: &mut Vec<u8, A>,
) -> Result<bool, AllocFailure> {
    if !(5..=MAX_FSE_LOG).contains(&accuracy_log) || norm.len() > MAX_FSE_SYMBOLS {
        return Ok(false);
    }
    let table_size: i32 = 1i32 << u32::from(accuracy_log);
    let mut sum: i32 = 0;
    for &p in norm {
        if p == -1 {
            sum += 1;
        } else if p > 0 {
            sum += i32::from(p);
        } else if p < -1 {
            return Ok(false);
        }
    }
    if sum != table_size {
        return Ok(false);
    }

    let mut bw = FwdBitWriter::new(out);
    bw.write(u32::from(accuracy_log - 5), 4)?;

    let mut remaining: i32 = table_size + 1;
    let mut threshold: i32 = table_size;
    let mut nb_bits: u32 = u32::from(accuracy_log) + 1;
    let mut idx: usize = 0;

    while remaining > 1 && idx < norm.len() {
        let Some(&prob_i16) = norm.get(idx) else {
            break;
        };
        let prob = i32::from(prob_i16);
        let max: i32 = (2 * threshold - 1) - remaining;
        let count: i32 = prob + 1;
        let small_bits = nb_bits.wrapping_sub(1);

        if count < max {
            bw.write(count.cast_unsigned(), small_bits)?;
        } else if count < threshold {
            bw.write(count.cast_unsigned(), nb_bits)?;
        } else {
            bw.write((count + max).cast_unsigned(), nb_bits)?;
        }

        if prob == -1 {
            remaining -= 1;
        } else if prob > 0 {
            remaining -= prob;
        }
        idx += 1;

        while remaining < threshold && nb_bits > 1 {
            nb_bits -= 1;
            threshold >>= 1;
        }

        if prob == 0 && remaining > 1 {
            let mut zeros: usize = 0;
            while let Some(&next_p) = norm.get(idx)
                && next_p == 0
            {
                zeros += 1;
                idx += 1;
            }
            while zeros >= 3 {
                bw.write(3, 2)?;
                zeros -= 3;
            }
            bw.write(zeros.truncate_to::<u32>(), 2)?;
        }
    }

    if remaining != 1 {
        return Ok(false);
    }
    bw.finish()?;
    Ok(true)
}

/// Write an RFC 8878 §4.1.1 FSE distribution header for `norm` (`accuracy_log` in `5..=9`) into
/// `out`. Exact inverse of [`FseDecTable::decode_header_and_build`].
pub(crate) fn write_fse_header<A: Allocator>(
    norm: &[i16],
    accuracy_log: u8,
    out: &mut Vec<u8, A>,
) -> Result<(), ZstdError> {
    if !write_fse_header_inner(norm, accuracy_log, out)? {
        return Err(ZstdError::CorruptFse);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FSE Decoding Table (RFC 8878 §4.1.1)
// ---------------------------------------------------------------------------

/// One state entry in an FSE decoding table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct FseDecEntry {
    pub(crate) symbol: u8,
    pub(crate) num_bits: u8,
    pub(crate) baseline: u16,
}

impl FseDecEntry {
    pub(crate) const ZERO: Self = Self { symbol: 0, num_bits: 0, baseline: 0 };
}

/// Pre-resolved sequence FSE decoding entry packed into a single `u64` (`#[repr(transparent)]`)
/// so each table lookup is one 64-bit load and all three active entries occupy just 3 GPRs:
/// - bits `0..32`: `base: u32` (`LL_BASE`, `ML_BASE`, or `1 << of_code`)
/// - bits `32..48`: `baseline: u16` (FSE next-state baseline, pre-biased by `0` / `512` / `1024`)
/// - bits `48..56`: `extra_bits: u8`
/// - bits `56..64`: `num_bits: u8` (FSE state transition bits)
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SeqDecEntry(pub(crate) u64);

impl SeqDecEntry {
    pub(crate) const ZERO: Self = Self(0);

    #[inline(always)]
    pub(crate) const fn new(base: u32, extra_bits: u8, num_bits: u8, baseline: u16) -> Self {
        Self(
            Cast::<u32, u64>::widen(base)
                | (Cast::<u16, u64>::widen(baseline) << 32)
                | (Cast::<u8, u64>::widen(extra_bits) << 48)
                | (Cast::<u8, u64>::widen(num_bits) << 56),
        )
    }

    #[inline(always)]
    pub(crate) fn base(self) -> u32 {
        self.0.truncate_to::<u32>()
    }

    #[inline(always)]
    pub(crate) fn baseline(self) -> usize {
        ((self.0 >> 32) & 0xFFFF).truncate_to::<usize>()
    }

    #[inline(always)]
    pub(crate) fn extra_bits(self) -> u32 {
        ((self.0 >> 48) & 0xFF).truncate_to::<u32>()
    }

    #[inline(always)]
    pub(crate) fn num_bits(self) -> u32 {
        (self.0 >> 56).truncate_to::<u32>()
    }

    #[inline(always)]
    pub(crate) fn num_bits_u64(self) -> u64 {
        self.0 >> 56
    }

    /// Unpack `(base, extra_bits, num_bits, baseline)` using pure bit shifts.
    #[inline(always)]
    pub(crate) fn unpack(self) -> (u32, u32, u32, usize) {
        (self.base(), self.extra_bits(), self.num_bits(), self.baseline())
    }
}

/// Which sequence field a [`SeqDecTable`] resolves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeqKind {
    Ll,
    Of,
    Ml,
}

/// Total flat sequence-table capacity: LL (`0..512`), OF (`512..1024`), ML (`1024..1536`),
/// padded to power-of-two `2048` so a single base pointer + `& 2047` indexes all three tables.
pub(crate) const SEQ_TABLES_FLAT_LEN: usize = 2048;
pub(crate) const SEQ_OF_OFFSET: usize = 512;
pub(crate) const SEQ_ML_OFFSET: usize = 1024;

/// Contiguous power-of-two flat table holding pre-resolved LL, OF, and ML sequence FSE entries
/// under a single base pointer register (OF baselines pre-biased by `+512`, ML by `+1024`).
pub(crate) struct SeqTables {
    entries: Pow2Array<SeqDecEntry, SEQ_TABLES_FLAT_LEN>,
    ll_log: u8,
    of_log: u8,
    ml_log: u8,
    ll_max_eb: u8,
    of_max_eb: u8,
    ml_max_eb: u8,
}

impl SeqTables {
    pub(crate) const fn new_empty() -> Self {
        Self {
            entries: Pow2Array::splat(SeqDecEntry::ZERO),
            ll_log: 0,
            of_log: 0,
            ml_log: 0,
            ll_max_eb: 0,
            of_max_eb: 0,
            ml_max_eb: 0,
        }
    }

    #[inline(always)]
    pub(crate) const fn ll_log(&self) -> u8 {
        self.ll_log
    }

    #[inline(always)]
    pub(crate) const fn of_log(&self) -> u8 {
        self.of_log
    }

    #[inline(always)]
    pub(crate) const fn ml_log(&self) -> u8 {
        self.ml_log
    }

    #[inline(always)]
    pub(crate) fn max_seq_bits(&self) -> u32 {
        u32::from(self.ll_max_eb)
            + u32::from(self.of_max_eb)
            + u32::from(self.ml_max_eb)
            + u32::from(self.ll_log)
            + u32::from(self.of_log)
            + u32::from(self.ml_log)
    }

    #[inline(always)]
    pub(crate) fn entry_flat(&self, state: usize) -> SeqDecEntry {
        self.entries.get_masked(state)
    }

    fn compile_inner(&mut self, dec: &FseDecTable, kind: SeqKind, log: u8) -> bool {
        let table_size = 1usize << u32::from(log);
        let mut max_eb: u8 = 0;
        match kind {
            SeqKind::Ll => {
                for state in 0..table_size {
                    let e = dec.entry_masked(state);
                    let idx = usize::from(e.symbol);
                    if idx > LL_MAX_CODE {
                        return false;
                    }
                    let base = LL_BASE.get_masked(idx);
                    let extra_bits = LL_BITS.get_masked(idx);
                    if extra_bits > max_eb {
                        max_eb = extra_bits;
                    }
                    self.entries.set_masked(
                        state,
                        SeqDecEntry::new(base, extra_bits, e.num_bits, e.baseline),
                    );
                }
                self.ll_log = log;
                self.ll_max_eb = max_eb;
            }
            SeqKind::Of => {
                for state in 0..table_size {
                    let e = dec.entry_masked(state);
                    if usize::from(e.symbol) > OF_MAX_CODE {
                        return false;
                    }
                    let base = 1u32 << u32::from(e.symbol);
                    let extra_bits = e.symbol;
                    if extra_bits > max_eb {
                        max_eb = extra_bits;
                    }
                    let biased_bl = e.baseline.wrapping_add(512);
                    self.entries.set_masked(
                        SEQ_OF_OFFSET.wrapping_add(state),
                        SeqDecEntry::new(base, extra_bits, e.num_bits, biased_bl),
                    );
                }
                self.of_log = log;
                self.of_max_eb = max_eb;
            }
            SeqKind::Ml => {
                for state in 0..table_size {
                    let e = dec.entry_masked(state);
                    let idx = usize::from(e.symbol);
                    if idx > ML_MAX_CODE {
                        return false;
                    }
                    let base = ML_BASE.get_masked(idx);
                    let extra_bits = ML_BITS.get_masked(idx);
                    if extra_bits > max_eb {
                        max_eb = extra_bits;
                    }
                    let biased_bl = e.baseline.wrapping_add(1024);
                    self.entries.set_masked(
                        SEQ_ML_OFFSET.wrapping_add(state),
                        SeqDecEntry::new(base, extra_bits, e.num_bits, biased_bl),
                    );
                }
                self.ml_log = log;
                self.ml_max_eb = max_eb;
            }
        }
        true
    }

    /// Compile an [`FseDecTable`] into the corresponding flat slice of `self`, pre-biasing
    /// `baseline` by `0` (LL), `512` (OF), or `1024` (ML).
    pub(crate) fn compile_from(&mut self, dec: &FseDecTable, kind: SeqKind) -> Result<(), ZstdError> {
        let log = dec.accuracy_log();
        if log > MAX_FSE_LOG {
            return Err(ZstdError::CorruptFse);
        }
        if !self.compile_inner(dec, kind, log) {
            return Err(ZstdError::CorruptSequences);
        }
        Ok(())
    }
}

/// Fixed-capacity FSE decoding table (up to `1 << 9 = 512` states, zero heap allocation).
pub(crate) struct FseDecTable {
    entries: Pow2Array<FseDecEntry, MAX_FSE_TABLE_SIZE>,
    accuracy_log: u8,
}

impl FseDecTable {
    pub(crate) const fn new_empty() -> Self {
        Self { entries: Pow2Array::splat(FseDecEntry::ZERO), accuracy_log: 0 }
    }

    #[inline(always)]
    pub(crate) const fn accuracy_log(&self) -> u8 {
        self.accuracy_log
    }

    #[inline(always)]
    pub(crate) fn entry_masked(&self, state: usize) -> FseDecEntry {
        self.entries.get_masked(state)
    }

    /// Configure this table as an RLE-mode single-symbol table (`accuracy_log = 0`).
    pub(crate) fn set_rle(&mut self, symbol: u8) {
        self.accuracy_log = 0;
        self.entries.set_masked(0, FseDecEntry { symbol, num_bits: 0, baseline: 0 });
    }

    /// Zero-`ZstdError` inner FSE table builder: validates `accuracy_log`, `norm` length, and sum
    /// once upfront, then populates `self.entries` via masked `Pow2Array` accesses with zero
    /// 40-byte `ZstdError` stack copies inside the table-fill loops.
    fn build_from_norm_inner(&mut self, norm: &[i16], accuracy_log: u8) -> bool {
        if accuracy_log > MAX_FSE_LOG || norm.len() > MAX_FSE_SYMBOLS {
            return false;
        }
        let log_u32 = u32::from(accuracy_log);
        let table_size = 1usize << log_u32;
        let table_mask = table_size - 1;
        let step = (table_size >> 1) + (table_size >> 3) + 3;

        let mut high_threshold = table_size;
        let mut symbol_next = Pow2Array::<u16, MAX_FSE_SYMBOLS>::splat(0);
        let mut total: usize = 0;

        // Pass 1: validate normalized probabilities, sum to `table_size`, place `-1` symbols
        // at the top of the table descending, and seed `symbol_next[sym]`.
        for (sym_idx, &prob) in norm.iter().enumerate() {
            let sym_u8 = sym_idx.truncate_to::<u8>();
            if prob == -1 {
                if high_threshold == 0 {
                    return false;
                }
                high_threshold = high_threshold.wrapping_sub(1);
                self.entries.set_masked(
                    high_threshold,
                    FseDecEntry { symbol: sym_u8, num_bits: 0, baseline: 0 },
                );
                symbol_next.set_masked(sym_idx, 1);
                total = total.wrapping_add(1);
            } else if prob > 0 {
                let count_u16 = prob.cast_unsigned();
                symbol_next.set_masked(sym_idx, count_u16);
                total = total.wrapping_add(usize::from(count_u16));
            } else if prob < -1 {
                return false;
            }
        }

        if total != table_size {
            return false;
        }
        // For `high_threshold >= 2`, `step` must be odd (`gcd(step, 2^L) == 1`) so spreading
        // visits every slot `0..high_threshold` in bijection (always true for RFC 8878 `L >= 5`).
        if high_threshold > 1 && (step & 1) == 0 {
            return false;
        }

        // Pass 2: spread positive-probability symbols across `0..high_threshold`.
        let mut pos: usize = 0;
        for (sym_idx, &prob) in norm.iter().enumerate() {
            if prob <= 0 {
                continue;
            }
            let count = usize::from(prob.cast_unsigned());
            let sym_u8 = sym_idx.truncate_to::<u8>();
            let entry = FseDecEntry { symbol: sym_u8, num_bits: 0, baseline: 0 };
            for _ in 0..count {
                self.entries.set_masked(pos, entry);
                pos = pos.wrapping_add(step) & table_mask;
                while pos >= high_threshold {
                    pos = pos.wrapping_add(step) & table_mask;
                }
            }
        }

        if pos != 0 {
            return false;
        }

        // Pass 3: compute `num_bits` and `baseline` for every state `0..table_size`.
        // Passes 1 & 2 proved every slot holds a symbol with `1 <= next_state < 2 * table_size`.
        for state in 0..table_size {
            let sym = self.entries.get_masked(state).symbol;
            let sym_idx = usize::from(sym);
            let next_state = symbol_next.get_masked(sym_idx);
            symbol_next.set_masked(sym_idx, next_state.wrapping_add(1));

            let high_bit = 15u32.wrapping_sub(next_state.leading_zeros());
            let nb = log_u32.wrapping_sub(high_bit);
            let base = (usize::from(next_state) << nb).wrapping_sub(table_size);

            self.entries.set_masked(
                state,
                FseDecEntry {
                    symbol: sym,
                    num_bits: nb.truncate_to::<u8>(),
                    baseline: base.truncate_to::<u16>(),
                },
            );
        }

        self.accuracy_log = accuracy_log;
        true
    }

    /// Build the FSE decoding table from a slice of normalized probabilities (`-1`, `0`, or `> 0`)
    /// summing to `1 << accuracy_log` (counting `-1` as `1`).
    pub(crate) fn build_from_norm(
        &mut self,
        norm: &[i16],
        accuracy_log: u8,
    ) -> Result<(), ZstdError> {
        if !self.build_from_norm_inner(norm, accuracy_log) {
            return Err(ZstdError::CorruptFse);
        }
        Ok(())
    }

    /// Zero-`ZstdError` RFC 8878 §4.1.1 FSE header decoder and table builder returning
    /// `Some(bytes_consumed)` on success.
    pub(crate) fn decode_header_and_build_opt(
        &mut self,
        src: &[u8],
        max_symbol: usize,
        max_log: u8,
    ) -> Option<usize> {
        if src.is_empty() || max_symbol >= MAX_FSE_SYMBOLS || max_log > MAX_FSE_LOG {
            return None;
        }
        let mut reader = FwdBitReader::new(src)?;

        let acc_raw = reader.peek(4);
        if !reader.consume(4) {
            return None;
        }
        let accuracy_log = acc_raw.truncate_to::<u8>().wrapping_add(5);
        if accuracy_log > max_log {
            return None;
        }

        let mut remaining: i32 = (1i32 << u32::from(accuracy_log)) + 1;
        let mut threshold: i32 = 1i32 << u32::from(accuracy_log);
        let mut nb_bits: u32 = u32::from(accuracy_log) + 1;
        let mut norm = Pow2Array::<i16, MAX_FSE_SYMBOLS>::splat(0);
        let mut symbol: usize = 0;
        let mut previous_is_zero = false;

        while remaining > 1 && symbol <= max_symbol {
            if previous_is_zero {
                // Repeat-zero run: 2 bits at a time; value 3 means add 3 and continue.
                for _ in 0..=MAX_FSE_SYMBOLS {
                    let rep = reader.peek(2);
                    if !reader.consume(2) {
                        return None;
                    }
                    symbol = symbol.saturating_add(rep.widen_to::<usize>());
                    if rep < 3 {
                        break;
                    }
                    if symbol > max_symbol + 1 {
                        return None;
                    }
                }
                if symbol > max_symbol {
                    break;
                }
            }

            if nb_bits == 0 {
                return None;
            }
            let max: i32 = (2 * threshold - 1) - remaining;
            let small_bits = nb_bits - 1;
            let small = reader.peek(small_bits).cast_signed();
            let count: i32 = if small < max {
                if !reader.consume(small_bits) {
                    return None;
                }
                small
            } else {
                let full = reader.peek(nb_bits).cast_signed();
                if !reader.consume(nb_bits) {
                    return None;
                }
                if full >= threshold { full - max } else { full }
            };

            let prob = count - 1;
            if prob == -1 {
                remaining -= 1;
            } else if prob > 0 {
                remaining -= prob;
            }
            if remaining < 1 {
                return None;
            }

            norm.set_masked(symbol, prob.truncate_to::<i16>());
            symbol += 1;
            previous_is_zero = prob == 0;

            while remaining < threshold && nb_bits > 1 {
                nb_bits -= 1;
                threshold >>= 1;
            }
        }

        if remaining != 1 || symbol == 0 || symbol > max_symbol + 1 {
            return None;
        }

        let bytes_read = reader.bytes_consumed();
        if bytes_read > src.len() {
            return None;
        }

        let norm_slice = norm.as_slice().get(..symbol)?;
        if !self.build_from_norm_inner(norm_slice, accuracy_log) {
            return None;
        }
        Some(bytes_read)
    }

    /// Decode an RFC 8878 §4.1.1 FSE distribution header from `src`, build the decoding table
    /// into `self`, and return the number of header bytes consumed.
    pub(crate) fn decode_header_and_build(
        &mut self,
        src: &[u8],
        max_symbol: usize,
        max_log: u8,
    ) -> Result<usize, ZstdError> {
        self.decode_header_and_build_opt(src, max_symbol, max_log).ok_or(ZstdError::CorruptFse)
    }
}

// ---------------------------------------------------------------------------
// O(1) FSE Encoding Table (exact inverse of any FseDecTable with log <= 8)
// ---------------------------------------------------------------------------

const MAX_ENC_LOG: u8 = 8;
const MAX_ENC_TABLE_SIZE: usize = 1 << MAX_ENC_LOG;

/// Normalize symbol frequency counts into RFC 8878 §4.1.1 FSE probabilities summing to
/// `1 << accuracy_log` (counting `-1` as `1`). Returns `Some(max_sym)` on success or `None` if
/// fewer than 2 distinct symbols exist (caller should use `RLE_Mode`) or normalization fails.
pub(crate) fn normalize_fse_counts(
    counts: &[u32],
    total: u32,
    accuracy_log: u8,
    norm: &mut [i16],
) -> Option<usize> {
    if total == 0 || accuracy_log > MAX_ENC_LOG || counts.len() > norm.len() {
        return None;
    }
    norm.fill(0);
    let table_size: i32 = 1i32 << u32::from(accuracy_log);
    let table_size_u64 = u64::try_from(table_size).ok()?;
    let total_u64 = u64::from(total);
    let mut max_sym: usize = 0;
    let mut distinct: usize = 0;
    let mut distributed: i32 = 0;
    let mut largest_sym: usize = 0;
    let mut largest_count: u32 = 0;

    for (sym, &c) in counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        distinct += 1;
        max_sym = sym;
        if c > largest_count {
            largest_count = c;
            largest_sym = sym;
        }
        // Round to nearest probability; tiny probabilities (`< 0.5` slot) use `-1` ("less than 1").
        let scaled = (u64::from(c) * table_size_u64 + (total_u64 >> 1)) / total_u64;
        let p: i16 = if scaled == 0 {
            -1
        } else {
            i16::try_from(scaled).ok()?
        };
        *norm.get_mut(sym)? = p;
        distributed += if p == -1 { 1 } else { i32::from(p) };
    }

    if distinct < 2 || i32::try_from(distinct).ok()? > table_size {
        return None;
    }

    let diff = table_size - distributed;
    if diff > 0 {
        let cur = i32::from(*norm.get(largest_sym)?);
        let eff = if cur == -1 { 1 } else { cur };
        *norm.get_mut(largest_sym)? = i16::try_from(eff + diff).ok()?;
    } else if diff < 0 {
        let mut to_cut = -diff;
        let cur = i32::from(*norm.get(largest_sym)?);
        if cur > to_cut + 1 {
            *norm.get_mut(largest_sym)? = i16::try_from(cur - to_cut).ok()?;
            to_cut = 0;
        }
        while to_cut > 0 {
            let mut best_s: Option<usize> = None;
            let mut best_p: i16 = 1;
            for (sym, &p) in norm.get(..=max_sym)?.iter().enumerate() {
                if p > best_p {
                    best_p = p;
                    best_s = Some(sym);
                }
            }
            let s = best_s?;
            *norm.get_mut(s)? -= 1;
            to_cut -= 1;
        }
    }

    // Verify exact sum and that no single symbol fills the entire table.
    let mut check_sum: i32 = 0;
    for &p in norm.get(..=max_sym)? {
        if p == -1 {
            check_sum += 1;
        } else if p > 0 {
            if i32::from(p) >= table_size {
                return None;
            }
            check_sum += i32::from(p);
        }
    }
    if check_sum != table_size {
        return None;
    }
    Some(max_sym)
}

/// O(1) exact inverse of any [`FseDecTable`] with `accuracy_log <= 8` (used for predefined,
/// custom sequence, RLE, and Huffman-weight FSE compression).
///
/// Uses C `zstd`'s branchless `FSE_symbolCompressionTransform` (`delta_nb_bits |
/// (delta_find_state << 32)` in `sym_tt`) over `+table_size`-biased states in
/// `state_table_biased`.
pub(crate) struct FseEncTable {
    state_table_biased: Pow2Array<u16, MAX_ENC_TABLE_SIZE>,
    sym_tt: Pow2Array<u64, MAX_FSE_SYMBOLS>,
    init_offset: Pow2Array<u8, MAX_FSE_SYMBOLS>,
    table_size: u16,
    accuracy_log: u8,
    present_mask: u64,
}

impl FseEncTable {
    pub(crate) const fn new_empty() -> Self {
        Self {
            state_table_biased: Pow2Array::splat(0),
            sym_tt: Pow2Array::splat(0),
            init_offset: Pow2Array::splat(0),
            table_size: 0,
            accuracy_log: 0,
            present_mask: 0,
        }
    }

    #[inline(always)]
    pub(crate) const fn present_mask(&self) -> u64 {
        self.present_mask
    }

    #[inline]
    pub(crate) fn write_ll_into(&self, dst: &mut blaze_own::FseSeqEncTables) {
        dst.ll_tt.copy_from_slice(self.sym_tt.as_array());
        dst.ll_st.copy_from_slice(self.state_table_biased.as_array());
        dst.ll_table_size = self.table_size;
        dst.ll_log = self.accuracy_log;
    }

    #[inline]
    pub(crate) fn write_of_into(&self, dst: &mut blaze_own::FseSeqEncTables) {
        dst.of_tt.copy_from_slice(self.sym_tt.as_array());
        dst.of_st.copy_from_slice(self.state_table_biased.as_array());
        dst.of_table_size = self.table_size;
        dst.of_log = self.accuracy_log;
    }

    #[inline]
    pub(crate) fn write_ml_into(&self, dst: &mut blaze_own::FseSeqEncTables) {
        dst.ml_tt.copy_from_slice(self.sym_tt.as_array());
        dst.ml_st.copy_from_slice(self.state_table_biased.as_array());
        dst.ml_table_size = self.table_size;
        dst.ml_log = self.accuracy_log;
    }

    /// Build a single-symbol RLE encoder table (`accuracy_log = 0`, 0 bits per transition).
    pub(crate) fn set_rle(&mut self, sym: u8) -> Result<(), ZstdError> {
        let sym_idx = usize::from(sym);
        if sym_idx >= MAX_FSE_SYMBOLS {
            return Err(ZstdError::CorruptFse);
        }
        self.sym_tt.fill(0);
        self.init_offset.fill(0);
        // `table_log = 0`, `table_size = 1`, `count = 1`, `cumul = 0`:
        // `delta_nb_bits = (0 << 16).wrapping_sub(1)`, `delta_find_state = 0u32.wrapping_sub(1)`.
        let delta_nb_bits = 0u32.wrapping_sub(1);
        let delta_find_state = 0u32.wrapping_sub(1);
        self.sym_tt.set_masked(
            sym_idx,
            u64::from(delta_nb_bits) | (u64::from(delta_find_state) << 32),
        );
        self.init_offset.set_masked(sym_idx, 0);
        self.state_table_biased.set_masked(0, 1);
        self.table_size = 1;
        self.accuracy_log = 0;
        self.present_mask = 1u64 << u32::from(sym);
        Ok(())
    }

    #[cfg(test)]
    #[inline(always)]
    pub(crate) const fn accuracy_log(&self) -> u8 {
        self.accuracy_log
    }

    pub(crate) fn build_from_dec(dec: &FseDecTable) -> Result<Self, ZstdError> {
        let mut table = Self::new_empty();
        table.rebuild_from_dec(dec)?;
        Ok(table)
    }

    fn rebuild_from_dec_inner(&mut self, dec: &FseDecTable) -> bool {
        let log = dec.accuracy_log();
        if log > MAX_ENC_LOG {
            return false;
        }
        let log_u32 = u32::from(log);
        let table_size = 1usize << log_u32;
        let table_size_u32 = 1u32 << log_u32;
        let table_size_u16 = table_size.truncate_to::<u16>();

        let mut counts = Pow2Array::<u16, MAX_FSE_SYMBOLS>::splat(0);
        for state in 0..table_size {
            let sym = usize::from(dec.entry_masked(state).symbol);
            if sym >= MAX_FSE_SYMBOLS {
                return false;
            }
            let c = counts.get_masked(sym);
            counts.set_masked(sym, c.wrapping_add(1));
        }

        self.sym_tt.fill(0);
        self.init_offset.fill(0);
        self.present_mask = 0;

        let mut write_pos = Pow2Array::<u16, MAX_FSE_SYMBOLS>::splat(0);
        let mut cursor: usize = 0;
        let mut present_mask: u64 = 0;
        for sym in 0..MAX_FSE_SYMBOLS {
            let count_u16 = counts.get_masked(sym);
            if count_u16 == 0 {
                continue;
            }
            present_mask |= 1u64 << sym;
            let count = u32::from(count_u16);
            let cursor_u32 = cursor.truncate_to::<u32>();

            // C `zstd` `FSE_symbolCompressionTransform`:
            // `delta_nb_bits = (max_bits_out << 16) - min_state_plus`
            // `delta_find_state = cumul - count` (wrapping u32, non-zero `delta_nb_bits` for all present symbols).
            let (max_bits_out, min_state_plus) = if count == 1 {
                (log_u32, table_size_u32)
            } else {
                let hb = 31u32.wrapping_sub(count.wrapping_sub(1).leading_zeros());
                let mbo = log_u32.wrapping_sub(hb);
                (mbo, count << mbo)
            };
            let delta_nb_bits = (max_bits_out << 16).wrapping_sub(min_state_plus);
            let delta_find_state = cursor_u32.wrapping_sub(count);
            self.sym_tt.set_masked(
                sym,
                u64::from(delta_nb_bits) | (u64::from(delta_find_state) << 32),
            );
            self.init_offset.set_masked(sym, cursor.truncate_to::<u8>());
            write_pos.set_masked(sym, cursor.truncate_to::<u16>());

            cursor = cursor.wrapping_add(usize::from(count_u16));
        }

        for state in 0..table_size {
            let sym = usize::from(dec.entry_masked(state).symbol);
            let idx = usize::from(write_pos.get_masked(sym));
            write_pos.set_masked(sym, idx.wrapping_add(1).truncate_to::<u16>());
            self.state_table_biased
                .set_masked(idx, state.wrapping_add(table_size).truncate_to::<u16>());
        }

        self.table_size = table_size_u16;
        self.accuracy_log = log;
        self.present_mask = present_mask;
        true
    }

    pub(crate) fn rebuild_from_dec(&mut self, dec: &FseDecTable) -> Result<(), ZstdError> {
        if !self.rebuild_from_dec_inner(dec) {
            return Err(ZstdError::CorruptFse);
        }
        Ok(())
    }

    fn rebuild_from_norm_inner(&mut self, norm: &[i16], accuracy_log: u8) -> bool {
        if accuracy_log > MAX_ENC_LOG || norm.len() > MAX_FSE_SYMBOLS {
            return false;
        }
        let log_u32 = u32::from(accuracy_log);
        let table_size = 1usize << log_u32;
        let table_size_u32 = 1u32 << log_u32;
        let table_size_u16 = table_size.truncate_to::<u16>();
        let table_mask = table_size - 1;
        let step = (table_size >> 1) + (table_size >> 3) + 3;

        self.sym_tt.fill(0);
        self.init_offset.fill(0);
        self.present_mask = 0;

        let mut table_sym = Pow2Array::<u8, MAX_ENC_TABLE_SIZE>::splat(0);
        let mut write_pos = Pow2Array::<u16, MAX_FSE_SYMBOLS>::splat(0);
        let mut high_threshold = table_size;
        let mut cursor: usize = 0;
        let mut present_mask: u64 = 0;

        // Pass 1: validate normalized probabilities, compute per-symbol transform entries and
        // cumulative write cursors, and place `-1` symbols at the top of `table_sym` descending.
        for (sym, &prob) in norm.iter().enumerate() {
            if prob == 0 {
                continue;
            }
            if prob < -1 {
                return false;
            }
            let count: u32 = if prob == -1 {
                if high_threshold == 0 {
                    return false;
                }
                high_threshold = high_threshold.wrapping_sub(1);
                table_sym.set_masked(high_threshold, sym.truncate_to::<u8>());
                1
            } else {
                let Ok(c) = u32::try_from(prob) else {
                    return false;
                };
                if c > table_size_u32 {
                    return false;
                }
                c
            };

            let next_cursor = cursor.wrapping_add(count.widen_to::<usize>());
            if next_cursor > table_size {
                return false;
            }

            present_mask |= 1u64 << sym;
            let cursor_u32 = cursor.truncate_to::<u32>();
            let (max_bits_out, min_state_plus) = if count == 1 {
                (log_u32, table_size_u32)
            } else {
                let hb = 31u32.wrapping_sub(count.wrapping_sub(1).leading_zeros());
                let mbo = log_u32.wrapping_sub(hb);
                (mbo, count << mbo)
            };
            let delta_nb_bits = (max_bits_out << 16).wrapping_sub(min_state_plus);
            let delta_find_state = cursor_u32.wrapping_sub(count);
            self.sym_tt.set_masked(
                sym,
                u64::from(delta_nb_bits) | (u64::from(delta_find_state) << 32),
            );
            self.init_offset.set_masked(sym, cursor.truncate_to::<u8>());
            write_pos.set_masked(sym, cursor.truncate_to::<u16>());
            cursor = next_cursor;
        }

        if cursor != table_size {
            return false;
        }
        if high_threshold > 1 && (step & 1) == 0 {
            return false;
        }

        // Pass 2: spread positive-probability symbols across `0..high_threshold` in exact
        // RFC 8878 §4.1.1 order (matching `FseDecTable::build_from_norm_inner`).
        let mut pos: usize = 0;
        for (sym, &prob) in norm.iter().enumerate() {
            if prob <= 0 {
                continue;
            }
            let count = usize::from(prob.cast_unsigned());
            let sym_u8 = sym.truncate_to::<u8>();
            for _ in 0..count {
                table_sym.set_masked(pos, sym_u8);
                pos = pos.wrapping_add(step) & table_mask;
                while pos >= high_threshold {
                    pos = pos.wrapping_add(step) & table_mask;
                }
            }
        }
        if pos != 0 {
            return false;
        }

        // Pass 3: scatter `+table_size`-biased state indices into per-symbol slots.
        for state in 0..table_size {
            let sym = usize::from(table_sym.get_masked(state));
            let idx = usize::from(write_pos.get_masked(sym));
            write_pos.set_masked(sym, idx.wrapping_add(1).truncate_to::<u16>());
            self.state_table_biased
                .set_masked(idx, state.wrapping_add(table_size).truncate_to::<u16>());
        }

        self.table_size = table_size_u16;
        self.accuracy_log = accuracy_log;
        self.present_mask = present_mask;
        true
    }

    /// Build the FSE encoding table directly from normalized probabilities (`-1`, `0`, or `> 0`)
    /// summing to `1 << accuracy_log` without constructing an intermediate [`FseDecTable`].
    pub(crate) fn rebuild_from_norm(
        &mut self,
        norm: &[i16],
        accuracy_log: u8,
    ) -> Result<(), ZstdError> {
        if !self.rebuild_from_norm_inner(norm, accuracy_log) {
            return Err(ZstdError::CorruptFse);
        }
        Ok(())
    }

    /// Choose a valid unbiased decoder state (`0..table_size`) for the last symbol `sym`.
    #[cfg(test)]
    #[inline(always)]
    pub(crate) fn init_state(&self, sym: u8) -> Option<u8> {
        let s = usize::from(sym);
        if s >= MAX_FSE_SYMBOLS || self.sym_tt.get_masked(s) == 0 {
            return None;
        }
        let off = usize::from(self.init_offset.get_masked(s));
        let biased = self.state_table_biased.get_masked(off);
        Some(biased.wrapping_sub(self.table_size).truncate_to::<u8>())
    }

    /// Choose a valid `+table_size`-biased state (`[table_size, 2 * table_size - 1]`) for `sym`.
    #[inline(always)]
    pub(crate) fn init_state_biased(&self, sym: u8) -> Option<u32> {
        let s = usize::from(sym);
        if s >= MAX_FSE_SYMBOLS || self.sym_tt.get_masked(s) == 0 {
            return None;
        }
        let off = usize::from(self.init_offset.get_masked(s));
        Some(u32::from(self.state_table_biased.get_masked(off)))
    }

    /// Remove the `+table_size` bias from a biased FSE state before writing the final `accuracy_log`
    /// state bits to the reverse bitstream.
    #[inline(always)]
    pub(crate) fn finish_state(&self, state: u32) -> u32 {
        state.wrapping_sub(u32::from(self.table_size))
    }

    /// Branchless C `zstd` `delta_nb_bits` + `delta_find_state` inverse transition on a biased
    /// state (`state` in `[table_size, 2 * table_size - 1]`). Returns `(next_state_biased,
    /// bits_val, num_bits)` with zero `cmp`/`cset` and zero `table_size` additions.
    #[inline(always)]
    pub(crate) fn encode_step_biased(&self, sym: u8, state: u32) -> (u32, u32, u32) {
        let packed = self.sym_tt.get_masked(usize::from(sym));
        let delta_nb_bits = low_u32(packed);
        let delta_find_state = (packed >> 32).truncate_to::<u32>();
        let nb_bits = state.wrapping_add(delta_nb_bits) >> 16;
        let bits_val = state & ((1u32 << nb_bits).wrapping_sub(1));
        let idx = (state >> nb_bits).wrapping_add(delta_find_state);
        let next_state = u32::from(self.state_table_biased.get_masked(idx.widen_to::<usize>()));
        (next_state, bits_val, nb_bits)
    }

    /// Branchless O(1) inverse transition when caller guarantees `sym` is present in this table
    /// (zero `Option`, zero `checked_sub`, masked `& 63` index).
    #[cfg(test)]
    #[inline(always)]
    pub(crate) fn encode_step_fast(&self, sym: u8, target_state: u8) -> (u8, u32, u32) {
        let ts = u32::from(self.table_size);
        let (next_biased, bits, nb) =
            self.encode_step_biased(sym, u32::from(target_state).wrapping_add(ts));
        (next_biased.wrapping_sub(ts).truncate_to::<u8>(), bits, nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_norm_matches_dec(norm: &[i16], accuracy_log: u8) {
        let mut dec = FseDecTable::new_empty();
        assert!(dec.build_from_norm(norm, accuracy_log).is_ok());
        let mut via_dec = FseEncTable::new_empty();
        assert!(via_dec.rebuild_from_dec(&dec).is_ok());

        let mut direct = FseEncTable::new_empty();
        assert!(direct.rebuild_from_norm(norm, accuracy_log).is_ok());

        assert_eq!(direct.table_size, via_dec.table_size);
        assert_eq!(direct.accuracy_log, via_dec.accuracy_log);
        assert_eq!(direct.present_mask(), via_dec.present_mask());
        assert_eq!(direct.sym_tt.as_slice(), via_dec.sym_tt.as_slice());
        assert_eq!(direct.init_offset.as_slice(), via_dec.init_offset.as_slice());
        assert_eq!(
            direct.state_table_biased.as_slice(),
            via_dec.state_table_biased.as_slice()
        );
    }

    #[test]
    fn rebuild_from_norm_matches_rebuild_from_dec_on_defaults_and_custom() {
        assert_norm_matches_dec(&LL_DEFAULT_NORM, LL_DEFAULT_LOG);
        assert_norm_matches_dec(&OF_DEFAULT_NORM, OF_DEFAULT_LOG);
        assert_norm_matches_dec(&ML_DEFAULT_NORM, ML_DEFAULT_LOG);

        let counts: [u32; 16] = [500, 0, 120, 45, 1, 0, 300, 12, 2, 0, 0, 80, 1, 9, 15, 3];
        let total: u32 = counts.iter().copied().sum();
        for log in [5u8, 6, 7, 8] {
            let mut norm = [0i16; 16];
            let max_sym = normalize_fse_counts(&counts, total, log, &mut norm);
            assert!(max_sym.is_some());
            if let Some(m) = max_sym
                && let Some(slice) = norm.get(..=m)
            {
                assert_norm_matches_dec(slice, log);
            }
        }
    }
}
