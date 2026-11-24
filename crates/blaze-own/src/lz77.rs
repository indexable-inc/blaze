//! High-throughput, zero-bounds-check LZ77 block match finders (`lz77_scan_fast` and
//! `lz77_scan_default`), Huffman literal encoders, and reverse FSE sequence packers backing
//! `bstd::zstd`.
//!
//! - `lz77_scan_fast`: 64 KiB L1D-resident 10-bit-tagged 7-byte prime hash table
//!   (`LZ77_FAST_HASH_LOG = 14`), 2-position pipelined miss loop with stride `1 + ((pos - anchor) >> 6).min(7)`,
//!   backward match extension, and `rep[0]`/`rep[1]` chain harvesting.
//! - `lz77_scan_default`: hybrid Double-Fast (`128K` 10-bit-tagged `hash7` long table + `16K`
//!   2-way SWAR `hash4` short table), 2-position pipelined miss loop with `kSearchStrength = 7`
//!   acceleration, `pos + 1` lazy override when `ml < 36`, and `rep[0]`/`rep[1]` chain harvesting.

use core::alloc::Allocator;
use core::ptr;

use crate::{AllocFailure, Int, SeqOp, Vec};

const HASH4_PRIME: u64 = 2_654_435_761;
const HASH7_PRIME: u64 = 58_295_818_150_454_627;
const MASK_4BYTES: u64 = 0x0000_0000_FFFF_FFFF;
const MASK_5BYTES: u64 = 0x0000_00FF_FFFF_FFFF;
const MASK_7BYTES: u64 = 0x00FF_FFFF_FFFF_FFFF;

/// Low 22 bits of each tagged Double-Fast hash-table `u32` store `pos & WIN_MASK` (4 MiB window).
const WIN_BITS: u32 = 22;
const MAX_WIN_MASK: usize = (1 << WIN_BITS) - 1;
const WIN_MASK_U32: u32 = (1u32 << WIN_BITS) - 1;
/// Top 10 bits store a non-zero fingerprint tag (bit 22 = `1`, bits 23..32 = 9 hash bits).
const TAG_MASK: u32 = !WIN_MASK_U32;
const TAG_NONZERO: u32 = 1u32 << WIN_BITS;
const TAG_PAIR_MASK: u64 = 0xFFC0_0000_FFC0_0000;

/// Hash table size for [`lz77_scan_fast`]: 16,384 `u32` entries = 64 KiB (fits in half of L1D).
pub const LZ77_FAST_HASH_LOG: u32 = 14;
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
    let idx = (prod >> (64u32 - hash_log)).truncate_to::<usize>();
    let tag = ((prod >> 10).truncate_to::<u32>() & TAG_MASK) | TAG_NONZERO;
    let low = (pos & MAX_WIN_MASK).truncate_to::<u32>();
    (idx, tag | low)
}

/// Single-multiply 7-byte hash returning `(table_index, tagged_entry)`.
#[inline(always)]
fn hash7_tagged(v: u64, pos: usize, hash_log: u32) -> (usize, u32) {
    let prod = (v & MASK_7BYTES).wrapping_mul(HASH7_PRIME);
    let idx = (prod >> (64u32 - hash_log)).truncate_to::<usize>();
    let tag = ((prod >> 10).truncate_to::<u32>() & TAG_MASK) | TAG_NONZERO;
    let low = (pos & MAX_WIN_MASK).truncate_to::<u32>();
    (idx, tag | low)
}

#[inline(always)]
const fn tag_matches(entry: u32, new_entry: u32) -> bool {
    ((entry ^ new_entry) & TAG_MASK) == 0
}

#[inline(always)]
fn offset_from_entry(pos: usize, entry: u32) -> usize {
    let low = (entry & WIN_MASK_U32).widen_to::<usize>();
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
                let tz = (diff.to_le().trailing_zeros() >> 3).widen_to::<usize>();
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

/// Pre-populates `hash_table` from a raw-content dictionary prefix (`dict`) so subsequent
/// [`lz77_scan_fast`] calls over `[dict | input]` (starting at `block_start = dict.len()`)
/// match into `dict` without re-hashing the dictionary per frame.
pub fn lz77_preload_fast(dict: &[u8], hash_table: &mut [u32; LZ77_FAST_HASH_SIZE]) {
    hash_table.fill(0);
    let Some(limit) = dict.len().checked_sub(8) else {
        return;
    };
    let base = dict.as_ptr();
    let ht = hash_table.as_mut_ptr();
    // SAFETY: `pos <= dict.len() - 8` gives 8 readable bytes at `base.add(pos)`;
    // `h < LZ77_FAST_HASH_SIZE` by `hash7_tagged`'s top-`LZ77_FAST_HASH_LOG` shift.
    unsafe {
        let mut pos = 0usize;
        while pos <= limit {
            let v = read_u64_unaligned(base.add(pos));
            let (h, e) = hash7_tagged(v, pos, LZ77_FAST_HASH_LOG);
            *ht.add(h) = e;
            pos += 1;
        }
    }
}

/// Pre-populates `buckets` (`ht_short` 2-way `hash4` + `ht_long` `hash7`) from a raw-content
/// dictionary prefix (`dict`) so subsequent [`lz77_scan_default`] calls over `[dict | input]`
/// match into `dict` without re-hashing the dictionary per frame.
pub fn lz77_preload_default(dict: &[u8], buckets: &mut [u64; LZ77_DEF_BUCKETS]) {
    buckets.fill(0);
    let Some(limit) = dict.len().checked_sub(8) else {
        return;
    };
    let base = dict.as_ptr();
    let ht_short = buckets.as_mut_ptr();
    // SAFETY: `LZ77_DEF_BUCKETS == DFAST_SHORT_U64S + DFAST_LONG_U64S`, `u64` align >= `u32`,
    // `pos <= dict.len() - 8` gives 8 readable bytes, and `hl`/`bs` are bounded by table logs.
    unsafe {
        let ht_long = ht_short.add(DFAST_SHORT_U64S).cast::<u32>();
        let mut pos = 0usize;
        while pos <= limit {
            let v = read_u64_unaligned(base.add(pos));
            let (hl, el) = hash7_tagged(v, pos, DFAST_LONG_LOG);
            let (bs, es) = hash4_tagged(v, pos, DFAST_SHORT_LOG);
            *ht_long.add(hl) = el;
            let ps = *ht_short.add(bs);
            *ht_short.add(bs) = (ps << 32) | u64::from(es);
            pos += 1;
        }
    }
}

/// Fast 10-bit-tagged 7-byte-hash LZ77 block scanner (64 KiB L1D hash table, 512 KiB window,
/// 2-position pipelined miss loop with stride `1 + ((pos - anchor) >> 6).min(7)`, backward match
/// extension, `rep[0]`/`rep[1]` chain harvesting, zero inner-loop bounds checks).
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
    let mut lit_dst = literals_out.as_mut_ptr();
    let mut seq_dst = seqs_out.as_mut_ptr();

    let mut pos = start.max(1);
    let mut lit_anchor = start;
    let match_limit = end.saturating_sub(32);
    let win = max_window.min(1 << 19).max(1);
    let [mut r0, mut r1, mut r2] = *rep;

    // SAFETY:
    // - Loop runs only while `1 <= pos <= match_limit = end - 32` (never when `end < 33`).
    // - `step = 1 + ((pos - lit_anchor) >> 6).min(7)` satisfies `1 <= step <= 8`, so
    //   `pos1 = pos + step <= end - 24`, giving >= 24 readable bytes at `pos` and `pos1`
    //   (sufficient for 8-byte unaligned loads, `match_len_ptr` from `+ 8`, and 16-byte
    //   wildcopy slack from `lit_anchor <= hit_pos <= end - 24`).
    // - `hash7_tagged` indices are top `LZ77_FAST_HASH_LOG` bits, so `h0, h1 < LZ77_FAST_HASH_SIZE`.
    // - For validated candidates, `1 <= off <= hit_pos` and `off <= win`, so
    //   `0 <= prev_pos < hit_pos <= end - 24` also has >= 24 readable bytes.
    // - In the repeat harvester, `rp <= match_limit = end - 32` and `1 <= r <= rp`, keeping
    //   all reads strictly within `base..base + end`.
    // - `lit_dst` writes at most `block_len` committed bytes (+ <= 16B wildcopy slack) within
    //   `literals_out`'s reserved `block_len + 32` capacity.
    // - Every emitted sequence consumes `match_len >= 4` input bytes, so at most `block_len / 4`
    //   `SeqOp`s are written within `seqs_out`'s reserved `block_len / 3 + 32` capacity.
    unsafe {
        let b_end = base.add(end);
        'scan: while pos <= match_limit {
            let (mut hit_pos, cur8, prev_entry, next_miss_pos) = loop {
                let step = 1usize + ((pos - lit_anchor) >> 6).min(7);
                let pos1 = pos + step;

                let c0 = read_u64_unaligned(base.add(pos));
                let c1 = read_u64_unaligned(base.add(pos1));
                let (h0, e0) = hash7_tagged(c0, pos, LZ77_FAST_HASH_LOG);
                let (h1, e1) = hash7_tagged(c1, pos1, LZ77_FAST_HASH_LOG);

                let p0 = *ht.add(h0);
                let p1 = *ht.add(h1);

                *ht.add(h0) = e0;
                if tag_matches(p0, e0) {
                    break (pos, c0, p0, pos1);
                }
                *ht.add(h1) = e1;
                if tag_matches(p1, e1) {
                    break (pos1, c1, p1, pos1 + step);
                }
                pos = pos1 + step;
                if pos > match_limit {
                    break 'scan;
                }
            };

            let off = offset_from_entry(hit_pos, prev_entry);
            if off == 0 || off > hit_pos || off > win {
                pos = next_miss_pos;
                continue;
            }
            let mut prev_pos = hit_pos - off;
            let diff = cur8 ^ read_u64_unaligned(base.add(prev_pos));
            if diff.truncate_to::<u32>() != 0 {
                pos = next_miss_pos;
                continue;
            }

            let mut ml = if diff != 0 {
                (diff.trailing_zeros() >> 3).widen_to::<usize>()
            } else {
                8 + match_len_ptr(base.add(prev_pos + 8), base.add(hit_pos + 8), b_end)
            };

            while hit_pos > lit_anchor
                && prev_pos > 0
                && *base.add(hit_pos - 1) == *base.add(prev_pos - 1)
            {
                hit_pos -= 1;
                prev_pos -= 1;
                ml += 1;
            }

            let lit_len = hit_pos - lit_anchor;
            let ov = if lit_len > 0 && off == r0 {
                1u32
            } else if lit_len > 0 && off == r1 {
                core::mem::swap(&mut r0, &mut r1);
                2u32
            } else if lit_len > 0 && off == r2 {
                let t = r2;
                r2 = r1;
                r1 = r0;
                r0 = t;
                3u32
            } else {
                r2 = r1;
                r1 = r0;
                r0 = off;
                off.truncate_to::<u32>().wrapping_add(3)
            };

            copy_lits_wild_ptr(base.add(lit_anchor), lit_dst, lit_len);
            lit_dst = lit_dst.add(lit_len);
            ptr::write(
                seq_dst,
                SeqOp {
                    lit_len: lit_len.truncate_to::<u32>(),
                    match_len: ml.truncate_to::<u32>(),
                    offset: ov,
                },
            );
            seq_dst = seq_dst.add(1);

            pos = hit_pos + ml;
            lit_anchor = pos;

            if pos <= match_limit && pos >= 2 {
                let tail8 = read_u64_unaligned(base.add(pos - 2));
                let (th, te) = hash7_tagged(tail8, pos - 2, LZ77_FAST_HASH_LOG);
                *ht.add(th) = te;
            }

            // Harvest `rep[0]` (at `pos + 1`) and `rep[1]` (at `pos + 2`) chains (zero hash lookups).
            loop {
                let mut rp = 0usize;
                let mut rprev = 0usize;
                let mut rdiff = 0u64;
                let mut r_lits = 0usize;
                let mut r_ov = 0u32;

                if pos + 1 <= match_limit && r0 != 0 && r0 <= pos + 1 && r0 <= win {
                    let rp0 = pos + 1;
                    let rprev0 = rp0 - r0;
                    let d0 = read_u64_unaligned(base.add(rprev0))
                        ^ read_u64_unaligned(base.add(rp0));
                    if d0.truncate_to::<u32>() == 0 {
                        rp = rp0;
                        rprev = rprev0;
                        rdiff = d0;
                        r_lits = 1;
                        r_ov = 1;
                    }
                }
                if r_lits == 0 && pos + 2 <= match_limit && r1 != 0 && r1 <= pos + 2 && r1 <= win {
                    let rp1 = pos + 2;
                    let rprev1 = rp1 - r1;
                    let d1 = read_u64_unaligned(base.add(rprev1))
                        ^ read_u64_unaligned(base.add(rp1));
                    if d1.truncate_to::<u32>() == 0 {
                        core::mem::swap(&mut r0, &mut r1);
                        rp = rp1;
                        rprev = rprev1;
                        rdiff = d1;
                        r_lits = 2;
                        r_ov = 2;
                    }
                }
                if r_lits == 0 {
                    break;
                }

                let rml = if rdiff != 0 {
                    (rdiff.trailing_zeros() >> 3).widen_to::<usize>()
                } else {
                    8 + match_len_ptr(base.add(rprev + 8), base.add(rp + 8), b_end)
                };
                if r_lits == 1 {
                    *lit_dst = *base.add(pos);
                } else {
                    ptr::write_unaligned(
                        lit_dst.cast::<u16>(),
                        ptr::read_unaligned(base.add(pos).cast::<u16>()),
                    );
                }
                lit_dst = lit_dst.add(r_lits);
                ptr::write(
                    seq_dst,
                    SeqOp {
                        lit_len: r_lits.truncate_to::<u32>(),
                        match_len: rml.truncate_to::<u32>(),
                        offset: r_ov,
                    },
                );
                seq_dst = seq_dst.add(1);
                pos = rp + rml;
                lit_anchor = pos;
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

        let new_lit_len = lit_dst.offset_from_unsigned(literals_out.as_mut_ptr());
        literals_out.set_len(new_lit_len);
        let new_seq_len = seq_dst.offset_from_unsigned(seqs_out.as_mut_ptr());
        seqs_out.set_len(new_seq_len);
    }

    *rep = [r0, r1, r2];
    Ok(())
}

#[inline(always)]
unsafe fn insert_dfast_both(
    base: *const u8,
    ht_short: *mut u64,
    ht_long: *mut u32,
    pos: usize,
) {
    // SAFETY: Caller guarantees `pos + 8 <= input.len()`, `ht_short` has `DFAST_SHORT_U64S`
    // entries, and `ht_long` has `1 << DFAST_LONG_LOG` entries.
    unsafe {
        let v = read_u64_unaligned(base.add(pos));
        let (hl, el) = hash7_tagged(v, pos, DFAST_LONG_LOG);
        let (bs, es) = hash4_tagged(v, pos, DFAST_SHORT_LOG);
        *ht_long.add(hl) = el;
        let pair = *ht_short.add(bs);
        *ht_short.add(bs) = (pair << 32) | u64::from(es);
    }
}

/// Validate one tagged Double-Fast match candidate `entry` at `hit_pos`, returning
/// `(prev_pos, offset, diff)` on a valid >=4-byte (or >=5-byte when far) match, or `(0, 0, 0)`
/// on miss (`offset == 0` unambiguously signals miss because valid offsets are `>= 1`).
#[inline(always)]
unsafe fn probe_match_cand(
    base: *const u8,
    hit_pos: usize,
    cur8: u64,
    entry: u32,
    win: usize,
    require_5_if_far: bool,
) -> (usize, usize, u64) {
    let off = offset_from_entry(hit_pos, entry);
    if off == 0 || off > hit_pos || off > win {
        return (0, 0, 0);
    }
    let prev_u = hit_pos - off;
    // SAFETY: Caller guarantees `hit_pos + 8 <= input.len()` and `1 <= off <= hit_pos`,
    // so `prev_u < hit_pos` also has >= 8 readable bytes.
    let diff = cur8 ^ unsafe { read_u64_unaligned(base.add(prev_u)) };
    let smask = if require_5_if_far && off >= 65_536 {
        MASK_5BYTES
    } else {
        MASK_4BYTES
    };
    if (diff & smask) != 0 {
        return (0, 0, 0);
    }
    (prev_u, off, diff)
}

/// Default hybrid 10-bit-tagged Double-Fast (`128K` `hash7` long table + `16K` 2-way SWAR `hash4`
/// short table) LZ77 block scanner: 2-position pipelined miss loop with `kSearchStrength = 7`
/// acceleration, long-match priority, `pos + 1` lazy override when `ml < 36`, repeat-offset chain
/// harvesting, backward extension, zero inner-loop bounds checks.
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

    let mut lit_dst = literals_out.as_mut_ptr();
    let mut seq_dst = seqs_out.as_mut_ptr();

    let mut pos = start.max(1);
    let mut lit_anchor = start;
    let match_limit = end.saturating_sub(32);
    let win = max_window.min(1 << 21).max(1); // 2 MiB window (matches C zstd `-3`).
    let [mut r0, mut r1, mut r2] = *rep;

    // SAFETY:
    // - Loop runs only while `1 <= pos <= match_limit = end - 32` (never when `end < 33`).
    // - `step = 1 + ((pos - lit_anchor) >> 7).min(5)` satisfies `1 <= step <= 6`, so
    //   `pos1 = pos + step <= end - 26` and lazy `p1 = m_pos + 1 <= end - 25`, giving >= 25
    //   readable bytes (sufficient for 8-byte loads, `match_len_ptr` from `+ 8`, and 16-byte
    //   wildcopy slack from `lit_anchor <= m_pos <= end - 25`).
    // - `hash7_tagged`/`hash4_tagged` indices are bounded by `DFAST_LONG_LOG`/`DFAST_SHORT_LOG`.
    // - Output buffer capacities match `lz77_scan_fast`.
    unsafe {
        let b_end = base.add(end);
        'scan: while pos <= match_limit {
            // 2-position pipelined probe of `ht_long` (`hash7`) + 2-way `ht_short` (`hash4`):
            // overlaps L2 + L1D table loads across `pos` and `pos1` on misses.
            let (hit_pos, cur8, pl, el, pair_s, x_short, next_miss_pos) = loop {
                let step = 1usize + ((pos - lit_anchor) >> 7).min(5);
                let pos1 = pos + step;

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
                if tag_matches(pl0, el0)
                    | (x0.truncate_to::<u32>() == 0)
                    | ((x0 >> 32) == 0)
                {
                    break (pos, c0, pl0, el0, ps0, x0, pos1);
                }

                let es1_u64 = u64::from(es1);
                *ht_long.add(hl1) = el1;
                *ht_short.add(bs1) = (ps1 << 32) | es1_u64;

                let after2 = pos1 + step;
                let tp1 = (es1_u64 << 32) | es1_u64;
                let x1 = (ps1 ^ tp1) & TAG_PAIR_MASK;
                if tag_matches(pl1, el1)
                    | (x1.truncate_to::<u32>() == 0)
                    | ((x1 >> 32) == 0)
                {
                    break (pos1, c1, pl1, el1, ps1, x1, after2);
                }

                pos = after2;
                if pos > match_limit {
                    break 'scan;
                }
            };

            let mut m_prev = 0usize;
            let mut m_off = 0usize;
            let mut m_diff = 0u64;

            if tag_matches(pl, el) {
                (m_prev, m_off, m_diff) =
                    probe_match_cand(base, hit_pos, cur8, pl, win, false);
            }
            if m_off == 0 && x_short.truncate_to::<u32>() == 0 {
                (m_prev, m_off, m_diff) = probe_match_cand(
                    base,
                    hit_pos,
                    cur8,
                    pair_s.truncate_to::<u32>(),
                    win,
                    true,
                );
            }
            if m_off == 0 && (x_short >> 32) == 0 {
                (m_prev, m_off, m_diff) = probe_match_cand(
                    base,
                    hit_pos,
                    cur8,
                    (pair_s >> 32).truncate_to::<u32>(),
                    win,
                    true,
                );
            }

            if m_off == 0 {
                pos = next_miss_pos;
                continue;
            }

            let mut m_pos = hit_pos;
            let mut m_ml = if m_diff != 0 {
                (m_diff.trailing_zeros() >> 3).widen_to::<usize>()
            } else {
                8 + match_len_ptr(base.add(m_prev + 8), base.add(hit_pos + 8), b_end)
            };

            // Double-Fast `m_pos + 1` lazy override: check `ht_long` when `m_ml < 36`,
            // and `ht_short` (2-way tagged) when `m_ml < 12` to beat C zstd `-3` ratio.
            if m_ml < 36 {
                let p1 = m_pos + 1;
                let next8 = read_u64_unaligned(base.add(p1));
                let (hl1, el1) = hash7_tagged(next8, p1, DFAST_LONG_LOG);
                let pl1 = *ht_long.add(hl1);
                *ht_long.add(hl1) = el1;

                let mut l_prev = 0usize;
                let mut l_off = 0usize;
                let mut l_diff = 0u64;
                let mut l_need_plus = 0usize;

                if tag_matches(pl1, el1) {
                    (l_prev, l_off, l_diff) =
                        probe_match_cand(base, p1, next8, pl1, win, false);
                }
                if l_off == 0 && m_ml < 12 {
                    let (bs1, es1) = hash4_tagged(next8, p1, DFAST_SHORT_LOG);
                    let ps1 = *ht_short.add(bs1);
                    let es1_u64 = u64::from(es1);
                    *ht_short.add(bs1) = (ps1 << 32) | es1_u64;
                    let tp1 = (es1_u64 << 32) | es1_u64;
                    let x1 = (ps1 ^ tp1) & TAG_PAIR_MASK;
                    if x1.truncate_to::<u32>() == 0 {
                        (l_prev, l_off, l_diff) = probe_match_cand(
                            base,
                            p1,
                            next8,
                            ps1.truncate_to::<u32>(),
                            win,
                            true,
                        );
                    }
                    if l_off == 0 && (x1 >> 32) == 0 {
                        (l_prev, l_off, l_diff) = probe_match_cand(
                            base,
                            p1,
                            next8,
                            (ps1 >> 32).truncate_to::<u32>(),
                            win,
                            true,
                        );
                    }
                    l_need_plus = 1;
                }
                if l_off != 0 {
                    let ml1 = if l_diff != 0 {
                        (l_diff.trailing_zeros() >> 3).widen_to::<usize>()
                    } else {
                        8 + match_len_ptr(base.add(l_prev + 8), base.add(p1 + 8), b_end)
                    };
                    if ml1 > m_ml + l_need_plus {
                        m_pos = p1;
                        m_prev = l_prev;
                        m_off = l_off;
                        m_ml = ml1;
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
                m_off.truncate_to::<u32>().wrapping_add(3)
            };

            copy_lits_wild_ptr(base.add(lit_anchor), lit_dst, lit_len);
            lit_dst = lit_dst.add(lit_len);
            ptr::write(
                seq_dst,
                SeqOp {
                    lit_len: lit_len.truncate_to::<u32>(),
                    match_len: m_ml.truncate_to::<u32>(),
                    offset: ov,
                },
            );
            seq_dst = seq_dst.add(1);

            let match_start = m_pos;
            pos += m_ml;
            lit_anchor = pos;
            if match_start + 2 < pos && match_start + 2 <= match_limit {
                insert_dfast_both(base, ht_short, ht_long, match_start + 2);
            }
            if pos <= match_limit && pos >= 2 {
                insert_dfast_both(base, ht_short, ht_long, pos - 2);
            }

            // Harvest `rep[0]` (at `pos + 1`) and `rep[1]` (at `pos + 2`) chains.
            loop {
                let mut rp = 0usize;
                let mut rprev = 0usize;
                let mut rdiff = 0u64;
                let mut r_lits = 0usize;
                let mut r_ov = 0u32;

                if pos + 1 <= match_limit && r0 != 0 && r0 <= pos + 1 && r0 <= win {
                    let rp0 = pos + 1;
                    let rprev0 = rp0 - r0;
                    let d0 = read_u64_unaligned(base.add(rprev0))
                        ^ read_u64_unaligned(base.add(rp0));
                    if d0.truncate_to::<u32>() == 0 {
                        rp = rp0;
                        rprev = rprev0;
                        rdiff = d0;
                        r_lits = 1;
                        r_ov = 1;
                    }
                }
                if r_lits == 0 && pos + 2 <= match_limit && r1 != 0 && r1 <= pos + 2 && r1 <= win {
                    let rp1 = pos + 2;
                    let rprev1 = rp1 - r1;
                    let d1 = read_u64_unaligned(base.add(rprev1))
                        ^ read_u64_unaligned(base.add(rp1));
                    if d1.truncate_to::<u32>() == 0 {
                        core::mem::swap(&mut r0, &mut r1);
                        rp = rp1;
                        rprev = rprev1;
                        rdiff = d1;
                        r_lits = 2;
                        r_ov = 2;
                    }
                }
                if r_lits == 0 {
                    break;
                }

                let rml = if rdiff != 0 {
                    (rdiff.trailing_zeros() >> 3).widen_to::<usize>()
                } else {
                    8 + match_len_ptr(base.add(rprev + 8), base.add(rp + 8), b_end)
                };
                if r_lits == 1 {
                    *lit_dst = *base.add(pos);
                } else {
                    ptr::write_unaligned(
                        lit_dst.cast::<u16>(),
                        ptr::read_unaligned(base.add(pos).cast::<u16>()),
                    );
                }
                lit_dst = lit_dst.add(r_lits);
                ptr::write(
                    seq_dst,
                    SeqOp {
                        lit_len: r_lits.truncate_to::<u32>(),
                        match_len: rml.truncate_to::<u32>(),
                        offset: r_ov,
                    },
                );
                seq_dst = seq_dst.add(1);
                pos = rp + rml;
                lit_anchor = pos;
                if pos <= match_limit && pos >= 2 {
                    insert_dfast_both(base, ht_short, ht_long, pos - 2);
                }
            }
        }

        let tail_len = end - lit_anchor;
        if tail_len > 0 {
            ptr::copy_nonoverlapping(base.add(lit_anchor), lit_dst, tail_len);
            lit_dst = lit_dst.add(tail_len);
        }

        let new_lit_len = lit_dst.offset_from_unsigned(literals_out.as_mut_ptr());
        literals_out.set_len(new_lit_len);
        let new_seq_len = seq_dst.offset_from_unsigned(seqs_out.as_mut_ptr());
        seqs_out.set_len(new_seq_len);
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
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
            buf >>= bits & !7u32;
            bits &= 7;
        }

        let mut p = body_len;
        while p >= 4 {
            p -= 4;
            let w = read_u32_unaligned(src_base.add(p));
            let e3 = *codes_ptr.add((w >> 24).widen_to::<usize>());
            let e2 = *codes_ptr.add(((w >> 16) & 0xFF).widen_to::<usize>());
            let e1 = *codes_ptr.add(((w >> 8) & 0xFF).widen_to::<usize>());
            let e0 = *codes_ptr.add((w & 0xFF).widen_to::<usize>());

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

            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
            buf >>= bits & !7u32;
            bits &= 7;
        }

        // Write sentinel `1` bit and flush final byte.
        buf |= 1u64 << bits;
        *out_ptr = buf.truncate_to::<u8>();
        out_ptr = out_ptr.add(1);

        dst.set_len(start_len + out_ptr.offset_from_unsigned(out_start));
    }

    Ok(())
}

/// Zero-bounds-check reverse Huffman literal stream encoder that returns `Ok(false)` (leaving
/// `dst.len()` unchanged) if any symbol in `segment` has `num_bits == 0` in `packed_codes`.
///
/// Enables zero-histogram `Treeless` Huffman encoding in `Fast` mode by attempting the previous
/// block's Huffman table directly and falling back only on an unseen literal byte.
#[inline(never)]
pub fn huff_encode_stream_checked_raw<A: Allocator>(
    segment: &[u8],
    packed_codes: &[u32; 256],
    dst: &mut Vec<u8, A>,
) -> Result<bool, AllocFailure> {
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

    // SAFETY: Same buffer, table, and capacity invariants as `huff_encode_stream_raw`. On early
    // `Ok(false)` return, `dst.set_len` has not been called so `dst.len()` remains `start_len`.
    unsafe {
        let mut t = segment.len();
        while t > body_len {
            t -= 1;
            let e = *codes_ptr.add(usize::from(*src_base.add(t)));
            let nb = e >> 16;
            if nb == 0 {
                return Ok(false);
            }
            let code = u64::from(e & 0xFFFF);
            buf |= code << bits;
            bits += nb;
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
            buf >>= bits & !7u32;
            bits &= 7;
        }

        let mut p = body_len;
        while p >= 4 {
            p -= 4;
            let w = read_u32_unaligned(src_base.add(p));
            let e3 = *codes_ptr.add((w >> 24).widen_to::<usize>());
            let e2 = *codes_ptr.add(((w >> 16) & 0xFF).widen_to::<usize>());
            let e1 = *codes_ptr.add(((w >> 8) & 0xFF).widen_to::<usize>());
            let e0 = *codes_ptr.add((w & 0xFF).widen_to::<usize>());

            let n3 = e3 >> 16;
            let n2 = e2 >> 16;
            let n1 = e1 >> 16;
            let n0 = e0 >> 16;
            if (n0 == 0) | (n1 == 0) | (n2 == 0) | (n3 == 0) {
                return Ok(false);
            }

            let p32 = u64::from(e3 & 0xFFFF) | (u64::from(e2 & 0xFFFF) << n3);
            let p10 = u64::from(e1 & 0xFFFF) | (u64::from(e0 & 0xFFFF) << n1);
            let n32 = n3 + n2;
            let n10 = n1 + n0;

            buf |= p32 << bits;
            buf |= p10 << (bits + n32);
            bits += n32 + n10;

            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
            buf >>= bits & !7u32;
            bits &= 7;
        }

        // Write sentinel `1` bit and flush final byte.
        buf |= 1u64 << bits;
        *out_ptr = buf.truncate_to::<u8>();
        out_ptr = out_ptr.add(1);

        dst.set_len(start_len + out_ptr.offset_from_unsigned(out_start));
    }

    Ok(true)
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
unsafe fn fse_step_raw(
    meta: *const u64,
    state_table: *const u8,
    table_size: u32,
    sym: u8,
    target_state: u8,
) -> (u8, u32, u32) {
    // SAFETY: Caller guarantees `meta` points to 64 `u64`s and `state_table` points to 256 `u8`s;
    // `sym & 63 < 64` and `idx & 255 < 256`.
    unsafe {
        let m = *meta.add(usize::from(sym) & 63);
        let offset = (m & 0xFFFF).truncate_to::<usize>();
        let count = ((m >> 16) & 0xFFFF).truncate_to::<usize>();
        let split = ((m >> 32) & 0xFFFF).truncate_to::<u32>();
        let max_nb = ((m >> 48) & 0xFF).truncate_to::<u32>();
        let target = u32::from(target_state);
        let nb = max_nb.wrapping_sub(u32::from(target < split));
        let bits = target & ((1u32 << nb) - 1);
        let u = (target + table_size) >> nb;
        let idx = offset
            .wrapping_add(u.widen_to::<usize>())
            .wrapping_sub(count)
            & 255;
        let prev = *state_table.add(idx);
        (prev, bits, nb)
    }
}

#[inline(always)]
unsafe fn encode_ll_lut_raw(lit_len: u32, ll_lut: *const u32) -> (u8, u32, u32) {
    if lit_len < 64 {
        // SAFETY: `lit_len < 64` is within `ll_lut: &[u32; 64]`.
        let p = unsafe { *ll_lut.add(lit_len.widen_to::<usize>()) };
        return (p.truncate_to::<u8>(), lit_len.wrapping_sub(p >> 16), (p >> 8) & 0xFF);
    }
    let bits = 31u32 - lit_len.leading_zeros();
    let base = 1u32 << bits;
    ((bits + 19).truncate_to::<u8>(), lit_len - base, bits)
}

#[inline(always)]
unsafe fn encode_ml_lut_raw(match_len: u32, ml_lut: *const u32) -> (u8, u32, u32) {
    let adj = match_len.wrapping_sub(3);
    if adj < 128 {
        // SAFETY: `adj < 128` is within `ml_lut: &[u32; 128]`.
        let p = unsafe { *ml_lut.add(adj.widen_to::<usize>()) };
        return (p.truncate_to::<u8>(), match_len.wrapping_sub(p >> 16), (p >> 8) & 0xFF);
    }
    let bits = 31u32 - adj.leading_zeros();
    let base = (1u32 << bits) + 3;
    ((bits + 36).truncate_to::<u8>(), match_len - base, bits)
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

    let ll_meta = ll_tbl.meta.as_ptr();
    let ll_st = ll_tbl.state_table.as_ptr();
    let ll_ts = u32::from(ll_tbl.table_size);
    let of_meta = of_tbl.meta.as_ptr();
    let of_st = of_tbl.state_table.as_ptr();
    let of_ts = u32::from(of_tbl.table_size);
    let ml_meta = ml_tbl.meta.as_ptr();
    let ml_st = ml_tbl.state_table.as_ptr();
    let ml_ts = u32::from(ml_tbl.table_size);

    let mut ll_state = init_ll;
    let mut of_state = init_of;
    let mut ml_state = init_ml;

    let mut buf: u64 = 0;
    let mut bits: u32 = 0;

    // SAFETY:
    // - `comp_buf` has `n * 11 + 48` reserved bytes (each sequence emits <= 11 bytes + 8B wild write).
    // - `seq_ptr` is walked backwards over `0..n - 1`.
    // - `ll_meta`/`of_meta`/`ml_meta` point to `[u64; 64]` and `ll_st`/`of_st`/`ml_st` to `[u8; 256]`.
    unsafe {
        let mut out_ptr = comp_buf.as_mut_ptr().add(start_len);

        // Emit last sequence's extra bits.
        buf |= u64::from(last_ll_extra) << bits;
        bits += last_ll_bits;
        buf |= u64::from(last_ml_extra) << bits;
        bits += last_ml_bits;
        ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
        out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
        buf >>= bits & !7u32;
        bits &= 7;

        buf |= u64::from(last_of_extra) << bits;
        bits += last_of_bits;
        ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
        out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
        buf >>= bits & !7u32;
        bits &= 7;

        let mut i = n - 1;
        while i > 0 {
            i -= 1;
            let raw = *seq_ptr.add(i);
            let (ll_code, ll_extra, ll_b) = encode_ll_lut_raw(raw.lit_len, ll_lut_ptr);
            let (ml_code, ml_extra, ml_b) = encode_ml_lut_raw(raw.match_len, ml_lut_ptr);
            let of_b = 31u32 - raw.offset.max(1).leading_zeros();
            let of_code = of_b.truncate_to::<u8>();
            let of_extra = raw.offset - (1u32 << of_b);

            let (next_of, of_st_bits, of_st_nb) =
                fse_step_raw(of_meta, of_st, of_ts, of_code, of_state);
            let (next_ml, ml_st_bits, ml_st_nb) =
                fse_step_raw(ml_meta, ml_st, ml_ts, ml_code, ml_state);
            let (next_ll, ll_st_bits, ll_st_nb) =
                fse_step_raw(ll_meta, ll_st, ll_ts, ll_code, ll_state);
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
                ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
                out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
                buf >>= bits & !7u32;
                bits &= 7;
            }
            buf |= u64::from(ml_extra) << bits;
            bits += ml_b;
            buf |= u64::from(of_extra) << bits;
            bits += of_b;
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
            buf >>= bits & !7u32;
            bits &= 7;
        }

        buf |= u64::from(ml_state) << bits;
        bits += u32::from(ml_tbl.accuracy_log);
        buf |= u64::from(of_state) << bits;
        bits += u32::from(of_tbl.accuracy_log);
        buf |= u64::from(ll_state) << bits;
        bits += u32::from(ll_tbl.accuracy_log);
        ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
        out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
        buf >>= bits & !7u32;
        bits &= 7;

        buf |= 1u64 << bits;
        *out_ptr = buf.truncate_to::<u8>();
        out_ptr = out_ptr.add(1);

        let new_len = out_ptr.offset_from_unsigned(comp_buf.as_mut_ptr());
        comp_buf.set_len(new_len);
    }

    Ok(())
}
