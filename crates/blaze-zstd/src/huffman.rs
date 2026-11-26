//! RFC 8878 §4.2 Huffman tree description codec, `HUF_4X2` double-symbol 4-stream decoder,
//! and Treeless-reusing 4-stream Huffman literals encoder.

use core::alloc::Allocator;

use blaze_own::blaze_int::Cast;
use blaze_own::byte_cursor::RevBitSlack8;
use blaze_own::{Int, Pow2Array, RevBitCursor, SliceWriter, Vec};

use crate::ZstdError;
use crate::fse::{
    BitWriter, FseDecTable, FseEncTable, normalize_fse_counts, write_fse_header,
};

const MAX_HUFF_BITS: u8 = 11;
const MAX_HUFF_TABLE_SIZE: usize = 1 << MAX_HUFF_BITS;
const HUFF_FSE_MAX_SYMBOL: usize = 12;
const HUFF_FSE_MAX_LOG: u8 = 6;

/// Single-symbol Huffman decoding entry packed into a single `u16` (`#[repr(transparent)]`)
/// so `slice::fill` vectorizes directly to 128-bit NEON stores:
/// - bits `0..8`: `symbol: u8`
/// - bits `8..16`: `num_bits: u8`
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct HuffEntry(u16);

impl HuffEntry {
    const ZERO: Self = Self(0);

    #[inline(always)]
    fn new(symbol: u8, num_bits: u8) -> Self {
        Self(u16::from(symbol) | (u16::from(num_bits) << 8))
    }

    #[inline(always)]
    fn symbol(self) -> u8 {
        self.0.truncate_to::<u8>()
    }

    #[inline(always)]
    fn num_bits(self) -> u8 {
        (self.0 >> 8).truncate_to::<u8>()
    }
}

/// Double-symbol (`HUF_4X2`) decoding entry packed into a single `u32` (`#[repr(transparent)]`):
/// - bits `0..16`: `syms: u16` (`sym0 | (sym1 << 8)`) whenever two consecutive codes fit within
///   `max_bits` (`<= 11`)
/// - bits `16..24`: `num_bits: u8`
/// - bits `24..32`: `sym_bits: u8` (`8` for 1 symbol, `16` for 2 symbols) so callers shift
///   `packed_u64() << bits_written` with zero byte-shuffling or extra multiply/shift.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct HuffEntryX2(u32);

impl HuffEntryX2 {
    const ZERO: Self = Self(0);

    #[inline(always)]
    const fn new(syms: u16, num_bits: u8, sym_bits: u8) -> Self {
        Self(
            Cast::<u16, u32>::widen(syms)
                | (Cast::<u8, u32>::widen(num_bits) << 16)
                | (Cast::<u8, u32>::widen(sym_bits) << 24),
        )
    }

    #[inline(always)]
    fn packed_u64(self) -> u64 {
        (self.0 & 0xFFFF).widen_to::<u64>()
    }

    /// Packed metadata `(num_bits as u32) | ((sym_bits as u32) << 8)` in bits `0..16`:
    /// low 6 bits hold `num_bits` (`<= 11 < 64`), and summing 4 entries never carries out of
    /// the low byte (`<= 44 < 256`) or upper byte (`<= 64 < 256`).
    #[inline(always)]
    fn meta(self) -> u32 {
        self.0 >> 16
    }

    #[inline(always)]
    fn sym_bits(self) -> u32 {
        self.0 >> 24
    }
}

/// Canonical flat Huffman lookup table with prebuilt `X2` double-symbol table (up to 11-bit
/// codes, zero heap allocation, fits comfortably in L1D).
pub(crate) struct HuffTable {
    entries: Pow2Array<HuffEntry, MAX_HUFF_TABLE_SIZE>,
    entries_x2: Pow2Array<HuffEntryX2, MAX_HUFF_TABLE_SIZE>,
    max_bits: u8,
}

impl HuffTable {
    pub(crate) const fn new_empty() -> Self {
        Self {
            entries: Pow2Array::splat(HuffEntry::ZERO),
            entries_x2: Pow2Array::splat(HuffEntryX2::ZERO),
            max_bits: 0,
        }
    }

    #[inline(always)]
    fn lookup_fast(&self, idx: usize) -> HuffEntry {
        self.entries.get_masked(idx)
    }

    fn decode_description_inner(&mut self, src: &[u8]) -> Option<usize> {
        let &header_byte = src.first()?;
        let mut weights = Pow2Array::<u8, 256>::splat(0);

        let (num_weights, consumed) = if header_byte >= 128 {
            // Direct 4-bit weight representation (RFC 8878 §4.2.1.2).
            let count = usize::from(header_byte - 127);
            let weight_bytes = count.div_ceil(2);
            let total_bytes = 1 + weight_bytes;
            let packed = src.get(1..total_bytes)?;
            let mut i: usize = 0;
            for &byte in packed {
                weights.set_masked(i, byte >> 4);
                i = i.wrapping_add(1);
                if i < count {
                    weights.set_masked(i, byte & 0x0F);
                    i = i.wrapping_add(1);
                }
            }
            (count, total_bytes)
        } else {
            // FSE-compressed weights (RFC 8878 §4.2.1.1): two interleaved FSE states.
            let comp_size = usize::from(header_byte);
            if comp_size == 0 {
                return None;
            }
            let total_bytes = 1 + comp_size;
            let fse_data = src.get(1..total_bytes)?;

            let mut fse_table = FseDecTable::new_empty();
            let hdr_len = fse_table.decode_header_and_build_opt(
                fse_data,
                HUFF_FSE_MAX_SYMBOL,
                HUFF_FSE_MAX_LOG,
            )?;
            let bitstream = fse_data.get(hdr_len..)?;
            let mut br = RevBitCursor::new(bitstream)?;

            let acc_log = u32::from(fse_table.accuracy_log());
            let mut state1 = br.read_bits(acc_log)?.widen_to::<usize>();
            let mut state2 = br.read_bits(acc_log)?.widen_to::<usize>();
            let mut count: usize = 0;

            loop {
                let e1 = fse_table.entry_masked(state1);
                if count >= 255 {
                    return None;
                }
                weights.set_masked(count, e1.symbol);
                count = count.wrapping_add(1);
                if let Some(bits) = br.read_bits(u32::from(e1.num_bits)) {
                    state1 = usize::from(e1.baseline).wrapping_add(bits.widen_to::<usize>());
                } else {
                    let e2_last = fse_table.entry_masked(state2);
                    if count >= 255 {
                        return None;
                    }
                    weights.set_masked(count, e2_last.symbol);
                    count = count.wrapping_add(1);
                    break;
                }

                let e2 = fse_table.entry_masked(state2);
                if count >= 255 {
                    return None;
                }
                weights.set_masked(count, e2.symbol);
                count = count.wrapping_add(1);
                if let Some(bits) = br.read_bits(u32::from(e2.num_bits)) {
                    state2 = usize::from(e2.baseline).wrapping_add(bits.widen_to::<usize>());
                } else {
                    let e1_last = fse_table.entry_masked(state1);
                    if count >= 255 {
                        return None;
                    }
                    weights.set_masked(count, e1_last.symbol);
                    count = count.wrapping_add(1);
                    break;
                }
            }

            (count, total_bytes)
        };

        if !self.build_from_weights_inner(&mut weights, num_weights) {
            return None;
        }
        Some(consumed)
    }

    /// Decode an RFC 8878 §4.2.1 Huffman tree description from the start of `src`, populate `self`,
    /// and return the number of bytes consumed.
    pub(crate) fn decode_description(&mut self, src: &[u8]) -> Result<usize, ZstdError> {
        self.decode_description_inner(src).ok_or(ZstdError::CorruptHuffman)
    }

    fn build_from_weights_inner(
        &mut self,
        weights: &mut Pow2Array<u8, 256>,
        num_explicit: usize,
    ) -> bool {
        if num_explicit == 0 || num_explicit > 255 {
            return false;
        }

        // Pass 1 (branchless): validate every explicit weight <= MAX_HUFF_BITS (11) and count
        // rank frequencies. `rank_count[0]` counts absent symbols (scattered past present symbols
        // in Pass 2 with zero branches).
        let mut max_w: u8 = 0;
        let mut rank_count = Pow2Array::<u16, 16>::splat(0);
        let Some(exp_slice) = weights.as_slice().get(..num_explicit) else {
            return false;
        };
        for &w in exp_slice {
            if w > max_w {
                max_w = w;
            }
            let w_idx = usize::from(w);
            rank_count.set_masked(w_idx, rank_count.get_masked(w_idx).wrapping_add(1));
        }
        if max_w > MAX_HUFF_BITS {
            return false;
        }

        // Sum `2^(w - 1)` across weights `1..=11` (`weight_sum <= 255 * 1024 = 261_120`).
        let mut weight_sum: u32 = 0;
        for w in 1..=usize::from(MAX_HUFF_BITS) {
            let cnt = u32::from(rank_count.get_masked(w));
            weight_sum = weight_sum.wrapping_add(cnt << (w - 1));
        }
        if weight_sum == 0 {
            return false;
        }

        let max_bits_u32 = 32u32.wrapping_sub(weight_sum.leading_zeros());
        if max_bits_u32 == 0 || max_bits_u32 > u32::from(MAX_HUFF_BITS) {
            return false;
        }
        let max_bits = max_bits_u32.truncate_to::<u8>();
        let total = 1u32 << max_bits_u32;
        let left = total.wrapping_sub(weight_sum);
        if left == 0 || !left.is_power_of_two() {
            return false;
        }
        let last_weight_u32 = left.trailing_zeros().wrapping_add(1);
        if last_weight_u32 > max_bits_u32 {
            return false;
        }
        let last_weight = last_weight_u32.truncate_to::<u8>();
        weights.set_masked(num_explicit, last_weight);
        let lw_idx = usize::from(last_weight);
        rank_count.set_masked(lw_idx, rank_count.get_masked(lw_idx).wrapping_add(1));
        if last_weight > max_w {
            max_w = last_weight;
        }
        let total_syms = num_explicit + 1;

        let max_bits_usize = usize::from(max_bits);
        let max_w_usize = usize::from(max_w);
        let mut rank_origin = Pow2Array::<usize, 16>::splat(0);
        let mut sym_start = Pow2Array::<usize, 16>::splat(0);
        let mut sym_cursor = Pow2Array::<usize, 16>::splat(0);
        let mut cursor: usize = 0;
        let mut s_off: usize = 0;
        for w in 1..=max_bits_usize {
            rank_origin.set_masked(w, cursor);
            sym_start.set_masked(w, s_off);
            sym_cursor.set_masked(w, s_off);
            let count = usize::from(rank_count.get_masked(w));
            cursor = cursor.wrapping_add(count << (w - 1));
            s_off = s_off.wrapping_add(count);
        }
        rank_origin.set_masked(max_bits_usize + 1, cursor);
        sym_start.set_masked(max_bits_usize + 1, s_off);

        let table_size = 1usize << max_bits_usize;
        if cursor != table_size {
            return false;
        }

        // Pass 2 (branchless): group symbols by ascending weight in canonical ascending symbol
        // order. Absent symbols (`w == 0`) scatter into `[s_off..total_syms)` past all present
        // symbols (`s_off + rank_count[0] == total_syms <= 256`).
        let mut sorted_syms = Pow2Array::<u8, 256>::splat(0);
        sym_cursor.set_masked(0, s_off);
        if let Some(all_w) = weights.as_slice().get(..total_syms) {
            for (sym, &w) in all_w.iter().enumerate() {
                let w_usize = usize::from(w);
                let dst = sym_cursor.get_masked(w_usize);
                sorted_syms.set_masked(dst, sym.truncate_to::<u8>());
                sym_cursor.set_masked(w_usize, dst.wrapping_add(1));
            }
        }

        // Pass 3: populate X1 (`entries`) and X2 (`entries_x2`) in weight-grouped monotonic order.
        // Weight 1 (`num_bits == max_bits`, `span == 1`, `rem == 0`) never pairs with a second
        // symbol; write 1 entry per symbol directly with zero fills or lookups.
        let w1_end = sym_start.get_masked(2);
        if let Some(w1_syms) = sorted_syms.as_slice().get(..w1_end) {
            for (i, &s0) in w1_syms.iter().enumerate() {
                self.entries.set_masked(i, HuffEntry::new(s0, max_bits));
                self.entries_x2.set_masked(i, HuffEntryX2::new(u16::from(s0), max_bits, 8));
            }
        }

        // Weights `2..=max_w`: for any weight `w0`, `nb0 = max_bits + 1 - w0`,
        // `span0 = 1 << (w0 - 1)`, and a second symbol `s1` fits iff `w1 >= min_w1 = nb0 + 1`.
        // Since `rank_origin[min_w1]` is an exact multiple of `2^(min_w1 - 1) = 2^nb0`, slots
        // `[0..cut_j)` are contiguous single-symbol fills and `[cut_j..span0)` are branchless
        // 2-symbol entries (`base0.wrapping_add(tail[k])`, auto-vectorizing to NEON `add.4s`).
        if max_w_usize >= 2 {
            let entries_slice = self.entries.as_mut_slice();
            let x2_slice = self.entries_x2.as_mut_slice();
            let sorted_slice = sorted_syms.as_slice();
            let mut tail = [0u32; MAX_HUFF_TABLE_SIZE / 2];

            for w0 in 2..=max_w_usize {
                let s0_start = sym_start.get_masked(w0);
                let s0_end = sym_start.get_masked(w0 + 1);
                let count0 = s0_end.wrapping_sub(s0_start);
                if count0 == 0 {
                    continue;
                }
                let Some(s0_list) = sorted_slice.get(s0_start..s0_end) else {
                    continue;
                };
                let span0 = 1usize << (w0 - 1);
                let nb0_usize = max_bits_usize + 1 - w0;
                let nb0 = nb0_usize.truncate_to::<u8>();
                let min_w1 = nb0_usize + 1;
                let mut pos = rank_origin.get_masked(w0);

                if min_w1 > max_w_usize {
                    for &s0 in s0_list {
                        let end = pos.wrapping_add(span0);
                        if let Some(slots) = entries_slice.get_mut(pos..end) {
                            slots.fill(HuffEntry::new(s0, nb0));
                        }
                        if let Some(x2_slots) = x2_slice.get_mut(pos..end) {
                            x2_slots.fill(HuffEntryX2::new(u16::from(s0), nb0, 8));
                        }
                        pos = end;
                    }
                    continue;
                }

                let cut_j = rank_origin.get_masked(min_w1) >> nb0_usize;
                let two_len = span0.wrapping_sub(cut_j);

                if count0 == 1 {
                    if let Some(&s0) = s0_list.first() {
                        let end = pos.wrapping_add(span0);
                        let two_start = pos.wrapping_add(cut_j);
                        if let Some(slots) = entries_slice.get_mut(pos..end) {
                            slots.fill(HuffEntry::new(s0, nb0));
                        }
                        if cut_j > 0
                            && let Some(x2_one) = x2_slice.get_mut(pos..two_start)
                        {
                            x2_one.fill(HuffEntryX2::new(u16::from(s0), nb0, 8));
                        }
                        let base0 = u32::from(s0) | (u32::from(nb0) << 16) | (16u32 << 24);
                        let mut dst_pos = two_start;
                        for w1 in min_w1..=max_w_usize {
                            let s1_start = sym_start.get_masked(w1);
                            let s1_end = sym_start.get_masked(w1 + 1);
                            if s1_start == s1_end {
                                continue;
                            }
                            let nb1 = (max_bits_usize + 1 - w1).truncate_to::<u8>();
                            let nb1_shifted = u32::from(nb1) << 16;
                            let rep = 1usize << (w1 - min_w1);
                            if let Some(s1_list) = sorted_slice.get(s1_start..s1_end) {
                                for &s1 in s1_list {
                                    let entry = HuffEntryX2(
                                        base0.wrapping_add((u32::from(s1) << 8) | nb1_shifted),
                                    );
                                    let next_pos = dst_pos.wrapping_add(rep);
                                    if let Some(dst) = x2_slice.get_mut(dst_pos..next_pos) {
                                        dst.fill(entry);
                                    }
                                    dst_pos = next_pos;
                                }
                            }
                        }
                    }
                    continue;
                }

                // Multiple symbols share weight `w0`: build `tail[0..two_len]` once, then add
                // `base0` linearly across each symbol's 2-symbol slice (NEON `add.4s`).
                let mut t_pos: usize = 0;
                for w1 in min_w1..=max_w_usize {
                    let s1_start = sym_start.get_masked(w1);
                    let s1_end = sym_start.get_masked(w1 + 1);
                    if s1_start == s1_end {
                        continue;
                    }
                    let nb1 = (max_bits_usize + 1 - w1).truncate_to::<u8>();
                    let nb1_shifted = u32::from(nb1) << 16;
                    let rep = 1usize << (w1 - min_w1);
                    if let Some(s1_list) = sorted_slice.get(s1_start..s1_end) {
                        for &s1 in s1_list {
                            let val = (u32::from(s1) << 8) | nb1_shifted;
                            let next_t = t_pos.wrapping_add(rep);
                            if let Some(dst) = tail.get_mut(t_pos..next_t) {
                                dst.fill(val);
                            }
                            t_pos = next_t;
                        }
                    }
                }

                let src_tail = tail.get(..two_len).unwrap_or(&[]);
                for &s0 in s0_list {
                    let end = pos.wrapping_add(span0);
                    let two_start = pos.wrapping_add(cut_j);
                    if let Some(slots) = entries_slice.get_mut(pos..end) {
                        slots.fill(HuffEntry::new(s0, nb0));
                    }
                    if cut_j > 0
                        && let Some(x2_one) = x2_slice.get_mut(pos..two_start)
                    {
                        x2_one.fill(HuffEntryX2::new(u16::from(s0), nb0, 8));
                    }
                    let base0 = u32::from(s0) | (u32::from(nb0) << 16) | (16u32 << 24);
                    if let Some(dst_two) = x2_slice.get_mut(two_start..end) {
                        for (dst, &t) in dst_two.iter_mut().zip(src_tail.iter()) {
                            *dst = HuffEntryX2(base0.wrapping_add(t));
                        }
                    }
                    pos = end;
                }
            }
        }

        self.max_bits = max_bits;
        true
    }

    /// Decode the exact remaining tail of one reverse Huffman stream into `dst`, checking that the
    /// stream terminates with zero trailing bits. Drains any remaining `>= 8`-byte slack with
    /// `X2` quads first (handling streams left behind when one of 4 streams ends early), then
    /// finishes the final `< 8`-byte tail with exact `X1` checking.
    #[cold]
    #[inline(never)]
    fn decode_tail(&self, mut br: RevBitCursor<'_>, dst: &mut [u8]) -> bool {
        let mut sw = SliceWriter::new(dst);
        if sw.has_8() && br.has_slack8() {
            let shift_r = 64u32 - u32::from(self.max_bits);
            let mut bf = br.open_slack8();
            while sw.has_8() {
                let Some(b) = bf.reload_window_slack8() else {
                    break;
                };
                decode_x2_quad(&self.entries_x2, b, &mut bf, &mut sw, shift_r);
            }
            bf.commit_to(&mut br);
        }
        let max_bits_u32 = u32::from(self.max_bits);
        for slot in sw.into_rest() {
            let idx = br.peek_bits_padded(max_bits_u32).widen_to::<usize>();
            let entry = self.lookup_fast(idx);
            if entry.num_bits() == 0 || !br.consume_bits(u32::from(entry.num_bits())) {
                return false;
            }
            *slot = entry.symbol();
        }
        br.is_empty()
    }

    /// Decode a single reverse Huffman bitstream directly into pre-sized `dst` using a
    /// register-resident [`RevBitSlack8`] cursor and double-symbol `X2` lookups (4 lookups =
    /// 4..8 bytes per 8-byte batched store).
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

        if sw.has_8() && br.has_slack8() {
            let mut bf = br.open_slack8();
            while sw.has_8() {
                let Some(b) = bf.reload_window_slack8() else {
                    break;
                };
                decode_x2_quad(&self.entries_x2, b, &mut bf, &mut sw, shift_r);
            }
            bf.commit_to(&mut br);
        }

        if !self.decode_tail(br, sw.into_rest()) {
            return Err(ZstdError::CorruptHuffman);
        }
        Ok(())
    }

    fn decode_4_streams_inner(&self, src: &[u8], dst: &mut [u8]) -> bool {
        let Some((jump, rest)) = src.split_at_checked(6) else {
            return false;
        };
        let Some(&[a0, a1, b0, b1, c0, c1]) = jump.first_chunk::<6>() else {
            return false;
        };
        let s1_len = usize::from(u16::from_le_bytes([a0, a1]));
        let s2_len = usize::from(u16::from_le_bytes([b0, b1]));
        let s3_len = usize::from(u16::from_le_bytes([c0, c1]));

        let Some((st1, rest)) = rest.split_at_checked(s1_len) else {
            return false;
        };
        let Some((st2, rest)) = rest.split_at_checked(s2_len) else {
            return false;
        };
        let Some((st3, st4)) = rest.split_at_checked(s3_len) else {
            return false;
        };

        let expected_len = dst.len();
        let seg = expected_len.div_ceil(4);
        let Some(three_segs) = seg.checked_mul(3) else {
            return false;
        };
        let Some(last_seg) = expected_len.checked_sub(three_segs) else {
            return false;
        };

        let Some((d1, rest)) = dst.split_at_mut_checked(seg) else {
            return false;
        };
        let Some((d2, rest)) = rest.split_at_mut_checked(seg) else {
            return false;
        };
        let Some((d3, d4)) = rest.split_at_mut_checked(seg) else {
            return false;
        };
        if d4.len() != last_seg {
            return false;
        }

        let Some(br1) = RevBitCursor::new(st1) else {
            return false;
        };
        let Some(br2) = RevBitCursor::new(st2) else {
            return false;
        };
        let Some(br3) = RevBitCursor::new(st3) else {
            return false;
        };
        let Some(br4) = RevBitCursor::new(st4) else {
            return false;
        };

        let rem = [d1.len(), d2.len(), d3.len(), d4.len()];
        let mut st = Huff4FastState { br1, br2, br3, br4, d1, d2, d3, d4, rem };

        if st.d1.len() >= 8
            && st.d2.len() >= 8
            && st.d3.len() >= 8
            && st.d4.len() >= 8
            && st.br1.has_slack8()
            && st.br2.has_slack8()
            && st.br3.has_slack8()
            && st.br4.has_slack8()
        {
            match self.max_bits {
                11 => decode_4_streams_fast_leaf_11(&self.entries_x2, &mut st),
                10 => decode_4_streams_fast_leaf_10(&self.entries_x2, &mut st),
                mb => {
                    let shift_r = 64u32 - u32::from(mb);
                    decode_4_streams_fast_leaf_var(&self.entries_x2, shift_r, &mut st);
                }
            }
        }

        let [rem1, rem2, rem3, rem4] = st.rem;
        let w1 = st.d1.len().saturating_sub(rem1);
        let w2 = st.d2.len().saturating_sub(rem2);
        let w3 = st.d3.len().saturating_sub(rem3);
        let w4 = st.d4.len().saturating_sub(rem4);
        let (Some(t1), Some(t2), Some(t3), Some(t4)) = (
            st.d1.get_mut(w1..),
            st.d2.get_mut(w2..),
            st.d3.get_mut(w3..),
            st.d4.get_mut(w4..),
        ) else {
            return false;
        };

        // Drain the exact remaining tail of each stream (X2 fast drain + X1 exact checking).
        self.decode_tail(st.br1, t1)
            && self.decode_tail(st.br2, t2)
            && self.decode_tail(st.br3, t3)
            && self.decode_tail(st.br4, t4)
    }

    /// Decode an RFC 8878 §4.2.2 4-stream Huffman literals payload directly into pre-sized `dst`
    /// using 4 independent double-symbol `HUF_4X2` streams, register-resident [`RevBitSlack8`]
    /// reloads, and `SliceWriter` 64-bit batched stores (16..32 symbols per iteration).
    pub(crate) fn decode_4_streams_into_slice(
        &self,
        src: &[u8],
        dst: &mut [u8],
    ) -> Result<(), ZstdError> {
        if self.max_bits == 0 || self.max_bits > MAX_HUFF_BITS {
            return Err(ZstdError::CorruptHuffman);
        }
        if !self.decode_4_streams_inner(src, dst) {
            return Err(ZstdError::CorruptHuffman);
        }
        Ok(())
    }
}

/// Out-of-loop handoff state passed by `&mut` (one pointer, zero aggregate copy) into the
/// zero-call 4-stream Huffman fast-loop leaves.
struct Huff4FastState<'a, 'b> {
    br1: RevBitCursor<'a>,
    br2: RevBitCursor<'a>,
    br3: RevBitCursor<'a>,
    br4: RevBitCursor<'a>,
    d1: &'b mut [u8],
    d2: &'b mut [u8],
    d3: &'b mut [u8],
    d4: &'b mut [u8],
    rem: [usize; 4],
}

/// Execute 4 double-symbol `X2` lookups (`4..=8` decoded symbols, consuming
/// `<= 4 * max_bits <= 44` bits) from an MSB-aligned 64-bit bit window `b` (`>= 57` live
/// bits), advance `bf`, and emit one batched 64-bit little-endian store into `sw`.
///
/// Indexes `table` directly via `(b >> shift_r).truncate_to::<usize>()` with no intermediate
/// `u16` truncation; when `shift_r >= 53` (`max_bits <= 11`) is a compile-time constant,
/// `b >> shift_r` is already in `0..2048` so the compiler eliminates `Pow2Array::get_masked`'s
/// `& 0x7ff` mask too.
///
/// Accumulates packed metadata `meta = (e0 >> 16) + (e1 >> 16) + (e2 >> 16) + (e3 >> 16)` in one
/// SWAR register (`low 6 bits = total bits consumed <= 44 < 64`, `bits 8..16 = total output bits
/// <= 64`, zero carry) and packs symbols in reverse Horner order (`e3 -> e2 -> e1 -> e0`),
/// eliminating both the separate `sh` accumulator and per-entry `n0..n2` live values so peak
/// scratch pressure stays within a zero-call leaf's 27-GPR budget with zero stack spills.
#[inline(always)]
fn decode_x2_quad(
    table: &Pow2Array<HuffEntryX2, MAX_HUFF_TABLE_SIZE>,
    b: u64,
    bf: &mut RevBitSlack8<'_>,
    sw: &mut SliceWriter<'_>,
    shift_r: u32,
) {
    let e0 = table.get_masked((b >> shift_r).truncate_to::<usize>());
    let mut meta = e0.meta();
    let b1 = b.wrapping_shl(meta);

    let e1 = table.get_masked((b1 >> shift_r).truncate_to::<usize>());
    meta = meta.wrapping_add(e1.meta());
    let b2 = b.wrapping_shl(meta);

    let e2 = table.get_masked((b2 >> shift_r).truncate_to::<usize>());
    meta = meta.wrapping_add(e2.meta());
    let b3 = b.wrapping_shl(meta);

    let e3 = table.get_masked((b3 >> shift_r).truncate_to::<usize>());
    meta = meta.wrapping_add(e3.meta());

    // Reverse Horner packing (`e3 -> e2 -> e1 -> e0`): upper metadata bits of `e3.0` stay
    // strictly past `meta >> 8` (`total_sym_bits`) and are never committed by `write_u64_le_adv`,
    // avoiding both an `e3 & 0xFFFF` mask and a separate `sh` accumulator.
    let out_word = u64::from(e3.0);
    let out_word = out_word.wrapping_shl(e2.sym_bits()) | e2.packed_u64();
    let out_word = out_word.wrapping_shl(e1.sym_bits()) | e1.packed_u64();
    let out_word = out_word.wrapping_shl(e0.sym_bits()) | e0.packed_u64();

    bf.advance(u64::from(meta & 0xFF));
    let _ = sw.write_u64_le_adv(out_word, (meta >> 11).widen_to::<usize>());
}

#[inline(always)]
fn decode_4_streams_fast_leaf_inner(
    table: &Pow2Array<HuffEntryX2, MAX_HUFF_TABLE_SIZE>,
    shift_r: u32,
    st: &mut Huff4FastState<'_, '_>,
) {
    let mut bf1 = st.br1.open_slack8();
    let mut bf2 = st.br2.open_slack8();
    let mut bf3 = st.br3.open_slack8();
    let mut bf4 = st.br4.open_slack8();
    let mut sw1 = SliceWriter::new(st.d1);
    let mut sw2 = SliceWriter::new(st.d2);
    let mut sw3 = SliceWriter::new(st.d3);
    let mut sw4 = SliceWriter::new(st.d4);

    // 1x round-robin across the 4 independent streams (16..32 symbols per iteration):
    // zero-call leaf unlocks `X30` (27 GPRs on AArch64) and takes `&entries_x2` directly (21
    // loop-carried GPRs for constant `shift_r`, 22 for variable `shift_r`), while `has_8()`
    // statically proves `rem >= 8` for each `write_u64_le_adv` call (advances by `<= 8` bytes).
    while sw1.has_8() && sw2.has_8() && sw3.has_8() && sw4.has_8() {
        let Some(b1) = bf1.reload_window_slack8() else {
            break;
        };
        decode_x2_quad(table, b1, &mut bf1, &mut sw1, shift_r);
        let Some(b2) = bf2.reload_window_slack8() else {
            break;
        };
        decode_x2_quad(table, b2, &mut bf2, &mut sw2, shift_r);
        let Some(b3) = bf3.reload_window_slack8() else {
            break;
        };
        decode_x2_quad(table, b3, &mut bf3, &mut sw3, shift_r);
        let Some(b4) = bf4.reload_window_slack8() else {
            break;
        };
        decode_x2_quad(table, b4, &mut bf4, &mut sw4, shift_r);
    }

    bf1.commit_to(&mut st.br1);
    bf2.commit_to(&mut st.br2);
    bf3.commit_to(&mut st.br3);
    bf4.commit_to(&mut st.br4);
    st.rem = [sw1.rem(), sw2.rem(), sw3.rem(), sw4.rem()];
}

/// Zero-call leaf fast loop specialized for 11-bit Huffman tables (`shift_r == 53`).
#[inline(never)]
fn decode_4_streams_fast_leaf_11(
    table: &Pow2Array<HuffEntryX2, MAX_HUFF_TABLE_SIZE>,
    st: &mut Huff4FastState<'_, '_>,
) {
    decode_4_streams_fast_leaf_inner(table, 53, st);
}

/// Zero-call leaf fast loop specialized for 10-bit Huffman tables (`shift_r == 54`).
#[inline(never)]
fn decode_4_streams_fast_leaf_10(
    table: &Pow2Array<HuffEntryX2, MAX_HUFF_TABLE_SIZE>,
    st: &mut Huff4FastState<'_, '_>,
) {
    decode_4_streams_fast_leaf_inner(table, 54, st);
}

/// Zero-call leaf fast loop for general `<= 9`-bit Huffman tables (`shift_r == 64 - max_bits`).
#[inline(never)]
fn decode_4_streams_fast_leaf_var(
    table: &Pow2Array<HuffEntryX2, MAX_HUFF_TABLE_SIZE>,
    shift_r: u32,
    st: &mut Huff4FastState<'_, '_>,
) {
    decode_4_streams_fast_leaf_inner(table, shift_r, st);
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

/// Reusable Huffman encoding table cache enabling `Treeless_Literals_Block` (`lit_type = 3`)
/// across blocks within a frame (skips histogram sorting, tree build, and weight header).
///
/// Each entry packs `code | (num_bits << 16)` (`0` when symbol is absent), matching
/// [`blaze_own::huff_encode_stream_raw`] and [`blaze_own::huff_encode_stream_checked_raw`]
/// directly with zero per-stream re-packing.
pub(crate) struct HuffEncCache {
    packed_codes: [u32; 256],
    pending_packed: [u32; 256],
    prev_lit_len: u32,
    prev_comp_size: u32,
    pending_lit_len: u32,
    pending_comp_size: u32,
    valid: bool,
    has_pending: bool,
}

impl HuffEncCache {
    pub(crate) const fn new() -> Self {
        Self {
            packed_codes: [0u32; 256],
            pending_packed: [0u32; 256],
            prev_lit_len: 0,
            prev_comp_size: 0,
            pending_lit_len: 0,
            pending_comp_size: 0,
            valid: false,
            has_pending: false,
        }
    }

    #[inline]
    pub(crate) fn reset(&mut self) {
        self.prev_lit_len = 0;
        self.prev_comp_size = 0;
        self.pending_lit_len = 0;
        self.pending_comp_size = 0;
        self.valid = false;
        self.has_pending = false;
    }

    /// Commit any freshly built table after the compressed block is actually emitted (not dropped
    /// in favor of a raw block fallback).
    #[inline]
    pub(crate) fn commit(&mut self) {
        if self.has_pending {
            self.packed_codes = self.pending_packed;
            self.prev_lit_len = self.pending_lit_len;
            self.prev_comp_size = self.pending_comp_size;
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

    let Some(mut state1) = enc.init_state_biased(last_even_w) else {
        return Ok(false);
    };
    let Some(mut state2) = enc.init_state_biased(last_odd_w) else {
        return Ok(false);
    };

    let mut bw = BitWriter::try_with_capacity_in(n, alloc)?;
    for i in (0..(n - 2)).rev() {
        let w = explicit_weights.get(i).copied().unwrap_or(0);
        if i.is_multiple_of(2) {
            let (prev, bits, nb) = enc.encode_step_biased(w, state1);
            state1 = prev;
            bw.write_bits(bits, nb)?;
        } else {
            let (prev, bits, nb) = enc.encode_step_biased(w, state2);
            state2 = prev;
            bw.write_bits(bits, nb)?;
        }
    }
    bw.write_bits(enc.finish_state(state2), u32::from(acc_log))?;
    bw.write_bits(enc.finish_state(state1), u32::from(acc_log))?;
    bw.finish_into(&mut fse_buf)?;

    if fse_buf.is_empty() || fse_buf.len() >= 128 {
        return Ok(false);
    }
    let hdr_byte = u8::try_from(fse_buf.len()).map_err(|_| ZstdError::CorruptHuffman)?;
    dst.try_push(hdr_byte).map_err(|r| r.why)?;
    dst.try_extend_from_slice(fse_buf.as_slice())?;
    Ok(true)
}

/// Encode `literals` (1 stream if `< 256`, 4 streams otherwise) into `payload` using
/// `packed_codes`, aborting with `Ok(false)` if any byte has `num_bits == 0` or a stream exceeds
/// `u16::MAX` bytes. Enables zero-histogram `Treeless` reuse in `Fast` mode.
fn encode_huff_streams_checked<A: Allocator>(
    literals: &[u8],
    packed_codes: &[u32; 256],
    payload: &mut Vec<u8, A>,
) -> Result<bool, ZstdError> {
    let lit_len = literals.len();
    if lit_len < 256 {
        return Ok(blaze_own::huff_encode_stream_checked_raw(literals, packed_codes, payload)?);
    }

    let seg = lit_len.div_ceil(4);
    let (st1, rest) = literals.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
    let (st2, rest) = rest.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
    let (st3, st4) = rest.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;

    let jump_pos = payload.len();
    payload.try_extend_from_slice(&[0u8; 6])?;

    let s1_start = payload.len();
    if !blaze_own::huff_encode_stream_checked_raw(st1, packed_codes, payload)? {
        return Ok(false);
    }
    let s1_len = payload.len() - s1_start;

    let s2_start = payload.len();
    if !blaze_own::huff_encode_stream_checked_raw(st2, packed_codes, payload)? {
        return Ok(false);
    }
    let s2_len = payload.len() - s2_start;

    let s3_start = payload.len();
    if !blaze_own::huff_encode_stream_checked_raw(st3, packed_codes, payload)? {
        return Ok(false);
    }
    let s3_len = payload.len() - s3_start;

    if !blaze_own::huff_encode_stream_checked_raw(st4, packed_codes, payload)? {
        return Ok(false);
    }

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
    Ok(true)
}

/// Encode `literals` (1 stream if `< 256`, 4 streams otherwise) into `payload` using
/// `packed_codes` (caller verified every byte in `literals` is present in `packed_codes`).
/// Returns `Ok(false)` only if a stream exceeds `u16::MAX` bytes.
fn encode_huff_streams<A: Allocator>(
    literals: &[u8],
    packed_codes: &[u32; 256],
    payload: &mut Vec<u8, A>,
) -> Result<bool, ZstdError> {
    let lit_len = literals.len();
    if lit_len < 256 {
        blaze_own::huff_encode_stream_raw(literals, packed_codes, payload)?;
        return Ok(true);
    }

    let seg = lit_len.div_ceil(4);
    let (st1, rest) = literals.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
    let (st2, rest) = rest.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
    let (st3, st4) = rest.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;

    let jump_pos = payload.len();
    payload.try_extend_from_slice(&[0u8; 6])?;

    let s1_start = payload.len();
    blaze_own::huff_encode_stream_raw(st1, packed_codes, payload)?;
    let s1_len = payload.len() - s1_start;

    let s2_start = payload.len();
    blaze_own::huff_encode_stream_raw(st2, packed_codes, payload)?;
    let s2_len = payload.len() - s2_start;

    let s3_start = payload.len();
    blaze_own::huff_encode_stream_raw(st3, packed_codes, payload)?;
    let s3_len = payload.len() - s3_start;

    blaze_own::huff_encode_stream_raw(st4, packed_codes, payload)?;

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
    Ok(true)
}

#[inline]
fn emit_compressed_literals<A: Allocator>(
    lit_type: u32,
    use_4_streams: bool,
    lit_len: usize,
    payload: &[u8],
    out: &mut Vec<u8, A>,
) -> Result<(), ZstdError> {
    let comp_size = payload.len();
    if !use_4_streams && lit_len < 1_024 && comp_size < 1_024 {
        let r = lit_len.truncate_to::<u32>();
        let c = comp_size.truncate_to::<u32>();
        let v = lit_type | (r << 4) | (c << 14);
        let [b0, b1, b2, _] = v.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2])?;
    } else if lit_len < 1_024 && comp_size < 1_024 {
        let r = lit_len.truncate_to::<u32>();
        let c = comp_size.truncate_to::<u32>();
        let v = lit_type | (1u32 << 2) | (r << 4) | (c << 14);
        let [b0, b1, b2, _] = v.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2])?;
    } else if lit_len < 16_384 && comp_size < 16_384 {
        let r = lit_len.truncate_to::<u32>();
        let c = comp_size.truncate_to::<u32>();
        let v = lit_type | (2u32 << 2) | (r << 4) | (c << 18);
        out.try_extend_from_slice(&v.to_le_bytes())?;
    } else {
        let r = lit_len.widen_to::<u64>();
        let c = comp_size.widen_to::<u64>();
        let v = u64::from(lit_type) | (3u64 << 2) | (r << 4) | (c << 22);
        let [b0, b1, b2, b3, b4, ..] = v.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2, b3, b4])?;
    }
    out.try_extend_from_slice(payload)?;
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

    // Fast-mode zero-histogram Treeless reuse (`lit_type = 3`) on blocks 1..N:
    // encode directly with cached `packed_codes`, skipping the 4-way byte histogram completely.
    // Accepts Treeless when the ratio stays within 5% of the trained block's ratio
    // (`prev_lit_len`/`prev_comp_size`) and saves at least the literals header; otherwise falls
    // through to rebuild a fresh Huffman table immediately.
    if fast_mode && cache.valid && lit_len >= 64 {
        payload.clear();
        if encode_huff_streams_checked(literals, &cache.packed_codes, payload)? {
            let comp_size = payload.len();
            let hdr_len = if lit_len < 1_024 && comp_size < 1_024 {
                3
            } else if lit_len < 16_384 && comp_size < 16_384 {
                4
            } else {
                5
            };
            let within_drift = cache.prev_lit_len > 0
                && comp_size.widen_to::<u64>()
                    * u64::from(cache.prev_lit_len)
                    * 100u64
                    <= u64::from(cache.prev_comp_size)
                        * lit_len.widen_to::<u64>()
                        * 107u64;
            if within_drift && comp_size + hdr_len < lit_len {
                emit_compressed_literals(3, lit_len >= 256, lit_len, payload.as_slice(), out)?;
                cache.has_pending = false;
                return Ok(true);
            }
        }
        payload.clear();
    }

    // 4-way unrolled histogram to avoid store-to-load forwarding stalls on repeated bytes.
    let mut c0 = [0u32; 256];
    let mut c1 = [0u32; 256];
    let mut c2 = [0u32; 256];
    let mut c3 = [0u32; 256];
    let (chunks, rem) = literals.as_chunks::<4>();
    for &[b0, b1, b2, b3] in chunks {
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
    for &b in rem {
        if let Some(s) = c0.get_mut(usize::from(b)) {
            *s += 1;
        }
    }

    let mut counts = [0u32; 256];
    let mut distinct: usize = 0;
    let mut only_byte: u8 = 0;
    let mut can_reuse = cache.valid && !fast_mode;
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
            only_byte = i.truncate_to::<u8>();
            if can_reuse {
                let nb = cache.packed_codes.get(i).copied().unwrap_or(0) >> 16;
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
            let v = (lit_len.truncate_to::<u16>() << 4) | 0b0101;
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
    let reuse_bytes = (reuse_bits >> 3).truncate_to::<usize>() + 16;

    let mut use_treeless = false;
    let mut fresh_packed = [0u32; 256];

    if fast_mode {
        // Seed printable ASCII + whitespace whenever fast-mode builds a tree so subsequent
        // source/text blocks take the 1-pass Treeless path without aborting on a rare punctuation byte.
        for (b, c) in counts.iter_mut().enumerate() {
            if *c == 0 && (b == 9 || b == 10 || b == 13 || (32..=126).contains(&b)) {
                *c = 1;
            }
        }
    }
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
    let est_new_bytes = (est_bits >> 3).truncate_to::<usize>() + est_hdr_bytes + 12;

    if can_reuse && reuse_bytes <= est_new_bytes + (lit_len >> 7) && reuse_bytes + 8 < lit_len {
        use_treeless = true;
    } else {
        if est_new_bytes >= lit_len {
            return Ok(false);
        }

        // Build canonical packed encoding table matching `HuffTable::build_from_weights_inner`.
        let mut rank_count = Pow2Array::<u16, 16>::splat(0);
        for &w in weights.get(..=max_sym).ok_or(ZstdError::CorruptHuffman)? {
            if w > 0 {
                let w_idx = usize::from(w);
                rank_count.set_masked(w_idx, rank_count.get_masked(w_idx).wrapping_add(1));
            }
        }
        let max_bits_usize = usize::from(max_bits);
        let mut rank_start = Pow2Array::<usize, 16>::splat(0);
        let mut cursor: usize = 0;
        for w in 1..=max_bits_usize {
            rank_start.set_masked(w, cursor);
            let cnt = usize::from(rank_count.get_masked(w));
            cursor = cursor.wrapping_add(cnt << (w - 1));
        }

        for sym in 0..=max_sym {
            let w = weights.get(sym).copied().unwrap_or(0);
            if w == 0 {
                continue;
            }
            let w_usize = usize::from(w);
            let span = 1usize << (w_usize - 1);
            let start = rank_start.get_masked(w_usize);
            rank_start.set_masked(w_usize, start.wrapping_add(span));
            let code_val = (start >> (w_usize - 1)).truncate_to::<u16>();
            let num_bits = max_bits.wrapping_add(1).wrapping_sub(w);
            if let Some(slot) = fresh_packed.get_mut(sym) {
                *slot = u32::from(code_val) | (u32::from(num_bits) << 16);
            }
        }

        // Write tree description into `payload`.
        let num_explicit = max_sym;
        let explicit_slice = weights.get(..num_explicit).ok_or(ZstdError::CorruptHuffman)?;
        if num_explicit <= 128 {
            let header_byte = (127usize + num_explicit).truncate_to::<u8>();
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

    let stream_start = payload.len();
    let active_packed = if use_treeless { &cache.packed_codes } else { &fresh_packed };
    if !encode_huff_streams(literals, active_packed, payload)? {
        return Ok(false);
    }

    let comp_size = payload.len();
    let stream_comp_size = comp_size.saturating_sub(stream_start);
    if comp_size + 8 >= lit_len {
        return Ok(false);
    }

    let lit_type: u32 = if use_treeless { 3 } else { 2 };
    emit_compressed_literals(lit_type, lit_len >= 256, lit_len, payload.as_slice(), out)?;
    if !use_treeless {
        cache.pending_packed = fresh_packed;
        cache.pending_lit_len = lit_len.saturate::<u32>();
        cache.pending_comp_size = stream_comp_size.saturate::<u32>();
        cache.has_pending = true;
    } else {
        cache.has_pending = false;
    }
    Ok(true)
}
