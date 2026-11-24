//! RFC 8878 Zstandard frame and block compressor: reusable [`ZstdEncoder`] context,
//! 5-byte SWAR / 2-way associative LZ77 with 1 MiB window, Treeless-reusing 4-stream Huffman
//! literals, custom per-block & RLE FSE sequence tables, and zero-pre-zeroing [`ByteCursor`]
//! bit-packing.

use core::alloc::Allocator;

use blaze_own::{Vec, common_prefix_len};

use crate::digest::xxh64;
use crate::zstd::fse::{
    FseDecTable, FseEncTable, LL_DEFAULT_LOG, LL_DEFAULT_NORM, LL_MAX_CODE, ML_DEFAULT_LOG,
    ML_DEFAULT_NORM, ML_MAX_CODE, OF_DEFAULT_LOG, OF_DEFAULT_NORM, OF_MAX_CODE, encode_ll,
    encode_ml, encode_of, normalize_fse_counts, write_fse_header,
};
use crate::zstd::huffman::{HuffEncCache, encode_huffman_literals_into};
use crate::zstd::{CompressOpts, Level, MAX_BLOCK_SIZE, ZSTD_MAGIC, ZstdError};

/// 4 MiB sliding window for multi-block frames (well within `ZstdLimits::USUAL` = 8 MiB).
const MAX_WINDOW: usize = 4 << 20;
/// Window_Descriptor for exact 4 MiB window: exponent = 12, mantissa = 0 (`1 << (10 + 12)`).
const WINDOW_DESC_4M: u8 = 12u8 << 3;
const HASH5_PRIME: u64 = 889_523_592_379;
const MASK_5BYTES: u64 = 0x0000_00FF_FFFF_FFFF;

const FAST_HASH_LOG: u32 = 15;
const FAST_HASH_SIZE: usize = 1 << FAST_HASH_LOG; // 32K entries = 128 KiB (L1D-resident)
const FAST_HASH_MASK: usize = FAST_HASH_SIZE - 1;

const DEF_HASH_LOG: u32 = 15;
const DEF_TABLE_SIZE: usize = 1 << (DEF_HASH_LOG + 1); // 2-way associative: 32K buckets x 2 = 256 KiB
const DEF_BUCKET_MASK: usize = (1 << DEF_HASH_LOG) - 1;

/// Raw LZ77 sequence `(lit_len, match_len, offset_value)` stored directly by match finders
/// without touching LL/ML/OF code LUTs in the hot LZ77 loop (mirrors C zstd `seqDef`).
#[derive(Clone, Copy)]
struct RawSeq {
    lit_len: u32,
    match_len: u32,
    offset_value: u32,
}

/// Compact 16-byte FSE-encoded sequence (`of_bits == u32::from(of_code)`).
#[derive(Clone, Copy)]
struct EncodedSeq {
    of_extra: u32,
    ll_extra: u16,
    ml_extra: u16,
    ll_code: u8,
    ml_code: u8,
    of_code: u8,
    ll_bits: u8,
    ml_bits: u8,
}

#[inline]
fn ensure_zero_hash<A: Allocator>(table: &mut Vec<u32, A>, len: usize) -> Result<(), ZstdError> {
    if table.len() >= len {
        if let Some(s) = table.as_mut_slice().get_mut(..len) {
            s.fill(0);
        }
        return Ok(());
    }
    table.clear();
    table.try_reserve(len)?;
    let chunk = [0u32; 256];
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
    Ok(())
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
    let s = input.get(pos..pos + 4)?;
    let arr: [u8; 4] = s.try_into().ok()?;
    Some(u32::from_le_bytes(arr))
}

#[inline(always)]
fn read_u64_at(input: &[u8], pos: usize) -> Option<u64> {
    let s = input.get(pos..pos + 8)?;
    let arr: [u8; 8] = s.try_into().ok()?;
    Some(u64::from_le_bytes(arr))
}

#[inline(always)]
fn hash5_u64(v: u64, hash_log: u32) -> usize {
    let idx = ((v & MASK_5BYTES).wrapping_mul(HASH5_PRIME)) >> (64u32 - hash_log);
    usize::try_from(idx).unwrap_or(0)
}

/// 8-byte SWAR match length starting at `a_start` and `b_start`, bounded by `limit_end`.
#[inline(always)]
fn match_len_at(input: &[u8], a_start: usize, b_start: usize, limit_end: usize) -> usize {
    common_prefix_len(input, a_start, b_start, limit_end)
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
fn emit_raw_seq<A: Allocator>(
    src: &[u8],
    lit_anchor: usize,
    pos: usize,
    ml: usize,
    ov: u32,
    rep: &mut [usize; 3],
    lit_cur: &mut blaze_own::ByteCursor<'_, A>,
    raw_seqs: &mut Vec<RawSeq, A>,
) -> Result<(), ZstdError> {
    let lit_len = pos - lit_anchor;
    if lit_len > 0 && !lit_cur.write_lits_wild(src, lit_anchor, lit_len) {
        return Err(ZstdError::CorruptLiterals);
    }
    let lit_u32 = u32::try_from(lit_len).map_err(|_| ZstdError::CorruptSequences)?;
    let ml_u32 = u32::try_from(ml).map_err(|_| ZstdError::CorruptSequences)?;
    raw_seqs
        .try_push(RawSeq { lit_len: lit_u32, match_len: ml_u32, offset_value: ov })
        .map_err(|r| r.why)?;
    apply_rep_update(rep, ov, lit_len);
    Ok(())
}

/// Fast 5-byte-hash single-probe matcher over a 17-bit zero-initialized `&mut [u32; FAST_HASH_SIZE]`
/// table. Marked `#[inline(never)]` so `HASH5_PRIME`, `src.as_ptr()`, `hash_table.as_ptr()`, and
/// `lit_cur` stay register-pinned with zero LL/ML/OF LUT lookups in the LZ77 hot loop.
#[inline(never)]
fn find_block_sequences_fast<A: Allocator>(
    input: &[u8],
    block_start: usize,
    block_end: usize,
    hash_table: &mut [u32; FAST_HASH_SIZE],
    rep: &mut [usize; 3],
    literals_buf: &mut Vec<u8, A>,
    raw_seqs: &mut Vec<RawSeq, A>,
) -> Result<(), ZstdError> {
    literals_buf.clear();
    raw_seqs.clear();

    let src = input.get(..block_end).ok_or(ZstdError::CorruptLiterals)?;
    let max_u32_idx = usize::try_from(u32::MAX - 1).unwrap_or(usize::MAX);
    if src.len() > max_u32_idx {
        return Err(ZstdError::CorruptSequences);
    }

    let block_len = block_end.saturating_sub(block_start);
    raw_seqs.try_reserve(block_len / 3 + 32)?;
    let mut lit_cur = literals_buf.byte_cursor();
    lit_cur.try_reserve(block_len.saturating_add(32))?;

    let mut pos = block_start.max(1);
    let mut lit_anchor = block_start;
    let match_limit = src.len().saturating_sub(8);
    let window_limit = MAX_WINDOW.min(src.len());

    while pos <= match_limit {
        let Some(cur8) = read_u64_at(src, pos) else {
            break;
        };

        // 1. Single 12-instruction L1D-resident 5-byte hash probe (zero per-byte `r0` random load
        //    on literal runs; `rep[0]` chains are harvested immediately after every match).
        let h = hash5_u64(cur8, FAST_HASH_LOG) & FAST_HASH_MASK;
        let prev = hash_table.get(h).copied().unwrap_or(0);
        if let Ok(pos_u32) = u32::try_from(pos)
            && let Some(slot) = hash_table.get_mut(h)
        {
            *slot = pos_u32;
        }

        if let Ok(mut prev_usize) = usize::try_from(prev) {
            let off = pos.wrapping_sub(prev_usize);
            if off.wrapping_sub(1) < window_limit
                && let Some(prev8) = read_u64_at(src, prev_usize)
            {
                let diff = cur8 ^ prev8;
                let mask = if off < 65_536 { 0xFFFF_FFFFu64 } else { MASK_5BYTES };
                if (diff & mask) == 0 {
                    let mut ml = if diff != 0 {
                        usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(4)
                    } else {
                        8 + match_len_at(src, prev_usize + 8, pos + 8, block_end)
                    };

                    while pos > lit_anchor
                        && prev_usize > 0
                        && src.get(pos - 1) == src.get(prev_usize - 1)
                    {
                        pos -= 1;
                        prev_usize -= 1;
                        ml += 1;
                    }

                    let [r0, r1, _] = *rep;
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
                    emit_raw_seq(src, lit_anchor, pos, ml, ov, rep, &mut lit_cur, raw_seqs)?;
                    pos += ml;
                    lit_anchor = pos;

                    if pos <= match_limit
                        && let Some(b8) = read_u64_at(src, pos - 2)
                        && let Ok(bu32) = u32::try_from(pos - 2)
                        && let Some(slot) =
                            hash_table.get_mut(hash5_u64(b8, FAST_HASH_LOG) & FAST_HASH_MASK)
                    {
                        *slot = bu32;
                    }

                    // Harvest consecutive `rep[0]` chains right after a match (at `pos + 1` or
                    // `pos + 2`) without paying a random load on literal-scanning iterations.
                    while pos + 1 <= match_limit {
                        let [cur_r0, _, _] = *rep;
                        if cur_r0 == 0 || cur_r0 > pos || cur_r0 > window_limit {
                            break;
                        }
                        let Some(next4) = read_u32_at(src, pos + 1) else {
                            break;
                        };
                        if read_u32_at(src, pos + 1 - cur_r0) != Some(next4) {
                            break;
                        }
                        pos += 1;
                        let rml = 4 + match_len_at(src, pos - cur_r0 + 4, pos + 4, block_end);
                        emit_raw_seq(src, lit_anchor, pos, rml, 1, rep, &mut lit_cur, raw_seqs)?;
                        pos += rml;
                        lit_anchor = pos;
                    }
                    continue;
                }
            }
        }

        let step = 1usize + ((pos - lit_anchor) >> 5);
        pos += step;
    }

    let trailing = src.get(lit_anchor..block_end).ok_or(ZstdError::CorruptLiterals)?;
    lit_cur.try_write_slice(trailing)?;
    lit_cur.finish();
    Ok(())
}

/// Default 2-way associative 16-bit 5-byte hash matcher with repeat-offset probing, early
/// good-match exit, backward match extension, and one-step lazy evaluation.
#[inline(never)]
fn find_block_sequences_default<A: Allocator>(
    input: &[u8],
    block_start: usize,
    block_end: usize,
    hash_table: &mut [u32; DEF_TABLE_SIZE],
    rep: &mut [usize; 3],
    literals_buf: &mut Vec<u8, A>,
    raw_seqs: &mut Vec<RawSeq, A>,
) -> Result<(), ZstdError> {
    literals_buf.clear();
    raw_seqs.clear();

    let src = input.get(..block_end).ok_or(ZstdError::CorruptLiterals)?;
    let max_u32_idx = usize::try_from(u32::MAX - 1).unwrap_or(usize::MAX);
    if src.len() > max_u32_idx {
        return Err(ZstdError::CorruptSequences);
    }

    let block_len = block_end.saturating_sub(block_start);
    raw_seqs.try_reserve(block_len / 3 + 32)?;
    let mut lit_cur = literals_buf.byte_cursor();
    lit_cur.try_reserve(block_len.saturating_add(32))?;

    let mut pos = block_start.max(1);
    let mut lit_anchor = block_start;
    let match_limit = src.len().saturating_sub(8);
    let window_limit = MAX_WINDOW.min(src.len());

    while pos <= match_limit {
        let Some(cur8) = read_u64_at(src, pos) else {
            break;
        };
        let [c0, c1, c2, c3, ..] = cur8.to_le_bytes();
        let cur4 = u32::from_le_bytes([c0, c1, c2, c3]);
        let lit_len = pos - lit_anchor;
        let [r0, r1, _] = *rep;

        let rep_off = if lit_len > 0 { r0 } else { r1 };
        let mut best_ml: usize = 0;
        let mut best_ov: u32 = 0;
        let mut best_is_rep = false;

        if lit_len < 8
            && rep_off <= pos
            && rep_off <= window_limit
            && rep_off >= 1
            && read_u32_at(src, pos - rep_off) == Some(cur4)
        {
            best_ml = 4 + match_len_at(src, pos - rep_off + 4, pos + 4, block_end);
            best_ov = 1;
            best_is_rep = true;
        }

        let b_idx = (hash5_u64(cur8, DEF_HASH_LOG) & DEF_BUCKET_MASK) << 1;
        let p0 = hash_table.get(b_idx).copied().unwrap_or(0);
        let p1 = hash_table.get(b_idx + 1).copied().unwrap_or(0);
        if let Ok(pos_u32) = u32::try_from(pos) {
            if let Some(s1) = hash_table.get_mut(b_idx + 1) {
                *s1 = p0;
            }
            if let Some(s0) = hash_table.get_mut(b_idx) {
                *s0 = pos_u32;
            }
        }

        let mut hash_prev: usize = 0;
        let mut hash_off: usize = 0;
        if !(best_is_rep && best_ml >= 16) {
            for cand in [p0, p1] {
                if let Ok(prev_u) = usize::try_from(cand) {
                    let off = pos.wrapping_sub(prev_u);
                    if off.wrapping_sub(1) < window_limit
                        && let Some(prev8) = read_u64_at(src, prev_u)
                    {
                        let diff = cur8 ^ prev8;
                        let mask = if off < 65_536 { 0xFFFF_FFFFu64 } else { MASK_5BYTES };
                        if (diff & mask) == 0 {
                            let ml = if diff != 0 {
                                usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(4)
                            } else {
                                8 + match_len_at(src, prev_u + 8, pos + 8, block_end)
                            };
                            let beats = if best_is_rep { ml > best_ml + 2 } else { ml > best_ml };
                            if beats {
                                best_ml = ml;
                                best_is_rep = false;
                                hash_prev = prev_u;
                                hash_off = off;
                                if ml >= 24 {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        if best_ml >= 4 {
            if best_ml < 24
                && pos + 1 <= match_limit
                && let Some(next8) = read_u64_at(src, pos + 1)
            {
                let nb_idx = (hash5_u64(next8, DEF_HASH_LOG) & DEF_BUCKET_MASK) << 1;
                let np0 = hash_table.get(nb_idx).copied().unwrap_or(0);
                if let Ok(prev_u) = usize::try_from(np0)
                    && (pos + 1).wrapping_sub(prev_u).wrapping_sub(1) < window_limit
                    && let Some(prev8) = read_u64_at(src, prev_u)
                    && ((next8 ^ prev8) & MASK_5BYTES) == 0
                {
                    let diff = next8 ^ prev8;
                    let lazy_ml = if diff != 0 {
                        usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(5)
                    } else {
                        8 + match_len_at(src, prev_u + 8, pos + 9, block_end)
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
                let mut prev_u = hash_prev;
                while pos > lit_anchor
                    && prev_u > 0
                    && src.get(pos - 1) == src.get(prev_u - 1)
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

            emit_raw_seq(src, lit_anchor, pos, best_ml, ov, rep, &mut lit_cur, raw_seqs)?;
            pos += best_ml;
            lit_anchor = pos;

            if pos <= match_limit
                && let Some(back8) = read_u64_at(src, pos - 2)
                && let Ok(back_u32) = u32::try_from(pos - 2)
            {
                let hb = (hash5_u64(back8, DEF_HASH_LOG) & DEF_BUCKET_MASK) << 1;
                let old0 = hash_table.get(hb).copied().unwrap_or(0);
                if let Some(s1) = hash_table.get_mut(hb + 1) {
                    *s1 = old0;
                }
                if let Some(s0) = hash_table.get_mut(hb) {
                    *s0 = back_u32;
                }
            }
        } else {
            let step = 1usize + (lit_len >> 6);
            pos += step;
        }
    }

    let trailing = src.get(lit_anchor..block_end).ok_or(ZstdError::CorruptLiterals)?;
    lit_cur.try_write_slice(trailing)?;
    lit_cur.finish();
    Ok(())
}

/// Convert raw LZ77 sequences into FSE codes/extras and tally symbol histograms in one sequential
/// pass outside the LZ77 match-finder loop.
#[inline(never)]
fn encode_raw_sequences<A: Allocator>(
    raw_seqs: &[RawSeq],
    out: &mut Vec<EncodedSeq, A>,
    ll_counts: &mut [u32; LL_MAX_CODE + 1],
    of_counts: &mut [u32; OF_MAX_CODE + 1],
    ml_counts: &mut [u32; ML_MAX_CODE + 1],
    count_hist: bool,
) -> Result<(), ZstdError> {
    out.clear();
    out.try_reserve(raw_seqs.len())?;
    for raw in raw_seqs {
        let lit_len = usize::try_from(raw.lit_len).map_err(|_| ZstdError::CorruptSequences)?;
        let ml = usize::try_from(raw.match_len).map_err(|_| ZstdError::CorruptSequences)?;
        let (ll_code, ll_extra_u32, ll_bits_u32) =
            encode_ll(lit_len).ok_or(ZstdError::CorruptSequences)?;
        let (ml_code, ml_extra_u32, ml_bits_u32) =
            encode_ml(ml).ok_or(ZstdError::CorruptSequences)?;
        let (of_code, of_extra, _of_bits) =
            encode_of(raw.offset_value).ok_or(ZstdError::CorruptSequences)?;
        let ll_extra = u16::try_from(ll_extra_u32).map_err(|_| ZstdError::CorruptSequences)?;
        let ml_extra = u16::try_from(ml_extra_u32).map_err(|_| ZstdError::CorruptSequences)?;
        let ll_bits = u8::try_from(ll_bits_u32).map_err(|_| ZstdError::CorruptSequences)?;
        let ml_bits = u8::try_from(ml_bits_u32).map_err(|_| ZstdError::CorruptSequences)?;

        if count_hist {
            if let Some(s) = ll_counts.get_mut(usize::from(ll_code)) {
                *s += 1;
            }
            if let Some(s) = of_counts.get_mut(usize::from(of_code)) {
                *s += 1;
            }
            if let Some(s) = ml_counts.get_mut(usize::from(ml_code)) {
                *s += 1;
            }
        }
        out.try_push(EncodedSeq {
            of_extra,
            ll_extra,
            ml_extra,
            ll_code,
            ml_code,
            of_code,
            ll_bits,
            ml_bits,
        })
        .map_err(|r| r.why)?;
    }
    Ok(())
}

/// Reverse-pack all FSE-encoded sequences directly into `comp_buf` via `ByteCursor`.
/// Marked `#[inline(never)]` so `ByteCursor` (`base`, `len`, `cap`) and FSE tables stay in
/// registers across the entire loop with zero stack spills.
#[inline(never)]
fn pack_sequences_fse<A: Allocator>(
    seqs: &[EncodedSeq],
    ll_tbl: &FseEncTable,
    of_tbl: &FseEncTable,
    ml_tbl: &FseEncTable,
    comp_buf: &mut Vec<u8, A>,
) -> Result<(), ZstdError> {
    let last = seqs.last().ok_or(ZstdError::CorruptSequences)?;
    let mut ll_state = ll_tbl.init_state(last.ll_code).ok_or(ZstdError::CorruptFse)?;
    let mut of_state = of_tbl.init_state(last.of_code).ok_or(ZstdError::CorruptFse)?;
    let mut ml_state = ml_tbl.init_state(last.ml_code).ok_or(ZstdError::CorruptFse)?;

    let mut cur = comp_buf.byte_cursor();
    cur.try_reserve(seqs.len().saturating_mul(11).saturating_add(48))?;
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;

    buf |= u64::from(last.ll_extra) << bits;
    bits += u32::from(last.ll_bits);
    buf |= u64::from(last.ml_extra) << bits;
    bits += u32::from(last.ml_bits);
    flush_bits_u64_cur(&mut buf, &mut bits, &mut cur);
    buf |= u64::from(last.of_extra) << bits;
    bits += u32::from(last.of_code);
    flush_bits_u64_cur(&mut buf, &mut bits, &mut cur);

    let head_seqs = seqs.get(..seqs.len().saturating_sub(1)).unwrap_or(&[]);
    for seq in head_seqs.iter().rev() {
        let (next_of, of_st_bits, of_st_nb) =
            of_tbl.encode_step(seq.of_code, of_state).ok_or(ZstdError::CorruptFse)?;
        let (next_ml, ml_st_bits, ml_st_nb) =
            ml_tbl.encode_step(seq.ml_code, ml_state).ok_or(ZstdError::CorruptFse)?;
        let (next_ll, ll_st_bits, ll_st_nb) =
            ll_tbl.encode_step(seq.ll_code, ll_state).ok_or(ZstdError::CorruptFse)?;
        of_state = next_of;
        ml_state = next_ml;
        ll_state = next_ll;

        buf |= u64::from(of_st_bits) << bits;
        bits += of_st_nb;
        buf |= u64::from(ml_st_bits) << bits;
        bits += ml_st_nb;
        buf |= u64::from(ll_st_bits) << bits;
        bits += ll_st_nb;
        buf |= u64::from(seq.ll_extra) << bits;
        bits += u32::from(seq.ll_bits);
        flush_bits_u64_cur(&mut buf, &mut bits, &mut cur);

        buf |= u64::from(seq.ml_extra) << bits;
        bits += u32::from(seq.ml_bits);
        buf |= u64::from(seq.of_extra) << bits;
        bits += u32::from(seq.of_code);
        flush_bits_u64_cur(&mut buf, &mut bits, &mut cur);
    }

    buf |= u64::from(ml_state) << bits;
    bits += u32::from(ml_tbl.accuracy_log());
    buf |= u64::from(of_state) << bits;
    bits += u32::from(of_tbl.accuracy_log());
    buf |= u64::from(ll_state) << bits;
    bits += u32::from(ll_tbl.accuracy_log());
    flush_bits_u64_cur(&mut buf, &mut bits, &mut cur);

    buf |= 1u64 << bits;
    let [b0, ..] = buf.to_le_bytes();
    let _ = cur.push_within_cap(b0);
    cur.finish();
    Ok(())
}

/// Choose between `RLE_Mode (1)`, `FSE_Compressed_Mode (2)`, or `Predefined_Mode (0)` for one
/// sequence code stream (`LL`, `OF`, or `ML`).
enum SeqTableChoice {
    Predefined,
    Rle(u8),
    Custom { norm: [i16; 64], max_sym: usize, log: u8 },
}

fn plan_seq_table(
    counts: &[u32],
    total: u32,
    target_log: u8,
    scratch_dec: &mut FseDecTable,
    custom_enc: &mut FseEncTable,
) -> SeqTableChoice {
    let mut distinct: usize = 0;
    let mut only_sym: u8 = 0;
    for (sym, &c) in counts.iter().enumerate() {
        if c > 0 {
            distinct += 1;
            only_sym = u8::try_from(sym).unwrap_or(0);
        }
    }
    if distinct == 1 {
        if custom_enc.set_rle(only_sym).is_ok() {
            return SeqTableChoice::Rle(only_sym);
        }
        return SeqTableChoice::Predefined;
    }
    let mut norm = [0i16; 64];
    if let Some(max_sym) = normalize_fse_counts(counts, total, target_log, &mut norm)
        && let Some(norm_slice) = norm.get(..=max_sym)
        && scratch_dec.build_from_norm(norm_slice, target_log).is_ok()
        && custom_enc.rebuild_from_dec(scratch_dec).is_ok()
    {
        return SeqTableChoice::Custom { norm, max_sym, log: target_log };
    }
    SeqTableChoice::Predefined
}

#[inline(always)]
fn flush_bits_u64_cur<A: Allocator>(
    buf: &mut u64,
    bits: &mut u32,
    cur: &mut blaze_own::ByteCursor<'_, A>,
) {
    let [lo, ..] = bits.to_le_bytes();
    let adv = usize::from(lo >> 3);
    let _ = cur.write_u64_le_adv(*buf, adv);
    *buf >>= u32::from(lo & !7u8);
    *bits &= 7;
}

/// Reusable RFC 8878 Zstandard compressor context.
///
/// Reuses prebuilt FSE tables, LZ77 hash table, Huffman table cache (for `Treeless_Literals_Block`),
/// and block scratch buffers across calls with zero per-call workspace allocation.
pub struct ZstdEncoder<A: Allocator + Copy> {
    predef: PredefinedEncTables,
    scratch_dec: FseDecTable,
    custom_ll: FseEncTable,
    custom_of: FseEncTable,
    custom_ml: FseEncTable,
    hash_table: Vec<u32, A>,
    literals_buf: Vec<u8, A>,
    raw_seqs: Vec<RawSeq, A>,
    seqs: Vec<EncodedSeq, A>,
    comp_buf: Vec<u8, A>,
    huff_payload: Vec<u8, A>,
    huff_cache: HuffEncCache,
    alloc: A,
}

impl<A: Allocator + Copy> ZstdEncoder<A> {
    /// Create a reusable Zstandard compressor context backed by `alloc`.
    pub fn try_new_in(alloc: A) -> Result<Self, ZstdError> {
        Ok(Self {
            predef: PredefinedEncTables::build()?,
            scratch_dec: FseDecTable::new_empty(),
            custom_ll: FseEncTable::new_empty(),
            custom_of: FseEncTable::new_empty(),
            custom_ml: FseEncTable::new_empty(),
            hash_table: Vec::new_in(alloc),
            literals_buf: Vec::new_in(alloc),
            raw_seqs: Vec::new_in(alloc),
            seqs: Vec::new_in(alloc),
            comp_buf: Vec::new_in(alloc),
            huff_payload: Vec::new_in(alloc),
            huff_cache: HuffEncCache::new(),
            alloc,
        })
    }

    /// Compress `input` into a freshly allocated RFC 8878 Zstandard frame in this encoder's
    /// allocator.
    pub fn compress(&mut self, input: &[u8], opts: CompressOpts) -> Result<Vec<u8, A>, ZstdError> {
        let init_cap = (input.len() / 2).max(256).min(input.len().saturating_add(64));
        let mut out = Vec::try_with_capacity_in(init_cap, self.alloc)?;
        self.compress_into(input, opts, &mut out)?;
        Ok(out)
    }

    /// Compress `input`, appending a complete RFC 8878 Zstandard frame into `out`.
    pub fn compress_into(
        &mut self,
        input: &[u8],
        opts: CompressOpts,
        out: &mut Vec<u8, A>,
    ) -> Result<(), ZstdError> {
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
            out.try_push(WINDOW_DESC_4M).map_err(|r| r.why)?;
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
            write_block_header(out, true, 0, 0)?;
        } else {
            let fast_mode = matches!(opts.level, Level::Fast);
            let table_len = if fast_mode { FAST_HASH_SIZE } else { DEF_TABLE_SIZE };
            ensure_zero_hash(&mut self.hash_table, table_len)?;
            self.huff_cache.reset();

            let mut rep = [1usize, 4usize, 8usize];
            let mut emitted_predef_fse = false;
            let mut block_start: usize = 0;

            while block_start < input.len() {
                let block_end = block_start.saturating_add(MAX_BLOCK_SIZE).min(input.len());
                let last_block = block_end == input.len();
                let block = input.get(block_start..block_end).ok_or(ZstdError::CorruptLiterals)?;

                if block.len() >= 4
                    && let Some(byte) = is_all_same(block)
                {
                    write_block_header(out, last_block, 1, block.len())?;
                    out.try_push(byte).map_err(|r| r.why)?;
                    block_start = block_end;
                    continue;
                }

                let mut trial_rep = rep;
                if fast_mode {
                    let table_arr: &mut [u32; FAST_HASH_SIZE] = self
                        .hash_table
                        .as_mut_slice()
                        .get_mut(..FAST_HASH_SIZE)
                        .and_then(|s| s.try_into().ok())
                        .ok_or(ZstdError::CorruptSequences)?;
                    find_block_sequences_fast(
                        input,
                        block_start,
                        block_end,
                        table_arr,
                        &mut trial_rep,
                        &mut self.literals_buf,
                        &mut self.raw_seqs,
                    )?;
                } else {
                    let table_arr: &mut [u32; DEF_TABLE_SIZE] = self
                        .hash_table
                        .as_mut_slice()
                        .get_mut(..DEF_TABLE_SIZE)
                        .and_then(|s| s.try_into().ok())
                        .ok_or(ZstdError::CorruptSequences)?;
                    find_block_sequences_default(
                        input,
                        block_start,
                        block_end,
                        table_arr,
                        &mut trial_rep,
                        &mut self.literals_buf,
                        &mut self.raw_seqs,
                    )?;
                }

                if !self.raw_seqs.is_empty() {
                    let use_custom_fse =
                        if fast_mode { self.raw_seqs.len() >= 256 } else { self.raw_seqs.len() >= 128 };
                    self.encode_compressed_block(
                        fast_mode,
                        use_custom_fse,
                        emitted_predef_fse && !use_custom_fse,
                    )?;
                    if self.comp_buf.len() < block.len() {
                        write_block_header(out, last_block, 2, self.comp_buf.len())?;
                        out.try_extend_from_slice(self.comp_buf.as_slice())?;
                        rep = trial_rep;
                        self.huff_cache.commit();
                        emitted_predef_fse = !use_custom_fse;
                        block_start = block_end;
                        continue;
                    }
                    self.huff_cache.discard();
                }

                write_block_header(out, last_block, 0, block.len())?;
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

        Ok(())
    }

    fn encode_compressed_block(
        &mut self,
        fast_mode: bool,
        use_custom_fse: bool,
        reuse_predef_fse: bool,
    ) -> Result<(), ZstdError> {
        self.comp_buf.clear();
        if !encode_huffman_literals_into(
            self.literals_buf.as_slice(),
            fast_mode,
            &mut self.huff_cache,
            &mut self.huff_payload,
            &mut self.comp_buf,
            self.alloc,
        )? {
            write_raw_literals_header(&mut self.comp_buf, self.literals_buf.len())?;
            self.comp_buf.try_extend_from_slice(self.literals_buf.as_slice())?;
        }

        let mut ll_counts = [0u32; LL_MAX_CODE + 1];
        let mut of_counts = [0u32; OF_MAX_CODE + 1];
        let mut ml_counts = [0u32; ML_MAX_CODE + 1];
        encode_raw_sequences(
            self.raw_seqs.as_slice(),
            &mut self.seqs,
            &mut ll_counts,
            &mut of_counts,
            &mut ml_counts,
            use_custom_fse,
        )?;

        let seqs = self.seqs.as_slice();
        write_num_sequences(&mut self.comp_buf, seqs.len())?;

        let (ll_tbl, of_tbl, ml_tbl): (&FseEncTable, &FseEncTable, &FseEncTable) = if use_custom_fse
        {
            let total = u32::try_from(seqs.len()).map_err(|_| ZstdError::CorruptSequences)?;

            let ll_choice =
                plan_seq_table(&ll_counts, total, 6, &mut self.scratch_dec, &mut self.custom_ll);
            let of_choice =
                plan_seq_table(&of_counts, total, 5, &mut self.scratch_dec, &mut self.custom_of);
            let ml_choice =
                plan_seq_table(&ml_counts, total, 6, &mut self.scratch_dec, &mut self.custom_ml);

            let ll_mode: u8 = match ll_choice {
                SeqTableChoice::Predefined => 0,
                SeqTableChoice::Rle(_) => 1,
                SeqTableChoice::Custom { .. } => 2,
            };
            let of_mode: u8 = match of_choice {
                SeqTableChoice::Predefined => 0,
                SeqTableChoice::Rle(_) => 1,
                SeqTableChoice::Custom { .. } => 2,
            };
            let ml_mode: u8 = match ml_choice {
                SeqTableChoice::Predefined => 0,
                SeqTableChoice::Rle(_) => 1,
                SeqTableChoice::Custom { .. } => 2,
            };
            let mode_byte = (ll_mode << 6) | (of_mode << 4) | (ml_mode << 2);
            self.comp_buf.try_push(mode_byte).map_err(|r| r.why)?;

            // Emit FSE / RLE table descriptions in RFC 8878 §3.1.1.3.2 order: LL, OF, ML.
            for choice in [&ll_choice, &of_choice, &ml_choice] {
                match *choice {
                    SeqTableChoice::Predefined => {}
                    SeqTableChoice::Rle(sym) => {
                        self.comp_buf.try_push(sym).map_err(|r| r.why)?;
                    }
                    SeqTableChoice::Custom { ref norm, max_sym, log } => {
                        let slice = norm.get(..=max_sym).ok_or(ZstdError::CorruptFse)?;
                        write_fse_header(slice, log, &mut self.comp_buf)?;
                    }
                }
            }

            let ll_t = match ll_choice {
                SeqTableChoice::Predefined => &self.predef.ll,
                SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } => &self.custom_ll,
            };
            let of_t = match of_choice {
                SeqTableChoice::Predefined => &self.predef.of,
                SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } => &self.custom_of,
            };
            let ml_t = match ml_choice {
                SeqTableChoice::Predefined => &self.predef.ml,
                SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } => &self.custom_ml,
            };
            (ll_t, of_t, ml_t)
        } else {
            let mode_byte: u8 = if reuse_predef_fse { 0xFC } else { 0x00 };
            self.comp_buf.try_push(mode_byte).map_err(|r| r.why)?;
            (&self.predef.ll, &self.predef.of, &self.predef.ml)
        };

        pack_sequences_fse(seqs, ll_tbl, of_tbl, ml_tbl, &mut self.comp_buf)
    }
}

pub(crate) fn compress_in_inner<A: Allocator + Copy>(
    input: &[u8],
    opts: CompressOpts,
    alloc: A,
) -> Result<Vec<u8, A>, ZstdError> {
    let mut enc = ZstdEncoder::try_new_in(alloc)?;
    enc.compress(input, opts)
}
