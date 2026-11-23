//! RFC 8878 §4.2 Huffman tree description codec and 1-stream / 4-stream literals codec.

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::zstd::ZstdError;
use crate::zstd::fse::{BitWriter, FseDecTable, FseEncTable, RevBitReader, write_fse_header};

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

/// Canonical flat Huffman lookup table (up to 11-bit codes / 2,048 entries, zero heap allocation).
pub(crate) struct HuffTable {
    entries: [HuffEntry; MAX_HUFF_TABLE_SIZE],
    max_bits: u8,
}

impl HuffTable {
    pub(crate) const fn new_empty() -> Self {
        Self { entries: [HuffEntry::ZERO; MAX_HUFF_TABLE_SIZE], max_bits: 0 }
    }

    #[inline(always)]
    fn lookup_fast(&self, idx: usize) -> HuffEntry {
        self.entries.get(idx & HUFF_TABLE_MASK).copied().unwrap_or(HuffEntry::ZERO)
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

        self.max_bits = max_bits;
        Ok(())
    }

    /// Decode the exact remaining tail of one reverse Huffman stream into `dst`, checking that the
    /// stream terminates with zero trailing bits.
    #[inline]
    fn decode_tail(&self, br: &mut RevBitReader<'_>, dst: &mut [u8]) -> Result<(), ZstdError> {
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

    /// Decode a single reverse Huffman bitstream directly into pre-sized `dst`, unrolled 4
    /// symbols per bulk 8-byte reload.
    pub(crate) fn decode_1_stream_into_slice(
        &self,
        src: &[u8],
        dst: &mut [u8],
    ) -> Result<(), ZstdError> {
        if self.max_bits == 0 || self.max_bits > MAX_HUFF_BITS {
            return Err(ZstdError::CorruptHuffman);
        }
        let mut br = RevBitReader::new(src).ok_or(ZstdError::CorruptHuffman)?;
        let shift_r = 64u32 - u32::from(self.max_bits);
        let mut pos: usize = 0;

        for chunk in dst.chunks_exact_mut(4) {
            br.reload();
            if !br.has_44_bits() {
                break;
            }
            let e0 = self.lookup_fast(br.peek_fast(shift_r));
            br.advance_fast(u32::from(e0.num_bits));
            let e1 = self.lookup_fast(br.peek_fast(shift_r));
            br.advance_fast(u32::from(e1.num_bits));
            let e2 = self.lookup_fast(br.peek_fast(shift_r));
            br.advance_fast(u32::from(e2.num_bits));
            let e3 = self.lookup_fast(br.peek_fast(shift_r));
            br.advance_fast(u32::from(e3.num_bits));

            if let Some(d4) = chunk.first_chunk_mut::<4>() {
                *d4 = [e0.symbol, e1.symbol, e2.symbol, e3.symbol];
            }
            pos += 4;
        }

        let tail = dst.get_mut(pos..).ok_or(ZstdError::CorruptHuffman)?;
        self.decode_tail(&mut br, tail)
    }

    /// Decode an RFC 8878 §4.2.2 4-stream Huffman literals payload directly into pre-sized `dst`
    /// with 4-way interleaved instruction-level parallelism (16 symbols per reload batch,
    /// packed 32-bit stores per stream).
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

        let mut br1 = RevBitReader::new(st1).ok_or(ZstdError::CorruptHuffman)?;
        let mut br2 = RevBitReader::new(st2).ok_or(ZstdError::CorruptHuffman)?;
        let mut br3 = RevBitReader::new(st3).ok_or(ZstdError::CorruptHuffman)?;
        let mut br4 = RevBitReader::new(st4).ok_or(ZstdError::CorruptHuffman)?;

        let (d1, rest) = dst.split_at_mut_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
        let (d2, rest) = rest.split_at_mut_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
        let (d3, d4) = rest.split_at_mut_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
        if d4.len() != last_seg {
            return Err(ZstdError::CorruptHuffman);
        }

        let shift_r = 64u32 - u32::from(self.max_bits);
        let mut pos: usize = 0;

        let mut it1 = d1.chunks_exact_mut(4);
        let mut it2 = d2.chunks_exact_mut(4);
        let mut it3 = d3.chunks_exact_mut(4);
        let mut it4 = d4.chunks_exact_mut(4);

        while let (Some(c1), Some(c2), Some(c3), Some(c4)) =
            (it1.next(), it2.next(), it3.next(), it4.next())
        {
            br1.reload();
            br2.reload();
            br3.reload();
            br4.reload();
            if !(br1.has_44_bits() && br2.has_44_bits() && br3.has_44_bits() && br4.has_44_bits()) {
                break;
            }

            // Step 0 across all 4 independent streams.
            let e10 = self.lookup_fast(br1.peek_fast(shift_r));
            let e20 = self.lookup_fast(br2.peek_fast(shift_r));
            let e30 = self.lookup_fast(br3.peek_fast(shift_r));
            let e40 = self.lookup_fast(br4.peek_fast(shift_r));
            br1.advance_fast(u32::from(e10.num_bits));
            br2.advance_fast(u32::from(e20.num_bits));
            br3.advance_fast(u32::from(e30.num_bits));
            br4.advance_fast(u32::from(e40.num_bits));

            // Step 1
            let e11 = self.lookup_fast(br1.peek_fast(shift_r));
            let e21 = self.lookup_fast(br2.peek_fast(shift_r));
            let e31 = self.lookup_fast(br3.peek_fast(shift_r));
            let e41 = self.lookup_fast(br4.peek_fast(shift_r));
            br1.advance_fast(u32::from(e11.num_bits));
            br2.advance_fast(u32::from(e21.num_bits));
            br3.advance_fast(u32::from(e31.num_bits));
            br4.advance_fast(u32::from(e41.num_bits));

            // Step 2
            let e12 = self.lookup_fast(br1.peek_fast(shift_r));
            let e22 = self.lookup_fast(br2.peek_fast(shift_r));
            let e32 = self.lookup_fast(br3.peek_fast(shift_r));
            let e42 = self.lookup_fast(br4.peek_fast(shift_r));
            br1.advance_fast(u32::from(e12.num_bits));
            br2.advance_fast(u32::from(e22.num_bits));
            br3.advance_fast(u32::from(e32.num_bits));
            br4.advance_fast(u32::from(e42.num_bits));

            // Step 3
            let e13 = self.lookup_fast(br1.peek_fast(shift_r));
            let e23 = self.lookup_fast(br2.peek_fast(shift_r));
            let e33 = self.lookup_fast(br3.peek_fast(shift_r));
            let e43 = self.lookup_fast(br4.peek_fast(shift_r));
            br1.advance_fast(u32::from(e13.num_bits));
            br2.advance_fast(u32::from(e23.num_bits));
            br3.advance_fast(u32::from(e33.num_bits));
            br4.advance_fast(u32::from(e43.num_bits));

            if let Some(p1) = c1.first_chunk_mut::<4>() {
                *p1 = [e10.symbol, e11.symbol, e12.symbol, e13.symbol];
            }
            if let Some(p2) = c2.first_chunk_mut::<4>() {
                *p2 = [e20.symbol, e21.symbol, e22.symbol, e23.symbol];
            }
            if let Some(p3) = c3.first_chunk_mut::<4>() {
                *p3 = [e30.symbol, e31.symbol, e32.symbol, e33.symbol];
            }
            if let Some(p4) = c4.first_chunk_mut::<4>() {
                *p4 = [e40.symbol, e41.symbol, e42.symbol, e43.symbol];
            }

            pos += 4;
        }

        // Drain the exact remaining tail of each stream.
        self.decode_tail(&mut br1, d1.get_mut(pos..).ok_or(ZstdError::CorruptHuffman)?)?;
        self.decode_tail(&mut br2, d2.get_mut(pos..).ok_or(ZstdError::CorruptHuffman)?)?;
        self.decode_tail(&mut br3, d3.get_mut(pos..).ok_or(ZstdError::CorruptHuffman)?)?;
        self.decode_tail(&mut br4, d4.get_mut(pos..).ok_or(ZstdError::CorruptHuffman)?)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fast RFC 8878 §4.2 Huffman Literals Encoder (1-stream & 4-stream + RLE)
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

    // Leaf lengths in ascending-frequency order (same order as `nodes[0..num_leaves]`).
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

    // Clamp to `max_bits` and redistribute Kraft sum if any leaf exceeded `MAX_HUFF_BITS`.
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

        // Pass 1: lengthen shortest-possible low-frequency symbols until `kraft <= target`.
        while kraft > target {
            let excess = kraft - target;
            // Find the lowest-frequency leaf (`smallest i`) with `len < max_bits` whose lengthening
            // reduces `kraft` by at most `excess`; if none fits, pick the largest `len < max_bits`.
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

        // Pass 2: shorten highest-frequency eligible symbols until `kraft == target`.
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

    // Map leaf lengths to RFC 8878 weights: `weight = max_bits + 1 - len`.
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

/// Normalize frequencies of Huffman weights (`0..=11`, 12 symbols) to sum to `1 << accuracy_log`
/// (`accuracy_log = 6` -> sum 64) for RFC 8878 §4.2.1.1 FSE-compressed weights.
fn normalize_weight_counts(
    w_counts: &[u32; 13],
    total: u32,
    accuracy_log: u8,
) -> Option<([i16; 13], usize)> {
    let table_size: i32 = 1i32 << u32::from(accuracy_log);
    let mut norm = [0i16; 13];
    let mut max_sym: usize = 0;
    let mut distinct: usize = 0;
    let mut distributed: i32 = 0;
    let mut largest_sym: usize = 0;
    let mut largest_count: u32 = 0;

    for (sym, &c) in w_counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        distinct += 1;
        max_sym = sym;
        if c > largest_count {
            largest_count = c;
            largest_sym = sym;
        }
        let scaled = u64::from(c) * u64::try_from(table_size).ok()? / u64::from(total);
        let p: i16 = if scaled == 0 {
            -1
        } else {
            i16::try_from(scaled).ok()?
        };
        let slot = norm.get_mut(sym)?;
        *slot = p;
        distributed += if p == -1 { 1 } else { i32::from(p) };
    }

    if distinct < 2 {
        return None;
    }

    // Adjust largest symbol (or other symbols if needed) so sum == table_size.
    let diff = table_size - distributed;
    if diff != 0 {
        let cur = i32::from(*norm.get(largest_sym)?);
        let eff = if cur == -1 { 1 } else { cur };
        let next = eff + diff;
        if next >= 1 {
            *norm.get_mut(largest_sym)? = i16::try_from(next).ok()?;
        } else {
            // Trim excess across symbols with prob > 1.
            let mut to_cut = -diff;
            while to_cut > 0 {
                let mut best_s: Option<usize> = None;
                let mut best_p: i16 = 1;
                for (sym, &p) in norm.iter().enumerate() {
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
    }

    // Final safety check: exact sum == table_size and every active symbol < table_size.
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

    Some((norm, max_sym))
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
    let Some((norm, max_w_sym)) = normalize_weight_counts(&w_counts, total, acc_log) else {
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

    // Two interleaved FSE states matching RFC 8878 §4.2.1.1 / `decode_description`:
    // Even indices use state1, odd indices use state2.
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

/// Encode one Huffman literal segment into `dst` (reverse bitstream, bulk 4-byte flushes).
#[inline]
fn encode_huff_stream<A: Allocator>(
    segment: &[u8],
    codes: &[HuffCode; 256],
    dst: &mut Vec<u8, A>,
) -> Result<(), ZstdError> {
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;

    // Process 2 bytes at a time in reverse: 2 * 11 = 22 bits max; starting with bits <= 31 gives
    // <= 53 bits < 64, guaranteeing zero shift overflow with at most one 4-byte flush per pair.
    let mut idx = segment.len();
    while idx >= 2 {
        let b0 = *segment.get(idx - 1).ok_or(ZstdError::CorruptHuffman)?;
        let b1 = *segment.get(idx - 2).ok_or(ZstdError::CorruptHuffman)?;
        let c0 = *codes.get(usize::from(b0)).ok_or(ZstdError::CorruptHuffman)?;
        let c1 = *codes.get(usize::from(b1)).ok_or(ZstdError::CorruptHuffman)?;

        buf |= u64::from(c0.code) << bits;
        bits += u32::from(c0.num_bits);
        buf |= u64::from(c1.code) << bits;
        bits += u32::from(c1.num_bits);

        if bits >= 32 {
            let [x0, x1, x2, x3, ..] = buf.to_le_bytes();
            dst.try_extend_from_slice(&[x0, x1, x2, x3])?;
            buf >>= 32;
            bits -= 32;
        }
        idx -= 2;
    }
    if idx == 1 {
        let b = *segment.first().ok_or(ZstdError::CorruptHuffman)?;
        let c = *codes.get(usize::from(b)).ok_or(ZstdError::CorruptHuffman)?;
        buf |= u64::from(c.code) << bits;
        bits += u32::from(c.num_bits);
    }

    while bits >= 8 {
        let [b0, ..] = buf.to_le_bytes();
        dst.try_push(b0).map_err(|r| r.why)?;
        buf >>= 8;
        bits -= 8;
    }
    buf |= 1u64 << bits;
    let [b0, ..] = buf.to_le_bytes();
    dst.try_push(b0).map_err(|r| r.why)?;
    Ok(())
}

/// Write a `Compressed_Literals_Block` (or `RLE_Literals_Block` when all bytes are equal) into
/// `out` if it saves bytes over a raw literals section. Returns `Ok(true)` if written, or
/// `Ok(false)` leaving `out` unchanged so the caller can emit raw literals.
pub(crate) fn encode_huffman_literals_into<A: Allocator + Copy>(
    literals: &[u8],
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

    let mut weights = [0u8; 256];
    let Some((max_bits, max_sym)) = build_huffman_weights(&counts, &mut weights) else {
        return Ok(false);
    };

    // Quick profitability check before encoding bitstreams: estimated compressed bits + header.
    let mut est_bits: u64 = 0;
    for sym in 0..=max_sym {
        let w = weights.get(sym).copied().unwrap_or(0);
        if w > 0 {
            let len = u64::from(max_bits + 1 - w);
            est_bits += u64::from(counts.get(sym).copied().unwrap_or(0)) * len;
        }
    }
    let est_bytes = usize::try_from(est_bits / 8).unwrap_or(lit_len) + 80;
    if est_bytes >= lit_len {
        return Ok(false);
    }

    // Build canonical encoding table (exact match to `HuffTable::build_from_weights`).
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

    let mut codes = [HuffCode::ZERO; 256];
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
        let code_val = u16::try_from(start >> (w_usize - 1)).map_err(|_| ZstdError::CorruptHuffman)?;
        let num_bits = max_bits + 1 - w;
        if let Some(slot) = codes.get_mut(sym) {
            *slot = HuffCode { code: code_val, num_bits };
        }
    }

    // Encode tree description + payload into a temporary buffer, then emit header + payload if
    // strictly smaller than raw literals.
    let mut payload = Vec::try_with_capacity_in(est_bytes + 64, alloc)?;
    let num_explicit = max_sym;
    let explicit_slice = weights.get(..num_explicit).ok_or(ZstdError::CorruptHuffman)?;

    if num_explicit <= 128 {
        // Direct 4-bit weights when `max_sym <= 128` (instant encode & decode, <= 65 bytes).
        // For larger ASCII/UTF-8 tables > 48 bytes, FSE weights can save ~25 bytes, but direct
        // 4-bit is ~10x faster to encode and decode and only ~30 bytes overhead per 128 KiB block!
        let header_byte = u8::try_from(127 + num_explicit).map_err(|_| ZstdError::CorruptHuffman)?;
        payload.try_push(header_byte).map_err(|r| r.why)?;
        let mut i: usize = 0;
        while i < num_explicit {
            let hi = explicit_slice.get(i).copied().unwrap_or(0);
            let lo = explicit_slice.get(i + 1).copied().unwrap_or(0);
            payload.try_push((hi << 4) | (lo & 0x0F)).map_err(|r| r.why)?;
            i += 2;
        }
    } else if !encode_fse_weights(explicit_slice, &mut payload, alloc)? {
        return Ok(false);
    }

    let use_4_streams = lit_len >= 256;
    if !use_4_streams {
        encode_huff_stream(literals, &codes, &mut payload)?;
    } else {
        let seg = lit_len.div_ceil(4);
        let (st1, rest) = literals.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
        let (st2, rest) = rest.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;
        let (st3, st4) = rest.split_at_checked(seg).ok_or(ZstdError::CorruptHuffman)?;

        let jump_pos = payload.len();
        payload.try_extend_from_slice(&[0u8; 6])?;

        let s1_start = payload.len();
        encode_huff_stream(st1, &codes, &mut payload)?;
        let s1_len = payload.len() - s1_start;

        let s2_start = payload.len();
        encode_huff_stream(st2, &codes, &mut payload)?;
        let s2_len = payload.len() - s2_start;

        let s3_start = payload.len();
        encode_huff_stream(st3, &codes, &mut payload)?;
        let s3_len = payload.len() - s3_start;

        encode_huff_stream(st4, &codes, &mut payload)?;

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
    // Require meaningful savings (>= 8 bytes including header difference) over raw literals.
    if comp_size + 8 >= lit_len {
        return Ok(false);
    }

    // Emit Compressed_Literals_Block header (lit_type = 2).
    if !use_4_streams && lit_len < 1_024 && comp_size < 1_024 {
        let r = u32::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let c = u32::try_from(comp_size).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = 2u32 | (r << 4) | (c << 14);
        let [b0, b1, b2, _] = v.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2])?;
    } else if lit_len < 1_024 && comp_size < 1_024 {
        let r = u32::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let c = u32::try_from(comp_size).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = 2u32 | (1u32 << 2) | (r << 4) | (c << 14);
        let [b0, b1, b2, _] = v.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2])?;
    } else if lit_len < 16_384 && comp_size < 16_384 {
        let r = u32::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let c = u32::try_from(comp_size).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = 2u32 | (2u32 << 2) | (r << 4) | (c << 18);
        out.try_extend_from_slice(&v.to_le_bytes())?;
    } else {
        let r = u64::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let c = u64::try_from(comp_size).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = 2u64 | (3u64 << 2) | (r << 4) | (c << 22);
        let [b0, b1, b2, b3, b4, ..] = v.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2, b3, b4])?;
    }

    out.try_extend_from_slice(payload.as_slice())?;
    Ok(true)
}
