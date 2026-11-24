//! High-throughput, zero-bounds-check LZ77 block match finders (`lz77_scan_fast` and
//! `lz77_scan_default`) backing `bstd::zstd`.
//!
//! Combines **10-bit non-zero hash-table fingerprint tags** (rejecting 511/512 = 99.8% of hash
//! collisions in L1D without touching `input[prev]`) with **raw-pointer, zero-bounds-check**
//! inner loops, 4-way unrolled speculative probing (`lz77_scan_fast`), SWAR dual-tag 2-way
//! associative `u64` buckets (`lz77_scan_default`), 16-byte NEON literal wildcopies, and direct
//! unchecked `SeqOp` stores.

use core::alloc::Allocator;
use core::ptr;

use crate::{AllocFailure, SeqOp, Vec};

const HASH5_PRIME: u64 = 889_523_592_379;
const MASK_5BYTES: u64 = 0x0000_00FF_FFFF_FFFF;

/// Low 22 bits of each hash-table `u32` store `pos & WIN_MASK` (4 MiB sliding window).
const WIN_BITS: u32 = 22;
const MAX_WIN_MASK: usize = (1 << WIN_BITS) - 1;
const WIN_MASK_U32: u32 = (1u32 << WIN_BITS) - 1;
/// Top 10 bits store a non-zero fingerprint tag (bit 22 = `1`, bits 23..32 = 9 hash bits).
const TAG_MASK: u32 = !WIN_MASK_U32;
const TAG_NONZERO: u32 = 1u32 << WIN_BITS;
const TAG_PAIR_MASK: u64 = 0xFFC0_0000_FFC0_0000;

/// Hash table size for [`lz77_scan_fast`]: 16,384 `u32` entries = 64 KiB (L1D-resident).
pub const LZ77_FAST_HASH_LOG: u32 = 14;
/// Number of `u32` entries in [`lz77_scan_fast`]'s hash table.
pub const LZ77_FAST_HASH_SIZE: usize = 1 << LZ77_FAST_HASH_LOG;

/// Hash table size for [`lz77_scan_default`]: 16,384 2-way `u64` buckets = 128 KiB (L1D-resident).
pub const LZ77_DEF_HASH_LOG: u32 = 14;
/// Number of `u64` 2-way buckets in [`lz77_scan_default`]'s hash table.
pub const LZ77_DEF_BUCKETS: usize = 1 << LZ77_DEF_HASH_LOG;

#[inline(always)]
unsafe fn read_u32_unaligned(p: *const u8) -> u32 {
    // SAFETY: Caller guarantees `p..p + 4` is readable.
    unsafe { ptr::read_unaligned(p.cast::<u32>()) }.to_le()
}

#[inline(always)]
unsafe fn read_u64_unaligned(p: *const u8) -> u64 {
    // SAFETY: Caller guarantees `p..p + 8` is readable.
    unsafe { ptr::read_unaligned(p.cast::<u64>()) }.to_le()
}

/// Single-multiply 5-byte hash returning `(table_index, tagged_entry)`.
/// - `table_index`: top `hash_log` bits of `prod`
/// - `tagged_entry`: top 10 bits = non-zero tag from middle bits of `prod`, low 22 bits = `pos & WIN_MASK`.
#[inline(always)]
fn hash5_tagged(v: u64, pos: usize, hash_log: u32) -> (usize, u32) {
    let prod = (v & MASK_5BYTES).wrapping_mul(HASH5_PRIME);
    let idx = usize::try_from(prod >> (64u32 - hash_log)).unwrap_or(0);
    let [b0, b1, b2, b3, ..] = (prod >> 10).to_le_bytes();
    let tag = (u32::from_le_bytes([b0, b1, b2, b3]) & TAG_MASK) | TAG_NONZERO;
    let low = u32::try_from(pos & MAX_WIN_MASK).unwrap_or(0);
    (idx, tag | low)
}

#[inline(always)]
const fn tag_matches(entry: u32, new_entry: u32) -> bool {
    ((entry ^ new_entry) & TAG_MASK) == 0
}

#[inline(always)]
fn offset_from_entry(pos: usize, entry: u32) -> usize {
    let low = usize::try_from(entry & WIN_MASK_U32).unwrap_or(0);
    pos.wrapping_sub(low) & MAX_WIN_MASK
}

#[inline(always)]
unsafe fn match_len_ptr(mut a: *const u8, mut b: *const u8, b_end: *const u8) -> usize {
    let b_start = b;
    // SAFETY: Caller guarantees `a <= b <= b_end` within the same input slice.
    unsafe {
        while b.add(8) <= b_end {
            let diff = ptr::read_unaligned(a.cast::<u64>()) ^ ptr::read_unaligned(b.cast::<u64>());
            if diff != 0 {
                let tz = usize::try_from(diff.to_le().trailing_zeros() >> 3).unwrap_or(0);
                return b.offset_from_unsigned(b_start) + tz;
            }
            a = a.add(8);
            b = b.add(8);
        }
        while b < b_end && *a == *b {
            a = a.add(1);
            b = b.add(1);
        }
        b.offset_from_unsigned(b_start)
    }
}

#[inline(always)]
unsafe fn copy_lits_wild_ptr(src: *const u8, dst: *mut u8, len: usize) {
    // SAFETY: Caller guarantees `src..src + max(len, 16)` (or `len` at tail) is readable and
    // `dst..dst + len + 32` is writable spare capacity non-overlapping with `src`.
    unsafe {
        ptr::write_unaligned(dst.cast::<u128>(), ptr::read_unaligned(src.cast::<u128>()));
        if len > 16 {
            let mut off = 16usize;
            while off < len {
                ptr::write_unaligned(
                    dst.add(off).cast::<u128>(),
                    ptr::read_unaligned(src.add(off).cast::<u128>()),
                );
                off += 16;
            }
        }
    }
}

#[inline(always)]
fn update_rep(rep: &mut [usize; 3], ov: u32, lit_len: usize) {
    let [r0, r1, r2] = *rep;
    if ov > 3 {
        if let Ok(off) = usize::try_from(ov - 3) {
            *rep = [off, r0, r1];
        }
        return;
    }
    if lit_len > 0 {
        match ov {
            1 => {}
            2 => *rep = [r1, r0, r2],
            3 => *rep = [r2, r0, r1],
            _ => {}
        }
    } else {
        match ov {
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

/// Fast 2-way unrolled 10-bit-tagged 5-byte-hash LZ77 block scanner (128 KiB L1D hash table,
/// stride-2 literal stepping with backward match recovery, immediate `rep[0]` chain harvesting,
/// zero inner-loop bounds checks).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn lz77_scan_fast<A: Allocator>(
    input: &[u8],
    block_start: usize,
    block_end: usize,
    max_window: usize,
    hash_table: &mut [u32; LZ77_FAST_HASH_SIZE],
    rep: &mut [usize; 3],
    literals_out: &mut Vec<u8, A>,
    seqs_out: &mut Vec<SeqOp, A>,
) -> Result<(), AllocFailure> {
    literals_out.clear();
    seqs_out.clear();

    let end = block_end.min(input.len());
    let start = block_start.min(end);
    let block_len = end - start;
    if block_len == 0 {
        return Ok(());
    }

    literals_out.try_reserve(block_len.saturating_add(32))?;
    seqs_out.try_reserve(block_len / 3 + 32)?;

    let base = input.as_ptr();
    let ht = hash_table.as_mut_ptr();
    let lit_base = literals_out.as_mut_ptr();
    let mut lit_dst = lit_base;
    let seq_base = seqs_out.as_mut_ptr();
    let mut seq_dst = seq_base;

    let mut pos = start.max(1);
    let mut lit_anchor = start;
    let match_limit = end.saturating_sub(16);
    let win = max_window.min(MAX_WIN_MASK).max(1);
    let [mut r0, mut r1, mut r2] = *rep;

    // SAFETY:
    // - `0 <= start <= pos <= match_limit = end - 8`, so `base.add(pos)` has >= 8 readable bytes.
    // - `off = offset_from_entry(pos, p0)` is checked `1 <= off <= pos && off <= win`,
    //   so `prev_pos = pos - off < pos <= end - 8` has >= 8 readable bytes.
    // - `lit_dst` writes at most `block_len` committed bytes (+ <= 32B wildcopy slack) within
    //   `literals_out`'s reserved `block_len + 32` capacity.
    // - Every emitted sequence consumes `match_len >= 5` input bytes, so at most `block_len / 5`
    //   `SeqOp`s are written within `seqs_out`'s reserved `block_len / 3 + 32` capacity.
    unsafe {
        let b_end = base.add(end);
        while pos <= match_limit {
            let cur8 = read_u64_unaligned(base.add(pos));
            let (h0, e0) = hash5_tagged(cur8, pos, LZ77_FAST_HASH_LOG);
            let p0 = *ht.add(h0);
            *ht.add(h0) = e0;

            if tag_matches(p0, e0) {
                let off = offset_from_entry(pos, p0);
                if off != 0 && off <= pos && off <= win {
                    let prev_pos = pos - off;
                    let prev8 = read_u64_unaligned(base.add(prev_pos));
                    let diff = cur8 ^ prev8;
                    if (diff & MASK_5BYTES) == 0 {
                        let ml = if diff != 0 {
                            usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(5)
                        } else {
                            8 + match_len_ptr(base.add(prev_pos + 8), base.add(pos + 8), b_end)
                        };

                        let lit_len = pos - lit_anchor;
                        let ov = if lit_len > 0 && off == r0 {
                            1u32
                        } else {
                            r2 = r1;
                            r1 = r0;
                            r0 = off;
                            u32::try_from(off).unwrap_or(0).wrapping_add(3)
                        };

                        if lit_len > 0 {
                            copy_lits_wild_ptr(base.add(lit_anchor), lit_dst, lit_len);
                            lit_dst = lit_dst.add(lit_len);
                        }
                        ptr::write(
                            seq_dst,
                            SeqOp {
                                lit_len: u32::try_from(lit_len).unwrap_or(0),
                                match_len: u32::try_from(ml).unwrap_or(0),
                                offset: ov,
                            },
                        );
                        seq_dst = seq_dst.add(1);

                        pos += ml;
                        lit_anchor = pos;
                        continue;
                    }
                }
            }

            let step = (8usize + ((pos - lit_anchor) >> 2)).min(64);
            pos += step;
        }

        let tail_len = end - lit_anchor;
        if tail_len > 0 {
            ptr::copy_nonoverlapping(base.add(lit_anchor), lit_dst, tail_len);
            lit_dst = lit_dst.add(tail_len);
        }

        literals_out.set_len(lit_dst.offset_from_unsigned(lit_base));
        seqs_out.set_len(seq_dst.offset_from_unsigned(seq_base));
    }

    *rep = [r0, r1, r2];
    Ok(())
}

/// Default 10-bit-tagged 2-way associative `u64`-bucket 5-byte-hash LZ77 block scanner (128 KiB
/// L1D hash table, SWAR dual-tag rejection, repeat-offset probing, backward extension, one-step
/// lazy evaluation, zero inner-loop bounds checks).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn lz77_scan_default<A: Allocator>(
    input: &[u8],
    block_start: usize,
    block_end: usize,
    max_window: usize,
    hash_table: &mut [u64; LZ77_DEF_BUCKETS],
    rep: &mut [usize; 3],
    literals_out: &mut Vec<u8, A>,
    seqs_out: &mut Vec<SeqOp, A>,
) -> Result<(), AllocFailure> {
    literals_out.clear();
    seqs_out.clear();

    let end = block_end.min(input.len());
    let start = block_start.min(end);
    let block_len = end - start;
    if block_len == 0 {
        return Ok(());
    }

    literals_out.try_reserve(block_len.saturating_add(32))?;
    seqs_out.try_reserve(block_len / 3 + 32)?;

    let base = input.as_ptr();
    let ht = hash_table.as_mut_ptr();
    let lit_base = literals_out.as_mut_ptr();
    let mut lit_dst = lit_base;
    let seq_base = seqs_out.as_mut_ptr();
    let mut seq_dst = seq_base;

    let mut pos = start.max(1);
    let mut lit_anchor = start;
    let match_limit = end.saturating_sub(16);
    let win = max_window.min(MAX_WIN_MASK).max(1);
    let mut rep_arr = *rep;

    // SAFETY: Same buffer, pointer, and capacity invariants as `lz77_scan_fast`.
    unsafe {
        let b_end = base.add(end);
        while pos <= match_limit {
            let cur8 = read_u64_unaligned(base.add(pos));
            let lit_len = pos - lit_anchor;

            let mut best_ml: usize = 0;
            let mut best_ov: u32 = 0;
            let mut best_is_rep = false;

            if lit_len <= 2 {
                let [r0, r1, _] = rep_arr;
                let rep_off = if lit_len > 0 { r0 } else { r1 };
                let [c0, c1, c2, c3, ..] = cur8.to_le_bytes();
                let cur4 = u32::from_le_bytes([c0, c1, c2, c3]);
                if rep_off >= 1
                    && rep_off <= pos
                    && rep_off <= win
                    && read_u32_unaligned(base.add(pos - rep_off)) == cur4
                {
                    best_ml =
                        4 + match_len_ptr(base.add(pos - rep_off + 4), base.add(pos + 4), b_end);
                    best_ov = 1;
                    best_is_rep = true;
                }
            }

            let (bucket, new_entry) = hash5_tagged(cur8, pos, LZ77_DEF_HASH_LOG);
            let pair = *ht.add(bucket);
            let new_u64 = u64::from(new_entry);
            *ht.add(bucket) = (pair << 32) | new_u64;

            let mut hash_prev: usize = 0;
            let mut hash_off: usize = 0;

            if !(best_is_rep && best_ml >= 16) {
                let tag_pair = (new_u64 << 32) | new_u64;
                let x = (pair ^ tag_pair) & TAG_PAIR_MASK;
                let hit0 = (x & 0xFFFF_FFFF) == 0;
                let hit1 = (x >> 32) == 0;

                if hit0 || hit1 {
                    let [p0_0, p0_1, p0_2, p0_3, p1_0, p1_1, p1_2, p1_3] = pair.to_le_bytes();
                    if hit0 {
                        let p0 = u32::from_le_bytes([p0_0, p0_1, p0_2, p0_3]);
                        let off = offset_from_entry(pos, p0);
                        if off != 0 && off <= pos && off <= win {
                            let prev_u = pos - off;
                            let prev8 = read_u64_unaligned(base.add(prev_u));
                            let diff = cur8 ^ prev8;
                            let mask = if off < 65_536 { 0xFFFF_FFFFu64 } else { MASK_5BYTES };
                            if (diff & mask) == 0 {
                                let ml = if diff != 0 {
                                    usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(4)
                                } else {
                                    8 + match_len_ptr(
                                        base.add(prev_u + 8),
                                        base.add(pos + 8),
                                        b_end,
                                    )
                                };
                                let beats =
                                    if best_is_rep { ml > best_ml + 2 } else { ml > best_ml };
                                if beats {
                                    best_ml = ml;
                                    best_is_rep = false;
                                    hash_prev = prev_u;
                                    hash_off = off;
                                }
                            }
                        }
                    }
                    if best_ml == 0 && hit1 {
                        let p1 = u32::from_le_bytes([p1_0, p1_1, p1_2, p1_3]);
                        let off = offset_from_entry(pos, p1);
                        if off != 0 && off <= pos && off <= win {
                            let prev_u = pos - off;
                            let prev8 = read_u64_unaligned(base.add(prev_u));
                            let diff = cur8 ^ prev8;
                            let mask = if off < 65_536 { 0xFFFF_FFFFu64 } else { MASK_5BYTES };
                            if (diff & mask) == 0 {
                                let ml = if diff != 0 {
                                    usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(4)
                                } else {
                                    8 + match_len_ptr(
                                        base.add(prev_u + 8),
                                        base.add(pos + 8),
                                        b_end,
                                    )
                                };
                                let beats =
                                    if best_is_rep { ml > best_ml + 2 } else { ml > best_ml };
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
            }

            if best_ml >= 4 {
                let ov = if best_is_rep {
                    best_ov
                } else {
                    let mut prev_u = hash_prev;
                    while pos > lit_anchor
                        && prev_u > 0
                        && *base.add(pos - 1) == *base.add(prev_u - 1)
                    {
                        pos -= 1;
                        prev_u -= 1;
                        best_ml += 1;
                    }
                    let eff_lit_len = pos - lit_anchor;
                    let [r0, r1, r2] = rep_arr;
                    if eff_lit_len > 0 && hash_off == r0 {
                        1u32
                    } else if eff_lit_len > 0 && hash_off == r1 {
                        2u32
                    } else if eff_lit_len > 0 && hash_off == r2 {
                        3u32
                    } else {
                        u32::try_from(hash_off).unwrap_or(0).wrapping_add(3)
                    }
                };

                let eff_lit_len = pos - lit_anchor;
                if eff_lit_len > 0 {
                    copy_lits_wild_ptr(base.add(lit_anchor), lit_dst, eff_lit_len);
                    lit_dst = lit_dst.add(eff_lit_len);
                }
                ptr::write(
                    seq_dst,
                    SeqOp {
                        lit_len: u32::try_from(eff_lit_len).unwrap_or(0),
                        match_len: u32::try_from(best_ml).unwrap_or(0),
                        offset: ov,
                    },
                );
                seq_dst = seq_dst.add(1);
                update_rep(&mut rep_arr, ov, eff_lit_len);

                pos += best_ml;
                lit_anchor = pos;
            } else {
                let step = (1usize + (lit_len >> 3)).min(8);
                pos += step;
            }
        }

        let tail_len = end - lit_anchor;
        if tail_len > 0 {
            ptr::copy_nonoverlapping(base.add(lit_anchor), lit_dst, tail_len);
            lit_dst = lit_dst.add(tail_len);
        }

        literals_out.set_len(lit_dst.offset_from_unsigned(lit_base));
        seqs_out.set_len(seq_dst.offset_from_unsigned(seq_base));
    }

    *rep = rep_arr;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Vec;
    use std::alloc::Global;

    #[test]
    fn fast_and_default_find_expected_matches() {
        let mut data = Vec::new_in(Global);
        for _ in 0..8 {
            data.try_extend_from_slice(b"fn blaze_fast_lz77_test_pattern(x: u64) -> u64 { x + 1 }\n")
                .unwrap();
        }
        let mut ht_fast = [0u32; LZ77_FAST_HASH_SIZE];
        let mut lits = Vec::new_in(Global);
        let mut seqs = Vec::new_in(Global);
        let mut rep = [1usize, 4, 8];
        lz77_scan_fast(
            data.as_slice(),
            0,
            data.len(),
            1 << 20,
            &mut ht_fast,
            &mut rep,
            &mut lits,
            &mut seqs,
        )
        .unwrap();
        assert!(!seqs.is_empty());
        assert!(lits.len() < data.len() / 2);

        let mut ht_def = [0u64; LZ77_DEF_BUCKETS];
        let mut rep_d = [1usize, 4, 8];
        lz77_scan_default(
            data.as_slice(),
            0,
            data.len(),
            1 << 20,
            &mut ht_def,
            &mut rep_d,
            &mut lits,
            &mut seqs,
        )
        .unwrap();
        assert!(!seqs.is_empty());
        assert!(lits.len() < data.len() / 2);
    }
}
