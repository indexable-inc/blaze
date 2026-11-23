//! RFC 8878 Zstandard frame and block compressor (RLE blocks, LZ77 + repeat offsets,
//! Predefined-mode FSE sequences, and Raw fallback per block).

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::digest::xxh64;
use crate::zstd::fse::{
    BitWriter, FseDecTable, FseEncTable, LL_DEFAULT_LOG, LL_DEFAULT_NORM, ML_DEFAULT_LOG,
    ML_DEFAULT_NORM, OF_DEFAULT_LOG, OF_DEFAULT_NORM, encode_ll, encode_ml, encode_of,
};
use crate::zstd::{CompressOpts, Level, MAX_BLOCK_SIZE, ZSTD_MAGIC, ZstdError};

const MAX_WINDOW: usize = MAX_BLOCK_SIZE;
/// Window_Descriptor for exact 128 KiB window: exponent = 7, mantissa = 0 (`1 << (10 + 7)`).
const WINDOW_DESC_128K: u8 = 7u8 << 3;
const EMPTY_HASH: u32 = u32::MAX;
const HASH_PRIME: u32 = 0x9E37_79B1;

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
        // Literals_Block_Type = 0 (Raw), Size_Format = 0 (5-bit size in bits 7..=3).
        out.try_push(len_u8 << 3).map_err(|r| r.why)?;
    } else if lit_len < 4_096 {
        let len_u16 = u16::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        // Literals_Block_Type = 0 (Raw), Size_Format = 1 (12-bit size).
        let v = (len_u16 << 4) | 0b0100;
        out.try_extend_from_slice(&v.to_le_bytes())?;
    } else {
        let len_u32 = u32::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        // Literals_Block_Type = 0 (Raw), Size_Format = 3 (20-bit size).
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

fn is_all_same(bytes: &[u8]) -> Option<u8> {
    let &first = bytes.first()?;
    for &b in bytes {
        if b != first {
            return None;
        }
    }
    Some(first)
}

#[inline]
fn hash4(input: &[u8], pos: usize, hash_log: u32) -> Option<usize> {
    let end = pos.checked_add(4)?;
    let slice = input.get(pos..end)?;
    let [b0, b1, b2, b3] = *slice else {
        return None;
    };
    let v = u32::from_le_bytes([b0, b1, b2, b3]);
    let shift = 32u32.checked_sub(hash_log)?;
    let idx = v.wrapping_mul(HASH_PRIME) >> shift;
    usize::try_from(idx).ok()
}

#[inline]
fn match_len_at(input: &[u8], a_start: usize, b_start: usize, limit_end: usize) -> usize {
    let max_len = limit_end.saturating_sub(b_start);
    let mut len: usize = 0;
    while len < max_len {
        let a = input.get(a_start + len).copied();
        let b = input.get(b_start + len).copied();
        if a.is_some() && a == b {
            len += 1;
        } else {
            break;
        }
    }
    len
}

#[inline]
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

fn find_block_sequences<A: Allocator>(
    input: &[u8],
    block_start: usize,
    block_end: usize,
    level: Level,
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
    let fast_mode = match level {
        Level::Fast => true,
        Level::Default => false,
    };

    while pos + 3 <= block_end {
        let lit_len = pos - lit_anchor;
        let [r0, r1, r2] = *rep;

        // 1. Probe repeat-offset candidates first (they cost 0-1 extra bits).
        let rep_candidates: [(u32, usize); 3] = if lit_len > 0 {
            [(1, r0), (2, r1), (3, r2)]
        } else {
            [(1, r1), (2, r2), (3, r0.saturating_sub(1))]
        };

        let mut best_rep_ml: usize = 0;
        let mut best_rep_ov: u32 = 0;
        for (ov, off) in rep_candidates {
            if off >= 1 && off <= pos && off <= MAX_WINDOW {
                let ml = match_len_at(input, pos - off, pos, block_end);
                if ml >= 3 && ml > best_rep_ml {
                    best_rep_ml = ml;
                    best_rep_ov = ov;
                }
            }
        }

        // 2. Probe 4-byte hash table candidate (if at least 4 bytes remain).
        let mut hash_ml: usize = 0;
        let mut hash_ov: u32 = 0;
        if pos + 4 <= block_end
            && let Some(h) = hash4(input, pos, hash_log)
        {
            let prev = hash_table.get(h).copied().unwrap_or(EMPTY_HASH);
            if let Ok(pos_u32) = u32::try_from(pos)
                && pos_u32 != EMPTY_HASH
                && let Some(slot) = hash_table.get_mut(h)
            {
                *slot = pos_u32;
            }
            if prev != EMPTY_HASH
                && let Ok(prev_usize) = usize::try_from(prev)
                && prev_usize < pos
            {
                let off = pos - prev_usize;
                if (1..=MAX_WINDOW).contains(&off) {
                    let ml = match_len_at(input, prev_usize, pos, block_end);
                    if ml >= 4
                        && let Ok(off_u32) = u32::try_from(off)
                        && let Some(ov) = off_u32.checked_add(3)
                    {
                        hash_ml = ml;
                        hash_ov = ov;
                    }
                }
            }
        }

        // Choose between repeat-offset match and hash match.
        let chosen: Option<(usize, u32)> =
            if best_rep_ml >= 3 && (hash_ml < 4 || best_rep_ml + 1 >= hash_ml) {
                Some((best_rep_ml, best_rep_ov))
            } else if hash_ml >= 4 {
                Some((hash_ml, hash_ov))
            } else {
                None
            };

        if let Some((ml, ov)) = chosen {
            // In Default mode, check one-step lazy match at `pos + 1` if current match is short.
            if !fast_mode && ml < 32 && pos + 5 <= block_end {
                let next_pos = pos + 1;
                if let Some(h_next) = hash4(input, next_pos, hash_log) {
                    let prev_next = hash_table.get(h_next).copied().unwrap_or(EMPTY_HASH);
                    if prev_next != EMPTY_HASH
                        && let Ok(prev_usize) = usize::try_from(prev_next)
                        && prev_usize < next_pos
                    {
                        let off = next_pos - prev_usize;
                        if (1..=MAX_WINDOW).contains(&off) {
                            let lazy_ml = match_len_at(input, prev_usize, next_pos, block_end);
                            if lazy_ml >= ml + 2 {
                                pos += 1;
                                continue;
                            }
                        }
                    }
                }
            }

            let lit_slice = input.get(lit_anchor..pos).ok_or(ZstdError::CorruptLiterals)?;
            literals_buf.try_extend_from_slice(lit_slice)?;

            let (ll_code, ll_extra, ll_bits) =
                encode_ll(lit_len).ok_or(ZstdError::CorruptSequences)?;
            let (ml_code, ml_extra, ml_bits) =
                encode_ml(ml).ok_or(ZstdError::CorruptSequences)?;
            let (of_code, of_extra, of_bits) =
                encode_of(ov).ok_or(ZstdError::CorruptSequences)?;

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
            pos += ml;
            lit_anchor = pos;

            // Refresh hash entry just before the end of the match for better chaining.
            if !fast_mode && pos >= 2 && pos + 2 <= block_end {
                let back = pos - 2;
                if let (Some(h), Ok(back_u32)) = (hash4(input, back, hash_log), u32::try_from(back))
                    && back_u32 != EMPTY_HASH
                    && let Some(slot) = hash_table.get_mut(h)
                {
                    *slot = back_u32;
                }
            }
        } else if fast_mode {
            let step = 1usize + (lit_len >> 6);
            pos = pos.saturating_add(step).min(block_end);
        } else {
            pos += 1;
        }
    }

    let trailing = input.get(lit_anchor..block_end).ok_or(ZstdError::CorruptLiterals)?;
    literals_buf.try_extend_from_slice(trailing)?;
    Ok(())
}

fn encode_compressed_block<A: Allocator + Copy>(
    literals: &[u8],
    seqs: &[EncodedSeq],
    tables: &PredefinedEncTables,
    comp_buf: &mut Vec<u8, A>,
    alloc: A,
) -> Result<(), ZstdError> {
    comp_buf.clear();
    write_raw_literals_header(comp_buf, literals.len())?;
    comp_buf.try_extend_from_slice(literals)?;

    write_num_sequences(comp_buf, seqs.len())?;
    // Symbol_Compression_Modes = 0x00: Predefined_Mode for LL, OF, and ML; reserved bits = 0.
    comp_buf.try_push(0u8).map_err(|r| r.why)?;

    let last = seqs.last().ok_or(ZstdError::CorruptSequences)?;
    let mut ll_state = tables.ll.init_state(last.ll_code).ok_or(ZstdError::CorruptFse)?;
    let mut of_state = tables.of.init_state(last.of_code).ok_or(ZstdError::CorruptFse)?;
    let mut ml_state = tables.ml.init_state(last.ml_code).ok_or(ZstdError::CorruptFse)?;

    let mut bw = BitWriter::try_with_capacity_in(seqs.len().saturating_mul(4).saturating_add(16), alloc)?;

    // Reverse bitstream: write LAST sequence's extra bits in reverse of decoder order (OF, ML, LL)
    // -> LL, ML, OF.
    bw.write_bits(last.ll_extra, last.ll_bits)?;
    bw.write_bits(last.ml_extra, last.ml_bits)?;
    bw.write_bits(last.of_extra, last.of_bits)?;

    // Walk backwards from seq `n - 2` down to `0`.
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

        // Write state updates in reverse of decoder order (LL, ML, OF) -> OF, ML, LL.
        bw.write_bits(of_st_bits, of_st_nb)?;
        bw.write_bits(ml_st_bits, ml_st_nb)?;
        bw.write_bits(ll_st_bits, ll_st_nb)?;

        // Write extra bits in reverse of decoder order (OF, ML, LL) -> LL, ML, OF.
        bw.write_bits(seq.ll_extra, seq.ll_bits)?;
        bw.write_bits(seq.ml_extra, seq.ml_bits)?;
        bw.write_bits(seq.of_extra, seq.of_bits)?;
    }

    // Write initial states in reverse of decoder order (LL, OF, ML) -> ML (6), OF (5), LL (6).
    bw.write_bits(u32::from(ml_state), u32::from(ML_DEFAULT_LOG))?;
    bw.write_bits(u32::from(of_state), u32::from(OF_DEFAULT_LOG))?;
    bw.write_bits(u32::from(ll_state), u32::from(LL_DEFAULT_LOG))?;

    let seq_bytes = bw.finish()?;
    comp_buf.try_extend_from_slice(seq_bytes.as_slice())?;
    Ok(())
}

pub(crate) fn compress_in_inner<A: Allocator + Copy>(
    input: &[u8],
    opts: CompressOpts,
    alloc: A,
) -> Result<Vec<u8, A>, ZstdError> {
    let mut out = Vec::try_with_capacity_in(input.len().min(MAX_BLOCK_SIZE).saturating_add(32), alloc)?;

    // 1. Frame magic
    out.try_extend_from_slice(&ZSTD_MAGIC.to_le_bytes())?;

    // 2. Frame header descriptor + optional Window_Descriptor + Frame_Content_Size
    let len_u64 = u64::try_from(input.len()).map_err(|_| ZstdError::CorruptSequences)?;
    let single_segment = input.len() <= MAX_WINDOW;
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
        out.try_push(WINDOW_DESC_128K).map_err(|r| r.why)?;
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
        let hash_log: u32 = match opts.level {
            Level::Fast => 12,
            Level::Default => 14,
        };
        let hash_size = 1usize << hash_log;
        let mut hash_table = Vec::try_with_capacity_in(hash_size, alloc)?;
        for _ in 0..hash_size {
            hash_table.try_push(EMPTY_HASH).map_err(|r| r.why)?;
        }

        let mut literals_buf = Vec::new_in(alloc);
        let mut seqs = Vec::new_in(alloc);
        let mut comp_buf = Vec::new_in(alloc);
        let mut rep = [1usize, 4usize, 8usize];

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
            find_block_sequences(
                input,
                block_start,
                block_end,
                opts.level,
                hash_log,
                hash_table.as_mut_slice(),
                &mut trial_rep,
                &mut literals_buf,
                &mut seqs,
            )?;

            if !seqs.is_empty() {
                encode_compressed_block(
                    literals_buf.as_slice(),
                    seqs.as_slice(),
                    &tables,
                    &mut comp_buf,
                    alloc,
                )?;
                if comp_buf.len() < block.len() {
                    write_block_header(&mut out, last_block, 2, comp_buf.len())?;
                    out.try_extend_from_slice(comp_buf.as_slice())?;
                    rep = trial_rep;
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
