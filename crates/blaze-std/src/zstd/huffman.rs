//! RFC 8878 §4.2 Huffman tree description codec, `HUF_4X2` double-symbol 4-stream decoder,
//! and Treeless-reusing 4-stream Huffman literals encoder.

use core::alloc::Allocator;

use blaze_own::{RevBitCursor, SliceWriter, Vec};

use crate::zstd::ZstdError;
use crate::zstd::fse::{
    BitWriter, FseDecTable, FseEncTable, normalize_fse_counts, write_fse_header,
};

const MAX_HUFF_BITS: u8 = 11;
const MAX_HUFF_TABLE_SIZE: usize = 1 << MAX_HUFF_BITS;
const HUFF_TABLE_MASK: usize = MAX_HUFF_TABLE_SIZE - 1;
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

/// Double-symbol (`HUF_4X2`) decoding entry: holds 1 or 2 decoded symbols whenever two
/// consecutive codes fit within `max_bits` (`<= 11`). Stores `sym_bits` (`8` for 1 symbol,
/// `16` for 2 symbols) directly so callers shift `packed_u64() << bits_written` with zero extra
/// multiply/shift.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct HuffEntryX2 {
    sym0: u8,
    sym1: u8,
    sym_bits: u8,
    num_bits: u8,
}

impl HuffEntryX2 {
    const ZERO: Self = Self { sym0: 0, sym1: 0, sym_bits: 0, num_bits: 0 };

    #[inline(always)]
    fn packed_u64(self) -> u64 {
        u64::from(u16::from_le_bytes([self.sym0, self.sym1]))
    }
}

/// Canonical flat Huffman lookup table with prebuilt `X2` double-symbol table (up to 11-bit
/// codes, zero heap allocation, fits comfortably in L1D).
pub(crate) struct HuffTable {
    entries: [HuffEntry; MAX_HUFF_TABLE_SIZE],
    entries_x2: [HuffEntryX2; MAX_HUFF_TABLE_SIZE],
    max_bits: u8,
}

impl HuffTable {
    pub(crate) const fn new_empty() -> Self {
        Self {
            entries: [HuffEntry::ZERO; MAX_HUFF_TABLE_SIZE],
            entries_x2: [HuffEntryX2::ZERO; MAX_HUFF_TABLE_SIZE],
            max_bits: 0,
        }
    }

    #[inline(always)]
    fn lookup_fast(&self, idx: usize) -> HuffEntry {
        self.entries.get(idx & HUFF_TABLE_MASK).copied().unwrap_or(HuffEntry::ZERO)
    }

    #[inline(always)]
    fn lookup_x2(&self, idx: usize) -> HuffEntryX2 {
        self.entries_x2.get(idx & HUFF_TABLE_MASK).copied().unwrap_or(HuffEntryX2::ZERO)
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
            let mut br = RevBitCursor::new(bitstream).ok_or(ZstdError::CorruptHuffman)?;

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

        let max_bits_u32 =
            32u32.checked_sub(weight_sum.leading_zeros()).ok_or(ZstdError::CorruptHuffman)?;
        if max_bits_u32 == 0 || max_bits_u32 > u32::from(MAX_HUFF_BITS) {
            return Err(ZstdError::CorruptHuffman);
        }
        let max_bits = u8::try_from(max_bits_u32).map_err(|_| ZstdError::CorruptHuffman)?;
        let total = 1u32 << max_bits_u32;
        let left = total.checked_sub(weight_sum).ok_or(ZstdError::CorruptHuffman)?;
        if !left.is_power_of_two() {
            return Err(ZstdError::CorruptHuffman);
        }
        let last_weight =
            u8::try_from(left.trailing_zeros() + 1).map_err(|_| ZstdError::CorruptHuffman)?;
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
            let num_bits =
                u8::try_from(max_bits_usize + 1 - w_usize).map_err(|_| ZstdError::CorruptHuffman)?;
            let sym_u8 = u8::try_from(sym).map_err(|_| ZstdError::CorruptHuffman)?;
            let start_ref = rank_start.get_mut(w_usize).ok_or(ZstdError::CorruptHuffman)?;
            let start = *start_ref;
            let end = start.checked_add(span).ok_or(ZstdError::CorruptHuffman)?;
            *start_ref = end;
            let slots = self.entries.get_mut(start..end).ok_or(ZstdError::CorruptHuffman)?;
            slots.fill(HuffEntry { symbol: sym_u8, num_bits });
        }

        // Compile double-symbol (`HUF_4X2`) table in one O(table_size) pass (~2 µs).
        let table_mask = table_size - 1;
        for idx in 0..table_size {
            let e0 = self.lookup_fast(idx);
            let rem = max_bits - e0.num_bits;
            let idx2 = (idx << u32::from(e0.num_bits)) & table_mask;
            let e1 = self.lookup_fast(idx2);
            let entry_x2 = if e1.num_bits <= rem {
                HuffEntryX2 {
                    sym0: e0.symbol,
                    sym1: e1.symbol,
                    sym_bits: 16,
                    num_bits: e0.num_bits + e1.num_bits,
                }
            } else {
                HuffEntryX2 { sym0: e0.symbol, sym1: 0, sym_bits: 8, num_bits: e0.num_bits }
            };
            if let Some(slot) = self.entries_x2.get_mut(idx) {
                *slot = entry_x2;
            }
        }

        self.max_bits = max_bits;
        Ok(())
    }

    /// Decode the exact remaining tail of one reverse Huffman stream into `dst`, checking that the
    /// stream terminates with zero trailing bits. Takes `br` by value so callers keep `br` in
    /// registers throughout the fast loop (preserving LLVM SROA).
    #[cold]
    #[inline(never)]
    fn decode_tail(&self, mut br: RevBitCursor<'_>, dst: &mut [u8]) -> Result<(), ZstdError> {
        let max_bits_u32 = u32::from(self.max_bits);
        for slot in dst {
            let idx = usize::try_from(br.peek_bits_padded(max_bits_u32))
                .map_err(|_| ZstdError::CorruptHuffman)?;
            let entry = self.lookup_fast(idx);
            if entry.num_bits == 0 || !br.consume_bits(u32::from(entry.num_bits)) {
                return Err(ZstdError::CorruptHuffman);
            }
            *slot = entry.symbol;
        }
        if !br.is_empty() {
            return Err(ZstdError::CorruptHuffman);
        }
        Ok(())
    }

    /// Decode a single reverse Huffman bitstream directly into pre-sized `dst` using double-symbol
    /// `X2` lookups (4 lookups = 4..8 bytes per 8-byte batched store).
    pub(crate) fn decode_1_stream_into_slice(
        &self,
        src: &[u8],
        dst: &mut [u8],
    ) -> Result<(), ZstdError> {
        if self.max_bits == 0 || self.max_bits > MAX_HUFF_BITS {
            return Err(ZstdError::CorruptHuffman);
        }
        let mut br = RevBitCursor::new(src).ok_or(ZstdError::CorruptHuffman)?;
        let mut sw = SliceWriter::new(dst);
        let shift_r = 64u32 - u32::from(self.max_bits);

        while sw.has_8() && br.reload() {
            let e0 = self.lookup_x2(br.peek_fast(shift_r));
            br.advance_fast(u32::from(e0.num_bits));
            let mut out_word = e0.packed_u64();
            let mut sh = u32::from(e0.sym_bits);

            let e1 = self.lookup_x2(br.peek_fast(shift_r));
            br.advance_fast(u32::from(e1.num_bits));
            out_word |= e1.packed_u64() << sh;
            sh += u32::from(e1.sym_bits);

            let e2 = self.lookup_x2(br.peek_fast(shift_r));
            br.advance_fast(u32::from(e2.num_bits));
            out_word |= e2.packed_u64() << sh;
            sh += u32::from(e2.sym_bits);

            let e3 = self.lookup_x2(br.peek_fast(shift_r));
            br.advance_fast(u32::from(e3.num_bits));
            out_word |= e3.packed_u64() << sh;
            sh += u32::from(e3.sym_bits);

            let _ = sw.write_u64_le_adv(out_word, usize::try_from(sh >> 3).unwrap_or(0));
        }

        self.decode_tail(br, sw.into_rest())
    }

    #[inline(always)]
    fn decode_4_streams_loop(
        &self,
        st1: &[u8],
        st2: &[u8],
        st3: &[u8],
        st4: &[u8],
        d1: &mut [u8],
        d2: &mut [u8],
        d3: &mut [u8],
        d4: &mut [u8],
        shift_r: u32,
    ) -> Result<(), ZstdError> {
        let mut br1 = RevBitCursor::new(st1).ok_or(ZstdError::CorruptHuffman)?;
        let mut br2 = RevBitCursor::new(st2).ok_or(ZstdError::CorruptHuffman)?;
        let mut br3 = RevBitCursor::new(st3).ok_or(ZstdError::CorruptHuffman)?;
        let mut br4 = RevBitCursor::new(st4).ok_or(ZstdError::CorruptHuffman)?;
        let mut sw1 = SliceWriter::new(d1);
        let mut sw2 = SliceWriter::new(d2);
        let mut sw3 = SliceWriter::new(d3);
        let mut sw4 = SliceWriter::new(d4);

        while sw1.has_8()
            && sw2.has_8()
            && sw3.has_8()
            && sw4.has_8()
            && br1.reload()
            && br2.reload()
            && br3.reload()
            && br4.reload()
        {
            // Step 0 across all 4 independent streams (up to 8 symbols).
            let e1 = self.lookup_x2(br1.peek_fast(shift_r));
            let e2 = self.lookup_x2(br2.peek_fast(shift_r));
            let e3 = self.lookup_x2(br3.peek_fast(shift_r));
            let e4 = self.lookup_x2(br4.peek_fast(shift_r));
            br1.advance_fast(u32::from(e1.num_bits));
            br2.advance_fast(u32::from(e2.num_bits));
            br3.advance_fast(u32::from(e3.num_bits));
            br4.advance_fast(u32::from(e4.num_bits));
            let mut w1 = e1.packed_u64();
            let mut w2 = e2.packed_u64();
            let mut w3 = e3.packed_u64();
            let mut w4 = e4.packed_u64();
            let mut sh1 = u32::from(e1.sym_bits);
            let mut sh2 = u32::from(e2.sym_bits);
            let mut sh3 = u32::from(e3.sym_bits);
            let mut sh4 = u32::from(e4.sym_bits);

            // Step 1
            let e1 = self.lookup_x2(br1.peek_fast(shift_r));
            let e2 = self.lookup_x2(br2.peek_fast(shift_r));
            let e3 = self.lookup_x2(br3.peek_fast(shift_r));
            let e4 = self.lookup_x2(br4.peek_fast(shift_r));
            br1.advance_fast(u32::from(e1.num_bits));
            br2.advance_fast(u32::from(e2.num_bits));
            br3.advance_fast(u32::from(e3.num_bits));
            br4.advance_fast(u32::from(e4.num_bits));
            w1 |= e1.packed_u64() << sh1;
            w2 |= e2.packed_u64() << sh2;
            w3 |= e3.packed_u64() << sh3;
            w4 |= e4.packed_u64() << sh4;
            sh1 += u32::from(e1.sym_bits);
            sh2 += u32::from(e2.sym_bits);
            sh3 += u32::from(e3.sym_bits);
            sh4 += u32::from(e4.sym_bits);

            // Step 2
            let e1 = self.lookup_x2(br1.peek_fast(shift_r));
            let e2 = self.lookup_x2(br2.peek_fast(shift_r));
            let e3 = self.lookup_x2(br3.peek_fast(shift_r));
            let e4 = self.lookup_x2(br4.peek_fast(shift_r));
            br1.advance_fast(u32::from(e1.num_bits));
            br2.advance_fast(u32::from(e2.num_bits));
            br3.advance_fast(u32::from(e3.num_bits));
            br4.advance_fast(u32::from(e4.num_bits));
            w1 |= e1.packed_u64() << sh1;
            w2 |= e2.packed_u64() << sh2;
            w3 |= e3.packed_u64() << sh3;
            w4 |= e4.packed_u64() << sh4;
            sh1 += u32::from(e1.sym_bits);
            sh2 += u32::from(e2.sym_bits);
            sh3 += u32::from(e3.sym_bits);
            sh4 += u32::from(e4.sym_bits);

            // Step 3
            let e1 = self.lookup_x2(br1.peek_fast(shift_r));
            let e2 = self.lookup_x2(br2.peek_fast(shift_r));
            let e3 = self.lookup_x2(br3.peek_fast(shift_r));
            let e4 = self.lookup_x2(br4.peek_fast(shift_r));
            br1.advance_fast(u32::from(e1.num_bits));
            br2.advance_fast(u32::from(e2.num_bits));
            br3.advance_fast(u32::from(e3.num_bits));
            br4.advance_fast(u32::from(e4.num_bits));
            w1 |= e1.packed_u64() << sh1;
            w2 |= e2.packed_u64() << sh2;
            w3 |= e3.packed_u64() << sh3;
            w4 |= e4.packed_u64() << sh4;
            sh1 += u32::from(e1.sym_bits);
            sh2 += u32::from(e2.sym_bits);
            sh3 += u32::from(e3.sym_bits);
            sh4 += u32::from(e4.sym_bits);

            // Single 64-bit batched store per stream (4 stores for up to 32 decoded bytes!).
            let _ = sw1.write_u64_le_adv(w1, usize::try_from(sh1 >> 3).unwrap_or(0));
            let _ = sw2.write_u64_le_adv(w2, usize::try_from(sh2 >> 3).unwrap_or(0));
            let _ = sw3.write_u64_le_adv(w3, usize::try_from(sh3 >> 3).unwrap_or(0));
            let _ = sw4.write_u64_le_adv(w4, usize::try_from(sh4 >> 3).unwrap_or(0));
        }

        // Drain the exact remaining tail of each stream with X1 exact checking.
        self.decode_tail(br1, sw1.into_rest())?;
        self.decode_tail(br2, sw2.into_rest())?;
        self.decode_tail(br3, sw3.into_rest())?;
        self.decode_tail(br4, sw4.into_rest())?;
        Ok(())
    }

    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn decode_4_streams_11(
        &self,
        st1: &[u8],
        st2: &[u8],
        st3: &[u8],
        st4: &[u8],
        d1: &mut [u8],
        d2: &mut [u8],
        d3: &mut [u8],
        d4: &mut [u8],
    ) -> Result<(), ZstdError> {
        self.decode_4_streams_loop(st1, st2, st3, st4, d1, d2, d3, d4, 53)
    }

    /// Decode an RFC 8878 §4.2.2 4-stream Huffman literals payload directly into pre-sized `dst`
    /// using 4-way interleaved double-symbol `HUF_4X2` lookups, pointer-resident `RevBitCursor`
    /// reloads, and `SliceWriter` 64-bit batched stores (16..32 symbols per batch).
    pub(crate) fn decode_4_streams_into_slice(
        &self,
        src: &[u8],
        dst: &mut [u8],
    ) -> Result<(), ZstdError> {
        if self.max_bits == 0 || self.max_bits > MAX_HUFF_BITS {
            return Err(ZstdError::CorruptHuffman);
        }
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

        let expected_len = dst.len();
        let seg = expected_len.div_ceil(4);
        let three_segs = seg.checked_mul(3).ok_or(ZstdError::CorruptHuffman)?;
        let last_seg = expected_len.checked_sub(three_segs).ok_or(ZstdError::CorruptHuffman)?;

        let (d1, rest) = dst.split_at_mut_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
        let (d2, rest) = rest.split_at_mut_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
        let (d3, d4) = rest.split_at_mut_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
        if d4.len() != last_seg {
            return Err(ZstdError::CorruptHuffman);
        }

        if self.max_bits == 11 {
            self.decode_4_streams_11(st1, st2, st3, st4, d1, d2, d3, d4)
        } else {
            let shift_r = 64u32 - u32::from(self.max_bits);
            self.decode_4_streams_loop(st1, st2, st3, st4, d1, d2, d3, d4, shift_r)
        }
    }
}

// ---------------------------------------------------------------------------
// Fast RFC 8878 §4.2 Huffman Literals Encoder (Treeless Reuse + 4-Stream + RLE)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct HuffNode {
    count: u32,
    parent: u16,
    sym: u8,
}

impl HuffNode {
    const ZERO: Self = Self { count: 0, parent: 0, sym: 0 };
}

#[derive(Clone, Copy)]
struct HuffCode {
    code: u16,
    num_bits: u8,
}

impl HuffCode {
    const ZERO: Self = Self { code: 0, num_bits: 0 };
}

/// Reusable Huffman encoding table cache enabling `Treeless_Literals_Block` (`lit_type = 3`)
/// across blocks within a frame (skips histogram sorting, tree build, and weight header).
pub(crate) struct HuffEncCache {
    codes: [HuffCode; 256],
    pending_codes: [HuffCode; 256],
    valid: bool,
    has_pending: bool,
}

impl HuffEncCache {
    pub(crate) const fn new() -> Self {
        Self {
            codes: [HuffCode::ZERO; 256],
            pending_codes: [HuffCode::ZERO; 256],
            valid: false,
            has_pending: false,
        }
    }

    #[inline]
    pub(crate) fn reset(&mut self) {
        self.valid = false;
        self.has_pending = false;
    }

    /// Commit any freshly built table after the compressed block is actually emitted (not dropped
    /// in favor of a raw block fallback).
    #[inline]
    pub(crate) fn commit(&mut self) {
        if self.has_pending {
            self.codes = self.pending_codes;
            self.valid = true;
            self.has_pending = false;
        }
    }

    /// Discard any staged table when the compressed block fell back to a raw block.
    #[inline]
    pub(crate) fn discard(&mut self) {
        self.has_pending = false;
    }
}

/// Build optimal length-limited (`<= 11` bits) RFC 8878 Huffman weights for the histogram `counts`.
/// Returns `(max_bits, max_sym)` or `None` if fewer than 2 distinct symbols are present.
fn build_huffman_weights(counts: &[u32; 256], weights: &mut [u8; 256]) -> Option<(u8, usize)> {
    let mut nodes = [HuffNode::ZERO; 512];
    let mut num_leaves: usize = 0;
    let mut max_sym: usize = 0;

    for (sym, &c) in counts.iter().enumerate() {
        if c > 0 {
            let sym_u8 = u8::try_from(sym).ok()?;
            let slot = nodes.get_mut(num_leaves)?;
            *slot = HuffNode { count: c, parent: 0, sym: sym_u8 };
            num_leaves += 1;
            max_sym = sym;
        }
    }
    if num_leaves < 2 {
        return None;
    }

    // Sort leaves ascending by `(count, sym)` (shell sort: O(K log K), <= 256 items, zero alloc).
    let mut gap = num_leaves / 2;
    while gap > 0 {
        for i in gap..num_leaves {
            let cur = *nodes.get(i)?;
            let mut j = i;
            while j >= gap {
                let prev = *nodes.get(j - gap)?;
                if prev.count > cur.count || (prev.count == cur.count && prev.sym > cur.sym) {
                    if let Some(dst) = nodes.get_mut(j) {
                        *dst = prev;
                    }
                    j -= gap;
                } else {
                    break;
                }
            }
            if let Some(dst) = nodes.get_mut(j) {
                *dst = cur;
            }
        }
        gap /= 2;
    }

    // Two-queue O(K) Huffman tree construction.
    let mut leaf_idx: usize = 0;
    let mut int_read: usize = num_leaves;
    let mut int_write: usize = num_leaves;

    for _ in 0..(num_leaves - 1) {
        let pick = |l_idx: &mut usize, i_rd: &mut usize, i_wr: usize, ns: &[HuffNode; 512]| -> Option<usize> {
            let has_leaf = *l_idx < num_leaves;
            let has_int = *i_rd < i_wr;
            match (has_leaf, has_int) {
                (true, true) => {
                    let lc = ns.get(*l_idx)?.count;
                    let ic = ns.get(*i_rd)?.count;
                    if lc <= ic {
                        let out = *l_idx;
                        *l_idx += 1;
                        Some(out)
                    } else {
                        let out = *i_rd;
                        *i_rd += 1;
                        Some(out)
                    }
                }
                (true, false) => {
                    let out = *l_idx;
                    *l_idx += 1;
                    Some(out)
                }
                (false, true) => {
                    let out = *i_rd;
                    *i_rd += 1;
                    Some(out)
                }
                (false, false) => None,
            }
        };

        let left = pick(&mut leaf_idx, &mut int_read, int_write, &nodes)?;
        let right = pick(&mut leaf_idx, &mut int_read, int_write, &nodes)?;
        let sum = nodes.get(left)?.count.saturating_add(nodes.get(right)?.count);
        let parent_u16 = u16::try_from(int_write).ok()?;
        if let Some(l) = nodes.get_mut(left) {
            l.parent = parent_u16;
        }
        if let Some(r) = nodes.get_mut(right) {
            r.parent = parent_u16;
        }
        if let Some(p) = nodes.get_mut(int_write) {
            *p = HuffNode { count: sum, parent: 0, sym: 0 };
        }
        int_write += 1;
    }

    // Compute depths top-down from root (`int_write - 1` down to `0`), zero recursion.
    let mut depth = [0u8; 512];
    let root = int_write - 1;
    for i in (0..root).rev() {
        let p = usize::from(nodes.get(i)?.parent);
        let pd = *depth.get(p)?;
        if let Some(d) = depth.get_mut(i) {
            *d = pd.saturating_add(1);
        }
    }

    let mut leaf_len = [0u8; 256];
    let mut raw_max: u8 = 0;
    for i in 0..num_leaves {
        let d = *depth.get(i)?;
        if d > raw_max {
            raw_max = d;
        }
        if let Some(slot) = leaf_len.get_mut(i) {
            *slot = d;
        }
    }

    let max_bits = raw_max.min(MAX_HUFF_BITS);
    if max_bits == 0 {
        return None;
    }

    if raw_max > MAX_HUFF_BITS {
        let target: u32 = 1u32 << u32::from(max_bits);
        let mut kraft: u32 = 0;
        for i in 0..num_leaves {
            let slot = leaf_len.get_mut(i)?;
            if *slot > max_bits {
                *slot = max_bits;
            }
            kraft += 1u32 << u32::from(max_bits - *slot);
        }

        while kraft > target {
            let excess = kraft - target;
            let mut chosen: Option<usize> = None;
            let mut best_len: u8 = 0;
            for i in 0..num_leaves {
                let len = *leaf_len.get(i)?;
                if len < max_bits {
                    let delta = 1u32 << u32::from(max_bits - len - 1);
                    if delta <= excess && len > best_len {
                        best_len = len;
                        chosen = Some(i);
                    }
                }
            }
            if chosen.is_none() {
                for i in 0..num_leaves {
                    let len = *leaf_len.get(i)?;
                    if len < max_bits && len > best_len {
                        best_len = len;
                        chosen = Some(i);
                    }
                }
            }
            let idx = chosen?;
            let slot = leaf_len.get_mut(idx)?;
            let delta = 1u32 << u32::from(max_bits - *slot - 1);
            *slot += 1;
            kraft -= delta;
        }

        while kraft < target {
            let deficit = target - kraft;
            let mut chosen: Option<usize> = None;
            for i in (0..num_leaves).rev() {
                let len = *leaf_len.get(i)?;
                if len >= 2 {
                    let delta = 1u32 << u32::from(max_bits - len);
                    if delta <= deficit {
                        chosen = Some(i);
                        break;
                    }
                }
            }
            let idx = chosen?;
            let slot = leaf_len.get_mut(idx)?;
            let delta = 1u32 << u32::from(max_bits - *slot);
            *slot -= 1;
            kraft += delta;
        }
    }

    weights.fill(0);
    for i in 0..num_leaves {
        let sym = usize::from(nodes.get(i)?.sym);
        let len = *leaf_len.get(i)?;
        if len == 0 || len > max_bits {
            return None;
        }
        if let Some(w_slot) = weights.get_mut(sym) {
            *w_slot = max_bits + 1 - len;
        }
    }

    Some((max_bits, max_sym))
}

/// Write an RFC 8878 §4.2.1.1 FSE-compressed Huffman weights description into `dst`.
/// Returns `Ok(true)` if written and `< 128` bytes payload, `Ok(false)` otherwise.
fn encode_fse_weights<A: Allocator + Copy>(
    explicit_weights: &[u8],
    dst: &mut Vec<u8, A>,
    alloc: A,
) -> Result<bool, ZstdError> {
    let n = explicit_weights.len();
    if n < 2 || n > 255 {
        return Ok(false);
    }

    let mut w_counts = [0u32; 13];
    for &w in explicit_weights {
        let Some(slot) = w_counts.get_mut(usize::from(w)) else {
            return Ok(false);
        };
        *slot += 1;
    }
    let total = u32::try_from(n).map_err(|_| ZstdError::CorruptHuffman)?;
    let acc_log: u8 = 6;
    let mut norm = [0i16; 13];
    let Some(max_w_sym) = normalize_fse_counts(&w_counts, total, acc_log, &mut norm) else {
        return Ok(false);
    };

    let norm_slice = norm.get(..=max_w_sym).ok_or(ZstdError::CorruptHuffman)?;
    let mut dec = FseDecTable::new_empty();
    if dec.build_from_norm(norm_slice, acc_log).is_err() {
        return Ok(false);
    }
    let Ok(enc) = FseEncTable::build_from_dec(&dec) else {
        return Ok(false);
    };

    let mut fse_buf = Vec::try_with_capacity_in(128, alloc)?;
    write_fse_header(norm_slice, acc_log, &mut fse_buf)?;

    let (last_even_w, last_odd_w) = if n.is_multiple_of(2) {
        (
            *explicit_weights.get(n - 2).ok_or(ZstdError::CorruptHuffman)?,
            *explicit_weights.get(n - 1).ok_or(ZstdError::CorruptHuffman)?,
        )
    } else {
        (
            *explicit_weights.get(n - 1).ok_or(ZstdError::CorruptHuffman)?,
            *explicit_weights.get(n - 2).ok_or(ZstdError::CorruptHuffman)?,
        )
    };

    let Some(mut state1) = enc.init_state(last_even_w) else {
        return Ok(false);
    };
    let Some(mut state2) = enc.init_state(last_odd_w) else {
        return Ok(false);
    };

    let mut bw = BitWriter::try_with_capacity_in(n, alloc)?;
    for i in (0..(n - 2)).rev() {
        let w = *explicit_weights.get(i).ok_or(ZstdError::CorruptHuffman)?;
        if i.is_multiple_of(2) {
            let Some((prev, bits, nb)) = enc.encode_step(w, state1) else {
                return Ok(false);
            };
            state1 = prev;
            bw.write_bits(bits, nb)?;
        } else {
            let Some((prev, bits, nb)) = enc.encode_step(w, state2) else {
                return Ok(false);
            };
            state2 = prev;
            bw.write_bits(bits, nb)?;
        }
    }
    bw.write_bits(u32::from(state2), u32::from(acc_log))?;
    bw.write_bits(u32::from(state1), u32::from(acc_log))?;
    bw.finish_into(&mut fse_buf)?;

    if fse_buf.is_empty() || fse_buf.len() >= 128 {
        return Ok(false);
    }
    let hdr_byte = u8::try_from(fse_buf.len()).map_err(|_| ZstdError::CorruptHuffman)?;
    dst.try_push(hdr_byte).map_err(|r| r.why)?;
    dst.try_extend_from_slice(fse_buf.as_slice())?;
    Ok(true)
}

/// Encode one Huffman literal segment directly into `dst` via `ByteCursor` using C-zstd's
/// branchless 64-bit `write_u64_le_adv` flush (4 symbols <= 44 bits + 7 residual = 51 <= 64 bits:
/// 1 branchless store per 4 symbols!).
#[inline]
fn encode_huff_stream<A: Allocator>(
    segment: &[u8],
    codes: &[HuffCode; 256],
    dst: &mut Vec<u8, A>,
) -> Result<(), ZstdError> {
    // Upper bound on reverse stream bytes: each symbol <= 11 bits -> <= 1.375 bytes + 24B slack.
    let max_bytes = segment.len() + (segment.len() >> 1) + 24;
    let mut cur = dst.byte_cursor();
    cur.try_reserve(max_bytes)?;

    let mut buf: u64 = 0;
    let mut bits: u32 = 0;

    // 4-byte reverse chunks: `bits <= 7` at loop top, 4 * 11 = 44 bits -> `bits <= 51 < 64`.
    let mut idx = segment.len();
    while idx >= 4 {
        let Some(ch) = segment.get(idx - 4..idx) else {
            break;
        };
        let [b3, b2, b1, b0] = *ch else {
            break;
        };
        let c0 = codes.get(usize::from(b0)).copied().unwrap_or(HuffCode::ZERO);
        let c1 = codes.get(usize::from(b1)).copied().unwrap_or(HuffCode::ZERO);
        let c2 = codes.get(usize::from(b2)).copied().unwrap_or(HuffCode::ZERO);
        let c3 = codes.get(usize::from(b3)).copied().unwrap_or(HuffCode::ZERO);

        buf |= u64::from(c0.code) << bits;
        bits += u32::from(c0.num_bits);
        buf |= u64::from(c1.code) << bits;
        bits += u32::from(c1.num_bits);
        buf |= u64::from(c2.code) << bits;
        bits += u32::from(c2.num_bits);
        buf |= u64::from(c3.code) << bits;
        bits += u32::from(c3.num_bits);

        let [lo, ..] = bits.to_le_bytes();
        let adv = usize::from(lo >> 3);
        let _ = cur.write_u64_le_adv(buf, adv);
        buf >>= u32::from(lo & !7u8);
        bits &= 7;
        idx -= 4;
    }
    while idx > 0 {
        idx -= 1;
        let b = segment.get(idx).copied().unwrap_or(0);
        let c = codes.get(usize::from(b)).copied().unwrap_or(HuffCode::ZERO);
        buf |= u64::from(c.code) << bits;
        bits += u32::from(c.num_bits);
        let [lo, ..] = bits.to_le_bytes();
        let adv = usize::from(lo >> 3);
        let _ = cur.write_u64_le_adv(buf, adv);
        buf >>= u32::from(lo & !7u8);
        bits &= 7;
    }

    buf |= 1u64 << bits;
    let [b0, ..] = buf.to_le_bytes();
    let _ = cur.push_within_cap(b0);
    cur.finish();
    Ok(())
}

/// Write a `Compressed_Literals_Block` (`lit_type = 2`), `Treeless_Literals_Block` (`lit_type = 3`,
/// reusing `cache` across blocks), or `RLE_Literals_Block` (`lit_type = 1`) into `out`.
/// Returns `Ok(true)` if written, or `Ok(false)` leaving `out` unchanged so caller emits raw.
pub(crate) fn encode_huffman_literals_into<A: Allocator + Copy>(
    literals: &[u8],
    fast_mode: bool,
    cache: &mut HuffEncCache,
    payload: &mut Vec<u8, A>,
    out: &mut Vec<u8, A>,
    alloc: A,
) -> Result<bool, ZstdError> {
    let lit_len = literals.len();
    if lit_len < 32 {
        return Ok(false);
    }

    // 4-way unrolled histogram to avoid store-to-load forwarding stalls on repeated bytes.
    let mut c0 = [0u32; 256];
    let mut c1 = [0u32; 256];
    let mut c2 = [0u32; 256];
    let mut c3 = [0u32; 256];
    let mut chunks = literals.chunks_exact(4);
    for ch in &mut chunks {
        let [b0, b1, b2, b3] = *ch else {
            break;
        };
        if let Some(s) = c0.get_mut(usize::from(b0)) {
            *s += 1;
        }
        if let Some(s) = c1.get_mut(usize::from(b1)) {
            *s += 1;
        }
        if let Some(s) = c2.get_mut(usize::from(b2)) {
            *s += 1;
        }
        if let Some(s) = c3.get_mut(usize::from(b3)) {
            *s += 1;
        }
    }
    for &b in chunks.remainder() {
        if let Some(s) = c0.get_mut(usize::from(b)) {
            *s += 1;
        }
    }

    let mut counts = [0u32; 256];
    let mut distinct: usize = 0;
    let mut only_byte: u8 = 0;
    let mut can_reuse = cache.valid;
    let mut reuse_bits: u64 = 0;

    for i in 0..256 {
        let total = c0.get(i).copied().unwrap_or(0)
            + c1.get(i).copied().unwrap_or(0)
            + c2.get(i).copied().unwrap_or(0)
            + c3.get(i).copied().unwrap_or(0);
        if total > 0 {
            if let Some(s) = counts.get_mut(i) {
                *s = total;
            }
            distinct += 1;
            only_byte = u8::try_from(i).unwrap_or(0);
            if can_reuse {
                let nb = cache.codes.get(i).copied().unwrap_or(HuffCode::ZERO).num_bits;
                if nb == 0 {
                    can_reuse = false;
                } else {
                    reuse_bits += u64::from(total) * u64::from(nb);
                }
            }
        }
    }

    // RLE_Literals_Block (lit_type = 1) when every literal byte is identical (`lit_len >= 32`).
    if distinct == 1 {
        if lit_len < 4_096 {
            let len_u16 = u16::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
            let v = (len_u16 << 4) | 0b0101;
            out.try_extend_from_slice(&v.to_le_bytes())?;
        } else {
            let len_u32 = u32::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
            let v = (len_u32 << 4) | 0b1101;
            let [b0, b1, b2, _] = v.to_le_bytes();
            out.try_extend_from_slice(&[b0, b1, b2])?;
        }
        out.try_push(only_byte).map_err(|r| r.why)?;
        return Ok(true);
    }

    if lit_len < 64 {
        return Ok(false);
    }

    payload.clear();
    let reuse_bytes = usize::try_from(reuse_bits / 8).unwrap_or(lit_len) + 16;

    // Fast path: Treeless reuse (`lit_type = 3`) skips tree building & weight header completely!
    let mut use_treeless = false;
    let mut fresh_codes = [HuffCode::ZERO; 256];

    if can_reuse && fast_mode && reuse_bytes + 8 < lit_len {
        use_treeless = true;
    } else {
        let mut weights = [0u8; 256];
        let Some((max_bits, max_sym)) = build_huffman_weights(&counts, &mut weights) else {
            return Ok(false);
        };

        let mut est_bits: u64 = 0;
        for sym in 0..=max_sym {
            let w = weights.get(sym).copied().unwrap_or(0);
            if w > 0 {
                let len = u64::from(max_bits + 1 - w);
                est_bits += u64::from(counts.get(sym).copied().unwrap_or(0)) * len;
            }
        }
        let est_hdr_bytes = if max_sym <= 128 { 1 + max_sym.div_ceil(2) } else { 48 };
        let est_new_bytes = usize::try_from(est_bits / 8).unwrap_or(lit_len) + est_hdr_bytes + 12;

        if can_reuse && reuse_bytes <= est_new_bytes + (lit_len >> 7) && reuse_bytes + 8 < lit_len {
            use_treeless = true;
        } else {
            if est_new_bytes >= lit_len {
                return Ok(false);
            }

            // Build canonical encoding table matching `HuffTable::build_from_weights`.
            let mut rank_count = [0u16; 13];
            for &w in weights.get(..=max_sym).ok_or(ZstdError::CorruptHuffman)? {
                if w > 0
                    && let Some(slot) = rank_count.get_mut(usize::from(w))
                {
                    *slot += 1;
                }
            }
            let max_bits_usize = usize::from(max_bits);
            let mut rank_start = [0usize; 13];
            let mut cursor: usize = 0;
            for w in 1..=max_bits_usize {
                if let Some(s) = rank_start.get_mut(w) {
                    *s = cursor;
                }
                let cnt = usize::from(rank_count.get(w).copied().unwrap_or(0));
                cursor += cnt << (w - 1);
            }

            for sym in 0..=max_sym {
                let w = weights.get(sym).copied().unwrap_or(0);
                if w == 0 {
                    continue;
                }
                let w_usize = usize::from(w);
                let span = 1usize << (w_usize - 1);
                let start_ref = rank_start.get_mut(w_usize).ok_or(ZstdError::CorruptHuffman)?;
                let start = *start_ref;
                *start_ref = start + span;
                let code_val =
                    u16::try_from(start >> (w_usize - 1)).map_err(|_| ZstdError::CorruptHuffman)?;
                let num_bits = max_bits + 1 - w;
                if let Some(slot) = fresh_codes.get_mut(sym) {
                    *slot = HuffCode { code: code_val, num_bits };
                }
            }

            // Write tree description into `payload`.
            let num_explicit = max_sym;
            let explicit_slice = weights.get(..num_explicit).ok_or(ZstdError::CorruptHuffman)?;
            if num_explicit <= 128 {
                let header_byte =
                    u8::try_from(127 + num_explicit).map_err(|_| ZstdError::CorruptHuffman)?;
                payload.try_push(header_byte).map_err(|r| r.why)?;
                let mut i: usize = 0;
                while i < num_explicit {
                    let hi = explicit_slice.get(i).copied().unwrap_or(0);
                    let lo = explicit_slice.get(i + 1).copied().unwrap_or(0);
                    payload.try_push((hi << 4) | (lo & 0x0F)).map_err(|r| r.why)?;
                    i += 2;
                }
            } else if !encode_fse_weights(explicit_slice, payload, alloc)? {
                return Ok(false);
            }
        }
    }

    let active_codes = if use_treeless { &cache.codes } else { &fresh_codes };
    let use_4_streams = lit_len >= 256;
    if !use_4_streams {
        encode_huff_stream(literals, active_codes, payload)?;
    } else {
        let seg = lit_len.div_ceil(4);
        let (st1, rest) = literals.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
        let (st2, rest) = rest.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
        let (st3, st4) = rest.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;

        let jump_pos = payload.len();
        payload.try_extend_from_slice(&[0u8; 6])?;

        let s1_start = payload.len();
        encode_huff_stream(st1, active_codes, payload)?;
        let s1_len = payload.len() - s1_start;

        let s2_start = payload.len();
        encode_huff_stream(st2, active_codes, payload)?;
        let s2_len = payload.len() - s2_start;

        let s3_start = payload.len();
        encode_huff_stream(st3, active_codes, payload)?;
        let s3_len = payload.len() - s3_start;

        encode_huff_stream(st4, active_codes, payload)?;

        let (Ok(l1), Ok(l2), Ok(l3)) =
            (u16::try_from(s1_len), u16::try_from(s2_len), u16::try_from(s3_len))
        else {
            return Ok(false);
        };
        let [a0, a1] = l1.to_le_bytes();
        let [b0, b1] = l2.to_le_bytes();
        let [c0, c1] = l3.to_le_bytes();
        if let Some(jump_dst) = payload.as_mut_slice().get_mut(jump_pos..jump_pos + 6) {
            jump_dst.copy_from_slice(&[a0, a1, b0, b1, c0, c1]);
        }
    }

    let comp_size = payload.len();
    if comp_size + 8 >= lit_len {
        return Ok(false);
    }

    // Emit Compressed_Literals_Block (`2`) or Treeless_Literals_Block (`3`) header.
    let lit_type: u32 = if use_treeless { 3 } else { 2 };
    if !use_4_streams && lit_len < 1_024 && comp_size < 1_024 {
        let r = u32::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let c = u32::try_from(comp_size).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = lit_type | (r << 4) | (c << 14);
        let [b0, b1, b2, _] = v.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2])?;
    } else if lit_len < 1_024 && comp_size < 1_024 {
        let r = u32::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let c = u32::try_from(comp_size).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = lit_type | (1u32 << 2) | (r << 4) | (c << 14);
        let [b0, b1, b2, _] = v.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2])?;
    } else if lit_len < 16_384 && comp_size < 16_384 {
        let r = u32::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let c = u32::try_from(comp_size).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = lit_type | (2u32 << 2) | (r << 4) | (c << 18);
        out.try_extend_from_slice(&v.to_le_bytes())?;
    } else {
        let r = u64::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let c = u64::try_from(comp_size).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = u64::from(lit_type) | (3u64 << 2) | (r << 4) | (c << 22);
        let [b0, b1, b2, b3, b4, ..] = v.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2, b3, b4])?;
    }

    out.try_extend_from_slice(payload.as_slice())?;
    if !use_treeless {
        cache.pending_codes = fresh_codes;
        cache.has_pending = true;
    } else {
        cache.has_pending = false;
    }
    Ok(true)
}
