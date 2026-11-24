//! High-throughput, zero-bounds-check LZ77 block match finders (`lz77_scan_fast` and
//! `lz77_scan_default`), Huffman literal encoders, and reverse FSE sequence packers backing
//! `bstd::zstd`.
//!
//! - `lz77_scan_fast`: 64 KiB 10-bit-tagged 7-byte prime hash table (`LZ77_FAST_HASH_LOG = 14`,
//!   half of L1D), 2-position pipelined miss loop with unclamped `step_acc >> 6` recurrence,
//!   backward match extension, and `rep[0]`/`rep[1]` chain harvesting in a zero-call leaf kernel.
//! - `lz77_scan_default`: 1-way 10-bit-tagged Double-Fast (`64K` `hash8` long table = 256 KiB +
//!   `64K` `hash5` short table = 256 KiB, 2 MiB window), sequential 2-position unrolled miss loop
//!   with `step_acc >> 6`, short `ht_long` (`m_ml < 12`) + `ht_short` `pos + 1` lazy upgrade,
//!   backward match extension, in-match table inserts (`ht_short` at `match_start + 1`, both tables
//!   at `match_start + 2` and `pos - 2`), and zero-literal `rep[1]` + 1-literal `rep[0]` chain
//!   harvesting in a zero-call leaf kernel.

use core::alloc::Allocator;
use core::ptr;

use crate::{AllocFailure, Int, SeqOp, Vec};

/// C `zstd` `prime8bytes` / `xxHash64` `PRIME64_1` (`0x9E37_79B1_85EB_CA87`).
const HASH8_PRIME: u64 = 11_400_714_785_074_694_791;
const MASK_5BYTES: u64 = 0x0000_00FF_FFFF_FFFF;
const MASK_7BYTES: u64 = 0x00FF_FFFF_FFFF_FFFF;

/// Low 22 bits of each tagged Double-Fast hash-table `u32` store `pos & WIN_MASK` (4 MiB window).
const WIN_BITS: u32 = 22;
const MAX_WIN_MASK: usize = (1 << WIN_BITS) - 1;
const WIN_MASK_U32: u32 = (1u32 << WIN_BITS) - 1;
/// Top 10 bits store a non-zero fingerprint tag (bit 22 = `1`, bits 23..32 = 9 hash bits).
const TAG_MASK: u32 = !WIN_MASK_U32;
const TAG_NONZERO: u32 = 1u32 << WIN_BITS;

/// Hash table size for [`lz77_scan_fast`]: 16,384 `u32` entries = 64 KiB (half of L1D).
pub const LZ77_FAST_HASH_LOG: u32 = 14;
/// Number of `u32` entries in [`lz77_scan_fast`]'s hash table.
pub const LZ77_FAST_HASH_SIZE: usize = 1 << LZ77_FAST_HASH_LOG;

/// Double-Fast long table (`hash8`, 1-way `u32` entries): 65,536 entries = 256 KiB.
const DFAST_LONG_LOG: u32 = 16;
const DFAST_LONG_U32S: usize = 1 << DFAST_LONG_LOG;
/// Double-Fast short table (`hash5`, 1-way `u32` entries): 32,768 entries = 128 KiB (L1D-resident).
const DFAST_SHORT_LOG: u32 = 15;

/// Number of `u64` words backing [`lz77_scan_default`]'s Double-Fast tables (512 KiB buffer:
/// 256 KiB `ht_long` + 256 KiB `ht_short`).
pub const LZ77_DEF_BUCKETS: usize = 65_536;

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

#[inline(always)]
fn pos_low_nz(pos: usize) -> u32 {
    (pos & MAX_WIN_MASK).truncate_to::<u32>() | TAG_NONZERO
}

/// Single-multiply 5-byte hash returning `(table_index, tagged_entry)`.
///
/// Masks to the low 40 bits (`MASK_5BYTES`, a 1-cycle AArch64 logical immediate) and multiplies
/// by `HASH8_PRIME` so all 5 input bytes avalanche into both the 16-bit bucket index (`48..64`)
/// and the disjoint 9-bit tag (`39..48`), while sharing the single 64-bit multiplier register
/// with [`hash8_tagged`].
#[inline(always)]
fn hash5_tagged(v: u64, low_nz: u32, hash_log: u32) -> (usize, u32) {
    let prod = (v & MASK_5BYTES).wrapping_mul(HASH8_PRIME);
    let idx = (prod >> (64u32 - hash_log)).truncate_to::<usize>();
    let tag = (prod >> 39).truncate_to::<u32>() << 23;
    (idx, low_nz | tag)
}

/// Single-multiply 7-byte hash returning `(table_index, tagged_entry)`.
///
/// Masks to the low 56 bits (`MASK_7BYTES`, a 1-cycle AArch64 logical immediate) and multiplies
/// by `HASH8_PRIME` so all 7 input bytes avalanche into both the 14-bit bucket index (`50..64`)
/// and the disjoint 9-bit tag (`39..48`).
#[inline(always)]
fn hash7_tagged(v: u64, pos: usize, hash_log: u32) -> (usize, u32) {
    let prod = (v & MASK_7BYTES).wrapping_mul(HASH8_PRIME);
    let idx = (prod >> (64u32 - hash_log)).truncate_to::<usize>();
    let tag = (prod >> 39).truncate_to::<u32>() << 23;
    (idx, pos_low_nz(pos) | tag)
}

/// Single-multiply 8-byte hash returning `(table_index, tagged_entry)` (zero masking needed).
#[inline(always)]
fn hash8_tagged(v: u64, low_nz: u32, hash_log: u32) -> (usize, u32) {
    let prod = v.wrapping_mul(HASH8_PRIME);
    let idx = (prod >> (64u32 - hash_log)).truncate_to::<usize>();
    let tag = (prod >> 39).truncate_to::<u32>() << 23;
    (idx, low_nz | tag)
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

/// Zero-call 16/32-byte wildcopy helper for LZ77 literal runs inside leaf scanner kernels.
///
/// Emits a single 16-byte unaligned load/store for the common `len <= 16` case and loops 32 bytes
/// per iteration as a pair of 16-byte unaligned `u128` loads/stores when `len > 16`, fusing into
/// AArch64 `ldp q, q; stp q, q` while keeping `store_size (16) != stride (32)` so LLVM's
/// `LoopIdiomRecognize` never lowers the loop to `bl memcpy` and preserves full leaf-function
/// register allocation.
#[inline(always)]
unsafe fn copy_literals_wild16(src: *const u8, dst: *mut u8, len: usize) {
    // SAFETY: Caller guarantees `src..src + len + 31` (and at least `src..src + 16`) is readable
    // and `dst..dst + len + 31` (and at least `dst..dst + 16`) is writable spare capacity
    // non-overlapping with `src`. When `len > 16`, every loop iteration has `off <= len - 1`, so
    // the paired 16B accesses at `off` and `off + 16` reach at most `off + 32 <= len + 31`.
    unsafe {
        ptr::write_unaligned(dst.cast::<u128>(), ptr::read_unaligned(src.cast::<u128>()));
        if len > 16 {
            let mut off = 16usize;
            while off < len {
                let s = src.add(off);
                let d = dst.add(off);
                let v0 = ptr::read_unaligned(s.cast::<u128>());
                let v1 = ptr::read_unaligned(s.add(16).cast::<u128>());
                ptr::write_unaligned(d.cast::<u128>(), v0);
                ptr::write_unaligned(d.add(16).cast::<u128>(), v1);
                off += 32;
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

/// Pre-populates `buckets` (`ht_long` 1-way `hash8` + `ht_short` 1-way `hash5`) from a
/// raw-content dictionary prefix (`dict`) so subsequent [`lz77_scan_default`] calls over
/// `[dict | input]` match into `dict` without re-hashing the dictionary per frame.
pub fn lz77_preload_default(dict: &[u8], buckets: &mut [u64; LZ77_DEF_BUCKETS]) {
    buckets.fill(0);
    let Some(limit) = dict.len().checked_sub(8) else {
        return;
    };
    let base = dict.as_ptr();
    let ht_long = buckets.as_mut_ptr().cast::<u32>();
    // SAFETY: `buckets` has `LZ77_DEF_BUCKETS = 65_536` `u64`s = `131_072` `u32`s with
    // `u64` align >= `u32`; `ht_long` uses `65_536` `u32`s (`DFAST_LONG_LOG = 16`) and
    // `ht_short` uses `65_536` `u32`s (`DFAST_SHORT_LOG = 16`), totaling
    // `131_072 == LZ77_DEF_BUCKETS * 2` `u32`s. `pos <= dict.len() - 8` gives 8 readable bytes,
    // and `hl`/`hs` are bounded by their respective table logs.
    unsafe {
        let ht_short = ht_long.add(DFAST_LONG_U32S);
        let mut pos = 0usize;
        while pos <= limit {
            let v = read_u64_unaligned(base.add(pos));
            let low_nz = pos_low_nz(pos);
            let (hl, el) = hash8_tagged(v, low_nz, DFAST_LONG_LOG);
            let (hs, es) = hash5_tagged(v, low_nz, DFAST_SHORT_LOG);
            *ht_long.add(hl) = el;
            *ht_short.add(hs) = es;
            pos += 1;
        }
    }
}

/// Zero-call leaf kernel for [`lz77_scan_fast`].
///
/// Keeping allocation (`try_reserve`) and trailing `copy_nonoverlapping` outside this function
/// lets LLVM allocate all loop-carried state across caller-saved + callee-saved AArch64 GPRs
/// without spilling across `bl memcpy`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn lz77_scan_fast_leaf(
    base: *const u8,
    start: usize,
    end: usize,
    win: usize,
    ht: *mut u32,
    rep: &mut [usize; 3],
    lit_start: *mut u8,
    seq_start: *mut SeqOp,
) -> (usize, usize, usize) {
    let mut lit_dst = lit_start;
    let mut seq_dst = seq_start;
    let mut pos = start.max(1);
    let mut lit_anchor = start;
    let mut step_acc: usize = 1 << 6;
    let match_limit = end.saturating_sub(33);
    let [mut r0, mut r1, mut r2] = *rep;

    // SAFETY:
    // - Caller guarantees `base..base + end` is readable (`start <= end <= isize::MAX`),
    //   `ht` points to `LZ77_FAST_HASH_SIZE` `u32`s, `lit_start` has `(end - start) + 48`
    //   writable bytes non-overlapping with `base`, and `seq_start` has `(end - start) / 3 + 32`
    //   writable `SeqOp` slots.
    // - Loop runs only while `1 <= pos < pos1 = pos + (step_acc >> 6) <= match_limit = end - 33`
    //   (never when `end < 35`), giving >= 33 readable bytes at both `pos` and `pos1`
    //   (sufficient for 8-byte unaligned loads, `match_len_ptr` from `+ 8`, and paired 32-byte
    //   wildcopy slack `lit_anchor + lit_len + 31 = hit_pos + 31 <= end - 2 < end`).
    // - `step_acc` starts at `64`, resets to `64` on match emit, and grows by <= 2 per >= 2
    //   bytes advanced, so `step_acc <= 64 + end` and `pos + (step_acc >> 6)` cannot overflow.
    // - `hash7_tagged` indices are top `LZ77_FAST_HASH_LOG` bits, so `h0, h1 < LZ77_FAST_HASH_SIZE`.
    // - For validated candidates, `1 <= off <= hit_pos` and `off <= win`, so
    //   `0 <= prev_pos < hit_pos <= end - 33` also has >= 33 readable bytes.
    // - In the repeat harvester, `rp <= match_limit = end - 33` and `1 <= r <= rp`, keeping
    //   all reads strictly within `base..base + end`.
    // - `lit_dst` writes at most `end - start` committed bytes (+ <= 31B paired wildcopy slack)
    //   within `lit_start`'s `(end - start) + 48` reserved bytes.
    // - Every emitted sequence consumes `match_len >= 4` input bytes, so at most `(end - start) / 4`
    //   `SeqOp`s are written within `seq_start`'s `(end - start) / 3 + 32` reserved slots.
    unsafe {
        let b_end = base.add(end);
        'scan: loop {
            let mut step = step_acc >> 6;
            let mut pos1 = pos + step;
            if pos1 > match_limit {
                break;
            }
            let (mut hit_pos, cur8, prev_entry, next_miss_pos, next_step_acc) = loop {
                let c0 = read_u64_unaligned(base.add(pos));
                let c1 = read_u64_unaligned(base.add(pos1));
                let (h0, e0) = hash7_tagged(c0, pos, LZ77_FAST_HASH_LOG);
                let (h1, e1) = hash7_tagged(c1, pos1, LZ77_FAST_HASH_LOG);

                let p0 = *ht.add(h0);
                let p1 = *ht.add(h1);

                *ht.add(h0) = e0;
                if tag_matches(p0, e0) {
                    break (pos, c0, p0, pos1, step_acc + 1);
                }
                *ht.add(h1) = e1;
                if tag_matches(p1, e1) {
                    break (pos1, c1, p1, pos1 + step, step_acc + 2);
                }
                pos = pos1 + step;
                step_acc += 2;
                step = step_acc >> 6;
                pos1 = pos + step;
                if pos1 > match_limit {
                    break 'scan;
                }
            };

            let off = offset_from_entry(hit_pos, prev_entry);
            if off == 0 || off > hit_pos || off > win {
                pos = next_miss_pos;
                step_acc = next_step_acc;
                continue;
            }
            let mut prev_pos = hit_pos - off;
            let diff = cur8 ^ read_u64_unaligned(base.add(prev_pos));
            if diff.truncate_to::<u32>() != 0 {
                pos = next_miss_pos;
                step_acc = next_step_acc;
                continue;
            }

            let mut ml = if diff != 0 {
                (diff.trailing_zeros() >> 3).widen_to::<usize>()
            } else {
                8 + match_len_ptr(base.add(prev_pos + 8), base.add(hit_pos + 8), b_end)
            };

            // Preserve `lit_len >= 1` on `rep[0]` matches so `ov = 1` stays valid (RFC 8878 §3.1.1.5).
            let back_floor = if off == r0 && hit_pos > lit_anchor {
                lit_anchor + 1
            } else {
                lit_anchor
            };
            while hit_pos > back_floor
                && prev_pos > 0
                && *base.add(hit_pos - 1) == *base.add(prev_pos - 1)
            {
                hit_pos -= 1;
                prev_pos -= 1;
                ml += 1;
            }

            let lit_len = hit_pos - lit_anchor;
            let ov = if lit_len > 0 {
                if off == r0 {
                    1u32
                } else if off == r1 {
                    let t = r0;
                    r0 = r1;
                    r1 = t;
                    2u32
                } else if off == r2 {
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
                }
            } else if off == r1 {
                let t = r0;
                r0 = r1;
                r1 = t;
                1u32
            } else if off == r2 {
                let t = r2;
                r2 = r1;
                r1 = r0;
                r0 = t;
                2u32
            } else if r0 > 1 && off == r0 - 1 {
                r2 = r1;
                r1 = r0;
                r0 = off;
                3u32
            } else {
                r2 = r1;
                r1 = r0;
                r0 = off;
                off.truncate_to::<u32>().wrapping_add(3)
            };

            copy_literals_wild16(base.add(lit_anchor), lit_dst, lit_len);
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
            step_acc = 1 << 6;

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
                        let t = r0;
                        r0 = r1;
                        r1 = t;
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

        *rep = [r0, r1, r2];
        (
            lit_dst.offset_from_unsigned(lit_start),
            seq_dst.offset_from_unsigned(seq_start),
            lit_anchor,
        )
    }
}

/// Fast 10-bit-tagged 7-byte-hash LZ77 block scanner (64 KiB hash table, 512 KiB window,
/// 2-position pipelined miss loop with unclamped `step_acc >> 6` recurrence, backward match
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

    literals_out.try_reserve(block_len.saturating_add(48))?;
    seqs_out.try_reserve(block_len / 3 + 32)?;

    let base = input.as_ptr();
    let ht = hash_table.as_mut_ptr();
    let lit_start = literals_out.as_mut_ptr();
    let seq_start = seqs_out.as_mut_ptr();
    let win = max_window.min(1 << 19).max(1);

    // SAFETY:
    // - `start <= end <= input.len()`, so `base..base + end` is readable.
    // - `ht` points to `LZ77_FAST_HASH_SIZE` mutable `u32`s.
    // - `literals_out` and `seqs_out` were cleared and reserved for `block_len + 48` bytes and
    //   `block_len / 3 + 32` `SeqOp`s, disjoint from `input`.
    // - The leaf returns `lit_written <= lit_anchor - start <= block_len` and
    //   `seq_written <= block_len / 4`, so `lit_written + (end - lit_anchor) <= block_len`
    //   fits in `literals_out` and `seq_written` fits in `seqs_out`.
    unsafe {
        let (lit_written, seq_written, lit_anchor) =
            lz77_scan_fast_leaf(base, start, end, win, ht, rep, lit_start, seq_start);

        let tail_len = end - lit_anchor;
        if tail_len > 0 {
            ptr::copy_nonoverlapping(base.add(lit_anchor), lit_start.add(lit_written), tail_len);
        }

        literals_out.set_len(lit_written + tail_len);
        seqs_out.set_len(seq_written);
    }

    Ok(())
}

#[inline(always)]
unsafe fn insert_dfast_short(base: *const u8, ht_short: *mut u32, pos: usize) {
    // SAFETY: Caller guarantees `pos + 8 <= input.len()` and `ht_short` has
    // `1 << DFAST_SHORT_LOG` entries.
    unsafe {
        let v = read_u64_unaligned(base.add(pos));
        let (hs, es) = hash5_tagged(v, pos_low_nz(pos), DFAST_SHORT_LOG);
        *ht_short.add(hs) = es;
    }
}

#[inline(always)]
unsafe fn insert_dfast_both(
    base: *const u8,
    ht_long: *mut u32,
    ht_short: *mut u32,
    pos: usize,
) {
    // SAFETY: Caller guarantees `pos + 8 <= input.len()`, `ht_long` has `1 << DFAST_LONG_LOG`
    // entries, and `ht_short` has `1 << DFAST_SHORT_LOG` entries.
    unsafe {
        let v = read_u64_unaligned(base.add(pos));
        let low_nz = pos_low_nz(pos);
        let (hl, el) = hash8_tagged(v, low_nz, DFAST_LONG_LOG);
        let (hs, es) = hash5_tagged(v, low_nz, DFAST_SHORT_LOG);
        *ht_long.add(hl) = el;
        *ht_short.add(hs) = es;
    }
}

/// Zero-call leaf kernel for [`lz77_scan_default`].
///
/// Keeping allocation (`try_reserve`) and trailing `copy_nonoverlapping` outside this function
/// lets LLVM allocate all loop-carried state across caller-saved + callee-saved AArch64 GPRs
/// without spilling across `bl memcpy`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn lz77_scan_default_leaf(
    base: *const u8,
    start: usize,
    end: usize,
    win: usize,
    ht_long: *mut u32,
    rep: &mut [usize; 3],
    lit_start: *mut u8,
    seq_start: *mut SeqOp,
) -> (usize, usize, usize) {
    let mut lit_dst = lit_start;
    let mut seq_dst = seq_start;
    let mut pos = start.max(1);
    let mut lit_anchor = start;
    let mut step_acc: usize = 1 << 6;
    let match_limit = end.saturating_sub(33);
    let [mut r0, mut r1, mut r2] = *rep;

    // SAFETY:
    // - Caller guarantees `base..base + end` is readable (`start <= end <= isize::MAX`),
    //   `ht_long` points to `DFAST_LONG_U32S + (1 << DFAST_SHORT_LOG) = 131_072` `u32`s,
    //   `lit_start` has `(end - start) + 48` writable bytes non-overlapping with `base`, and
    //   `seq_start` has `(end - start) / 3 + 32` writable `SeqOp` slots.
    // - Loop runs only while `1 <= pos < pos1 = pos + (step_acc >> 6) <= match_limit = end - 33`
    //   (never when `end < 35`), giving >= 33 readable bytes at both `pos` and `pos1` (and thus
    //   at `hit_pos`), and >= 32 readable bytes at lazy/rep `p1 = hit_pos + 1 <= end - 32`
    //   (sufficient for 8-byte unaligned loads, `match_len_ptr` from `+ 8`, and paired 32-byte
    //   wildcopy slack `lit_anchor + lit_len + 31 = m_pos + 31 <= end - 1 < end`).
    // - `step_acc` starts at `64`, resets to `64` on match emit, and grows by <= 3 per >= 2
    //   bytes advanced while `step_acc < 128` (at most 22 iterations, reaching <= 130) and
    //   thereafter `step = step_acc >> 6 >= 2` so `step_acc` grows by <= 3 per >= 4 bytes (or 1
    //   per >= 2 bytes on false positives). Hence `step_acc <= 130 + end`, `step <= 2 + (end >> 6)`,
    //   and `pos + step` / `pos1 + step` stay `< 2 * end <= usize::MAX` (since `end <= isize::MAX`).
    // - `hash8_tagged`/`hash5_tagged` indices are top `DFAST_LONG_LOG`/`DFAST_SHORT_LOG` bits,
    //   so `hl0, hl1, hl < 1 << DFAST_LONG_LOG` and `hs0, hs1, hs < 1 << DFAST_SHORT_LOG`.
    // - Whenever `rep[0]` is checked at `p1 = hit_pos + 1`, `1 <= r0 <= win && r0 <= p1`, so
    //   `0 <= p1 - r0 <= hit_pos <= end - 33` has >= 33 readable bytes.
    // - For validated candidates, `1 <= off <= hit_pos` (or `<= p1`) and `off <= win`, so
    //   `0 <= prev < hit_pos <= end - 33` (or `0 <= prev1 < p1 <= end - 32`) also has >= 32
    //   readable bytes.
    // - Every `insert_dfast_*` call checks its target position `<= match_limit = end - 33`,
    //   giving >= 33 readable bytes for `read_u64_unaligned`.
    // - In the repeat harvester, `rp <= match_limit = end - 33` and `1 <= r <= rp`, keeping
    //   all reads strictly within `base..base + end`.
    // - `lit_dst` writes at most `end - start` committed bytes (+ <= 31B paired wildcopy slack)
    //   within `lit_start`'s `(end - start) + 48` reserved bytes; `seq_dst` matches
    //   `lz77_scan_fast_leaf`.
    unsafe {
        let ht_short = ht_long.add(DFAST_LONG_U32S);
        let b_end = base.add(end);
        'scan: loop {
            let mut pos1 = pos + (step_acc >> 6);
            if pos1 > match_limit {
                break;
            }
            let (is_long, hit_pos, cur8, entry) = loop {
                // Position 0: keep only 6 temporaries live so all loop state stays in GPRs
                // with zero stack spills.
                let cur8_0 = read_u64_unaligned(base.add(pos));
                let low_nz0 = pos_low_nz(pos);
                let (hl0, el0) = hash8_tagged(cur8_0, low_nz0, DFAST_LONG_LOG);
                let (hs0, es0) = hash5_tagged(cur8_0, low_nz0, DFAST_SHORT_LOG);
                let pl0 = *ht_long.add(hl0);
                let ps0 = *ht_short.add(hs0);
                *ht_long.add(hl0) = el0;
                *ht_short.add(hs0) = es0;
                if tag_matches(pl0, el0) {
                    break (true, pos, cur8_0, pl0);
                }
                if tag_matches(ps0, es0) {
                    break (false, pos, cur8_0, ps0);
                }

                // Position 1: all p0 temporaries are dead; the OOO core speculates past p0's
                // rare-hit checks and overlaps p1's table loads with p0's automatically.
                let cur8_1 = read_u64_unaligned(base.add(pos1));
                let low_nz1 = pos_low_nz(pos1);
                let (hl1, el1) = hash8_tagged(cur8_1, low_nz1, DFAST_LONG_LOG);
                let (hs1, es1) = hash5_tagged(cur8_1, low_nz1, DFAST_SHORT_LOG);
                let pl1 = *ht_long.add(hl1);
                let ps1 = *ht_short.add(hs1);
                *ht_long.add(hl1) = el1;
                *ht_short.add(hs1) = es1;
                if tag_matches(pl1, el1) {
                    break (true, pos1, cur8_1, pl1);
                }
                if tag_matches(ps1, es1) {
                    break (false, pos1, cur8_1, ps1);
                }

                pos = pos1 + (step_acc >> 6);
                step_acc += 3;
                pos1 = pos + (step_acc >> 6);
                if pos1 > match_limit {
                    break 'scan;
                }
            };

            let mut m_pos = hit_pos;
            let mut m_prev = 0usize;
            let mut m_off = 0usize;
            let mut m_ml = 0usize;

            if is_long {
                // 1. Long (`hash8`) hit at `hit_pos`: verify first.
                let off = offset_from_entry(hit_pos, entry);
                if off != 0 && off <= hit_pos && off <= win {
                    let prev = hit_pos - off;
                    let diff = cur8 ^ read_u64_unaligned(base.add(prev));
                    if diff.truncate_to::<u32>() == 0 {
                        m_prev = prev;
                        m_off = off;
                        m_ml = if diff != 0 {
                            (diff.trailing_zeros() >> 3).widen_to::<usize>()
                        } else {
                            8 + match_len_ptr(base.add(prev + 8), base.add(hit_pos + 8), b_end)
                        };
                    }
                }
                if m_off == 0 {
                    // Rare (~0.2%) 9-bit tag collision: advance without carrying `ps`/`es`.
                    pos = hit_pos + (step_acc >> 6);
                    step_acc += 1;
                    continue 'scan;
                }
                // Short `ht_long` hit (`m_ml < 12`): probe `ht_long` at `hit_pos + 1` to catch
                // longer matches starting 1 byte later, and check `rep[0]` only when `m_ml <= 9`
                // so long matches skip a wasted random cache load.
                if m_ml < 12 {
                    let p1 = hit_pos + 1;
                    let next8 = read_u64_unaligned(base.add(p1));
                    let (hl1, el1) = hash8_tagged(next8, pos_low_nz(p1), DFAST_LONG_LOG);
                    let pl1 = *ht_long.add(hl1);
                    *ht_long.add(hl1) = el1;
                    if tag_matches(pl1, el1) {
                        let off1 = offset_from_entry(p1, pl1);
                        if off1 != 0 && off1 <= p1 && off1 <= win {
                            let prev1 = p1 - off1;
                            let diff1 = next8 ^ read_u64_unaligned(base.add(prev1));
                            if diff1.truncate_to::<u32>() == 0 {
                                let ml1 = if diff1 != 0 {
                                    (diff1.trailing_zeros() >> 3).widen_to::<usize>()
                                } else {
                                    8 + match_len_ptr(
                                        base.add(prev1 + 8),
                                        base.add(p1 + 8),
                                        b_end,
                                    )
                                };
                                if ml1 > m_ml + 1 || (ml1 > m_ml && off1 < (m_off >> 3)) {
                                    m_pos = p1;
                                    m_prev = prev1;
                                    m_off = off1;
                                    m_ml = ml1;
                                }
                            }
                        }
                    }
                    if m_ml <= 9
                        && r0 != 0
                        && r0 <= win
                        && r0 <= p1
                        && (m_off != r0 || m_pos == lit_anchor)
                        && read_u32_unaligned(base.add(p1 - r0)) == next8.truncate_to::<u32>()
                    {
                        let rprev = p1 - r0;
                        let rdiff = next8 ^ read_u64_unaligned(base.add(rprev));
                        let rml = if rdiff != 0 {
                            (rdiff.trailing_zeros() >> 3).widen_to::<usize>()
                        } else {
                            8 + match_len_ptr(base.add(rprev + 8), base.add(p1 + 8), b_end)
                        };
                        if rml + 1 >= m_ml {
                            m_pos = p1;
                            m_prev = rprev;
                            m_off = r0;
                            m_ml = rml;
                        }
                    }
                }
            } else {
                // 2. Short (`hash5`, minMatch = 5) hit + single `hit_pos + 1` lazy upgrade
                //    (`ht_long` when `m_ml < 48`, `rep[0]` when `m_ml <= 7`, and `ht_short` when
                //    `m_ml == 5`).
                let off = offset_from_entry(hit_pos, entry);
                if off != 0 && off <= hit_pos && off <= win {
                    let prev = hit_pos - off;
                    let diff = cur8 ^ read_u64_unaligned(base.add(prev));
                    if (diff & MASK_5BYTES) == 0 {
                        m_prev = prev;
                        m_off = off;
                        m_ml = if diff != 0 {
                            (diff.trailing_zeros() >> 3).widen_to::<usize>()
                        } else {
                            8 + match_len_ptr(base.add(prev + 8), base.add(hit_pos + 8), b_end)
                        };
                    }
                }
                if m_off == 0 {
                    // False-positive or <5-byte collision: check `rep[0]` at `hit_pos + 1`
                    // before advancing as a miss.
                    let p1 = hit_pos + 1;
                    if r0 != 0
                        && r0 <= win
                        && r0 <= p1
                        && read_u32_unaligned(base.add(p1 - r0))
                            == (cur8 >> 8).truncate_to::<u32>()
                    {
                        let rprev = p1 - r0;
                        let rdiff =
                            read_u64_unaligned(base.add(p1)) ^ read_u64_unaligned(base.add(rprev));
                        m_pos = p1;
                        m_prev = rprev;
                        m_off = r0;
                        m_ml = if rdiff != 0 {
                            (rdiff.trailing_zeros() >> 3).widen_to::<usize>()
                        } else {
                            8 + match_len_ptr(base.add(rprev + 8), base.add(p1 + 8), b_end)
                        };
                    } else {
                        pos = hit_pos + (step_acc >> 6);
                        step_acc += 1;
                        continue 'scan;
                    }
                } else if m_ml < 48 {
                    let p1 = hit_pos + 1;
                    let next8 = read_u64_unaligned(base.add(p1));
                    let (hl1, el1) = hash8_tagged(next8, pos_low_nz(p1), DFAST_LONG_LOG);
                    let pl1 = *ht_long.add(hl1);
                    *ht_long.add(hl1) = el1;

                    let mut upgraded = false;
                    if tag_matches(pl1, el1) {
                        let off1 = offset_from_entry(p1, pl1);
                        if off1 != 0 && off1 <= p1 && off1 <= win {
                            let prev1 = p1 - off1;
                            let diff1 = next8 ^ read_u64_unaligned(base.add(prev1));
                            if diff1.truncate_to::<u32>() == 0 {
                                let ml1 = if diff1 != 0 {
                                    (diff1.trailing_zeros() >> 3).widen_to::<usize>()
                                } else {
                                    8 + match_len_ptr(
                                        base.add(prev1 + 8),
                                        base.add(p1 + 8),
                                        b_end,
                                    )
                                };
                                if ml1 > m_ml + 1 || (ml1 > m_ml && off1 < (m_off >> 3)) {
                                    m_pos = p1;
                                    m_prev = prev1;
                                    m_off = off1;
                                    m_ml = ml1;
                                    upgraded = true;
                                }
                            }
                        }
                    }

                    if !upgraded
                        && m_ml <= 7
                        && r0 != 0
                        && r0 <= win
                        && r0 <= p1
                        && m_off != r0
                        && m_off != r1
                        && m_off != r2
                        && read_u32_unaligned(base.add(p1 - r0)) == next8.truncate_to::<u32>()
                    {
                        let rprev = p1 - r0;
                        let rdiff = next8 ^ read_u64_unaligned(base.add(rprev));
                        let rml = if rdiff != 0 {
                            (rdiff.trailing_zeros() >> 3).widen_to::<usize>()
                        } else {
                            8 + match_len_ptr(base.add(rprev + 8), base.add(p1 + 8), b_end)
                        };
                        if rml + 1 >= m_ml {
                            m_pos = p1;
                            m_prev = rprev;
                            m_off = r0;
                            m_ml = rml;
                            upgraded = true;
                        }
                    }

                    if !upgraded && m_ml == 5 {
                        let (hs1, es1) = hash5_tagged(next8, pos_low_nz(p1), DFAST_SHORT_LOG);
                        let ps1 = *ht_short.add(hs1);
                        *ht_short.add(hs1) = es1;
                        if tag_matches(ps1, es1) {
                            let off1 = offset_from_entry(p1, ps1);
                            if off1 != 0 && off1 <= p1 && off1 <= win {
                                let prev1 = p1 - off1;
                                let diff1 = next8 ^ read_u64_unaligned(base.add(prev1));
                                if (diff1 & MASK_5BYTES) == 0 {
                                    let ml1 = if diff1 != 0 {
                                        (diff1.trailing_zeros() >> 3).widen_to::<usize>()
                                    } else {
                                        8 + match_len_ptr(
                                            base.add(prev1 + 8),
                                            base.add(p1 + 8),
                                            b_end,
                                        )
                                    };
                                    if ml1 >= m_ml + 2 || (ml1 > m_ml && off1 < (m_off >> 3)) {
                                        m_pos = p1;
                                        m_prev = prev1;
                                        m_off = off1;
                                        m_ml = ml1;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Preserve `lit_len >= 1` on `rep[0]` matches so `ov = 1` stays valid (RFC 8878 §3.1.1.5).
            let back_floor = if m_off == r0 && m_pos > lit_anchor {
                lit_anchor + 1
            } else {
                lit_anchor
            };
            while m_pos > back_floor
                && m_prev > 0
                && *base.add(m_pos - 1) == *base.add(m_prev - 1)
            {
                m_pos -= 1;
                m_prev -= 1;
                m_ml += 1;
            }

            let lit_len = m_pos - lit_anchor;
            let ov = if lit_len > 0 {
                if m_off == r0 {
                    1u32
                } else if m_off == r1 {
                    let t = r0;
                    r0 = r1;
                    r1 = t;
                    2u32
                } else if m_off == r2 {
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
                }
            } else if m_off == r1 {
                let t = r0;
                r0 = r1;
                r1 = t;
                1u32
            } else if m_off == r2 {
                let t = r2;
                r2 = r1;
                r1 = r0;
                r0 = t;
                2u32
            } else if r0 > 1 && m_off == r0 - 1 {
                r2 = r1;
                r1 = r0;
                r0 = m_off;
                3u32
            } else {
                r2 = r1;
                r1 = r0;
                r0 = m_off;
                m_off.truncate_to::<u32>().wrapping_add(3)
            };

            copy_literals_wild16(base.add(lit_anchor), lit_dst, lit_len);
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
            pos = m_pos + m_ml;
            lit_anchor = pos;
            step_acc = 1 << 6;

            // Populate tables inside the match (`ht_short` at `match_start + 1`, plus both
            // tables at `match_start + 2` and `pos - 2`, matching C `zstd_double_fast`).
            if match_start + 1 <= match_limit {
                insert_dfast_short(base, ht_short, match_start + 1);
            }
            if m_ml >= 5 && match_start + 2 <= match_limit {
                insert_dfast_both(base, ht_long, ht_short, match_start + 2);
            }
            if pos <= match_limit && pos >= 2 {
                insert_dfast_both(base, ht_long, ht_short, pos - 2);
            }

            // Harvest zero-literal `rep[1]` ping-pong (at `pos`) and 1-literal `rep[0]` chains
            // (at `pos + 1`).
            loop {
                let mut rp = 0usize;
                let mut rprev = 0usize;
                let mut rdiff = 0u64;
                let mut r_lits = 0usize;
                let mut r_ov = 0u32;

                if pos <= match_limit && r1 != 0 && r1 <= pos && r1 <= win {
                    let rprev0 = pos - r1;
                    let d0 =
                        read_u64_unaligned(base.add(rprev0)) ^ read_u64_unaligned(base.add(pos));
                    if d0.truncate_to::<u32>() == 0 {
                        let t = r0;
                        r0 = r1;
                        r1 = t;
                        rp = pos;
                        rprev = rprev0;
                        rdiff = d0;
                        r_lits = 0;
                        r_ov = 1;
                    }
                }
                if r_ov == 0 && pos + 1 <= match_limit && r0 != 0 && r0 <= pos + 1 && r0 <= win {
                    let rp1 = pos + 1;
                    let rprev1 = rp1 - r0;
                    let d1 =
                        read_u64_unaligned(base.add(rprev1)) ^ read_u64_unaligned(base.add(rp1));
                    if d1.truncate_to::<u32>() == 0 {
                        rp = rp1;
                        rprev = rprev1;
                        rdiff = d1;
                        r_lits = 1;
                        r_ov = 1;
                    }
                }
                if r_ov == 0 {
                    break;
                }

                let rml = if rdiff != 0 {
                    (rdiff.trailing_zeros() >> 3).widen_to::<usize>()
                } else {
                    8 + match_len_ptr(base.add(rprev + 8), base.add(rp + 8), b_end)
                };
                if r_lits == 1 {
                    *lit_dst = *base.add(pos);
                    lit_dst = lit_dst.add(1);
                }
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
                    insert_dfast_both(base, ht_long, ht_short, pos - 2);
                }
            }
        }

        *rep = [r0, r1, r2];
        (
            lit_dst.offset_from_unsigned(lit_start),
            seq_dst.offset_from_unsigned(seq_start),
            lit_anchor,
        )
    }
}

/// Default 1-way 10-bit-tagged Double-Fast (`64K` `hash8` long table = 256 KiB + `64K`
/// `hash5` short table = 256 KiB, 2 MiB window) LZ77 block scanner: sequential 2-position
/// unrolled miss loop with `step_acc >> 6`, short `ht_long` (`m_ml < 12`, `rep[0]` when
/// `m_ml <= 9`) + `ht_short` `hit_pos + 1` lazy check (`ht_long` when `m_ml < 48`, `rep[0]`
/// when `m_ml <= 7`, `ht_short` when `m_ml == 5`), backward extension, in-match table inserts
/// (`ht_short` at `match_start + 1`, both tables at `match_start + 2` and `pos - 2`),
/// zero-literal `rep[1]` + 1-literal `rep[0]` chain harvesting, zero inner-loop bounds checks.
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

    literals_out.try_reserve(block_len.saturating_add(48))?;
    seqs_out.try_reserve(block_len / 3 + 32)?;

    let base = input.as_ptr();
    let ht_long = hash_table.as_mut_ptr().cast::<u32>();
    let lit_start = literals_out.as_mut_ptr();
    let seq_start = seqs_out.as_mut_ptr();
    let win = max_window.min(1 << 21).max(1); // 2 MiB window (matches C zstd `-3`).

    // SAFETY:
    // - `start <= end <= input.len()`, so `base..base + end` is readable.
    // - `hash_table` has `LZ77_DEF_BUCKETS = 65_536` `u64`s = `131_072` `u32`s with `u64`
    //   align >= `u32`; `ht_long` covers `DFAST_LONG_U32S = 65_536` `u32`s and `ht_short`
    //   covers `1 << DFAST_SHORT_LOG = 65_536` `u32`s (`131_072 == LZ77_DEF_BUCKETS * 2`).
    // - `literals_out` and `seqs_out` were cleared and reserved for `block_len + 48` bytes and
    //   `block_len / 3 + 32` `SeqOp`s, disjoint from `input`.
    // - The leaf returns `lit_written <= lit_anchor - start <= block_len` and
    //   `seq_written <= block_len / 4`, so `lit_written + (end - lit_anchor) <= block_len`
    //   fits in `literals_out` and `seq_written` fits in `seqs_out`.
    unsafe {
        let (lit_written, seq_written, lit_anchor) =
            lz77_scan_default_leaf(base, start, end, win, ht_long, rep, lit_start, seq_start);

        let tail_len = end - lit_anchor;
        if tail_len > 0 {
            ptr::copy_nonoverlapping(base.add(lit_anchor), lit_start.add(lit_written), tail_len);
        }

        literals_out.set_len(lit_written + tail_len);
        seqs_out.set_len(seq_written);
    }

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

/// Single-base-pointer packed FSE sequence encoder tables for [`fse_pack_sequences_fast`].
///
/// Consolidating all three `sym_tt` (`delta_nb_bits | (delta_find_state << 32)`) and
/// `+table_size`-biased `state_table` arrays into one 3 KiB struct frees 5 GPRs inside the
/// reverse sequence packing loop so zero table pointers spill on AArch64.
#[repr(C)]
pub struct FseSeqEncTables {
    pub ll_tt: [u64; 64],
    pub of_tt: [u64; 64],
    pub ml_tt: [u64; 64],
    pub ll_st: [u16; 256],
    pub of_st: [u16; 256],
    pub ml_st: [u16; 256],
    pub ll_table_size: u16,
    pub of_table_size: u16,
    pub ml_table_size: u16,
    pub ll_log: u8,
    pub of_log: u8,
    pub ml_log: u8,
}

impl FseSeqEncTables {
    #[inline(always)]
    #[must_use]
    pub const fn new_zeroed() -> Self {
        Self {
            ll_tt: [0u64; 64],
            of_tt: [0u64; 64],
            ml_tt: [0u64; 64],
            ll_st: [0u16; 256],
            of_st: [0u16; 256],
            ml_st: [0u16; 256],
            ll_table_size: 0,
            of_table_size: 0,
            ml_table_size: 0,
            ll_log: 0,
            of_log: 0,
            ml_log: 0,
        }
    }
}

#[inline(always)]
unsafe fn fse_step_raw(
    sym_tt: *const u64,
    state_table: *const u16,
    sym: u8,
    state: u32,
) -> (u32, u32, u32) {
    // SAFETY: Caller guarantees `sym_tt` points to 64 `u64`s and `state_table` points to 256 `u16`s;
    // `sym & 63 < 64` and `idx & 255 < 256`.
    unsafe {
        let packed = *sym_tt.add(usize::from(sym) & 63);
        let delta_nb_bits = packed.truncate_to::<u32>();
        let delta_find_state = (packed >> 32).truncate_to::<u32>();
        let nb = state.wrapping_add(delta_nb_bits) >> 16;
        let bits = state & ((1u32 << nb).wrapping_sub(1));
        let idx = (state >> nb).wrapping_add(delta_find_state) & 255;
        let next = u32::from(*state_table.add(idx.widen_to::<usize>()));
        (next, bits, nb)
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
    tables: &FseSeqEncTables,
    init_ll: u32,
    init_of: u32,
    init_ml: u32,
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
    let t: *const FseSeqEncTables = tables;

    let mut ll_state = init_ll;
    let mut of_state = init_of;
    let mut ml_state = init_ml;

    let mut buf: u64 = 0;
    let mut bits: u32 = 0;

    // SAFETY:
    // - `comp_buf` has `n * 11 + 48` reserved bytes (each sequence emits <= 11 bytes + 8B wild write).
    // - `seq_ptr` is walked backwards over `0..n - 1`.
    // - `t` points to `tables: &FseSeqEncTables`, valid and immutable for the entire call; each
    //   `*_tt` has 64 `u64`s and each `*_st` has 256 `u16`s.
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
            let sp = seq_ptr.add(i);
            let ll_len = (*sp).lit_len;
            let ml_len = (*sp).match_len;
            let off = (*sp).offset;
            let (ll_code, ll_extra, ll_b) = encode_ll_lut_raw(ll_len, ll_lut_ptr);
            let (ml_code, ml_extra, ml_b) = encode_ml_lut_raw(ml_len, ml_lut_ptr);
            let of_b = 31u32 - off.max(1).leading_zeros();
            let of_code = of_b.truncate_to::<u8>();
            let of_extra = off.wrapping_sub(1u32 << of_b);

            let (next_of, of_st_bits, of_st_nb) =
                fse_step_raw((*t).of_tt.as_ptr(), (*t).of_st.as_ptr(), of_code, of_state);
            let (next_ml, ml_st_bits, ml_st_nb) =
                fse_step_raw((*t).ml_tt.as_ptr(), (*t).ml_st.as_ptr(), ml_code, ml_state);
            let (next_ll, ll_st_bits, ll_st_nb) =
                fse_step_raw((*t).ll_tt.as_ptr(), (*t).ll_st.as_ptr(), ll_code, ll_state);
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

        let ml_final = ml_state.wrapping_sub(u32::from((*t).ml_table_size));
        let of_final = of_state.wrapping_sub(u32::from((*t).of_table_size));
        let ll_final = ll_state.wrapping_sub(u32::from((*t).ll_table_size));
        buf |= u64::from(ml_final) << bits;
        bits += u32::from((*t).ml_log);
        buf |= u64::from(of_final) << bits;
        bits += u32::from((*t).of_log);
        buf |= u64::from(ll_final) << bits;
        bits += u32::from((*t).ll_log);
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
