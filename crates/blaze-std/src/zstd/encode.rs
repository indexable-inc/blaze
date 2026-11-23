//! RFC 8878 Zstandard frame and block compressor (RLE blocks, SWAR 5-byte/2-way LZ77 + repeat
//! offsets, 4-stream Huffman literals, O(1) Predefined/Repeat-mode FSE sequences, Raw fallback).

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::digest::xxh64;
use crate::zstd::fse::{
    FseDecTable, FseEncTable, LL_DEFAULT_LOG, LL_DEFAULT_NORM, ML_DEFAULT_LOG,
    ML_DEFAULT_NORM, OF_DEFAULT_LOG, OF_DEFAULT_NORM, encode_ll, encode_ml, encode_of,
};
use crate::zstd::huffman::encode_huffman_literals_into;
use crate::zstd::{CompressOpts, Level, MAX_BLOCK_SIZE, ZSTD_MAGIC, ZstdError};

/// 1 MiB sliding window for multi-block frames (well within `ZstdLimits::USUAL` = 8 MiB).
const MAX_WINDOW: usize = 1 << 20;
/// Window_Descriptor for exact 1 MiB window: exponent = 10, mantissa = 0 (`1 << (10 + 10)`).
const WINDOW_DESC_1M: u8 = 10u8 << 3;
const EMPTY_HASH: u32 = u32::MAX;
const HASH5_PRIME: u64 = 889_523_592_379;
const MASK_5BYTES: u64 = 0x0000_00FF_FFFF_FFFF;

#[derive(Clone, Copy)]
struct EncodedSeq {
    ll_code: u8,
    ll_extra: u32,
    ll_bits: u32,
    ml_code: u8,
    ml_extra: u32,
    ml_bits: u32,
    of_code: u8,
    of_extra: u32,
    of_bits: u32,
}

#[inline]
fn alloc_empty_hash<A: Allocator>(len: usize, alloc: A) -> Result<Vec<u32, A>, ZstdError> {
    let mut table = Vec::try_with_capacity_in(len, alloc)?;
    let chunk = [EMPTY_HASH; 256];
    let mut rem = len;
    while rem >= chunk.len() {
        table.try_extend_from_slice(&chunk)?;
        rem -= chunk.len();
    }
    if rem > 0
        && let Some(tail) = chunk.get(..rem)
    {
        table.try_extend_from_slice(tail)?;
    }
    Ok(table)
}

#[inline]
fn write_block_header<A: Allocator>(
    out: &mut Vec<u8, A>,
    last_block: bool,
    block_type: u32,
    block_size: usize,
) -> Result<(), ZstdError> {
    let size_u32 = u32::try_from(block_size).map_err(|_| ZstdError::BlockTooLarge {
        size: block_size,
    })?;
    let last_bit: u32 = if last_block { 1 } else { 0 };
    let hdr = (size_u32 << 3) | ((block_type & 0x03) << 1) | last_bit;
    let [b0, b1, b2, _] = hdr.to_le_bytes();
    out.try_extend_from_slice(&[b0, b1, b2])?;
    Ok(())
}

#[inline]
fn write_raw_literals_header<A: Allocator>(
    out: &mut Vec<u8, A>,
    lit_len: usize,
) -> Result<(), ZstdError> {
    if lit_len < 32 {
        let len_u8 = u8::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        out.try_push(len_u8 << 3).map_err(|r| r.why)?;
    } else if lit_len < 4_096 {
        let len_u16 = u16::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = (len_u16 << 4) | 0b0100;
        out.try_extend_from_slice(&v.to_le_bytes())?;
    } else {
        let len_u32 = u32::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = (len_u32 << 4) | 0b1100;
        let [b0, b1, b2, _] = v.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2])?;
    }
    Ok(())
}

#[inline]
fn write_num_sequences<A: Allocator>(out: &mut Vec<u8, A>, n: usize) -> Result<(), ZstdError> {
    if n < 128 {
        let b0 = u8::try_from(n).map_err(|_| ZstdError::CorruptSequences)?;
        out.try_push(b0).map_err(|r| r.why)?;
    } else if n < 0x7F00 {
        let hi = u8::try_from((n >> 8) + 128).map_err(|_| ZstdError::CorruptSequences)?;
        let lo = u8::try_from(n & 0xFF).map_err(|_| ZstdError::CorruptSequences)?;
        out.try_extend_from_slice(&[hi, lo])?;
    } else {
        let adj = n.checked_sub(0x7F00).ok_or(ZstdError::CorruptSequences)?;
        let adj_u16 = u16::try_from(adj).map_err(|_| ZstdError::CorruptSequences)?;
        let [lo, hi] = adj_u16.to_le_bytes();
        out.try_extend_from_slice(&[255u8, lo, hi])?;
    }
    Ok(())
}

/// 8-byte SWAR check whether every byte in `bytes` equals `bytes[0]`.
#[inline]
fn is_all_same(bytes: &[u8]) -> Option<u8> {
    let &first = bytes.first()?;
    let pattern = u64::from_ne_bytes([first; 8]);
    let mut chunks = bytes.chunks_exact(8);
    for ch in &mut chunks {
        let Ok(arr) = <[u8; 8]>::try_from(ch) else {
            return None;
        };
        if u64::from_ne_bytes(arr) != pattern {
            return None;
        }
    }
    for &b in chunks.remainder() {
        if b != first {
            return None;
        }
    }
    Some(first)
}

#[inline(always)]
fn read_u32_at(input: &[u8], pos: usize) -> Option<u32> {
    let arr = input.get(pos..)?.first_chunk::<4>()?;
    Some(u32::from_le_bytes(*arr))
}

#[inline(always)]
fn read_u64_at(input: &[u8], pos: usize) -> Option<u64> {
    let arr = input.get(pos..)?.first_chunk::<8>()?;
    Some(u64::from_le_bytes(*arr))
}

#[inline(always)]
fn hash5_u64(v: u64, hash_log: u32) -> usize {
    let idx = ((v & MASK_5BYTES).wrapping_mul(HASH5_PRIME)) >> (64u32 - hash_log);
    usize::try_from(idx).unwrap_or(0)
}

/// 8-byte SWAR match length starting at `a_start` and `b_start`, bounded by `limit_end`.
#[inline(always)]
fn match_len_at(input: &[u8], a_start: usize, b_start: usize, limit_end: usize) -> usize {
    let Some(a_full) = input.get(a_start..) else {
        return 0;
    };
    let Some(b_full) = input.get(b_start..limit_end) else {
        return 0;
    };
    let max_len = a_full.len().min(b_full.len());
    let (Some(sa), Some(sb)) = (a_full.get(..max_len), b_full.get(..max_len)) else {
        return 0;
    };
    let mut len: usize = 0;
    let chunks_a = sa.chunks_exact(8);
    let chunks_b = sb.chunks_exact(8);
    let rem_a = chunks_a.remainder();
    let rem_b = chunks_b.remainder();

    for (ca, cb) in chunks_a.zip(chunks_b) {
        let (Ok(aa), Ok(bb)) = (<[u8; 8]>::try_from(ca), <[u8; 8]>::try_from(cb)) else {
            break;
        };
        let diff = u64::from_le_bytes(aa) ^ u64::from_le_bytes(bb);
        if diff != 0 {
            return len + usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(0);
        }
        len += 8;
    }

    for (&ba, &bb) in rem_a.iter().zip(rem_b.iter()) {
        if ba != bb {
            break;
        }
        len += 1;
    }
    len
}

#[inline(always)]
fn apply_rep_update(rep: &mut [usize; 3], offset_value: u32, lit_len: usize) {
    let [r0, r1, r2] = *rep;
    if offset_value > 3 {
        if let Ok(off) = usize::try_from(offset_value - 3) {
            *rep = [off, r0, r1];
        }
        return;
    }
    if lit_len > 0 {
        match offset_value {
            1 => {}
            2 => *rep = [r1, r0, r2],
            3 => *rep = [r2, r0, r1],
            _ => {}
        }
    } else {
        match offset_value {
            1 => *rep = [r1, r0, r2],
            2 => *rep = [r2, r0, r1],
            3 => {
                if let Some(off) = r0.checked_sub(1)
                    && off > 0
                {
                    *rep = [off, r0, r1];
                }
            }
            _ => {}
        }
    }
}

struct PredefinedEncTables {
    ll: FseEncTable,
    of: FseEncTable,
    ml: FseEncTable,
}

impl PredefinedEncTables {
    fn build() -> Result<Self, ZstdError> {
        let mut dec = FseDecTable::new_empty();
        dec.build_from_norm(&LL_DEFAULT_NORM, LL_DEFAULT_LOG)?;
        let ll = FseEncTable::build_from_dec(&dec)?;

        dec.build_from_norm(&OF_DEFAULT_NORM, OF_DEFAULT_LOG)?;
        let of = FseEncTable::build_from_dec(&dec)?;

        dec.build_from_norm(&ML_DEFAULT_NORM, ML_DEFAULT_LOG)?;
        let ml = FseEncTable::build_from_dec(&dec)?;

        Ok(Self { ll, of, ml })
    }
}

#[inline(always)]
fn emit_sequence<A: Allocator>(
    input: &[u8],
    lit_anchor: usize,
    pos: usize,
    ml: usize,
    ov: u32,
    rep: &mut [usize; 3],
    literals_buf: &mut Vec<u8, A>,
    seqs: &mut Vec<EncodedSeq, A>,
) -> Result<(), ZstdError> {
    let lit_len = pos - lit_anchor;
    let lit_slice = input.get(lit_anchor..pos).ok_or(ZstdError::CorruptLiterals)?;
    literals_buf.try_extend_from_slice(lit_slice)?;

    let (ll_code, ll_extra, ll_bits) = encode_ll(lit_len).ok_or(ZstdError::CorruptSequences)?;
    let (ml_code, ml_extra, ml_bits) = encode_ml(ml).ok_or(ZstdError::CorruptSequences)?;
    let (of_code, of_extra, of_bits) = encode_of(ov).ok_or(ZstdError::CorruptSequences)?;

    seqs.try_push(EncodedSeq {
        ll_code,
        ll_extra,
        ll_bits,
        ml_code,
        ml_extra,
        ml_bits,
        of_code,
        of_extra,
        of_bits,
    })
    .map_err(|r| r.why)?;

    apply_rep_update(rep, ov, lit_len);
    Ok(())
}

/// Fast 5-byte-hash single-probe matcher modeled on C zstd's `ZSTD_compressBlock_fast`:
/// skips unprofitable 4-byte far matches (which cost more bits than Huffman literals), checks
/// `rep[0]` at `pos + 1` in one 32-bit compare, and resolves 5..7-byte matches in a single
/// 64-bit XOR + `trailing_zeros`.
fn find_block_sequences_fast<A: Allocator>(
    input: &[u8],
    block_start: usize,
    block_end: usize,
    hash_log: u32,
    hash_table: &mut [u32],
    rep: &mut [usize; 3],
    literals_buf: &mut Vec<u8, A>,
    seqs: &mut Vec<EncodedSeq, A>,
) -> Result<(), ZstdError> {
    literals_buf.clear();
    seqs.clear();

    let mut pos = block_start;
    let mut lit_anchor = block_start;
    let match_limit = block_end.saturating_sub(8);
    let window_limit = MAX_WINDOW.min(input.len());

    while pos <= match_limit {
        let Some(cur8) = read_u64_at(input, pos) else {
            break;
        };

        // 1. Fast 1-compare repeat-offset probe (`rep[0]` at `pos + 1`, where `lit_len >= 1`).
        let [r0, r1, _] = *rep;
        if pos + 1 <= match_limit
            && r0 >= 1
            && r0 <= pos + 1
            && r0 <= window_limit
            && let Some(next4) = read_u32_at(input, pos + 1)
            && read_u32_at(input, pos + 1 - r0) == Some(next4)
        {
            let h = hash5_u64(cur8, hash_log);
            if let Ok(pos_u32) = u32::try_from(pos)
                && pos_u32 != EMPTY_HASH
                && let Some(slot) = hash_table.get_mut(h)
            {
                *slot = pos_u32;
            }
            pos += 1;
            let ml = 4 + match_len_at(input, pos - r0 + 4, pos + 4, block_end);
            emit_sequence(input, lit_anchor, pos, ml, 1, rep, literals_buf, seqs)?;
            pos += ml;
            lit_anchor = pos;
            continue;
        }

        // 2. Single 5-byte hash probe with 64-bit SWAR prefix compare.
        let h = hash5_u64(cur8, hash_log);
        let prev = hash_table.get(h).copied().unwrap_or(EMPTY_HASH);
        if let Ok(pos_u32) = u32::try_from(pos)
            && pos_u32 != EMPTY_HASH
            && let Some(slot) = hash_table.get_mut(h)
        {
            *slot = pos_u32;
        }

        if prev != EMPTY_HASH
            && let Ok(mut prev_usize) = usize::try_from(prev)
            && prev_usize < pos
        {
            let off = pos - prev_usize;
            if off <= window_limit
                && let Some(prev8) = read_u64_at(input, prev_usize)
            {
                let diff = cur8 ^ prev8;
                if (diff & MASK_5BYTES) == 0 {
                    let mut ml = if diff != 0 {
                        usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(5)
                    } else {
                        8 + match_len_at(input, prev_usize + 8, pos + 8, block_end)
                    };

                    // Extend match backwards into pending literals.
                    while pos > lit_anchor
                        && prev_usize > 0
                        && input.get(pos - 1) == input.get(prev_usize - 1)
                    {
                        pos -= 1;
                        prev_usize -= 1;
                        ml += 1;
                    }

                    let lit_len = pos - lit_anchor;
                    let ov = if lit_len > 0 && off == r0 {
                        1
                    } else if lit_len > 0 && off == r1 {
                        2
                    } else if let Ok(off_u32) = u32::try_from(off)
                        && let Some(v) = off_u32.checked_add(3)
                    {
                        v
                    } else {
                        pos += 1;
                        continue;
                    };
                    emit_sequence(input, lit_anchor, pos, ml, ov, rep, literals_buf, seqs)?;
                    pos += ml;
                    lit_anchor = pos;

                    // Index `pos - 2` after long matches to improve subsequent hits.
                    if pos >= 2
                        && pos + 6 <= block_end
                        && let Some(b8) = read_u64_at(input, pos - 2)
                        && let Ok(bu32) = u32::try_from(pos - 2)
                        && bu32 != EMPTY_HASH
                        && let Some(slot) = hash_table.get_mut(hash5_u64(b8, hash_log))
                    {
                        *slot = bu32;
                    }
                    continue;
                }
            }
        }

        let step = 1usize + ((pos - lit_anchor) >> 5);
        pos = pos.saturating_add(step);
    }

    let trailing = input.get(lit_anchor..block_end).ok_or(ZstdError::CorruptLiterals)?;
    literals_buf.try_extend_from_slice(trailing)?;
    Ok(())
}

/// Default 2-way bucket LZ77 matcher with offset-aware profitability filtering (`ml >= 5` or
/// close `ml == 4` with `off < 2048`), repeat-offset probing, backward match extension, and
/// one-step lazy evaluation.
fn find_block_sequences_default<A: Allocator>(
    input: &[u8],
    block_start: usize,
    block_end: usize,
    hash_log: u32,
    hash_table: &mut [u32],
    rep: &mut [usize; 3],
    literals_buf: &mut Vec<u8, A>,
    seqs: &mut Vec<EncodedSeq, A>,
) -> Result<(), ZstdError> {
    literals_buf.clear();
    seqs.clear();

    let mut pos = block_start;
    let mut lit_anchor = block_start;
    let match_limit = block_end.saturating_sub(8);
    let window_limit = MAX_WINDOW.min(input.len());

    while pos <= match_limit {
        let Some(cur8) = read_u64_at(input, pos) else {
            break;
        };
        let [c0, c1, c2, c3, ..] = cur8.to_le_bytes();
        let cur4 = u32::from_le_bytes([c0, c1, c2, c3]);
        let lit_len = pos - lit_anchor;
        let [r0, r1, _] = *rep;

        // 1. Inline `u32`-guarded repeat-offset check (costs 0 extra offset bits).
        let rep_off = if lit_len > 0 { r0 } else { r1 };
        let mut best_ml: usize = 0;
        let mut best_ov: u32 = 0;
        let mut best_is_rep = false;

        if rep_off >= 1
            && rep_off <= pos
            && rep_off <= window_limit
            && read_u32_at(input, pos - rep_off) == Some(cur4)
        {
            best_ml = 4 + match_len_at(input, pos - rep_off + 4, pos + 4, block_end);
            best_ov = 1;
            best_is_rep = true;
        }

        // 2. 2-way associative 5-byte hash bucket (stores 2 recent positions per bucket).
        let b_idx = hash5_u64(cur8, hash_log) << 1;
        let p0 = hash_table.get(b_idx).copied().unwrap_or(EMPTY_HASH);
        let p1 = hash_table.get(b_idx + 1).copied().unwrap_or(EMPTY_HASH);
        if let Ok(pos_u32) = u32::try_from(pos)
            && pos_u32 != EMPTY_HASH
        {
            if let Some(s1) = hash_table.get_mut(b_idx + 1) {
                *s1 = p0;
            }
            if let Some(s0) = hash_table.get_mut(b_idx) {
                *s0 = pos_u32;
            }
        }

        let mut hash_prev: usize = 0;
        let mut hash_off: usize = 0;
        for cand in [p0, p1] {
            if cand != EMPTY_HASH
                && let Ok(prev_u) = usize::try_from(cand)
                && prev_u < pos
            {
                let off = pos - prev_u;
                if off <= window_limit
                    && let Some(prev8) = read_u64_at(input, prev_u)
                {
                    let diff = cur8 ^ prev8;
                    if (diff & MASK_5BYTES) == 0 {
                        let ml = if diff != 0 {
                            usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(5)
                        } else {
                            8 + match_len_at(input, prev_u + 8, pos + 8, block_end)
                        };
                        let beats = if best_is_rep {
                            ml > best_ml + 2
                        } else {
                            ml > best_ml
                        };
                        if beats {
                            best_ml = ml;
                            best_is_rep = false;
                            hash_prev = prev_u;
                            hash_off = off;
                        }
                    }
                }
            }
        }

        // Also check 4-byte short-offset repeat (`r1` when `lit_len > 0`) if no match yet.
        if best_ml == 0
            && lit_len > 0
            && r1 >= 1
            && r1 <= pos
            && r1 <= window_limit
            && read_u32_at(input, pos - r1) == Some(cur4)
        {
            best_ml = 4 + match_len_at(input, pos - r1 + 4, pos + 4, block_end);
            best_ov = 2;
            best_is_rep = true;
        }

        if best_ml >= 4 {
            // One-step lazy check at `pos + 1` for short matches.
            if best_ml < 32
                && pos + 1 <= match_limit
                && let Some(next8) = read_u64_at(input, pos + 1)
            {
                let nb_idx = hash5_u64(next8, hash_log) << 1;
                let np0 = hash_table.get(nb_idx).copied().unwrap_or(EMPTY_HASH);
                if np0 != EMPTY_HASH
                    && let Ok(prev_u) = usize::try_from(np0)
                    && prev_u < pos + 1
                    && (pos + 1 - prev_u) <= window_limit
                    && let Some(prev8) = read_u64_at(input, prev_u)
                    && ((next8 ^ prev8) & MASK_5BYTES) == 0
                {
                    let diff = next8 ^ prev8;
                    let lazy_ml = if diff != 0 {
                        usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(5)
                    } else {
                        8 + match_len_at(input, prev_u + 8, pos + 9, block_end)
                    };
                    if lazy_ml >= best_ml + 2 {
                        pos += 1;
                        continue;
                    }
                }
            }

            let ov = if best_is_rep {
                best_ov
            } else {
                // Extend hash match backwards into pending literals.
                let mut prev_u = hash_prev;
                while pos > lit_anchor
                    && prev_u > 0
                    && input.get(pos - 1) == input.get(prev_u - 1)
                {
                    pos -= 1;
                    prev_u -= 1;
                    best_ml += 1;
                }
                let eff_lit_len = pos - lit_anchor;
                let [c0, c1, c2] = *rep;
                if eff_lit_len > 0 && hash_off == c0 {
                    1
                } else if eff_lit_len > 0 && hash_off == c1 {
                    2
                } else if eff_lit_len > 0 && hash_off == c2 {
                    3
                } else if let Ok(off_u32) = u32::try_from(hash_off)
                    && let Some(v) = off_u32.checked_add(3)
                {
                    v
                } else {
                    pos += 1;
                    continue;
                }
            };

            emit_sequence(input, lit_anchor, pos, best_ml, ov, rep, literals_buf, seqs)?;
            pos += best_ml;
            lit_anchor = pos;

            // Refresh hash bucket just before the end of the match for chaining.
            if pos >= 2
                && pos + 6 <= block_end
                && let Some(back8) = read_u64_at(input, pos - 2)
                && let Ok(back_u32) = u32::try_from(pos - 2)
                && back_u32 != EMPTY_HASH
            {
                let hb = hash5_u64(back8, hash_log) << 1;
                let old0 = hash_table.get(hb).copied().unwrap_or(EMPTY_HASH);
                if let Some(s1) = hash_table.get_mut(hb + 1) {
                    *s1 = old0;
                }
                if let Some(s0) = hash_table.get_mut(hb) {
                    *s0 = back_u32;
                }
            }
        } else {
            let step = 1usize + (lit_len >> 7);
            pos = pos.saturating_add(step);
        }
    }

    let trailing = input.get(lit_anchor..block_end).ok_or(ZstdError::CorruptLiterals)?;
    literals_buf.try_extend_from_slice(trailing)?;
    Ok(())
}

#[inline(always)]
fn flush_bits_32<A: Allocator>(
    buf: &mut u64,
    bits: &mut u32,
    out: &mut Vec<u8, A>,
) -> Result<(), ZstdError> {
    if *bits >= 32 {
        let [b0, b1, b2, b3, ..] = buf.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2, b3])?;
        *buf >>= 32;
        *bits -= 32;
    }
    Ok(())
}

fn encode_compressed_block<A: Allocator + Copy>(
    literals: &[u8],
    seqs: &[EncodedSeq],
    tables: &PredefinedEncTables,
    reuse_fse_tables: bool,
    comp_buf: &mut Vec<u8, A>,
    alloc: A,
) -> Result<(), ZstdError> {
    comp_buf.clear();
    if !encode_huffman_literals_into(literals, comp_buf, alloc)? {
        write_raw_literals_header(comp_buf, literals.len())?;
        comp_buf.try_extend_from_slice(literals)?;
    }

    write_num_sequences(comp_buf, seqs.len())?;
    let mode_byte: u8 = if reuse_fse_tables { 0xFC } else { 0x00 };
    comp_buf.try_push(mode_byte).map_err(|r| r.why)?;

    let last = seqs.last().ok_or(ZstdError::CorruptSequences)?;
    let mut ll_state = tables.ll.init_state(last.ll_code).ok_or(ZstdError::CorruptFse)?;
    let mut of_state = tables.of.init_state(last.of_code).ok_or(ZstdError::CorruptFse)?;
    let mut ml_state = tables.ml.init_state(last.ml_code).ok_or(ZstdError::CorruptFse)?;

    // Reserve enough for the entire sequences bitstream once; write directly into `comp_buf`
    // using a register-resident 64-bit accumulator (zero intermediate allocation).
    comp_buf.try_reserve(seqs.len().saturating_mul(5).saturating_add(16))?;
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;

    // Last sequence's extra bits: LL (<=16) + ML (<=16) = <=32, then OF (<=28).
    buf |= u64::from(last.ll_extra) << bits;
    bits += last.ll_bits;
    buf |= u64::from(last.ml_extra) << bits;
    bits += last.ml_bits;
    flush_bits_32(&mut buf, &mut bits, comp_buf)?;
    buf |= u64::from(last.of_extra) << bits;
    bits += last.of_bits;
    flush_bits_32(&mut buf, &mut bits, comp_buf)?;

    let head_seqs = seqs.get(..seqs.len().saturating_sub(1)).unwrap_or(&[]);
    for seq in head_seqs.iter().rev() {
        let (next_of, of_st_bits, of_st_nb) =
            tables.of.encode_step(seq.of_code, of_state).ok_or(ZstdError::CorruptFse)?;
        let (next_ml, ml_st_bits, ml_st_nb) =
            tables.ml.encode_step(seq.ml_code, ml_state).ok_or(ZstdError::CorruptFse)?;
        let (next_ll, ll_st_bits, ll_st_nb) =
            tables.ll.encode_step(seq.ll_code, ll_state).ok_or(ZstdError::CorruptFse)?;
        of_state = next_of;
        ml_state = next_ml;
        ll_state = next_ll;

        // Pack all 3 state updates (OF <= 5 + ML <= 6 + LL <= 6 = <= 17 bits; bits <= 31 -> <= 48 < 64).
        buf |= u64::from(of_st_bits) << bits;
        bits += of_st_nb;
        buf |= u64::from(ml_st_bits) << bits;
        bits += ml_st_nb;
        buf |= u64::from(ll_st_bits) << bits;
        bits += ll_st_nb;
        flush_bits_32(&mut buf, &mut bits, comp_buf)?;

        // Pack LL + ML extra bits (<= 32 bits; bits <= 31 -> <= 63 < 64).
        buf |= u64::from(seq.ll_extra) << bits;
        bits += seq.ll_bits;
        buf |= u64::from(seq.ml_extra) << bits;
        bits += seq.ml_bits;
        flush_bits_32(&mut buf, &mut bits, comp_buf)?;

        // Pack OF extra bits (<= 28 bits; bits <= 31 -> <= 59 < 64).
        buf |= u64::from(seq.of_extra) << bits;
        bits += seq.of_bits;
        flush_bits_32(&mut buf, &mut bits, comp_buf)?;
    }

    // Initial states: ML (6) + OF (5) + LL (6) = 17 bits (bits <= 31 -> <= 48 < 64).
    buf |= u64::from(ml_state) << bits;
    bits += u32::from(ML_DEFAULT_LOG);
    buf |= u64::from(of_state) << bits;
    bits += u32::from(OF_DEFAULT_LOG);
    buf |= u64::from(ll_state) << bits;
    bits += u32::from(LL_DEFAULT_LOG);

    while bits >= 8 {
        let [b0, ..] = buf.to_le_bytes();
        comp_buf.try_push(b0).map_err(|r| r.why)?;
        buf >>= 8;
        bits -= 8;
    }
    buf |= 1u64 << bits;
    let [b0, ..] = buf.to_le_bytes();
    comp_buf.try_push(b0).map_err(|r| r.why)?;
    Ok(())
}

pub(crate) fn compress_in_inner<A: Allocator + Copy>(
    input: &[u8],
    opts: CompressOpts,
    alloc: A,
) -> Result<Vec<u8, A>, ZstdError> {
    let init_cap = (input.len() / 2).max(256).min(input.len().saturating_add(64));
    let mut out = Vec::try_with_capacity_in(init_cap, alloc)?;

    // 1. Frame magic
    out.try_extend_from_slice(&ZSTD_MAGIC.to_le_bytes())?;

    // 2. Frame header descriptor + optional Window_Descriptor + Frame_Content_Size
    let len_u64 = u64::try_from(input.len()).map_err(|_| ZstdError::CorruptSequences)?;
    let single_segment = input.len() <= MAX_BLOCK_SIZE;
    let fcs_flag: u8 = if single_segment && len_u64 <= 255 {
        0
    } else if (256..=65_791).contains(&len_u64) {
        1
    } else if len_u64 <= u64::from(u32::MAX) {
        2
    } else {
        3
    };

    let mut fhd: u8 = fcs_flag << 6;
    if single_segment {
        fhd |= 0x20;
    }
    if opts.checksum {
        fhd |= 0x04;
    }
    out.try_push(fhd).map_err(|r| r.why)?;

    if !single_segment {
        out.try_push(WINDOW_DESC_1M).map_err(|r| r.why)?;
    }

    match fcs_flag {
        0 => {
            let b = u8::try_from(len_u64).map_err(|_| ZstdError::CorruptSequences)?;
            out.try_push(b).map_err(|r| r.why)?;
        }
        1 => {
            let adj = u16::try_from(len_u64 - 256).map_err(|_| ZstdError::CorruptSequences)?;
            out.try_extend_from_slice(&adj.to_le_bytes())?;
        }
        2 => {
            let v = u32::try_from(len_u64).map_err(|_| ZstdError::CorruptSequences)?;
            out.try_extend_from_slice(&v.to_le_bytes())?;
        }
        _ => {
            out.try_extend_from_slice(&len_u64.to_le_bytes())?;
        }
    }

    // 3. Blocks
    if input.is_empty() {
        write_block_header(&mut out, true, 0, 0)?;
    } else {
        let tables = PredefinedEncTables::build()?;
        let (hash_log, table_len): (u32, usize) = match opts.level {
            Level::Fast => (14, 1usize << 14),
            Level::Default => (14, 1usize << 15), // 2-way associative: 16K buckets x 2
        };
        let mut hash_table = alloc_empty_hash(table_len, alloc)?;

        let mut literals_buf = Vec::try_with_capacity_in(input.len().min(MAX_BLOCK_SIZE), alloc)?;
        let mut seqs = Vec::try_with_capacity_in(4_096, alloc)?;
        let mut comp_buf = Vec::try_with_capacity_in(input.len().min(MAX_BLOCK_SIZE), alloc)?;
        let mut rep = [1usize, 4usize, 8usize];
        let mut emitted_compressed = false;

        let mut block_start: usize = 0;
        while block_start < input.len() {
            let block_end = block_start.saturating_add(MAX_BLOCK_SIZE).min(input.len());
            let last_block = block_end == input.len();
            let block = input.get(block_start..block_end).ok_or(ZstdError::CorruptLiterals)?;

            if block.len() >= 4
                && let Some(byte) = is_all_same(block)
            {
                write_block_header(&mut out, last_block, 1, block.len())?;
                out.try_push(byte).map_err(|r| r.why)?;
                block_start = block_end;
                continue;
            }

            let mut trial_rep = rep;
            match opts.level {
                Level::Fast => find_block_sequences_fast(
                    input,
                    block_start,
                    block_end,
                    hash_log,
                    hash_table.as_mut_slice(),
                    &mut trial_rep,
                    &mut literals_buf,
                    &mut seqs,
                )?,
                Level::Default => find_block_sequences_default(
                    input,
                    block_start,
                    block_end,
                    hash_log,
                    hash_table.as_mut_slice(),
                    &mut trial_rep,
                    &mut literals_buf,
                    &mut seqs,
                )?,
            }

            if !seqs.is_empty() {
                encode_compressed_block(
                    literals_buf.as_slice(),
                    seqs.as_slice(),
                    &tables,
                    emitted_compressed,
                    &mut comp_buf,
                    alloc,
                )?;
                if comp_buf.len() < block.len() {
                    write_block_header(&mut out, last_block, 2, comp_buf.len())?;
                    out.try_extend_from_slice(comp_buf.as_slice())?;
                    rep = trial_rep;
                    emitted_compressed = true;
                    block_start = block_end;
                    continue;
                }
            }

            write_block_header(&mut out, last_block, 0, block.len())?;
            out.try_extend_from_slice(block)?;
            block_start = block_end;
        }
    }

    // 4. Optional trailing 32-bit XXH64 content checksum
    if opts.checksum {
        let hash = xxh64(input, 0);
        let [c0, c1, c2, c3, ..] = hash.to_le_bytes();
        out.try_extend_from_slice(&[c0, c1, c2, c3])?;
    }

    Ok(out)
}
