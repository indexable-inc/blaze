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

#[inline]
pub(crate) fn ll_base_and_bits(code: u8) -> Option<(usize, u8)> {
    let idx = usize::from(code);
    let base = *LL_BASE.get(idx)?;
    let bits = *LL_BITS.get(idx)?;
    let base_usize = usize::try_from(base).ok()?;
    Some((base_usize, bits))
}

#[inline]
pub(crate) fn ml_base_and_bits(code: u8) -> Option<(usize, u8)> {
    let idx = usize::from(code);
    let base = *ML_BASE.get(idx)?;
    let bits = *ML_BITS.get(idx)?;
    let base_usize = usize::try_from(base).ok()?;
    Some((base_usize, bits))
}

/// Encode a literal length (`0..=131_071`) into `(ll_code, extra_val, extra_bits)`.
pub(crate) fn encode_ll(lit_len: usize) -> Option<(u8, u32, u32)> {
    let len_u32 = u32::try_from(lit_len).ok()?;
    if len_u32 < 16 {
        let code = u8::try_from(len_u32).ok()?;
        return Some((code, 0, 0));
    }
    for (idx, (&base, &bits)) in LL_BASE.iter().zip(LL_BITS.iter()).enumerate().skip(16) {
        let span = 1u32 << u32::from(bits);
        if len_u32 >= base && len_u32 - base < span {
            let code = u8::try_from(idx).ok()?;
            return Some((code, len_u32 - base, u32::from(bits)));
        }
    }
    None
}

/// Encode a match length (`3..=131_074`) into `(ml_code, extra_val, extra_bits)`.
pub(crate) fn encode_ml(match_len: usize) -> Option<(u8, u32, u32)> {
    let len_u32 = u32::try_from(match_len).ok()?;
    if len_u32 < 3 {
        return None;
    }
    if len_u32 <= 34 {
        let code = u8::try_from(len_u32 - 3).ok()?;
        return Some((code, 0, 0));
    }
    for (idx, (&base, &bits)) in ML_BASE.iter().zip(ML_BITS.iter()).enumerate().skip(32) {
        let span = 1u32 << u32::from(bits);
        if len_u32 >= base && len_u32 - base < span {
            let code = u8::try_from(idx).ok()?;
            return Some((code, len_u32 - base, u32::from(bits)));
        }
    }
    None
}

/// Encode an `Offset_Value` (`>= 1`) into `(of_code, extra_val, extra_bits)`.
#[inline]
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
// Reverse bitstream reader (RFC 8878 §4.1 / §4.2) and writer
// ---------------------------------------------------------------------------

/// Reads a Zstandard reverse bitstream from MSB (just below the trailing sentinel `1` bit) down to
/// bit 0 of byte 0.
pub(crate) struct RevBitReader<'a> {
    bytes: &'a [u8],
    byte_pos: usize,
    container: u64,
    bits_in_buf: u32,
}

impl<'a> RevBitReader<'a> {
    /// Open a reverse bitstream, stripping the high padding zeros and sentinel `1` bit in the
    /// final byte. Returns `None` if `bytes` is empty or its final byte is `0`.
    pub(crate) fn new(bytes: &'a [u8]) -> Option<Self> {
        let &last = bytes.last()?;
        if last == 0 {
            return None;
        }
        let leading = last.leading_zeros();
        let valid_in_last = 7u32.checked_sub(leading)?;
        let mask = (1u64 << valid_in_last) - 1;
        let container = u64::from(last) & mask;
        let mut reader = Self {
            bytes,
            byte_pos: bytes.len().saturating_sub(1),
            container,
            bits_in_buf: valid_in_last,
        };
        reader.refill();
        Some(reader)
    }

    #[inline]
    fn refill(&mut self) {
        while self.bits_in_buf <= 56 && self.byte_pos > 0 {
            self.byte_pos -= 1;
            let b = self.bytes.get(self.byte_pos).copied().unwrap_or(0);
            self.container = (self.container << 8) | u64::from(b);
            self.bits_in_buf += 8;
        }
    }

    /// Read exactly `n` bits (`0..=32`) from the stream, returning `None` if fewer than `n` bits
    /// remain.
    #[inline]
    pub(crate) fn read_bits(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if n > 32 {
            return None;
        }
        if self.bits_in_buf < n {
            self.refill();
        }
        if self.bits_in_buf < n {
            return None;
        }
        let shift = self.bits_in_buf - n;
        let val = self.container >> shift;
        self.bits_in_buf = shift;
        if shift == 0 {
            self.container = 0;
        } else {
            self.container &= (1u64 << shift) - 1;
        }
        u32::try_from(val).ok()
    }

    /// Peek `n` bits (`1..=11`) for Huffman lookup, right-padding with zeros when fewer than `n`
    /// bits remain at the tail of the stream.
    #[inline]
    pub(crate) fn peek_bits_padded(&mut self, n: u32) -> u32 {
        if self.bits_in_buf < n && self.byte_pos > 0 {
            self.refill();
        }
        if self.bits_in_buf >= n {
            u32::try_from(self.container >> (self.bits_in_buf - n)).unwrap_or(0)
        } else {
            u32::try_from(self.container << (n - self.bits_in_buf)).unwrap_or(0)
        }
    }

    /// Consume `n` bits (`0..=32`), returning `false` if fewer than `n` bits remain.
    #[inline]
    pub(crate) fn consume_bits(&mut self, n: u32) -> bool {
        if n == 0 {
            return true;
        }
        if self.bits_in_buf < n {
            self.refill();
        }
        if self.bits_in_buf < n {
            return false;
        }
        let shift = self.bits_in_buf - n;
        self.bits_in_buf = shift;
        if shift == 0 {
            self.container = 0;
        } else {
            self.container &= (1u64 << shift) - 1;
        }
        true
    }

    /// Returns `true` when every bit in the reverse bitstream has been consumed.
    #[inline]
    pub(crate) const fn is_empty(&self) -> bool {
        self.byte_pos == 0 && self.bits_in_buf == 0
    }
}

/// Writes bits LSB-first and appends the RFC 8878 terminating `1` sentinel bit so that
/// [`RevBitReader`] pops fields in the exact reverse order they were written.
pub(crate) struct BitWriter<A: Allocator> {
    out: Vec<u8, A>,
    buf: u64,
    bits: u32,
}

impl<A: Allocator> BitWriter<A> {
    pub(crate) fn try_with_capacity_in(cap: usize, alloc: A) -> Result<Self, AllocFailure> {
        Ok(Self { out: Vec::try_with_capacity_in(cap, alloc)?, buf: 0, bits: 0 })
    }

    #[inline]
    pub(crate) fn write_bits(&mut self, val: u32, n: u32) -> Result<(), AllocFailure> {
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

    /// Write the sentinel `1` bit and flush the final byte.
    pub(crate) fn finish(mut self) -> Result<Vec<u8, A>, AllocFailure> {
        self.buf |= 1u64 << self.bits;
        let [b0, ..] = self.buf.to_le_bytes();
        self.out.try_push(b0).map_err(|r| r.why)?;
        Ok(self.out)
    }
}

// ---------------------------------------------------------------------------
// Forward bitstream reader for FSE table headers (RFC 8878 §4.1.1)
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
    const ZERO: Self = Self { symbol: 0, num_bits: 0, baseline: 0 };
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

    #[inline]
    pub(crate) const fn accuracy_log(&self) -> u8 {
        self.accuracy_log
    }

    #[inline]
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

            let high_bit = 15u32.checked_sub(next_state.leading_zeros()).ok_or(ZstdError::CorruptFse)?;
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
            let small = i32::try_from(reader.peek(small_bits)).map_err(|_| ZstdError::CorruptFse)?;
            let count: i32 = if small < max {
                reader.consume(small_bits)?;
                small
            } else {
                let full = i32::try_from(reader.peek(nb_bits)).map_err(|_| ZstdError::CorruptFse)?;
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
// FSE Encoding Table (exact inverse of Predefined_Mode FseDecTable)
// ---------------------------------------------------------------------------

const MAX_PREDEF_STATES_PER_SYM: usize = 4;

#[derive(Clone, Copy)]
struct EncState {
    state: u8,
    num_bits: u8,
    baseline: u16,
    end: u16,
}

impl EncState {
    const ZERO: Self = Self { state: 0, num_bits: 0, baseline: 0, end: 0 };
}

/// Exact inverse of a predefined [`FseDecTable`] (`accuracy_log <= 6`, at most 4 states per
/// symbol). Every transition produced by [`Self::encode_step`] is guaranteed by construction to
/// invert [`FseDecTable::entry`].
pub(crate) struct FseEncTable {
    by_sym: [[EncState; MAX_PREDEF_STATES_PER_SYM]; MAX_FSE_SYMBOLS],
    sym_len: [u8; MAX_FSE_SYMBOLS],
}

impl FseEncTable {
    pub(crate) fn build_from_dec(dec: &FseDecTable) -> Result<Self, ZstdError> {
        let log = dec.accuracy_log();
        if log > 6 {
            return Err(ZstdError::CorruptFse);
        }
        let table_size = 1usize << u32::from(log);
        let mut table = Self {
            by_sym: [[EncState::ZERO; MAX_PREDEF_STATES_PER_SYM]; MAX_FSE_SYMBOLS],
            sym_len: [0u8; MAX_FSE_SYMBOLS],
        };

        for state in 0..table_size {
            let entry = dec.entry(state).ok_or(ZstdError::CorruptFse)?;
            let sym = usize::from(entry.symbol);
            let len_slot = table.sym_len.get_mut(sym).ok_or(ZstdError::CorruptFse)?;
            let idx = usize::from(*len_slot);
            if idx >= MAX_PREDEF_STATES_PER_SYM {
                return Err(ZstdError::CorruptFse);
            }
            let span = 1u16.checked_shl(u32::from(entry.num_bits)).ok_or(ZstdError::CorruptFse)?;
            let end = entry.baseline.checked_add(span).ok_or(ZstdError::CorruptFse)?;
            let state_u8 = u8::try_from(state).map_err(|_| ZstdError::CorruptFse)?;
            let row = table.by_sym.get_mut(sym).ok_or(ZstdError::CorruptFse)?;
            let dst = row.get_mut(idx).ok_or(ZstdError::CorruptFse)?;
            *dst = EncState { state: state_u8, num_bits: entry.num_bits, baseline: entry.baseline, end };
            *len_slot += 1;
        }

        Ok(table)
    }

    /// Choose a valid decoder state for the last sequence's symbol `sym`.
    #[inline]
    pub(crate) fn init_state(&self, sym: u8) -> Option<u8> {
        let sym_idx = usize::from(sym);
        let count = *self.sym_len.get(sym_idx)?;
        if count == 0 {
            return None;
        }
        let first = self.by_sym.get(sym_idx)?.first()?;
        Some(first.state)
    }

    /// Find the unique `(prev_state, bits_val, num_bits)` whose decoder transition from
    /// `prev_state` emits `sym` and lands on `target_state`.
    #[inline]
    pub(crate) fn encode_step(&self, sym: u8, target_state: u8) -> Option<(u8, u32, u32)> {
        let sym_idx = usize::from(sym);
        let count = usize::from(*self.sym_len.get(sym_idx)?);
        let row = self.by_sym.get(sym_idx)?;
        let active = row.get(..count)?;
        let target = u16::from(target_state);
        for e in active {
            if target >= e.baseline && target < e.end {
                let bits = u32::from(target - e.baseline);
                return Some((e.state, bits, u32::from(e.num_bits)));
            }
        }
        None
    }
}
