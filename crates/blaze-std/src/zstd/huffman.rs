//! RFC 8878 §4.2 Huffman tree description decoder and 1-stream / 4-stream literals decoder.

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::zstd::ZstdError;
use crate::zstd::fse::{FseDecTable, RevBitReader};

const MAX_HUFF_BITS: u8 = 11;
const MAX_HUFF_TABLE_SIZE: usize = 1 << MAX_HUFF_BITS;
const HUFF_FSE_MAX_SYMBOL: usize = 12;
const HUFF_FSE_MAX_LOG: u8 = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct HuffEntry {
    symbol: u8,
    num_bits: u8,
}

impl HuffEntry {
    const ZERO: Self = Self { symbol: 0, num_bits: 0 };
}

/// Canonical flat Huffman lookup table (up to 11-bit codes / 2,048 entries, zero heap allocation).
pub(crate) struct HuffTable {
    entries: [HuffEntry; MAX_HUFF_TABLE_SIZE],
    max_bits: u8,
}

impl HuffTable {
    pub(crate) const fn new_empty() -> Self {
        Self { entries: [HuffEntry::ZERO; MAX_HUFF_TABLE_SIZE], max_bits: 0 }
    }

    /// Decode an RFC 8878 §4.2.1 Huffman tree description from the start of `src`, populate `self`,
    /// and return the number of bytes consumed.
    pub(crate) fn decode_description(&mut self, src: &[u8]) -> Result<usize, ZstdError> {
        let &header_byte = src.first().ok_or(ZstdError::CorruptHuffman)?;
        let mut weights = [0u8; 256];

        let (num_weights, consumed) = if header_byte >= 128 {
            // Direct 4-bit weight representation (RFC 8878 §4.2.1.2).
            let count = usize::from(header_byte - 127);
            let weight_bytes = count.div_ceil(2);
            let total_bytes = 1usize.checked_add(weight_bytes).ok_or(ZstdError::CorruptHuffman)?;
            let packed = src.get(1..total_bytes).ok_or(ZstdError::CorruptHuffman)?;
            for i in 0..count {
                let &byte = packed.get(i / 2).ok_or(ZstdError::CorruptHuffman)?;
                let w = if i.is_multiple_of(2) { byte >> 4 } else { byte & 0x0F };
                let dst = weights.get_mut(i).ok_or(ZstdError::CorruptHuffman)?;
                *dst = w;
            }
            (count, total_bytes)
        } else {
            // FSE-compressed weights (RFC 8878 §4.2.1.1): two interleaved FSE states.
            let comp_size = usize::from(header_byte);
            if comp_size == 0 {
                return Err(ZstdError::CorruptHuffman);
            }
            let total_bytes = 1usize.checked_add(comp_size).ok_or(ZstdError::CorruptHuffman)?;
            let fse_data = src.get(1..total_bytes).ok_or(ZstdError::CorruptHuffman)?;

            let mut fse_table = FseDecTable::new_empty();
            let hdr_len = fse_table
                .decode_header_and_build(fse_data, HUFF_FSE_MAX_SYMBOL, HUFF_FSE_MAX_LOG)
                .map_err(|_| ZstdError::CorruptHuffman)?;
            let bitstream = fse_data.get(hdr_len..).ok_or(ZstdError::CorruptHuffman)?;
            let mut br = RevBitReader::new(bitstream).ok_or(ZstdError::CorruptHuffman)?;

            let acc_log = u32::from(fse_table.accuracy_log());
            let s1_u32 = br.read_bits(acc_log).ok_or(ZstdError::CorruptHuffman)?;
            let s2_u32 = br.read_bits(acc_log).ok_or(ZstdError::CorruptHuffman)?;
            let mut state1 = usize::try_from(s1_u32).map_err(|_| ZstdError::CorruptHuffman)?;
            let mut state2 = usize::try_from(s2_u32).map_err(|_| ZstdError::CorruptHuffman)?;
            let mut count: usize = 0;

            loop {
                let e1 = fse_table.entry(state1).ok_or(ZstdError::CorruptHuffman)?;
                if count >= 255 {
                    return Err(ZstdError::CorruptHuffman);
                }
                let dst1 = weights.get_mut(count).ok_or(ZstdError::CorruptHuffman)?;
                *dst1 = e1.symbol;
                count += 1;
                if let Some(bits) = br.read_bits(u32::from(e1.num_bits)) {
                    let add = usize::try_from(bits).map_err(|_| ZstdError::CorruptHuffman)?;
                    state1 = usize::from(e1.baseline) + add;
                } else {
                    let e2_last = fse_table.entry(state2).ok_or(ZstdError::CorruptHuffman)?;
                    if count >= 255 {
                        return Err(ZstdError::CorruptHuffman);
                    }
                    let dst_last = weights.get_mut(count).ok_or(ZstdError::CorruptHuffman)?;
                    *dst_last = e2_last.symbol;
                    count += 1;
                    break;
                }

                let e2 = fse_table.entry(state2).ok_or(ZstdError::CorruptHuffman)?;
                if count >= 255 {
                    return Err(ZstdError::CorruptHuffman);
                }
                let dst2 = weights.get_mut(count).ok_or(ZstdError::CorruptHuffman)?;
                *dst2 = e2.symbol;
                count += 1;
                if let Some(bits) = br.read_bits(u32::from(e2.num_bits)) {
                    let add = usize::try_from(bits).map_err(|_| ZstdError::CorruptHuffman)?;
                    state2 = usize::from(e2.baseline) + add;
                } else {
                    let e1_last = fse_table.entry(state1).ok_or(ZstdError::CorruptHuffman)?;
                    if count >= 255 {
                        return Err(ZstdError::CorruptHuffman);
                    }
                    let dst_last = weights.get_mut(count).ok_or(ZstdError::CorruptHuffman)?;
                    *dst_last = e1_last.symbol;
                    count += 1;
                    break;
                }
            }

            (count, total_bytes)
        };

        self.build_from_weights(&mut weights, num_weights)?;
        Ok(consumed)
    }

    fn build_from_weights(
        &mut self,
        weights: &mut [u8; 256],
        num_explicit: usize,
    ) -> Result<(), ZstdError> {
        if num_explicit == 0 || num_explicit > 255 {
            return Err(ZstdError::CorruptHuffman);
        }

        let mut weight_sum: u32 = 0;
        let explicit_slice = weights.get(..num_explicit).ok_or(ZstdError::CorruptHuffman)?;
        for &w in explicit_slice {
            if w > MAX_HUFF_BITS {
                return Err(ZstdError::CorruptHuffman);
            }
            if w > 0 {
                weight_sum = weight_sum
                    .checked_add(1u32 << u32::from(w - 1))
                    .ok_or(ZstdError::CorruptHuffman)?;
            }
        }
        if weight_sum == 0 {
            return Err(ZstdError::CorruptHuffman);
        }

        let max_bits_u32 = 32u32.checked_sub(weight_sum.leading_zeros()).ok_or(ZstdError::CorruptHuffman)?;
        if max_bits_u32 == 0 || max_bits_u32 > u32::from(MAX_HUFF_BITS) {
            return Err(ZstdError::CorruptHuffman);
        }
        let max_bits = u8::try_from(max_bits_u32).map_err(|_| ZstdError::CorruptHuffman)?;
        let total = 1u32 << max_bits_u32;
        let left = total.checked_sub(weight_sum).ok_or(ZstdError::CorruptHuffman)?;
        if !left.is_power_of_two() {
            return Err(ZstdError::CorruptHuffman);
        }
        let last_weight = u8::try_from(left.trailing_zeros() + 1).map_err(|_| ZstdError::CorruptHuffman)?;
        if last_weight > max_bits {
            return Err(ZstdError::CorruptHuffman);
        }
        let last_slot = weights.get_mut(num_explicit).ok_or(ZstdError::CorruptHuffman)?;
        *last_slot = last_weight;
        let total_syms = num_explicit + 1;

        let mut rank_count = [0u16; 13];
        let all_weights = weights.get(..total_syms).ok_or(ZstdError::CorruptHuffman)?;
        for &w in all_weights {
            if w > max_bits {
                return Err(ZstdError::CorruptHuffman);
            }
            let slot = rank_count.get_mut(usize::from(w)).ok_or(ZstdError::CorruptHuffman)?;
            *slot = slot.checked_add(1).ok_or(ZstdError::CorruptHuffman)?;
        }

        let max_bits_usize = usize::from(max_bits);
        let mut rank_start = [0usize; 13];
        let mut cursor: usize = 0;
        for w in 1..=max_bits_usize {
            let start_slot = rank_start.get_mut(w).ok_or(ZstdError::CorruptHuffman)?;
            *start_slot = cursor;
            let count = usize::from(*rank_count.get(w).ok_or(ZstdError::CorruptHuffman)?);
            let span = 1usize << (w - 1);
            let delta = count.checked_mul(span).ok_or(ZstdError::CorruptHuffman)?;
            cursor = cursor.checked_add(delta).ok_or(ZstdError::CorruptHuffman)?;
        }

        let table_size = 1usize << max_bits_usize;
        if cursor != table_size {
            return Err(ZstdError::CorruptHuffman);
        }

        for (sym, &w) in all_weights.iter().enumerate() {
            if w == 0 {
                continue;
            }
            let w_usize = usize::from(w);
            let span = 1usize << (w_usize - 1);
            let num_bits = u8::try_from(max_bits_usize + 1 - w_usize).map_err(|_| ZstdError::CorruptHuffman)?;
            let sym_u8 = u8::try_from(sym).map_err(|_| ZstdError::CorruptHuffman)?;
            let start_ref = rank_start.get_mut(w_usize).ok_or(ZstdError::CorruptHuffman)?;
            let start = *start_ref;
            let end = start.checked_add(span).ok_or(ZstdError::CorruptHuffman)?;
            *start_ref = end;
            let slots = self.entries.get_mut(start..end).ok_or(ZstdError::CorruptHuffman)?;
            for slot in slots {
                *slot = HuffEntry { symbol: sym_u8, num_bits };
            }
        }

        self.max_bits = max_bits;
        Ok(())
    }

    /// Decode a single reverse Huffman bitstream of `expected_len` symbols into `out`.
    pub(crate) fn decode_1_stream<A: Allocator>(
        &self,
        src: &[u8],
        expected_len: usize,
        out: &mut Vec<u8, A>,
    ) -> Result<(), ZstdError> {
        if self.max_bits == 0 || self.max_bits > MAX_HUFF_BITS {
            return Err(ZstdError::CorruptHuffman);
        }
        let mut br = RevBitReader::new(src).ok_or(ZstdError::CorruptHuffman)?;
        out.try_reserve(expected_len)?;

        let max_bits_u32 = u32::from(self.max_bits);
        let mut decoded: usize = 0;
        while !br.is_empty() {
            if decoded >= expected_len {
                return Err(ZstdError::CorruptHuffman);
            }
            let idx = usize::try_from(br.peek_bits_padded(max_bits_u32))
                .map_err(|_| ZstdError::CorruptHuffman)?;
            let entry = *self.entries.get(idx).ok_or(ZstdError::CorruptHuffman)?;
            if entry.num_bits == 0 || !br.consume_bits(u32::from(entry.num_bits)) {
                return Err(ZstdError::CorruptHuffman);
            }
            out.try_push(entry.symbol).map_err(|r| r.why)?;
            decoded += 1;
        }

        if decoded != expected_len {
            return Err(ZstdError::CorruptHuffman);
        }
        Ok(())
    }

    /// Decode an RFC 8878 §4.2.2 4-stream Huffman literals payload (6-byte LE jump table + 4
    /// independent reverse bitstreams) into `out`.
    pub(crate) fn decode_4_streams<A: Allocator>(
        &self,
        src: &[u8],
        expected_len: usize,
        out: &mut Vec<u8, A>,
    ) -> Result<(), ZstdError> {
        let (jump, rest) = src.split_at_checked(6).ok_or(ZstdError::CorruptHuffman)?;
        let [a0, a1, b0, b1, c0, c1] = *jump else {
            return Err(ZstdError::CorruptHuffman);
        };
        let s1_len = usize::from(u16::from_le_bytes([a0, a1]));
        let s2_len = usize::from(u16::from_le_bytes([b0, b1]));
        let s3_len = usize::from(u16::from_le_bytes([c0, c1]));

        let (st1, rest) = rest.split_at_checked(s1_len).ok_or(ZstdError::CorruptHuffman)?;
        let (st2, rest) = rest.split_at_checked(s2_len).ok_or(ZstdError::CorruptHuffman)?;
        let (st3, st4) = rest.split_at_checked(s3_len).ok_or(ZstdError::CorruptHuffman)?;

        let seg = expected_len.div_ceil(4);
        let three_segs = seg.checked_mul(3).ok_or(ZstdError::CorruptHuffman)?;
        let last_seg = expected_len.checked_sub(three_segs).ok_or(ZstdError::CorruptHuffman)?;

        out.try_reserve(expected_len)?;
        self.decode_1_stream(st1, seg, out)?;
        self.decode_1_stream(st2, seg, out)?;
        self.decode_1_stream(st3, seg, out)?;
        self.decode_1_stream(st4, last_seg, out)?;
        Ok(())
    }
}
