//! RFC 8878 §4.1 Finite State Entropy (FSE) tables, sequence code tables, and bitstreams.

use core::alloc::Allocator;

use blaze_own::{AllocFailure, Vec};

use crate::zstd::ZstdError;

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

/// RFC 8878 §3.1.1.3.2.1.1 Literals_Length baselines for codes 0..=35.
const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1_024, 2_048, 4_096, 8_192, 16_384, 32_768, 65_536,
];

/// RFC 8878 §3.1.1.3.2.1.1 Literals_Length extra bit counts for codes 0..=35.
const LL_BITS: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];

/// Direct LUT mapping `lit_len` (`0..64`) to its Literals_Length code.
const LL_CODE_LUT: [u8; 64] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 16, 17, 17, 18, 18, 19, 19, 20, 20,
    20, 20, 21, 21, 21, 21, 22, 22, 22, 22, 22, 22, 22, 22, 23, 23, 23, 23, 23, 23, 23, 23, 24, 24,
    24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24,
];

/// RFC 8878 §3.1.1.3.2.1.1 Match_Length baselines for codes 0..=52.
const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1_027,
    2_051, 4_099, 8_195, 16_387, 32_771, 65_539,
];

/// RFC 8878 §3.1.1.3.2.1.1 Match_Length extra bit counts for codes 0..=52.
const ML_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// Direct LUT mapping `match_len - 3` (`0..128`) to its Match_Length code.
const ML_CODE_LUT: [u8; 128] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31, 32, 32, 33, 33, 34, 34, 35, 35, 36, 36, 36, 36, 37, 37, 37, 37, 38, 38,
    38, 38, 38, 38, 38, 38, 39, 39, 39, 39, 39, 39, 39, 39, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40,
    40, 40, 40, 40, 40, 40, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 42, 42,
    42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42,
    42, 42, 42, 42, 42, 42,
];

/// Encode a literal length (`0..=131_071`) into `(ll_code, extra_val, extra_bits)` in O(1).
#[inline(always)]
pub(crate) fn encode_ll(lit_len: usize) -> Option<(u8, u32, u32)> {
    if lit_len < 64 {
        let code = *LL_CODE_LUT.get(lit_len)?;
        let code_idx = usize::from(code);
        let base = *LL_BASE.get(code_idx)?;
        let bits = u32::from(*LL_BITS.get(code_idx)?);
        let len_u32 = u32::try_from(lit_len).ok()?;
        return Some((code, len_u32 - base, bits));
    }
    let len_u32 = u32::try_from(lit_len).ok()?;
    if len_u32 < 128 {
        return Some((25, len_u32 - 64, 6));
    }
    if len_u32 < 131_072 {
        let bits = 31u32 - len_u32.leading_zeros();
        let code = u8::try_from(bits + 19).ok()?;
        let base = 1u32 << bits;
        return Some((code, len_u32 - base, bits));
    }
    None
}

/// Encode a match length (`3..=131_074`) into `(ml_code, extra_val, extra_bits)` in O(1).
#[inline(always)]
pub(crate) fn encode_ml(match_len: usize) -> Option<(u8, u32, u32)> {
    let m = match_len.checked_sub(3)?;
    if m < 128 {
        let code = *ML_CODE_LUT.get(m)?;
        let code_idx = usize::from(code);
        let base = *ML_BASE.get(code_idx)?;
        let bits = u32::from(*ML_BITS.get(code_idx)?);
        let len_u32 = u32::try_from(match_len).ok()?;
        return Some((code, len_u32 - base, bits));
    }
    let m_u32 = u32::try_from(m).ok()?;
    if m_u32 < 256 {
        return Some((43, m_u32 - 128, 7));
    }
    if m_u32 < 131_072 {
        let bits = 31u32 - m_u32.leading_zeros();
        let code = u8::try_from(bits + 36).ok()?;
        let base = 1u32 << bits;
        return Some((code, m_u32 - base, bits));
    }
    None
}

/// Encode an `Offset_Value` (`>= 1`) into `(of_code, extra_val, extra_bits)`.
#[inline(always)]
pub(crate) fn encode_of(offset_value: u32) -> Option<(u8, u32, u32)> {
    if offset_value == 0 {
        return None;
    }
    let bits = 31u32 - offset_value.leading_zeros();
    if bits > 28 {
        return None;
    }
    let code = u8::try_from(bits).ok()?;
    let extra = offset_value - (1u32 << bits);
    Some((code, extra, bits))
}

// ---------------------------------------------------------------------------
// Reverse bitstream reader (RFC 8878 §4.1 / §4.2, bulk 8-byte reload) and writer
// ---------------------------------------------------------------------------

/// Reads a Zstandard reverse bitstream from MSB (just below the trailing sentinel `1` bit) down to
/// bit 0 of byte 0 using C-zstd's bulk 64-bit `container` + `bits_consumed` window.
pub(crate) struct RevBitReader<'a> {
    bytes: &'a [u8],
    byte_pos: usize,
    container: u64,
    bits_consumed: u32,
}

impl<'a> RevBitReader<'a> {
    /// Open a reverse bitstream, stripping the high padding zeros and sentinel `1` bit in the
    /// final byte. Returns `None` if `bytes` is empty or its final byte is `0`.
    #[inline]
    pub(crate) fn new(bytes: &'a [u8]) -> Option<Self> {
        let &last = bytes.last()?;
        if last == 0 {
            return None;
        }
        let leading = last.leading_zeros();
        let n = bytes.len();
        if n >= 8 {
            let start = n - 8;
            let chunk = bytes.get(start..n)?;
            let arr = <[u8; 8]>::try_from(chunk).ok()?;
            Some(Self {
                bytes,
                byte_pos: start,
                container: u64::from_le_bytes(arr),
                bits_consumed: leading + 1,
            })
        } else {
            let mut buf = [0u8; 8];
            if let Some(dst) = buf.get_mut(..n) {
                dst.copy_from_slice(bytes);
            }
            let unused_high = u32::try_from(8 - n).ok()?;
            Some(Self {
                bytes,
                byte_pos: 0,
                container: u64::from_le_bytes(buf),
                bits_consumed: unused_high * 8 + leading + 1,
            })
        }
    }

    /// Bulk-refill `container` with up to 8 bytes from `bytes` in a single 64-bit load.
    /// After `reload()`, whenever `byte_pos > 0`, `bits_consumed <= 7` (at least 57 bits live).
    #[inline(always)]
    pub(crate) fn reload(&mut self) {
        if self.byte_pos == 0 {
            return;
        }
        let [lo, ..] = self.bits_consumed.to_le_bytes();
        let bytes_consumed = usize::from(lo >> 3);
        if self.byte_pos >= bytes_consumed {
            self.byte_pos -= bytes_consumed;
            self.bits_consumed &= 7;
        } else {
            let step = u32::try_from(self.byte_pos).unwrap_or(0);
            self.bits_consumed -= step * 8;
            self.byte_pos = 0;
        }
        if let Some(chunk) = self.bytes.get(self.byte_pos..self.byte_pos + 8)
            && let Ok(arr) = <[u8; 8]>::try_from(chunk)
        {
            self.container = u64::from_le_bytes(arr);
        }
    }

    /// Current unread byte index (`> 0` guarantees `reload()` leaves `bits_consumed <= 7`).
    #[inline(always)]
    pub(crate) const fn byte_pos(&self) -> usize {
        self.byte_pos
    }

    /// Number of top bits already consumed from `container` (`0..=64`).
    #[inline(always)]
    pub(crate) const fn bits_consumed(&self) -> u32 {
        self.bits_consumed
    }

    /// Returns `true` when at least 44 bits remain in `container` without another reload
    /// (enough for 4 consecutive 11-bit Huffman lookups).
    #[inline(always)]
    pub(crate) const fn has_44_bits(&self) -> bool {
        self.bits_consumed <= 20
    }

    /// Branchless peek of `64 - shift_r` bits (`1..=16`). Caller MUST ensure
    /// `self.bits_consumed < 64` (e.g. `has_44_bits()` before a 4-symbol unroll).
    #[inline(always)]
    pub(crate) fn peek_fast(&self, shift_r: u32) -> usize {
        let shifted = (self.container << self.bits_consumed) >> shift_r;
        let [b0, b1, ..] = shifted.to_le_bytes();
        usize::from(u16::from_le_bytes([b0, b1]))
    }

    /// Branchless advance of `n` bits after a verified fast peek.
    #[inline(always)]
    pub(crate) const fn advance_fast(&mut self, n: u32) {
        self.bits_consumed += n;
    }

    /// Branchless read of `n` bits (`0..=32`). Caller MUST ensure `self.bits_consumed < 64`
    /// and `self.bits_consumed + n <= 64`.
    #[inline(always)]
    pub(crate) fn read_bits_branchless(&mut self, n: u32) -> u32 {
        let shifted = ((self.container << self.bits_consumed) >> 1) >> (63 - n);
        self.bits_consumed += n;
        let [b0, b1, b2, b3, ..] = shifted.to_le_bytes();
        u32::from_le_bytes([b0, b1, b2, b3])
    }

    /// Read exactly `n` bits (`0..=32`) from the stream, reloading if needed.
    #[inline(always)]
    pub(crate) fn read_bits(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if n > 32 {
            return None;
        }
        if self.bits_consumed + n > 64 {
            self.reload();
        }
        let next = self.bits_consumed + n;
        if next > 64 {
            return None;
        }
        let shifted = (self.container << self.bits_consumed) >> (64 - n);
        self.bits_consumed = next;
        let [b0, b1, b2, b3, ..] = shifted.to_le_bytes();
        Some(u32::from_le_bytes([b0, b1, b2, b3]))
    }

    /// Peek `n` bits (`1..=11`) for Huffman lookup, right-padding with zeros when fewer than `n`
    /// bits remain at the tail of the stream.
    #[inline(always)]
    pub(crate) fn peek_bits_padded(&mut self, n: u32) -> u32 {
        if self.bits_consumed + n > 64 && self.byte_pos > 0 {
            self.reload();
        }
        if self.bits_consumed >= 64 {
            return 0;
        }
        let shifted = (self.container << self.bits_consumed) >> (64 - n);
        let [b0, b1, ..] = shifted.to_le_bytes();
        u32::from(u16::from_le_bytes([b0, b1]))
    }

    /// Consume `n` bits (`0..=32`), returning `false` if fewer than `n` bits remain.
    #[inline(always)]
    pub(crate) fn consume_bits(&mut self, n: u32) -> bool {
        if n == 0 {
            return true;
        }
        if self.bits_consumed + n > 64 {
            self.reload();
        }
        let next = self.bits_consumed + n;
        if next > 64 {
            return false;
        }
        self.bits_consumed = next;
        true
    }

    /// Returns `true` when every bit in the reverse bitstream has been consumed.
    #[inline(always)]
    pub(crate) const fn is_empty(&self) -> bool {
        self.byte_pos == 0 && self.bits_consumed == 64
    }
}

/// Writes bits LSB-first in bulk 4-byte flushes and appends the RFC 8878 terminating `1` sentinel
/// bit so that [`RevBitReader`] pops fields in the exact reverse order they were written.
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
            let [b0, b1, b2, b3, ..] = self.buf.to_le_bytes();
            self.out.try_extend_from_slice(&[b0, b1, b2, b3])?;
            self.buf >>= 32;
            self.bits -= 32;
        }
        Ok(())
    }

    /// Flush into an existing `Vec<u8, A>` (appending the finished reverse bitstream).
    pub(crate) fn finish_into(mut self, dst: &mut Vec<u8, A>) -> Result<(), AllocFailure> {
        while self.bits >= 8 {
            let [b0, ..] = self.buf.to_le_bytes();
            self.out.try_push(b0).map_err(|r| r.why)?;
            self.buf >>= 8;
            self.bits -= 8;
        }
        self.buf |= 1u64 << self.bits;
        let [b0, ..] = self.buf.to_le_bytes();
        self.out.try_push(b0).map_err(|r| r.why)?;
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
        let bit_off = u32::try_from(self.bit_pos % 8).unwrap_or(0);
        let b0 = self.bytes.get(byte_idx).copied().unwrap_or(0);
        let b1 = self.bytes.get(byte_idx + 1).copied().unwrap_or(0);
        let b2 = self.bytes.get(byte_idx + 2).copied().unwrap_or(0);
        let b3 = self.bytes.get(byte_idx + 3).copied().unwrap_or(0);
        let word = u32::from_le_bytes([b0, b1, b2, b3]) >> bit_off;
        word & ((1u32 << n) - 1)
    }

    #[inline]
    fn consume(&mut self, n: u32) -> Result<(), ZstdError> {
        let adv = usize::try_from(n).map_err(|_| ZstdError::CorruptFse)?;
        let next = self.bit_pos.checked_add(adv).ok_or(ZstdError::CorruptFse)?;
        if next > self.total_bits {
            return Err(ZstdError::CorruptFse);
        }
        self.bit_pos = next;
        Ok(())
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
            let [b0, ..] = self.buf.to_le_bytes();
            self.out.try_push(b0).map_err(|r| r.why)?;
            self.buf >>= 8;
            self.bits -= 8;
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<(), AllocFailure> {
        if self.bits > 0 {
            let [b0, ..] = self.buf.to_le_bytes();
            self.out.try_push(b0).map_err(|r| r.why)?;
        }
        Ok(())
    }
}

/// Write an RFC 8878 §4.1.1 FSE distribution header for `norm` (`accuracy_log` in `5..=9`) into
/// `out`. Exact inverse of [`FseDecTable::decode_header_and_build`].
pub(crate) fn write_fse_header<A: Allocator>(
    norm: &[i16],
    accuracy_log: u8,
    out: &mut Vec<u8, A>,
) -> Result<(), ZstdError> {
    if !(5..=MAX_FSE_LOG).contains(&accuracy_log) || norm.len() > MAX_FSE_SYMBOLS {
        return Err(ZstdError::CorruptFse);
    }
    let mut bw = FwdBitWriter::new(out);
    bw.write(u32::from(accuracy_log - 5), 4)?;

    let mut remaining: i32 = (1i32 << u32::from(accuracy_log)) + 1;
    let mut threshold: i32 = 1i32 << u32::from(accuracy_log);
    let mut nb_bits: u32 = u32::from(accuracy_log) + 1;
    let mut idx: usize = 0;

    while remaining > 1 && idx < norm.len() {
        let prob = i32::from(*norm.get(idx).ok_or(ZstdError::CorruptFse)?);
        let max: i32 = (2 * threshold - 1) - remaining;
        let count: i32 = prob + 1;
        let small_bits = nb_bits.checked_sub(1).ok_or(ZstdError::CorruptFse)?;

        if count < max {
            let val = u32::try_from(count).map_err(|_| ZstdError::CorruptFse)?;
            bw.write(val, small_bits)?;
        } else if count < threshold {
            let val = u32::try_from(count).map_err(|_| ZstdError::CorruptFse)?;
            bw.write(val, nb_bits)?;
        } else {
            let val = u32::try_from(count + max).map_err(|_| ZstdError::CorruptFse)?;
            bw.write(val, nb_bits)?;
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
            let rem_u32 = u32::try_from(zeros).map_err(|_| ZstdError::CorruptFse)?;
            bw.write(rem_u32, 2)?;
        }
    }

    if remaining != 1 {
        return Err(ZstdError::CorruptFse);
    }
    bw.finish()?;
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

/// Pre-resolved sequence FSE decoding entry (exact C zstd `ZSTD_seqSymbol` layout): stores
/// the resolved baseline value (`LL_BASE`, `ML_BASE`, or `1 << of_code`) and `extra_bits`
/// directly in the FSE table so the inner sequence loop does zero secondary LUT lookups.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SeqDecEntry {
    pub(crate) base: u32,
    pub(crate) extra_bits: u8,
    pub(crate) num_bits: u8,
    pub(crate) baseline: u16,
}

impl SeqDecEntry {
    pub(crate) const ZERO: Self = Self { base: 0, extra_bits: 0, num_bits: 0, baseline: 0 };
}

/// Which sequence field a [`SeqDecTable`] resolves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeqKind {
    Ll,
    Of,
    Ml,
}

/// Fixed-capacity pre-resolved sequence FSE decoding table (512 entries = 4 KiB in L1D).
pub(crate) struct SeqDecTable {
    entries: [SeqDecEntry; MAX_FSE_TABLE_SIZE],
    accuracy_log: u8,
}

impl SeqDecTable {
    pub(crate) const fn new_empty() -> Self {
        Self { entries: [SeqDecEntry::ZERO; MAX_FSE_TABLE_SIZE], accuracy_log: 0 }
    }

    #[inline(always)]
    pub(crate) const fn accuracy_log(&self) -> u8 {
        self.accuracy_log
    }

    #[inline(always)]
    pub(crate) fn entry_fast(&self, state: usize) -> SeqDecEntry {
        self.entries
            .get(state & (MAX_FSE_TABLE_SIZE - 1))
            .copied()
            .unwrap_or(SeqDecEntry::ZERO)
    }

    /// Compile an [`FseDecTable`] into pre-resolved [`SeqDecEntry`] values, validating every
    /// symbol code once up front.
    pub(crate) fn compile_from(&mut self, dec: &FseDecTable, kind: SeqKind) -> Result<(), ZstdError> {
        let log = dec.accuracy_log();
        if log > MAX_FSE_LOG {
            return Err(ZstdError::CorruptFse);
        }
        let table_size = 1usize << u32::from(log);
        for state in 0..table_size {
            let e = dec.entry(state).ok_or(ZstdError::CorruptFse)?;
            let (base, extra_bits) = match kind {
                SeqKind::Ll => {
                    let idx = usize::from(e.symbol);
                    let b = *LL_BASE.get(idx).ok_or(ZstdError::CorruptSequences)?;
                    let eb = *LL_BITS.get(idx).ok_or(ZstdError::CorruptSequences)?;
                    (b, eb)
                }
                SeqKind::Of => {
                    if usize::from(e.symbol) > OF_MAX_CODE {
                        return Err(ZstdError::CorruptSequences);
                    }
                    (1u32 << u32::from(e.symbol), e.symbol)
                }
                SeqKind::Ml => {
                    let idx = usize::from(e.symbol);
                    let b = *ML_BASE.get(idx).ok_or(ZstdError::CorruptSequences)?;
                    let eb = *ML_BITS.get(idx).ok_or(ZstdError::CorruptSequences)?;
                    (b, eb)
                }
            };
            if let Some(slot) = self.entries.get_mut(state) {
                *slot = SeqDecEntry {
                    base,
                    extra_bits,
                    num_bits: e.num_bits,
                    baseline: e.baseline,
                };
            }
        }
        self.accuracy_log = log;
        Ok(())
    }
}

/// Fixed-capacity FSE decoding table (up to `1 << 9 = 512` states, zero heap allocation).
pub(crate) struct FseDecTable {
    entries: [FseDecEntry; MAX_FSE_TABLE_SIZE],
    accuracy_log: u8,
}

impl FseDecTable {
    pub(crate) const fn new_empty() -> Self {
        Self { entries: [FseDecEntry::ZERO; MAX_FSE_TABLE_SIZE], accuracy_log: 0 }
    }

    #[inline(always)]
    pub(crate) const fn accuracy_log(&self) -> u8 {
        self.accuracy_log
    }

    #[inline(always)]
    pub(crate) fn entry(&self, state: usize) -> Option<FseDecEntry> {
        let size = 1usize << u32::from(self.accuracy_log);
        if state >= size {
            return None;
        }
        self.entries.get(state).copied()
    }

    /// Configure this table as an RLE-mode single-symbol table (`accuracy_log = 0`).
    pub(crate) fn set_rle(&mut self, symbol: u8) {
        self.accuracy_log = 0;
        if let Some(first) = self.entries.first_mut() {
            *first = FseDecEntry { symbol, num_bits: 0, baseline: 0 };
        }
    }

    /// Build the FSE decoding table from a slice of normalized probabilities (`-1`, `0`, or `> 0`)
    /// summing to `1 << accuracy_log` (counting `-1` as `1`).
    pub(crate) fn build_from_norm(
        &mut self,
        norm: &[i16],
        accuracy_log: u8,
    ) -> Result<(), ZstdError> {
        if accuracy_log > MAX_FSE_LOG || norm.len() > MAX_FSE_SYMBOLS {
            return Err(ZstdError::CorruptFse);
        }
        let table_size = 1usize << u32::from(accuracy_log);
        let table_mask = table_size - 1;
        let step = (table_size >> 1) + (table_size >> 3) + 3;

        let mut high_threshold = table_size;
        let mut symbol_next = [0u16; MAX_FSE_SYMBOLS];
        let mut total: usize = 0;

        // First pass: place "less than 1" (-1) symbols at the top of the table, descending.
        for (sym_idx, &prob) in norm.iter().enumerate() {
            let sym_u8 = u8::try_from(sym_idx).map_err(|_| ZstdError::CorruptFse)?;
            if prob == -1 {
                high_threshold = high_threshold.checked_sub(1).ok_or(ZstdError::CorruptFse)?;
                let slot = self.entries.get_mut(high_threshold).ok_or(ZstdError::CorruptFse)?;
                slot.symbol = sym_u8;
                let next_slot = symbol_next.get_mut(sym_idx).ok_or(ZstdError::CorruptFse)?;
                *next_slot = 1;
                total = total.checked_add(1).ok_or(ZstdError::CorruptFse)?;
            } else if prob > 0 {
                let count = usize::try_from(prob).map_err(|_| ZstdError::CorruptFse)?;
                let count_u16 = u16::try_from(count).map_err(|_| ZstdError::CorruptFse)?;
                let next_slot = symbol_next.get_mut(sym_idx).ok_or(ZstdError::CorruptFse)?;
                *next_slot = count_u16;
                total = total.checked_add(count).ok_or(ZstdError::CorruptFse)?;
            } else if prob < -1 {
                return Err(ZstdError::CorruptFse);
            }
        }

        if total != table_size {
            return Err(ZstdError::CorruptFse);
        }

        // Second pass: spread positive-probability symbols across `0..high_threshold`.
        let mut pos: usize = 0;
        for (sym_idx, &prob) in norm.iter().enumerate() {
            if prob <= 0 {
                continue;
            }
            let count = usize::try_from(prob).map_err(|_| ZstdError::CorruptFse)?;
            let sym_u8 = u8::try_from(sym_idx).map_err(|_| ZstdError::CorruptFse)?;
            for _ in 0..count {
                let slot = self.entries.get_mut(pos).ok_or(ZstdError::CorruptFse)?;
                slot.symbol = sym_u8;
                let mut found = false;
                for _ in 0..table_size {
                    pos = (pos + step) & table_mask;
                    if pos < high_threshold {
                        found = true;
                        break;
                    }
                }
                if !found {
                    return Err(ZstdError::CorruptFse);
                }
            }
        }

        if pos != 0 {
            return Err(ZstdError::CorruptFse);
        }

        // Third pass: compute `num_bits` and `baseline` for every state `0..table_size`.
        for state in 0..table_size {
            let slot = self.entries.get_mut(state).ok_or(ZstdError::CorruptFse)?;
            let sym_idx = usize::from(slot.symbol);
            let next_ref = symbol_next.get_mut(sym_idx).ok_or(ZstdError::CorruptFse)?;
            let next_state = *next_ref;
            if next_state == 0 {
                return Err(ZstdError::CorruptFse);
            }
            *next_ref = next_state.checked_add(1).ok_or(ZstdError::CorruptFse)?;

            let high_bit =
                15u32.checked_sub(next_state.leading_zeros()).ok_or(ZstdError::CorruptFse)?;
            let nb = u32::from(accuracy_log).checked_sub(high_bit).ok_or(ZstdError::CorruptFse)?;
            let shifted = usize::from(next_state).checked_shl(nb).ok_or(ZstdError::CorruptFse)?;
            let base = shifted.checked_sub(table_size).ok_or(ZstdError::CorruptFse)?;

            slot.num_bits = u8::try_from(nb).map_err(|_| ZstdError::CorruptFse)?;
            slot.baseline = u16::try_from(base).map_err(|_| ZstdError::CorruptFse)?;
        }

        self.accuracy_log = accuracy_log;
        Ok(())
    }

    /// Decode an RFC 8878 §4.1.1 FSE distribution header from `src`, build the decoding table
    /// into `self`, and return the number of header bytes consumed.
    pub(crate) fn decode_header_and_build(
        &mut self,
        src: &[u8],
        max_symbol: usize,
        max_log: u8,
    ) -> Result<usize, ZstdError> {
        if src.is_empty() || max_symbol >= MAX_FSE_SYMBOLS || max_log > MAX_FSE_LOG {
            return Err(ZstdError::CorruptFse);
        }
        let mut reader = FwdBitReader::new(src).ok_or(ZstdError::CorruptFse)?;

        let acc_raw = reader.peek(4);
        reader.consume(4)?;
        let accuracy_log = u8::try_from(acc_raw)
            .ok()
            .and_then(|v| v.checked_add(5))
            .ok_or(ZstdError::CorruptFse)?;
        if accuracy_log > max_log {
            return Err(ZstdError::CorruptFse);
        }

        let mut remaining: i32 = (1i32 << u32::from(accuracy_log)) + 1;
        let mut threshold: i32 = 1i32 << u32::from(accuracy_log);
        let mut nb_bits: u32 = u32::from(accuracy_log) + 1;
        let mut norm = [0i16; MAX_FSE_SYMBOLS];
        let mut symbol: usize = 0;
        let mut previous_is_zero = false;

        while remaining > 1 && symbol <= max_symbol {
            if previous_is_zero {
                // Repeat-zero run: 2 bits at a time; value 3 means add 3 and continue.
                for _ in 0..=MAX_FSE_SYMBOLS {
                    let rep = reader.peek(2);
                    reader.consume(2)?;
                    let rep_usize = usize::try_from(rep).map_err(|_| ZstdError::CorruptFse)?;
                    symbol = symbol.saturating_add(rep_usize);
                    if rep < 3 {
                        break;
                    }
                    if symbol > max_symbol + 1 {
                        return Err(ZstdError::CorruptFse);
                    }
                }
                if symbol > max_symbol {
                    break;
                }
            }

            let max: i32 = (2 * threshold - 1) - remaining;
            let small_bits = nb_bits.checked_sub(1).ok_or(ZstdError::CorruptFse)?;
            let small =
                i32::try_from(reader.peek(small_bits)).map_err(|_| ZstdError::CorruptFse)?;
            let count: i32 = if small < max {
                reader.consume(small_bits)?;
                small
            } else {
                let full =
                    i32::try_from(reader.peek(nb_bits)).map_err(|_| ZstdError::CorruptFse)?;
                reader.consume(nb_bits)?;
                if full >= threshold { full - max } else { full }
            };

            let prob = count - 1;
            if prob == -1 {
                remaining -= 1;
            } else if prob > 0 {
                remaining -= prob;
            }
            if remaining < 1 {
                return Err(ZstdError::CorruptFse);
            }

            let slot = norm.get_mut(symbol).ok_or(ZstdError::CorruptFse)?;
            *slot = i16::try_from(prob).map_err(|_| ZstdError::CorruptFse)?;
            symbol += 1;
            previous_is_zero = prob == 0;

            while remaining < threshold && nb_bits > 1 {
                nb_bits -= 1;
                threshold >>= 1;
            }
        }

        if remaining != 1 || symbol == 0 || symbol > max_symbol + 1 {
            return Err(ZstdError::CorruptFse);
        }

        let bytes_read = reader.bytes_consumed();
        if bytes_read > src.len() {
            return Err(ZstdError::CorruptFse);
        }

        let norm_slice = norm.get(..symbol).ok_or(ZstdError::CorruptFse)?;
        self.build_from_norm(norm_slice, accuracy_log)?;
        Ok(bytes_read)
    }
}

// ---------------------------------------------------------------------------
// O(1) FSE Encoding Table (exact inverse of any FseDecTable with log <= 6)
// ---------------------------------------------------------------------------

const MAX_ENC_TABLE_SIZE: usize = 1 << 6;

#[derive(Clone, Copy)]
struct FseSymMeta {
    /// Start index of this symbol's states in `state_table`.
    offset: u8,
    /// Number of states assigned to this symbol (`0` if unused).
    count: u8,
    /// `target < split` uses `max_nb - 1` bits; `target >= split` uses `max_nb` bits.
    split: u8,
    max_nb: u8,
}

impl FseSymMeta {
    const ZERO: Self = Self { offset: 0, count: 0, split: 0, max_nb: 0 };
}

/// O(1) exact inverse of any [`FseDecTable`] with `accuracy_log <= 6` (used for predefined
/// sequence tables and for Huffman-weight FSE compression).
pub(crate) struct FseEncTable {
    state_table: [u8; MAX_ENC_TABLE_SIZE],
    meta: [FseSymMeta; MAX_FSE_SYMBOLS],
    table_size: u16,
}

impl FseEncTable {
    pub(crate) fn build_from_dec(dec: &FseDecTable) -> Result<Self, ZstdError> {
        let log = dec.accuracy_log();
        if log > 6 {
            return Err(ZstdError::CorruptFse);
        }
        let table_size = 1usize << u32::from(log);
        let mut counts = [0u8; MAX_FSE_SYMBOLS];
        for state in 0..table_size {
            let entry = dec.entry(state).ok_or(ZstdError::CorruptFse)?;
            let c = counts.get_mut(usize::from(entry.symbol)).ok_or(ZstdError::CorruptFse)?;
            *c = c.checked_add(1).ok_or(ZstdError::CorruptFse)?;
        }

        let mut meta = [FseSymMeta::ZERO; MAX_FSE_SYMBOLS];
        let mut cursor: usize = 0;
        for sym in 0..MAX_FSE_SYMBOLS {
            let count = *counts.get(sym).ok_or(ZstdError::CorruptFse)?;
            if count == 0 {
                continue;
            }
            let p = u32::from(count);
            let high_bit = 31u32 - p.leading_zeros();
            let max_nb = u32::from(log).checked_sub(high_bit).ok_or(ZstdError::CorruptFse)?;
            let next_pow2 = 1u32 << (high_bit + 1);
            let split_u32 = if max_nb == 0 {
                0
            } else {
                (2u32 * p).saturating_sub(next_pow2) << (max_nb - 1)
            };
            let slot = meta.get_mut(sym).ok_or(ZstdError::CorruptFse)?;
            *slot = FseSymMeta {
                offset: u8::try_from(cursor).map_err(|_| ZstdError::CorruptFse)?,
                count,
                split: u8::try_from(split_u32).map_err(|_| ZstdError::CorruptFse)?,
                max_nb: u8::try_from(max_nb).map_err(|_| ZstdError::CorruptFse)?,
            };
            cursor += usize::from(count);
        }

        let mut write_pos = [0u8; MAX_FSE_SYMBOLS];
        for sym in 0..MAX_FSE_SYMBOLS {
            let dst = write_pos.get_mut(sym).ok_or(ZstdError::CorruptFse)?;
            let m = *meta.get(sym).ok_or(ZstdError::CorruptFse)?;
            *dst = m.offset;
        }

        let mut state_table = [0u8; MAX_ENC_TABLE_SIZE];
        for state in 0..table_size {
            let entry = dec.entry(state).ok_or(ZstdError::CorruptFse)?;
            let sym = usize::from(entry.symbol);
            let pos_ref = write_pos.get_mut(sym).ok_or(ZstdError::CorruptFse)?;
            let idx = usize::from(*pos_ref);
            let slot = state_table.get_mut(idx).ok_or(ZstdError::CorruptFse)?;
            *slot = u8::try_from(state).map_err(|_| ZstdError::CorruptFse)?;
            *pos_ref = pos_ref.checked_add(1).ok_or(ZstdError::CorruptFse)?;
        }

        let table_size_u16 = u16::try_from(table_size).map_err(|_| ZstdError::CorruptFse)?;
        Ok(Self { state_table, meta, table_size: table_size_u16 })
    }

    /// Choose a valid decoder state for the last symbol `sym`.
    #[inline(always)]
    pub(crate) fn init_state(&self, sym: u8) -> Option<u8> {
        let m = *self.meta.get(usize::from(sym))?;
        if m.count == 0 {
            return None;
        }
        self.state_table.get(usize::from(m.offset)).copied()
    }

    /// O(1) branch-minimal inverse transition: find the unique `(prev_state, bits_val, num_bits)`
    /// whose decoder transition from `prev_state` emits `sym` and lands on `target_state`.
    #[inline(always)]
    pub(crate) fn encode_step(&self, sym: u8, target_state: u8) -> Option<(u8, u32, u32)> {
        let m = *self.meta.get(usize::from(sym))?;
        if m.count == 0 {
            return None;
        }
        let nb = if target_state < m.split {
            u32::from(m.max_nb - 1)
        } else {
            u32::from(m.max_nb)
        };
        let target = u32::from(target_state);
        let bits = target & ((1u32 << nb) - 1);
        let u = (target + u32::from(self.table_size)) >> nb;
        let rel = usize::try_from(u.checked_sub(u32::from(m.count))?).ok()?;
        let prev_state = *self.state_table.get(usize::from(m.offset) + rel)?;
        Some((prev_state, bits, nb))
    }
}
