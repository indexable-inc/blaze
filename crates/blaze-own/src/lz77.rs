//! High-throughput, zero-bounds-check LZ77 block match finders (`lz77_scan_fast` and
//! `lz77_scan_default`) backing `bstd::zstd`.
//!
//! Combines **10-bit non-zero hash-table fingerprint tags** (rejecting 511/512 = 99.8% of hash
//! collisions in L1D without touching `input[prev]`) with **raw-pointer, zero-bounds-check**
//! inner loops, 2-position pipelined 7-byte `hash7` probing (`lz77_scan_fast`), hybrid
//! Double-Fast (`128K` `hash7` long table + `16K` 2-way SWAR `hash4` short table in
//! `lz77_scan_default`), repeat-offset chain harvesting, inline 16-byte `u128` literal wildcopies,
//! and direct unchecked `SeqOp` stores.

use core::alloc::Allocator;
use core::ptr;

use crate::{AllocFailure, SeqOp, Vec};

const HASH4_PRIME: u64 = 2_654_435_761;
const HASH7_PRIME: u64 = 58_295_818_150_454_627;
const MASK_4BYTES: u64 = 0x0000_0000_FFFF_FFFF;
const MASK_5BYTES: u64 = 0x0000_00FF_FFFF_FFFF;
const MASK_7BYTES: u64 = 0x00FF_FFFF_FFFF_FFFF;

/// Low 22 bits of each hash-table `u32` store `pos & WIN_MASK` (4 MiB sliding window).
const WIN_BITS: u32 = 22;
const MAX_WIN_MASK: usize = (1 << WIN_BITS) - 1;
const WIN_MASK_U32: u32 = (1u32 << WIN_BITS) - 1;
/// Top 10 bits store a non-zero fingerprint tag (bit 22 = `1`, bits 23..32 = 9 hash bits).
const TAG_MASK: u32 = !WIN_MASK_U32;
const TAG_NONZERO: u32 = 1u32 << WIN_BITS;
const TAG_PAIR_MASK: u64 = 0xFFC0_0000_FFC0_0000;

/// Hash table size for [`lz77_scan_fast`]: 32,768 `u32` entries = 128 KiB (L1D-resident).
pub const LZ77_FAST_HASH_LOG: u32 = 15;
/// Number of `u32` entries in [`lz77_scan_fast`]'s hash table.
pub const LZ77_FAST_HASH_SIZE: usize = 1 << LZ77_FAST_HASH_LOG;

/// Double-Fast short table (`hash4`, 2-way `u64` buckets): 16,384 buckets = 128 KiB (L1D).
const DFAST_SHORT_LOG: u32 = 14;
const DFAST_SHORT_U64S: usize = 1 << DFAST_SHORT_LOG;
/// Double-Fast long table (`hash7`, 1-way `u32` entries): 131,072 entries = 512 KiB (L2).
const DFAST_LONG_LOG: u32 = 17;
const DFAST_LONG_U64S: usize = (1 << DFAST_LONG_LOG) / 2;

/// Number of `u64` words backing [`lz77_scan_default`]'s hybrid Double-Fast tables.
pub const LZ77_DEF_BUCKETS: usize = DFAST_SHORT_U64S + DFAST_LONG_U64S;

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

/// Single-multiply 4-byte hash returning `(table_index, tagged_entry)`.
#[inline(always)]
fn hash4_tagged(v: u64, pos: usize, hash_log: u32) -> (usize, u32) {
    let prod = (v & MASK_4BYTES).wrapping_mul(HASH4_PRIME.wrapping_mul(0x9E37_79B1));
    let idx = usize::try_from(prod >> (64u32 - hash_log)).unwrap_or(0);
    let [b0, b1, b2, b3, ..] = (prod >> 10).to_le_bytes();
    let tag = (u32::from_le_bytes([b0, b1, b2, b3]) & TAG_MASK) | TAG_NONZERO;
    let low = u32::try_from(pos & MAX_WIN_MASK).unwrap_or(0);
    (idx, tag | low)
}

/// Single-multiply 7-byte hash returning `(table_index, tagged_entry)`.
#[inline(always)]
fn hash7_tagged(v: u64, pos: usize, hash_log: u32) -> (usize, u32) {
    let prod = (v & MASK_7BYTES).wrapping_mul(HASH7_PRIME);
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
    // SAFETY: Caller guarantees `src..src + max(len, 16)` is readable and
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

/// Fast 2-position pipelined 10-bit-tagged 7-byte-hash LZ77 block scanner (128 KiB L1D hash
/// table, 512 KiB window, repeat-offset `rep[0]`/`rep[1]` chain harvesting, backward match
/// extension, zero inner-loop bounds checks).
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
    let win = max_window.min(1 << 19).max(1);
    let [mut r0, mut r1, mut r2] = *rep;

    // SAFETY:
    // - `0 <= start <= pos <= match_limit = end - 16`, so `base.add(pos)` has >= 16 readable bytes.
    // - `off = offset_from_entry(hit_pos, prev_entry)` is checked `1 <= off <= hit_pos && off <= win`,
    //   so `prev_pos = hit_pos - off < hit_pos <= end - 16` has >= 16 readable bytes.
    // - `lit_dst` writes at most `block_len` committed bytes (+ <= 32B wildcopy slack) within
    //   `literals_out`'s reserved `block_len + 32` capacity.
    // - Every emitted sequence consumes `match_len >= 4` input bytes, so at most `block_len / 4`
    //   `SeqOp`s are written within `seqs_out`'s reserved `block_len / 3 + 32` capacity.
    unsafe {
        let b_end = base.add(end);
        let mut step = 1usize;
        let mut next_step = pos + 64;
        while pos <= match_limit {
            if pos >= next_step {
                step += 1;
                next_step = pos + 64;
            }
            let pos1 = pos + step;

            // 2-position pipelined 10-bit-tagged 7-byte hash probe: overlaps both table loads.
            let (hit_pos, cur8, prev_entry, next_miss_pos) = if pos1 <= match_limit {
                let c0 = read_u64_unaligned(base.add(pos));
                let c1 = read_u64_unaligned(base.add(pos1));
                let (h0, e0) = hash7_tagged(c0, pos, LZ77_FAST_HASH_LOG);
                let (h1, e1) = hash7_tagged(c1, pos1, LZ77_FAST_HASH_LOG);
                let p0 = *ht.add(h0);
                let p1 = *ht.add(h1);
                *ht.add(h0) = e0;
                if tag_matches(p0, e0) {
                    (pos, c0, p0, pos1)
                } else {
                    *ht.add(h1) = e1;
                    let after2 = pos1 + step;
                    if tag_matches(p1, e1) {
                        (pos1, c1, p1, after2)
                    } else {
                        pos = after2;
                        continue;
                    }
                }
            } else {
                let c0 = read_u64_unaligned(base.add(pos));
                let (h0, e0) = hash7_tagged(c0, pos, LZ77_FAST_HASH_LOG);
                let p0 = *ht.add(h0);
                *ht.add(h0) = e0;
                if !tag_matches(p0, e0) {
                    pos = pos1;
                    continue;
                }
                (pos, c0, p0, pos1)
            };

            let off = offset_from_entry(hit_pos, prev_entry);
            if off == 0 || off > hit_pos || off > win {
                pos = next_miss_pos;
                continue;
            }
            let mut prev_pos = hit_pos - off;
            let prev8 = read_u64_unaligned(base.add(prev_pos));
            let diff = cur8 ^ prev8;
            if (diff & MASK_4BYTES) != 0 {
                pos = next_miss_pos;
                continue;
            }

            pos = hit_pos;
            let mut ml = if diff != 0 {
                usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(4)
            } else {
                8 + match_len_ptr(base.add(prev_pos + 8), base.add(pos + 8), b_end)
            };

            while pos > lit_anchor
                && prev_pos > 0
                && *base.add(pos - 1) == *base.add(prev_pos - 1)
            {
                pos -= 1;
                prev_pos -= 1;
                ml += 1;
            }

            let lit_len = pos - lit_anchor;
            let ov = if lit_len > 0 && off == r0 {
                1u32
            } else if lit_len > 0 && off == r1 {
                core::mem::swap(&mut r0, &mut r1);
                2u32
            } else {
                r2 = r1;
                r1 = r0;
                r0 = off;
                u32::try_from(off).unwrap_or(0).wrapping_add(3)
            };

            copy_lits_wild_ptr(base.add(lit_anchor), lit_dst, lit_len);
            lit_dst = lit_dst.add(lit_len);
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
            step = 1;
            next_step = pos + 64;
            if pos <= match_limit && pos >= 2 {
                let tail8 = read_u64_unaligned(base.add(pos - 2));
                let (th, te) = hash7_tagged(tail8, pos - 2, LZ77_FAST_HASH_LOG);
                *ht.add(th) = te;
            }

            // Harvest `rep[0]` (at `pos + 1`) chains (zero hash lookups).
            while pos + 1 <= match_limit && r0 <= pos + 1 && r0 <= win {
                let rp = pos + 1;
                let rprev = rp - r0;
                if read_u32_unaligned(base.add(rprev)) != read_u32_unaligned(base.add(rp)) {
                    break;
                }
                let rml = 4 + match_len_ptr(base.add(rprev + 4), base.add(rp + 4), b_end);
                *lit_dst = *base.add(pos);
                lit_dst = lit_dst.add(1);
                ptr::write(
                    seq_dst,
                    SeqOp {
                        lit_len: 1,
                        match_len: u32::try_from(rml).unwrap_or(0),
                        offset: 1,
                    },
                );
                seq_dst = seq_dst.add(1);
                pos = rp + rml;
                lit_anchor = pos;
                next_step = pos + 64;
                if pos <= match_limit && pos >= 2 {
                    let tail8 = read_u64_unaligned(base.add(pos - 2));
                    let (th, te) = hash7_tagged(tail8, pos - 2, LZ77_FAST_HASH_LOG);
                    *ht.add(th) = te;
                }
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

    *rep = [r0, r1, r2];
    Ok(())
}

#[inline(always)]
unsafe fn insert_dfast_short(base: *const u8, ht_short: *mut u64, pos: usize) {
    // SAFETY: Caller guarantees `pos + 8 <= input.len()` and `ht_short` has `DFAST_SHORT_U64S` entries.
    unsafe {
        let v = read_u64_unaligned(base.add(pos));
        let (b, e) = hash4_tagged(v, pos, DFAST_SHORT_LOG);
        let pair = *ht_short.add(b);
        *ht_short.add(b) = (pair << 32) | u64::from(e);
    }
}

/// Default hybrid 10-bit-tagged Double-Fast (`128K` `hash7` long table + `16K` 2-way SWAR `hash4`
/// short table) LZ77 block scanner: long-match priority, 2-way short-bucket best-match selection,
/// `pos + 1` long-table override, repeat-offset chain harvesting, backward extension, zero
/// inner-loop bounds checks.
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
    let ht_short = hash_table.as_mut_ptr();
    // SAFETY: `LZ77_DEF_BUCKETS == DFAST_SHORT_U64S + DFAST_LONG_U64S`, `u64` align >= `u32`.
    let ht_long = unsafe { ht_short.add(DFAST_SHORT_U64S).cast::<u32>() };

    let lit_base = literals_out.as_mut_ptr();
    let mut lit_dst = lit_base;
    let seq_base = seqs_out.as_mut_ptr();
    let mut seq_dst = seq_base;

    let mut pos = start.max(1);
    let mut lit_anchor = start;
    let match_limit = end.saturating_sub(16);
    let win = max_window.min(1 << 21).max(1); // 2 MiB window (matches C zstd `-3`).
    let [mut r0, mut r1, mut r2] = *rep;

    // SAFETY: Same buffer, pointer, and capacity invariants as `lz77_scan_fast`.
    unsafe {
        let b_end = base.add(end);
        let mut step = 1usize;
        let mut next_step = pos + 64;
        while pos <= match_limit {
            if pos >= next_step {
                step += 1;
                next_step = pos + 64;
            }
            let pos1 = pos + step;

            // 2-position pipelined probe of `ht_long` (`hash7`) + 2-way `ht_short` (`hash4`):
            // overlaps table loads across `pos` and `pos1` on misses.
            let (hit_pos, cur8, pl, el, pair_s, es_u64, next_miss_pos) = if pos1 <= match_limit {
                let c0 = read_u64_unaligned(base.add(pos));
                let c1 = read_u64_unaligned(base.add(pos1));
                let (hl0, el0) = hash7_tagged(c0, pos, DFAST_LONG_LOG);
                let (bs0, es0) = hash4_tagged(c0, pos, DFAST_SHORT_LOG);
                let (hl1, el1) = hash7_tagged(c1, pos1, DFAST_LONG_LOG);
                let (bs1, es1) = hash4_tagged(c1, pos1, DFAST_SHORT_LOG);

                let pl0 = *ht_long.add(hl0);
                let ps0 = *ht_short.add(bs0);
                let pl1 = *ht_long.add(hl1);
                let ps1 = *ht_short.add(bs1);

                let es0_u64 = u64::from(es0);
                *ht_long.add(hl0) = el0;
                *ht_short.add(bs0) = (ps0 << 32) | es0_u64;

                let tp0 = (es0_u64 << 32) | es0_u64;
                let x0 = (ps0 ^ tp0) & TAG_PAIR_MASK;
                let any0 = tag_matches(pl0, el0)
                    || (x0 & 0xFFFF_FFFF) == 0
                    || (x0 >> 32) == 0;
                if any0 {
                    (pos, c0, pl0, el0, ps0, es0_u64, pos1)
                } else {
                    let es1_u64 = u64::from(es1);
                    *ht_long.add(hl1) = el1;
                    *ht_short.add(bs1) = (ps1 << 32) | es1_u64;
                    let after2 = pos1 + step;
                    let tp1 = (es1_u64 << 32) | es1_u64;
                    let x1 = (ps1 ^ tp1) & TAG_PAIR_MASK;
                    let any1 = tag_matches(pl1, el1)
                        || (x1 & 0xFFFF_FFFF) == 0
                        || (x1 >> 32) == 0;
                    if any1 {
                        (pos1, c1, pl1, el1, ps1, es1_u64, after2)
                    } else {
                        pos = after2;
                        continue;
                    }
                }
            } else {
                let c0 = read_u64_unaligned(base.add(pos));
                let (hl0, el0) = hash7_tagged(c0, pos, DFAST_LONG_LOG);
                let (bs0, es0) = hash4_tagged(c0, pos, DFAST_SHORT_LOG);
                let pl0 = *ht_long.add(hl0);
                let ps0 = *ht_short.add(bs0);
                let es0_u64 = u64::from(es0);
                *ht_long.add(hl0) = el0;
                *ht_short.add(bs0) = (ps0 << 32) | es0_u64;
                (pos, c0, pl0, el0, ps0, es0_u64, pos1)
            };

            let mut m_ml: usize = 0;
            let mut m_pos = hit_pos;
            let mut m_prev: usize = 0;
            let mut m_off: usize = 0;

            if tag_matches(pl, el) {
                let off = offset_from_entry(hit_pos, pl);
                if off != 0 && off <= hit_pos && off <= win {
                    let prev_u = hit_pos - off;
                    let diff = cur8 ^ read_u64_unaligned(base.add(prev_u));
                    if (diff & MASK_4BYTES) == 0 {
                        m_ml = if diff != 0 {
                            usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(4)
                        } else {
                            8 + match_len_ptr(
                                base.add(prev_u + 8),
                                base.add(hit_pos + 8),
                                b_end,
                            )
                        };
                        m_prev = prev_u;
                        m_off = off;
                    }
                }
            }

            if m_ml == 0 {
                let tag_pair = (es_u64 << 32) | es_u64;
                let x = (pair_s ^ tag_pair) & TAG_PAIR_MASK;
                let h0 = (x & 0xFFFF_FFFF) == 0;
                let h1 = (x >> 32) == 0;
                if h0 || h1 {
                    let [a0, a1, a2, a3, b0, b1, b2, b3] = pair_s.to_le_bytes();
                    if h0 {
                        let off = offset_from_entry(hit_pos, u32::from_le_bytes([a0, a1, a2, a3]));
                        if off != 0 && off <= hit_pos && off <= win {
                            let prev_u = hit_pos - off;
                            let diff = cur8 ^ read_u64_unaligned(base.add(prev_u));
                            let smask = if off < 65_536 { MASK_4BYTES } else { MASK_5BYTES };
                            if (diff & smask) == 0 {
                                m_ml = if diff != 0 {
                                    usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(4)
                                } else {
                                    8 + match_len_ptr(
                                        base.add(prev_u + 8),
                                        base.add(hit_pos + 8),
                                        b_end,
                                    )
                                };
                                m_prev = prev_u;
                                m_off = off;
                            }
                        }
                    }
                    if m_ml == 0 && h1 {
                        let off = offset_from_entry(hit_pos, u32::from_le_bytes([b0, b1, b2, b3]));
                        if off != 0 && off <= hit_pos && off <= win {
                            let prev_u = hit_pos - off;
                            let diff = cur8 ^ read_u64_unaligned(base.add(prev_u));
                            let smask = if off < 65_536 { MASK_4BYTES } else { MASK_5BYTES };
                            if (diff & smask) == 0 {
                                m_ml = if diff != 0 {
                                    usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(4)
                                } else {
                                    8 + match_len_ptr(
                                        base.add(prev_u + 8),
                                        base.add(hit_pos + 8),
                                        b_end,
                                    )
                                };
                                m_prev = prev_u;
                                m_off = off;
                            }
                        }
                    }
                }
            }

            if m_ml >= 4 {
                // Double-Fast `m_pos + 1` long-table override when current match is short.
                if m_ml < 24 && m_pos < match_limit {
                    let p1 = m_pos + 1;
                    let next8 = read_u64_unaligned(base.add(p1));
                    let (hl1, el1) = hash7_tagged(next8, p1, DFAST_LONG_LOG);
                    let pl1 = *ht_long.add(hl1);
                    *ht_long.add(hl1) = el1;
                    if tag_matches(pl1, el1) {
                        let off1 = offset_from_entry(p1, pl1);
                        if off1 != 0 && off1 <= p1 && off1 <= win {
                            let prev1 = p1 - off1;
                            let diff1 = next8 ^ read_u64_unaligned(base.add(prev1));
                            if (diff1 & MASK_4BYTES) == 0 {
                                let ml1 = if diff1 != 0 {
                                    usize::try_from(diff1.trailing_zeros() >> 3).unwrap_or(4)
                                } else {
                                    8 + match_len_ptr(
                                        base.add(prev1 + 8),
                                        base.add(p1 + 8),
                                        b_end,
                                    )
                                };
                                if ml1 > m_ml {
                                    m_pos = p1;
                                    m_prev = prev1;
                                    m_off = off1;
                                    m_ml = ml1;
                                }
                            }
                        }
                    }
                }

                pos = m_pos;
                let mut prev_u = m_prev;
                while pos > lit_anchor
                    && prev_u > 0
                    && *base.add(pos - 1) == *base.add(prev_u - 1)
                {
                    pos -= 1;
                    prev_u -= 1;
                    m_ml += 1;
                }

                let lit_len = pos - lit_anchor;
                let ov = if lit_len > 0 && m_off == r0 {
                    1u32
                } else if lit_len > 0 && m_off == r1 {
                    core::mem::swap(&mut r0, &mut r1);
                    2u32
                } else if lit_len > 0 && m_off == r2 {
                    let t = r2;
                    r2 = r1;
                    r1 = r0;
                    r0 = t;
                    3u32
                } else {
                    r2 = r1;
                    r1 = r0;
                    r0 = m_off;
                    u32::try_from(m_off).unwrap_or(0).wrapping_add(3)
                };

                copy_lits_wild_ptr(base.add(lit_anchor), lit_dst, lit_len);
                lit_dst = lit_dst.add(lit_len);
                ptr::write(
                    seq_dst,
                    SeqOp {
                        lit_len: u32::try_from(lit_len).unwrap_or(0),
                        match_len: u32::try_from(m_ml).unwrap_or(0),
                        offset: ov,
                    },
                );
                seq_dst = seq_dst.add(1);

                let match_start = m_pos;
                pos += m_ml;
                lit_anchor = pos;
                step = 1;
                next_step = pos + 64;
                if match_start + 2 < pos && match_start + 2 <= match_limit {
                    insert_dfast_short(base, ht_short, match_start + 2);
                    let m8 = read_u64_unaligned(base.add(match_start + 2));
                    let (hl, el) = hash7_tagged(m8, match_start + 2, DFAST_LONG_LOG);
                    *ht_long.add(hl) = el;
                }
                if pos <= match_limit && pos >= 2 {
                    insert_dfast_short(base, ht_short, pos - 2);
                    let t8 = read_u64_unaligned(base.add(pos - 2));
                    let (hl, el) = hash7_tagged(t8, pos - 2, DFAST_LONG_LOG);
                    *ht_long.add(hl) = el;
                }

                // Harvest `rep[0]` (at `pos + 1`) and `rep[1]` (at `pos + 2`) chains.
                loop {
                    if pos + 1 <= match_limit && r0 <= pos + 1 && r0 <= win {
                        let rp = pos + 1;
                        let rprev = rp - r0;
                        if read_u32_unaligned(base.add(rprev)) == read_u32_unaligned(base.add(rp)) {
                            let rml =
                                4 + match_len_ptr(base.add(rprev + 4), base.add(rp + 4), b_end);
                            *lit_dst = *base.add(pos);
                            lit_dst = lit_dst.add(1);
                            ptr::write(
                                seq_dst,
                                SeqOp {
                                    lit_len: 1,
                                    match_len: u32::try_from(rml).unwrap_or(0),
                                    offset: 1,
                                },
                            );
                            seq_dst = seq_dst.add(1);
                            pos = rp + rml;
                            lit_anchor = pos;
                            next_step = pos + 64;
                            if pos <= match_limit && pos >= 2 {
                                insert_dfast_short(base, ht_short, pos - 2);
                                let t8 = read_u64_unaligned(base.add(pos - 2));
                                let (hl, el) = hash7_tagged(t8, pos - 2, DFAST_LONG_LOG);
                                *ht_long.add(hl) = el;
                            }
                            continue;
                        }
                    }
                    if pos + 2 <= match_limit && r1 <= pos + 2 && r1 <= win {
                        let rp = pos + 2;
                        let rprev = rp - r1;
                        if read_u32_unaligned(base.add(rprev)) == read_u32_unaligned(base.add(rp)) {
                            let rml =
                                4 + match_len_ptr(base.add(rprev + 4), base.add(rp + 4), b_end);
                            ptr::write_unaligned(
                                lit_dst.cast::<u16>(),
                                ptr::read_unaligned(base.add(pos).cast::<u16>()),
                            );
                            lit_dst = lit_dst.add(2);
                            ptr::write(
                                seq_dst,
                                SeqOp {
                                    lit_len: 2,
                                    match_len: u32::try_from(rml).unwrap_or(0),
                                    offset: 2,
                                },
                            );
                            seq_dst = seq_dst.add(1);
                            core::mem::swap(&mut r0, &mut r1);
                            pos = rp + rml;
                            lit_anchor = pos;
                            next_step = pos + 64;
                            if pos <= match_limit && pos >= 2 {
                                insert_dfast_short(base, ht_short, pos - 2);
                                let t8 = read_u64_unaligned(base.add(pos - 2));
                                let (hl, el) = hash7_tagged(t8, pos - 2, DFAST_LONG_LOG);
                                *ht_long.add(hl) = el;
                            }
                            continue;
                        }
                    }
                    break;
                }
                continue;
            }

            pos = next_miss_pos;
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

/// Zero-bounds-check reverse Huffman literal stream encoder using balanced 2-pair parallel shift
/// packing and branchless 64-bit unaligned flushes (4 symbols <= 44 bits + 7 residual = 51 <= 64).
///
/// Each entry in `packed_codes` packs `code | (num_bits << 16)` for byte `0..256`.
#[inline(never)]
pub fn huff_encode_stream_raw<A: Allocator>(
    segment: &[u8],
    packed_codes: &[u32; 256],
    dst: &mut Vec<u8, A>,
) -> Result<(), AllocFailure> {
    // Upper bound: each symbol <= 11 bits -> <= 1.375 bytes + 24B slack for 8-byte wild writes.
    let max_bytes = segment.len() + (segment.len() >> 1) + 24;
    dst.try_reserve(max_bytes)?;

    let start_len = dst.len();
    let src_base = segment.as_ptr();
    let codes_ptr = packed_codes.as_ptr();
    let mut out_ptr = unsafe { dst.as_mut_ptr().add(start_len) };
    let out_start = out_ptr;

    let mut buf: u64 = 0;
    let mut bits: u32 = 0;

    let rem = segment.len() & 3;
    let body_len = segment.len() - rem;

    // SAFETY:
    // - `src_base..src_base + segment.len()` is readable; `p` walks backwards over `[0..body_len]`
    //   in 4-byte steps and `[body_len..segment.len()]` byte-by-byte.
    // - `codes_ptr` has 256 elements, indexed by `u8` (`0..=255`).
    // - `dst` has `max_bytes >= segment.len() * 11 / 8 + 24` spare capacity, so every 8-byte
    //   unaligned flush stays strictly within reserved capacity.
    unsafe {
        let mut t = segment.len();
        while t > body_len {
            t -= 1;
            let e = *codes_ptr.add(usize::from(*src_base.add(t)));
            let code = u64::from(e & 0xFFFF);
            let nb = e >> 16;
            buf |= code << bits;
            bits += nb;
            let [lo, ..] = bits.to_le_bytes();
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add(usize::from(lo >> 3));
            buf >>= u32::from(lo & !7u8);
            bits &= 7;
        }

        let mut p = body_len;
        while p >= 4 {
            p -= 4;
            let w = read_u32_unaligned(src_base.add(p));
            let [b0, b1, b2, b3] = w.to_le_bytes();
            let e3 = *codes_ptr.add(usize::from(b3));
            let e2 = *codes_ptr.add(usize::from(b2));
            let e1 = *codes_ptr.add(usize::from(b1));
            let e0 = *codes_ptr.add(usize::from(b0));

            let n3 = e3 >> 16;
            let n2 = e2 >> 16;
            let n1 = e1 >> 16;
            let n0 = e0 >> 16;

            let p32 = u64::from(e3 & 0xFFFF) | (u64::from(e2 & 0xFFFF) << n3);
            let p10 = u64::from(e1 & 0xFFFF) | (u64::from(e0 & 0xFFFF) << n1);
            let n32 = n3 + n2;
            let n10 = n1 + n0;

            buf |= p32 << bits;
            buf |= p10 << (bits + n32);
            bits += n32 + n10;

            let [lo, ..] = bits.to_le_bytes();
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add(usize::from(lo >> 3));
            buf >>= u32::from(lo & !7u8);
            bits &= 7;
        }

        // Write sentinel `1` bit and flush final byte.
        buf |= 1u64 << bits;
        let [b0, ..] = buf.to_le_bytes();
        *out_ptr = b0;
        out_ptr = out_ptr.add(1);

        dst.set_len(start_len + out_ptr.offset_from_unsigned(out_start));
    }

    Ok(())
}

/// Packed FSE encoder table view for [`fse_pack_sequences_fast`].
///
/// Each `meta[sym]` packs `offset | (count << 16) | (split << 32) | (max_nb << 48)` as `u64`.
#[derive(Clone, Copy)]
pub struct FsePackTable<'a> {
    pub state_table: &'a [u8; 256],
    pub meta: &'a [u64; 64],
    pub table_size: u16,
    pub accuracy_log: u8,
}

#[inline(always)]
unsafe fn fse_step_raw(tbl: &FsePackTable<'_>, sym: u8, target_state: u8) -> (u8, u32, u32) {
    // SAFETY: `sym & 63 < 64` and `idx & 255 < 256`.
    unsafe {
        let m = *tbl.meta.as_ptr().add(usize::from(sym) & 63);
        let offset = usize::try_from(m & 0xFFFF).unwrap_or(0);
        let count = usize::try_from((m >> 16) & 0xFFFF).unwrap_or(0);
        let split = u32::try_from((m >> 32) & 0xFFFF).unwrap_or(0);
        let max_nb = u32::try_from((m >> 48) & 0xFF).unwrap_or(0);
        let target = u32::from(target_state);
        let nb = max_nb.wrapping_sub(u32::from(target < split));
        let bits = target & ((1u32 << nb) - 1);
        let u = (target + u32::from(tbl.table_size)) >> nb;
        let idx = offset
            .wrapping_add(usize::try_from(u).unwrap_or(0))
            .wrapping_sub(count)
            & 255;
        let prev = *tbl.state_table.as_ptr().add(idx);
        (prev, bits, nb)
    }
}

#[inline(always)]
unsafe fn encode_ll_lut_raw(lit_len: u32, ll_lut: *const u32) -> (u8, u32, u32) {
    if lit_len < 64 {
        // SAFETY: `lit_len < 64` is within `ll_lut: &[u32; 64]`.
        let p = unsafe { *ll_lut.add(usize::try_from(lit_len).unwrap_or(0)) };
        let [code, bits_b, ..] = p.to_le_bytes();
        return (code, lit_len.wrapping_sub(p >> 16), u32::from(bits_b));
    }
    let bits = 31u32 - lit_len.leading_zeros();
    let [code, ..] = (bits + 19).to_le_bytes();
    let base = 1u32 << bits;
    (code, lit_len - base, bits)
}

#[inline(always)]
unsafe fn encode_ml_lut_raw(match_len: u32, ml_lut: *const u32) -> (u8, u32, u32) {
    let adj = match_len.wrapping_sub(3);
    if adj < 128 {
        // SAFETY: `adj < 128` is within `ml_lut: &[u32; 128]`.
        let p = unsafe { *ml_lut.add(usize::try_from(adj).unwrap_or(0)) };
        let [code, bits_b, ..] = p.to_le_bytes();
        return (code, match_len.wrapping_sub(p >> 16), u32::from(bits_b));
    }
    let bits = 31u32 - adj.leading_zeros();
    let [code, ..] = (bits + 36).to_le_bytes();
    let base = (1u32 << bits) + 3;
    (code, match_len - base, bits)
}

/// Zero-bounds-check single-pass reverse FSE sequence packer directly from `&[SeqOp]`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn fse_pack_sequences_fast<A: Allocator>(
    raw_seqs: &[SeqOp],
    ll_lut: &[u32; 64],
    ml_lut: &[u32; 128],
    ll_tbl: FsePackTable<'_>,
    of_tbl: FsePackTable<'_>,
    ml_tbl: FsePackTable<'_>,
    init_ll: u8,
    init_of: u8,
    init_ml: u8,
    last_ll_extra: u32,
    last_ll_bits: u32,
    last_ml_extra: u32,
    last_ml_bits: u32,
    last_of_extra: u32,
    last_of_bits: u32,
    comp_buf: &mut Vec<u8, A>,
) -> Result<(), AllocFailure> {
    let n = raw_seqs.len();
    if n == 0 {
        return Ok(());
    }
    let max_bytes = n.saturating_mul(11).saturating_add(48);
    comp_buf.try_reserve(max_bytes)?;

    let start_len = comp_buf.len();
    let ll_lut_ptr = ll_lut.as_ptr();
    let ml_lut_ptr = ml_lut.as_ptr();
    let seq_ptr = raw_seqs.as_ptr();

    let mut ll_state = init_ll;
    let mut of_state = init_of;
    let mut ml_state = init_ml;

    let mut buf: u64 = 0;
    let mut bits: u32 = 0;

    // SAFETY:
    // - `comp_buf` has `n * 11 + 48` reserved bytes (each sequence emits <= 11 bytes + 8B wild write).
    // - `seq_ptr` is walked backwards over `0..n - 1`.
    unsafe {
        let mut out_ptr = comp_buf.as_mut_ptr().add(start_len);
        let out_start = out_ptr;

        // Emit last sequence's extra bits.
        buf |= u64::from(last_ll_extra) << bits;
        bits += last_ll_bits;
        buf |= u64::from(last_ml_extra) << bits;
        bits += last_ml_bits;
        {
            let [lo, ..] = bits.to_le_bytes();
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add(usize::from(lo >> 3));
            buf >>= u32::from(lo & !7u8);
            bits &= 7;
        }
        buf |= u64::from(last_of_extra) << bits;
        bits += last_of_bits;
        {
            let [lo, ..] = bits.to_le_bytes();
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add(usize::from(lo >> 3));
            buf >>= u32::from(lo & !7u8);
            bits &= 7;
        }

        let mut i = n - 1;
        while i > 0 {
            i -= 1;
            let raw = *seq_ptr.add(i);
            let (ll_code, ll_extra, ll_b) = encode_ll_lut_raw(raw.lit_len, ll_lut_ptr);
            let (ml_code, ml_extra, ml_b) = encode_ml_lut_raw(raw.match_len, ml_lut_ptr);
            let of_b = 31u32 - raw.offset.max(1).leading_zeros();
            let [of_code, ..] = of_b.to_le_bytes();
            let of_extra = raw.offset - (1u32 << of_b);

            let (next_of, of_st_bits, of_st_nb) = fse_step_raw(&of_tbl, of_code, of_state);
            let (next_ml, ml_st_bits, ml_st_nb) = fse_step_raw(&ml_tbl, ml_code, ml_state);
            let (next_ll, ll_st_bits, ll_st_nb) = fse_step_raw(&ll_tbl, ll_code, ll_state);
            of_state = next_of;
            ml_state = next_ml;
            ll_state = next_ll;

            buf |= u64::from(of_st_bits) << bits;
            bits += of_st_nb;
            buf |= u64::from(ml_st_bits) << bits;
            bits += ml_st_nb;
            buf |= u64::from(ll_st_bits) << bits;
            bits += ll_st_nb;
            buf |= u64::from(ll_extra) << bits;
            bits += ll_b;
            if bits + ml_b + of_b > 63 {
                let [lo, ..] = bits.to_le_bytes();
                ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
                out_ptr = out_ptr.add(usize::from(lo >> 3));
                buf >>= u32::from(lo & !7u8);
                bits &= 7;
            }
            buf |= u64::from(ml_extra) << bits;
            bits += ml_b;
            buf |= u64::from(of_extra) << bits;
            bits += of_b;
            let [lo, ..] = bits.to_le_bytes();
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add(usize::from(lo >> 3));
            buf >>= u32::from(lo & !7u8);
            bits &= 7;
        }

        buf |= u64::from(ml_state) << bits;
        bits += u32::from(ml_tbl.accuracy_log);
        buf |= u64::from(of_state) << bits;
        bits += u32::from(of_tbl.accuracy_log);
        buf |= u64::from(ll_state) << bits;
        bits += u32::from(ll_tbl.accuracy_log);
        {
            let [lo, ..] = bits.to_le_bytes();
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add(usize::from(lo >> 3));
            buf >>= u32::from(lo & !7u8);
            bits &= 7;
        }

        buf |= 1u64 << bits;
        let [b0, ..] = buf.to_le_bytes();
        *out_ptr = b0;
        out_ptr = out_ptr.add(1);

        comp_buf.set_len(start_len + out_ptr.offset_from_unsigned(out_start));
    }

    Ok(())
}
